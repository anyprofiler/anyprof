//! Decisions the samplers must get right, tested on every platform.
//!
//! The Mach calls live in `macos`, the perf calls in `linux`, and the Win32
//! calls in `windows`. These functions are the rules those calls follow:
//! which threads count as on-CPU, how a `task_threads` buffer is released,
//! how many times a resume is retried, how a return address is turned into a
//! symbol, when a pointer-authentication strip is allowed, and what a
//! permission error tells the user. A helper that one platform does not call
//! is still called by the tests in this file.
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

/// `IMAGE_FILE_MACHINE_UNKNOWN`. A process that is not running under WOW64.
pub(crate) const IMAGE_FILE_MACHINE_UNKNOWN: u16 = 0;
/// `IMAGE_FILE_MACHINE_I386`.
pub(crate) const IMAGE_FILE_MACHINE_I386: u16 = 0x014c;
/// `IMAGE_FILE_MACHINE_AMD64`.
pub(crate) const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
/// `IMAGE_FILE_MACHINE_ARMNT`.
pub(crate) const IMAGE_FILE_MACHINE_ARMNT: u16 = 0x01c4;
/// `IMAGE_FILE_MACHINE_ARM64`.
pub(crate) const IMAGE_FILE_MACHINE_ARM64: u16 = 0xaa64;

/// Whether `IsWow64Process2` describes an x64 process.
///
/// A native x64 process reports an unknown process machine and an AMD64 host.
/// An x64 process emulated on an ARM64 host reports AMD64 as the process
/// machine. A native ARM64 process reports an unknown process machine and an
/// ARM64 host. `IsWow64Process` cannot tell that last case from x64.
#[cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn windows_process_is_x64(process_machine: u16, native_machine: u16) -> bool {
    match process_machine {
        IMAGE_FILE_MACHINE_AMD64 => true,
        IMAGE_FILE_MACHINE_UNKNOWN => native_machine == IMAGE_FILE_MACHINE_AMD64,
        _ => false,
    }
}

/// Short name for a process `IsWow64Process2` says is not x64.
#[cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn windows_target_kind(process_machine: u16, native_machine: u16) -> &'static str {
    match process_machine {
        IMAGE_FILE_MACHINE_I386 => "32-bit",
        IMAGE_FILE_MACHINE_ARMNT => "32-bit ARM",
        IMAGE_FILE_MACHINE_ARM64 => "ARM64",
        IMAGE_FILE_MACHINE_UNKNOWN => match native_machine {
            IMAGE_FILE_MACHINE_ARM64 => "ARM64",
            IMAGE_FILE_MACHINE_I386 => "32-bit",
            _ => "not x64",
        },
        _ => "not x64",
    }
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
         Run 'anyprof setup' once to sign anyprof with debugger entitlements, \
         or use 'sudo anyprof rec -p {pid}' to sample an ordinary process. \
         Neither grants access to a process protected by System Integrity Protection \
         or a hardened runtime without debuggable entitlements. anyprof will not change those protections."
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

/// What `perf_event_open` was refused, and the sysctls that explain why.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinuxDenial {
    /// Target pid.
    pub pid: u32,
    /// `errno` from the syscall.
    pub errno: i32,
    /// `kernel.perf_event_paranoid`, when it could be read.
    pub paranoid: Option<i32>,
    /// `kernel.yama.ptrace_scope`, when Yama is enabled.
    pub ptrace_scope: Option<i32>,
    /// Uid that owns the target, when `/proc/<pid>/status` could be read.
    pub target_uid: Option<u32>,
    /// Uid of this process.
    pub self_uid: u32,
    /// The user passed `-k`.
    pub kernel: bool,
}

