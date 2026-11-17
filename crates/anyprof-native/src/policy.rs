//! Decisions the macOS sampler must get right, tested on every platform.
//!
//! The Mach calls live in `macos`. These functions are the rules those calls
//! follow: which threads count as on-CPU, how a `task_threads` buffer is
//! released, how many times `thread_resume` is retried, how a return address
//! is turned into a symbol, and when a pointer-authentication strip is allowed.
//! On Windows and Linux nothing in the sampler calls them, so `dead_code` is
//! allowed there. The unit tests in this file are the callers.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// `thread_get_state` flag: pc, lr, sp, and fp are already raw addresses.
///
/// Darwin calls this `__DARWIN_ARM_THREAD_STATE64_FLAGS_NO_PTRAUTH` (`0x1`).
pub(crate) const NO_PTRAUTH: u32 = 0x1;

/// `thread_get_state` flag: `lr` is signed with the IB key, not the IA key.
///
/// Darwin calls this `__DARWIN_ARM_THREAD_STATE64_FLAGS_IB_SIGNED_LR` (`0x2`).
/// `__darwin_arm_thread_state64_ptrauth_strip` leaves that register alone.
pub(crate) const IB_SIGNED_LR: u32 = 0x2;

/// `pc` was signed with a kernel key.
///
/// Darwin calls this `__DARWIN_ARM_THREAD_STATE64_FLAGS_KERNEL_SIGNED_PC`
/// (`0x4`). The strip macro does not branch on it: `ptrauth_strip` removes the
/// bits without authenticating, so a kernel signature cannot trap. The flag
/// is cleared only after the value is raw, which matters when writing a
/// thread state back. Sampling only reads.
pub(crate) const KERNEL_SIGNED_PC: u32 = 0x4;

/// `lr` was signed with a kernel key.
///
/// Darwin calls this `__DARWIN_ARM_THREAD_STATE64_FLAGS_KERNEL_SIGNED_LR`
/// (`0x8`). Same rule as [`KERNEL_SIGNED_PC`]: strip, do not authenticate.
pub(crate) const KERNEL_SIGNED_LR: u32 = 0x8;

/// Most threads `task_threads` is allowed to hand the sampler.
const SAMPLE_MAX: u32 = 100_000;

/// Largest `task_threads` array whose length is still safe to walk.
const CLEANUP_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// What to do with the array `task_threads` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadArrayPlan {
    /// A null list or a zero count. There are no send rights, and a zero count
    /// does not prove an allocation size, so nothing is unmapped.
    Nothing,
    /// Copy `count` ports, sample them, and free only the array.
    Sample { count: u32 },
    /// Too many threads to sample. The byte size is still proven, so every
    /// send right and the array are released, then the sample fails.
    Release { count: u32 },
    /// `count` does not name a bounded region. Reading or unmapping it could
    /// touch unrelated memory, so the array is left in place and the failure
    /// is reported.
    Unprovable { count: u32 },
}

/// Classify a `task_threads` result before any port is read.
pub(crate) fn plan_thread_array(
    list_is_null: bool,
    count: u32,
    port_bytes: u32,
) -> ThreadArrayPlan {
    if list_is_null || count == 0 || port_bytes == 0 {
        return ThreadArrayPlan::Nothing;
    }
    let Some(bytes) = u64::from(count).checked_mul(u64::from(port_bytes)) else {
        return ThreadArrayPlan::Unprovable { count };
    };
    if bytes > CLEANUP_MAX_BYTES {
        return ThreadArrayPlan::Unprovable { count };
    }
    if count <= SAMPLE_MAX {
        ThreadArrayPlan::Sample { count }
    } else {
        ThreadArrayPlan::Release { count }
    }
}

/// Threads that were actually running.
///
/// `every` is the full thread list, including sleepers. An empty `running`
/// slice is an idle tick: those sleepers are not reported as on-CPU samples.
pub(crate) fn on_cpu_threads<'a, T>(running: &'a [T], every: &'a [T]) -> &'a [T] {
    let _ = every;
    running
}

/// Call `op` until it returns `success`, at most `attempts` times.
///
/// The `Ok` value is how many calls succeeded on the last try's attempt
/// number (1-based). `Err` is the last non-success code. The macOS sampler
/// uses this for `thread_resume` and does not sleep between attempts: the
/// target thread is still suspended.
pub(crate) fn retry_until_success(
    attempts: u32,
    success: i32,
    mut op: impl FnMut() -> i32,
) -> Result<u32, i32> {
    let mut last = success;
    for n in 1..=attempts {
        last = op();
        if last == success {
            return Ok(n);
        }
    }
    Err(last)
}

/// Address passed to the symbol table.
///
/// The leaf program counter is the instruction that was running. A return
/// address points at the instruction after the call, so one byte back lands
/// in the call and stays inside the caller when the return address is the
/// first byte of the next symbol. x86 and arm64 both use that one-byte step.
pub(crate) fn symbol_lookup_address(addr: u64, leaf: bool) -> u64 {
    if leaf { addr } else { addr.saturating_sub(1) }
}

/// Which Darwin strip instruction a signed pointer needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StripKind {
    /// `xpaci` / `ptrauth_strip(..., ptrauth_key_process_independent_code)`.
    Code,
    /// `xpacd` / `ptrauth_strip(..., ptrauth_key_process_independent_data)`.
    Data,
}

/// Whether Darwin's strip macro would clear a signature.
///
/// `__darwin_arm_thread_state64_ptrauth_strip` uses `ptrauth_strip` (the
/// `xpaci` / `xpacd` instructions), never `ptrauth_auth_*`. Authentication
/// can trap on a bad signature. `NO_PTRAUTH` means the kernel already stored
/// raw addresses. An IB-signed link register is left as-is because the IA
/// strip would not be the matching key. `KERNEL_SIGNED_PC` and
/// `KERNEL_SIGNED_LR` do not skip the strip.
pub(crate) fn should_strip(flags: u32, kind: StripKind, is_lr: bool) -> bool {
    let _ = kind;
    // Kernel-signed values are still stripped. Those flags tell a writer
    // where the signature came from; they do not mean the address is raw.
    let flags = flags & !(KERNEL_SIGNED_PC | KERNEL_SIGNED_LR);
    if flags & NO_PTRAUTH != 0 {
        return false;
    }
    if is_lr && flags & IB_SIGNED_LR != 0 {
        return false;
    }
    true
}

/// Flags for a frame-pointer slot loaded from the target's stack.
///
/// `__DARWIN_ARM_THREAD_STATE64_FLAGS_NO_PTRAUTH` describes pc, lr, sp, and fp
/// in the thread-state registers. It does not describe memory. An arm64
/// process can be inside an arm64e system function whose saved frame pointer
/// and return address are still signed. Those slots are stripped with
/// `xpacd` and `xpaci` even when the registers were already raw. The strip is
/// the identity on an unsigned pointer, so an arm64 user frame is unchanged.
/// `IB_SIGNED_LR` applies to the `lr` register, not to a spilled return address.
pub(crate) fn stack_strip_flags() -> u32 {
    0
}

/// A `task_threads` list that was too large to sample.
///
/// The ports were released. The sampler stops the recording; it does not
/// continue with later ticks.
pub(crate) fn too_many_threads(count: u32) -> String {
    format!(
        "task_threads returned {count} threads; the ports were released and the profile stopped"
    )
}

/// Strip `addr` when [`should_strip`] says so.
///
/// `hw` is `xpaci` or `xpacd` where those instructions exist. Callers without
/// them pass a function that returns the address unchanged; a signed pointer
/// is then left intact rather than masked with a guessed bit width.
pub(crate) fn strip_pointer(
    addr: u64,
    flags: u32,
    kind: StripKind,
    is_lr: bool,
    hw: impl FnOnce(u64, StripKind) -> u64,
) -> u64 {
    if addr == 0 || !should_strip(flags, kind, is_lr) {
        addr
    } else {
        hw(addr, kind)
    }
}

/// Error for `task_for_pid` when the kernel refuses the attach.
pub(crate) fn task_for_pid_denied(pid: u32, kr: i32) -> String {
    format!(
        "cannot sample pid {pid}: task_for_pid failed (kern return {kr}). \
         sudo anyprof rec -p {pid} can sample an ordinary process. \
         It does not grant access to a process protected by System Integrity Protection \
         or a hardened runtime. anyprof will not change those protections."
    )
}