/// Permission error for a refused `perf_event_open`.
///
/// The text names the sysctl values that were actually read. anyprof does not
/// change them. User-space samples and `-k` are not the same requirement.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_perf_denied(denial: LinuxDenial) -> String {
    let errno_name = match denial.errno {
        1 => "EPERM",
        13 => "EACCES",
        22 => "EINVAL",
        other => {
            let _ = other;
            "error"
        }
    };
    let mut parts = Vec::new();
    parts.push(format!(
        "cannot sample pid {}: perf_event_open failed ({errno_name}, errno {}).",
        denial.pid, denial.errno
    ));
    if let Some(target) = denial.target_uid
        && target != denial.self_uid
    {
        parts.push(format!(
            "pid {} is uid {target}; this process is uid {}. Sampling another user's process needs root or CAP_SYS_PTRACE.",
            denial.pid, denial.self_uid
        ));
    }
    match denial.paranoid {
        Some(value) if value >= 3 => parts.push(format!(
            "kernel.perf_event_paranoid is {value}. Values of 3 or more block perf_event_open for an unprivileged process, including its own children. `sudo sysctl kernel.perf_event_paranoid=1` allows per-process sampling, or run `sudo anyprof rec -p {}` (CAP_PERFMON or CAP_SYS_ADMIN).",
            denial.pid
        )),
        Some(value) if value >= 2 && denial.kernel => parts.push(format!(
            "kernel.perf_event_paranoid is {value}. User-space samples of a same-user process are allowed, but -k needs kernel samples, which need paranoid of 1 or less, or root (CAP_PERFMON or CAP_SYS_ADMIN)."
        )),
        Some(value) => parts.push(format!(
            "kernel.perf_event_paranoid is {value}. User-space samples of a same-user process do not need root at this setting. -k needs paranoid of 1 or less, or root."
        )),
        None => parts.push("kernel.perf_event_paranoid could not be read.".to_string()),
    }
    match denial.ptrace_scope {
        Some(0) => parts.push(
            "kernel.yama.ptrace_scope is 0. That sysctl limits ptrace attach. perf_event_open does not use the attach check, so same-user sampling follows perf_event_paranoid.".to_string(),
        ),
        Some(1) => parts.push(
            "kernel.yama.ptrace_scope is 1. ptrace attach is limited to descendants. perf_event_open does not use that attach check, so an unrelated process of the same user can still be sampled when perf_event_paranoid allows it.".to_string(),
        ),
        Some(2) => parts.push(
            "kernel.yama.ptrace_scope is 2. ptrace attach needs CAP_SYS_PTRACE. perf_event_open of a same-user process does not use that attach check.".to_string(),
        ),
        Some(3) => parts.push(
            "kernel.yama.ptrace_scope is 3. ptrace attach is disabled. perf_event_open of a same-user process does not use that attach check.".to_string(),
        ),
        Some(other) => parts.push(format!("kernel.yama.ptrace_scope is {other}.")),
        None => parts.push(
            "Yama ptrace_scope is not enabled. Same-user sampling follows the classic ptrace rule.".to_string(),
        ),
    }
    parts.push(
        "anyprof does not change these sysctls, and it does not write the target's memory or registers.".to_string(),
    );
    parts.join(" ")
}

/// `PROTECTION_LEVEL_NONE` from the Windows SDK.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const PROTECTION_LEVEL_NONE: u32 = 0xFFFF_FFFE;

/// Whether this process could enable `SeDebugPrivilege`.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DebugPrivilege {
    /// The token does not have the privilege.
    NotHeld,
    /// `AdjustTokenPrivileges` enabled it.
    Enabled,
}

/// Permission error for a refused `OpenProcess` on Windows.
///
/// A protected process is not the same failure as a missing privilege.
/// anyprof does not change protection or integrity.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_open_denied(
    pid: u32,
    os_error: u32,
    protection_level: Option<u32>,
    privilege: DebugPrivilege,
) -> String {
    if let Some(level) = protection_level
        && level != PROTECTION_LEVEL_NONE
    {
        return format!(
            "cannot sample pid {pid}: the process is protected (protection level {level:#x}). \
             SeDebugPrivilege does not grant access to a protected process. \
             anyprof will not change that protection."
        );
    }
    if os_error == 87 {
        return format!(
            "cannot sample pid {pid}: OpenProcess failed because the process is not running"
        );
    }
    let privilege = match privilege {
        DebugPrivilege::NotHeld => "This process does not hold SeDebugPrivilege.",
        DebugPrivilege::Enabled => {
            "SeDebugPrivilege was enabled and the process is still inaccessible."
        }
    };
    format!(
        "cannot sample pid {pid}: OpenProcess failed (access denied, error {os_error}). \
         A same-user, same-integrity process can be sampled without elevation. \
         Another user's process, or a higher-integrity process, needs an elevated Administrator token with SeDebugPrivilege. \
         {privilege} anyprof will not change process protection or integrity."
    )
}

#[cfg_attr(
    not(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(windows, target_arch = "x86_64"),
    )),
    allow(dead_code)
)]
pub(crate) struct SampleCollector {
    unique: Vec<(crate::python::CountedStack, u64)>,
    index: std::collections::HashMap<crate::python::CountedStack, u32>,
    timed: Vec<(u64, u64, u32)>,
    thread_names: std::collections::HashMap<u64, Option<String>>,
    cap: usize,
    capped: bool,
}

#[cfg_attr(
    not(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(windows, target_arch = "x86_64"),
    )),
    allow(dead_code)
)]
impl SampleCollector {
    pub(crate) fn new() -> Self {
        Self::with_cap(anyprof_core::MAX_TIMED_SAMPLES)
    }

    pub(crate) fn with_cap(cap: usize) -> Self {
        Self {
            unique: Vec::new(),
            index: std::collections::HashMap::new(),
            timed: Vec::new(),
            thread_names: std::collections::HashMap::new(),
            cap,
            capped: false,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.unique.is_empty()
    }

    pub(crate) fn record(
        &mut self,
        tid: u64,
        offset_ns: u64,
        key: crate::python::CountedStack,
        lookup_name: impl FnOnce(u64) -> Option<String>,
    ) {
        let uid = if let Some(&id) = self.index.get(&key) {
            self.unique[id as usize].1 = self.unique[id as usize].1.saturating_add(1);
            id
        } else {
            let id = self.unique.len() as u32;
            self.index.insert(key.clone(), id);
            self.unique.push((key, 1));
            id
        };
        if self.capped {
            return;
        }
        if self.timed.len() < self.cap {
            if tid != 0 && !self.thread_names.contains_key(&tid) {
                let cleaned = lookup_name(tid)
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty());
                self.thread_names.insert(tid, cleaned);
            }
            self.timed.push((tid, offset_ns, uid));
        } else {
            self.capped = true;
            self.timed.clear();
            self.thread_names.clear();
        }
    }

    pub(crate) fn finish(
        mut self,
        mut to_frames: impl FnMut(crate::python::CountedStack) -> Vec<anyprof_core::Frame>,
    ) -> (Vec<anyprof_core::Stack>, Vec<anyprof_core::TimedSample>) {
        let count = self.unique.len();
        let mut indexed: Vec<(usize, anyprof_core::Stack)> = self
            .unique
            .into_iter()
            .enumerate()
            .map(|(uid, (key, weight))| {
                (
                    uid,
                    anyprof_core::Stack {
                        frames: to_frames(key),
                        weight,
                    },
                )
            })
            .collect();
        indexed.sort_by_key(|(_, stack)| std::cmp::Reverse(stack.weight));
        let mut remap = vec![0usize; count];
        let mut stacks = Vec::with_capacity(count);
        for (new_idx, (old_uid, stack)) in indexed.into_iter().enumerate() {
            remap[old_uid] = new_idx;
            stacks.push(stack);
        }
        if self.capped || self.timed.is_empty() {
            return (stacks, Vec::new());
        }
        self.timed.sort_by_key(|(_, offset_ns, _)| *offset_ns);
        let samples = self
            .timed
            .into_iter()
            .map(|(tid, offset_ns, uid)| anyprof_core::TimedSample {
                tid,
                thread_name: self.thread_names.get(&tid).cloned().flatten(),
                offset_ns,
                stack_index: remap[uid as usize],
                weight: 1,
            })
            .collect();
        (stacks, samples)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DebugPrivilege, IB_SIGNED_LR, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
        IMAGE_FILE_MACHINE_ARMNT, IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
        KERNEL_SIGNED_LR, KERNEL_SIGNED_PC, LinuxDenial, NO_PTRAUTH, PROTECTION_LEVEL_NONE,
        SampleCollector, StripKind, ThreadArrayPlan, is_dyld_shared_cache, linux_perf_denied,
        on_cpu_threads, plan_thread_array, reject_self_sample, retry_until_success,
        should_deallocate_send_right, should_strip, stack_strip_flags, strip_pointer,
        symbol_lookup_address, task_for_pid_denied, too_many_threads, windows_open_denied,
        windows_process_is_x64, windows_target_kind,
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
    fn windows_sampling_accepts_x64_and_refuses_arm64() {
        assert!(windows_process_is_x64(
            IMAGE_FILE_MACHINE_UNKNOWN,
            IMAGE_FILE_MACHINE_AMD64
        ));
        assert!(windows_process_is_x64(
            IMAGE_FILE_MACHINE_AMD64,
            IMAGE_FILE_MACHINE_ARM64
        ));
        assert!(!windows_process_is_x64(
            IMAGE_FILE_MACHINE_UNKNOWN,
            IMAGE_FILE_MACHINE_ARM64
        ));
        assert!(!windows_process_is_x64(
            IMAGE_FILE_MACHINE_ARM64,
            IMAGE_FILE_MACHINE_ARM64
        ));
        assert!(!windows_process_is_x64(
            IMAGE_FILE_MACHINE_I386,
            IMAGE_FILE_MACHINE_AMD64
        ));
        assert!(!windows_process_is_x64(
            IMAGE_FILE_MACHINE_ARMNT,
            IMAGE_FILE_MACHINE_ARM64
        ));
        assert_eq!(
            windows_target_kind(IMAGE_FILE_MACHINE_UNKNOWN, IMAGE_FILE_MACHINE_ARM64),
            "ARM64"
        );
        assert_eq!(
            windows_target_kind(IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_ARM64),
            "ARM64"
        );
        assert_eq!(
            windows_target_kind(IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_AMD64),
            "32-bit"
        );
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

    #[test]
    fn linux_permission_text_names_the_sysctls_it_read() {
        let text = linux_perf_denied(LinuxDenial {
            pid: 42,
            errno: 13,
            paranoid: Some(4),
            ptrace_scope: Some(1),
            target_uid: Some(1000),
            self_uid: 1000,
            kernel: false,
        });
        assert!(text.contains("perf_event_open"), "{text}");
        assert!(text.contains("EACCES"), "{text}");
        assert!(text.contains("perf_event_paranoid is 4"), "{text}");
        assert!(text.contains("ptrace_scope is 1"), "{text}");
        assert!(text.contains("does not use that attach check"), "{text}");
        assert!(!text.contains("not an unrelated process"), "{text}");
        assert!(text.contains("does not change these sysctls"), "{text}");
        assert!(!text.contains("uid 1000"), "{text}");

        let cross_user = linux_perf_denied(LinuxDenial {
            pid: 1,
            errno: 13,
            paranoid: Some(2),
            ptrace_scope: None,
            target_uid: Some(0),
            self_uid: 1000,
            kernel: true,
        });
        assert!(cross_user.contains("uid 0"), "{cross_user}");
        assert!(cross_user.contains("CAP_SYS_PTRACE"), "{cross_user}");
        assert!(cross_user.contains("-k"), "{cross_user}");
        assert!(
            cross_user.contains("Yama ptrace_scope is not enabled"),
            "{cross_user}"
        );
    }

    #[test]
    fn windows_permission_text_separates_protection_from_privilege() {
        let protected = windows_open_denied(9, 5, Some(1), DebugPrivilege::Enabled);
        assert!(protected.contains("protected"), "{protected}");
        assert!(
            protected.contains("will not change that protection"),
            "{protected}"
        );
        assert!(!protected.to_lowercase().contains("disable"), "{protected}");

        let denied =
            windows_open_denied(9, 5, Some(PROTECTION_LEVEL_NONE), DebugPrivilege::NotHeld);
        assert!(denied.contains("SeDebugPrivilege"), "{denied}");
        assert!(denied.contains("same-user"), "{denied}");
        assert!(denied.contains("does not hold"), "{denied}");

        let enabled = windows_open_denied(9, 5, None, DebugPrivilege::Enabled);
        assert!(enabled.contains("was enabled"), "{enabled}");

        let gone = windows_open_denied(9, 87, None, DebugPrivilege::NotHeld);
        assert!(gone.contains("not running"), "{gone}");
        assert!(!gone.contains("SeDebugPrivilege"), "{gone}");
    }

    #[test]
    fn sample_collector_orders_by_time_and_degrades_past_cap() {
        use crate::python::CountedStack;
        use anyprof_core::{Frame, Runtime};

        let mut collector = SampleCollector::with_cap(3);
        let stack_a = CountedStack::Addresses {
            frames: vec![0x1000],
            kernel_len: 0,
        };
        let stack_b = CountedStack::Addresses {
            frames: vec![0x2000],
            kernel_len: 0,
        };
        collector.record(11, 200, stack_b.clone(), |_| Some("worker\n".to_string()));
        collector.record(10, 50, stack_a, |_| Some("main".to_string()));
        collector.record(11, 300, stack_b.clone(), |_| panic!("cached"));

        let (stacks, samples) = collector.finish(|key| match key {
            CountedStack::Addresses { frames, .. } => {
                vec![Frame::new(Runtime::Native, format!("0x{:x}", frames[0]))]
            }
            _ => unreachable!(),
        });
        assert_eq!(stacks.len(), 2);
        assert_eq!(stacks[0].weight, 2);
        assert_eq!(stacks[0].frames[0].name, "0x2000");
        assert_eq!(stacks[1].weight, 1);
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].offset_ns, 50);
        assert_eq!(samples[0].tid, 10);
        assert_eq!(samples[0].thread_name.as_deref(), Some("main"));
        assert_eq!(samples[0].stack_index, 1);
        assert_eq!(samples[1].offset_ns, 200);
        assert_eq!(samples[1].thread_name.as_deref(), Some("worker"));
        assert_eq!(samples[1].stack_index, 0);

        // Exceeding cap drops timed series while keeping all aggregate stack weights.
        let mut capped = SampleCollector::with_cap(2);
        for t in [10, 20, 30] {
            capped.record(1, t, stack_b.clone(), |_| None);
        }
        let (stacks, samples) = capped.finish(|_| vec![Frame::new(Runtime::Native, "f")]);
        assert_eq!(stacks[0].weight, 3);
        assert!(samples.is_empty());
    }
}