/// Sampling this process can suspend the thread that would resume it.
pub(crate) fn reject_self_sample(target: u32, current: u32) -> Result<(), &'static str> {
    if target == current {
        Err(
            "refusing to sample this anyprof process: suspending its own threads can leave it stuck. Profile a different pid.",
        )
    } else {
        Ok(())
    }
}

/// `dyld_v1` files are the shared cache, not one Mach-O image.
pub(crate) fn is_dyld_shared_cache(bytes: &[u8]) -> bool {
    bytes.starts_with(b"dyld_v1")
}

/// `MACH_PORT_NULL` is not a send right. Deallocating it is not required and
/// is skipped so a zero entry cannot be treated as a live port.
pub(crate) fn should_deallocate_send_right(port: u32) -> bool {
    port != 0
}

#[cfg(test)]
mod tests {
    use super::{
        IB_SIGNED_LR, KERNEL_SIGNED_LR, KERNEL_SIGNED_PC, NO_PTRAUTH, StripKind, ThreadArrayPlan,
        is_dyld_shared_cache, on_cpu_threads, plan_thread_array, reject_self_sample,
        retry_until_success, should_deallocate_send_right, should_strip, stack_strip_flags,
        strip_pointer, symbol_lookup_address, task_for_pid_denied, too_many_threads,
    };

    #[test]
    fn thread_array_plans_cover_the_leak_paths() {
        assert_eq!(plan_thread_array(true, 4, 4), ThreadArrayPlan::Nothing);
        assert_eq!(plan_thread_array(false, 0, 4), ThreadArrayPlan::Nothing);
        assert_eq!(
            plan_thread_array(false, 3, 4),
            ThreadArrayPlan::Sample { count: 3 }
        );
        assert_eq!(
            plan_thread_array(false, 100_001, 4),
            ThreadArrayPlan::Release { count: 100_001 }
        );
        assert_eq!(
            plan_thread_array(false, u32::MAX, 4),
            ThreadArrayPlan::Unprovable { count: u32::MAX }
        );
        assert_eq!(plan_thread_array(false, 10, 0), ThreadArrayPlan::Nothing);
    }

    #[test]
    fn an_idle_tick_does_not_sample_sleeping_threads() {
        let sleeping = [1u32, 2, 3];
        let idle: [u32; 0] = [];
        assert!(on_cpu_threads(&idle, &sleeping).is_empty());
        assert_eq!(on_cpu_threads(&[2], &sleeping), &[2]);
    }

    #[test]
    fn resume_retries_then_reports_the_last_failure() {
        let mut calls = 0;
        let ok = retry_until_success(3, 0, || {
            calls += 1;
            if calls < 3 { 5 } else { 0 }
        });
        assert_eq!(ok, Ok(3));
        assert_eq!(calls, 3);

        let mut calls = 0;
        let err = retry_until_success(3, 0, || {
            calls += 1;
            5
        });
        assert_eq!(err, Err(5));
        assert_eq!(calls, 3);

        let mut calls = 0;
        let once = retry_until_success(3, 0, || {
            calls += 1;
            0
        });
        assert_eq!(once, Ok(1));
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_return_address_on_a_symbol_boundary_stays_in_the_caller() {
        let boundary = 0x1040_0200u64;
        assert_eq!(symbol_lookup_address(boundary, true), boundary);
        assert_eq!(symbol_lookup_address(boundary, false), boundary - 1);
        assert_eq!(symbol_lookup_address(0, false), 0);
    }

    #[test]
    fn pac_flags_choose_strip_without_authenticating() {
        let signed = 0x00F0_0001_0000_1000u64;
        assert!(!should_strip(NO_PTRAUTH, StripKind::Code, false));
        assert!(!should_strip(NO_PTRAUTH, StripKind::Data, false));
        assert!(should_strip(0, StripKind::Code, false));
        assert!(should_strip(0, StripKind::Data, false));
        assert!(!should_strip(IB_SIGNED_LR, StripKind::Code, true));
        assert!(should_strip(IB_SIGNED_LR, StripKind::Code, false));
        assert!(should_strip(KERNEL_SIGNED_PC, StripKind::Code, false));
        assert!(should_strip(KERNEL_SIGNED_LR, StripKind::Code, true));
        assert!(should_strip(
            KERNEL_SIGNED_PC | KERNEL_SIGNED_LR,
            StripKind::Data,
            false
        ));
        assert!(!should_strip(
            NO_PTRAUTH | KERNEL_SIGNED_PC,
            StripKind::Code,
            false
        ));

        let mut calls = 0;
        let kept = strip_pointer(signed, NO_PTRAUTH, StripKind::Code, false, |addr, kind| {
            calls += 1;
            let _ = (addr, kind);
            1
        });
        assert_eq!(kept, signed);
        assert_eq!(calls, 0);

        let stripped = strip_pointer(signed, 0, StripKind::Data, false, |addr, kind| {
            calls += 1;
            assert_eq!(kind, StripKind::Data);
            addr & 0x0000_0000_ffff_ffff
        });
        assert_eq!(stripped, 0x1000);
        assert_eq!(calls, 1);

        let lr = strip_pointer(signed, IB_SIGNED_LR, StripKind::Code, true, |addr, _| {
            calls += 1;
            addr
        });
        assert_eq!(lr, signed);
        assert_eq!(calls, 1);

        let zero = strip_pointer(0, 0, StripKind::Code, false, |_, _| {
            calls += 1;
            9
        });
        assert_eq!(zero, 0);
        assert_eq!(calls, 1);
    }

    #[test]
    fn spilled_frames_are_stripped_when_the_registers_are_already_raw() {
        let signed = 0x00F0_0001_0000_1000u64;
        let stack_flags = stack_strip_flags();
        assert_eq!(stack_flags & NO_PTRAUTH, 0);
        assert!(!should_strip(NO_PTRAUTH, StripKind::Code, false));
        assert!(should_strip(stack_flags, StripKind::Code, false));
        assert!(should_strip(stack_flags, StripKind::Data, false));

        let kept = strip_pointer(signed, NO_PTRAUTH, StripKind::Code, false, |_, _| {
            panic!("a raw register must not be stripped");
        });
        assert_eq!(kept, signed);

        let mut calls = 0;
        let stripped = strip_pointer(signed, stack_flags, StripKind::Code, false, |addr, kind| {
            calls += 1;
            assert_eq!(kind, StripKind::Code);
            addr & 0x0000_0000_ffff_ffff
        });
        assert_eq!(calls, 1);
        assert_eq!(stripped, 0x1000);

        let frame = strip_pointer(signed, stack_flags, StripKind::Data, false, |addr, kind| {
            assert_eq!(kind, StripKind::Data);
            addr & 0x0000_0000_ffff_ffff
        });
        assert_eq!(frame, 0x1000);
    }

    #[test]
    fn too_many_threads_stops_the_profile() {
        let text = too_many_threads(100_001);
        assert!(text.contains("100001"), "{text}");
        assert!(text.contains("profile stopped"), "{text}");
        assert!(!text.to_lowercase().contains("skipped"), "{text}");
    }

    #[test]
    fn permission_text_does_not_ask_to_turn_protections_off() {
        let text = task_for_pid_denied(42, 5);
        assert!(text.contains("sudo anyprof rec -p 42"), "{text}");
        assert!(text.contains("System Integrity Protection"), "{text}");
        assert!(text.contains("hardened runtime"), "{text}");
        let lower = text.to_lowercase();
        assert!(!lower.contains("csrutil"), "{text}");
        assert!(!lower.contains("disable"), "{text}");
    }

    #[test]
    fn sampling_the_current_process_is_refused() {
        let err = reject_self_sample(10, 10).unwrap_err();
        assert!(err.contains("suspending"), "{err}");
        assert!(reject_self_sample(11, 10).is_ok());
    }

    #[test]
    fn dyld_shared_cache_magic_is_recognized() {
        assert!(is_dyld_shared_cache(b"dyld_v1  arm64e"));
        assert!(!is_dyld_shared_cache(b"\xcf\xfa\xed\xfe"));
        assert!(!is_dyld_shared_cache(b"dyld"));
    }

    #[test]
    fn a_null_mach_port_is_not_deallocated() {
        assert!(!should_deallocate_send_right(0));
        assert!(should_deallocate_send_right(1));
    }
}
