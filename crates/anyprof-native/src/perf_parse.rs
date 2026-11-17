//! Parse one `PERF_RECORD_SAMPLE` from a `perf_event_open` ring buffer.
//!
//! The field order is the order in `uapi/linux/perf_event.h`. This sampler
//! asks for IP, TID, CALLCHAIN, REGS_USER, and STACK_USER, in that order.
//! The stack `size` field is the static size requested in the attribute
//! (8 KiB, rounded up by the kernel). `dyn_size` is how many of those bytes
//! were copied from the user stack; only that prefix is kept.
//!
//! A register ABI of 0 means the kernel did not save user registers, and the
//! register array is absent. A stack size of 0 means the data and `dyn_size`
//! fields are absent.

#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]

use crate::walk::plausible_user_address;

/// `PERF_SAMPLE_REGS_ABI_NONE`.
const ABI_NONE: u64 = 0;

/// `PERF_CONTEXT_MAX`. Values at or above this are call-chain markers.
const PERF_CONTEXT_MAX: u64 = u64::MAX - 4094;

/// `PERF_CONTEXT_KERNEL` and the guest equivalent.
pub(crate) const CONTEXT_KERNEL: u64 = u64::MAX - 127;
const CONTEXT_GUEST_KERNEL: u64 = u64::MAX - 2175;
/// `PERF_CONTEXT_USER` and the guest equivalent.
pub(crate) const CONTEXT_USER: u64 = u64::MAX - 511;
const CONTEXT_GUEST_USER: u64 = u64::MAX - 2559;

/// Flags word of `perf_event_attr` for this sampler.
///
/// Bit positions were checked against `linux/perf_event.h` on x86_64:
/// `disabled` is bit 0, `exclude_kernel` bit 5, `exclude_hv` bit 6,
/// `exclude_callchain_kernel` bit 21. The event starts disabled and is
/// enabled with `PERF_EVENT_IOC_ENABLE` after the ring buffer exists.
pub(crate) fn event_flags(include_kernel: bool) -> u64 {
    let mut flags = 1 << 0;
    flags |= 1 << 6;
    if !include_kernel {
        flags |= 1 << 5;
        flags |= 1 << 21;
    }
    flags
}

/// User registers this sampler requests: `bp`, then `sp`, then `ip`.
///
/// The kernel writes set bits from low to high. Those three bits are
/// `PERF_REG_X86_BP` (6), `PERF_REG_X86_SP` (7), and `PERF_REG_X86_IP` (8).
pub(crate) const X86_USER_REGS: u64 = (1 << 6) | (1 << 7) | (1 << 8);

/// One sample after the header has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSample {
    /// `PERF_SAMPLE_IP`. This is a kernel address when the sample landed in
    /// the kernel.
    pub ip: u64,
    /// Target pid.
    pub pid: u32,
    /// Target tid.
    pub tid: u32,
    /// Raw call-chain values, including context markers.
    pub callchain: Vec<u64>,
    /// User instruction pointer from `PERF_SAMPLE_REGS_USER`, when saved.
    pub user_ip: Option<u64>,
    /// User stack pointer.
    pub user_sp: Option<u64>,
    /// User frame pointer.
    pub user_fp: Option<u64>,
    /// Bytes copied from the user stack, starting at `user_sp`.
    pub stack: Vec<u8>,
}

/// Split a call chain into kernel addresses and user addresses.
///
/// Markers choose the mode. Entries before the first marker are classified
/// by the address itself: the high canonical half is the kernel.
pub(crate) fn split_callchain(ips: &[u64]) -> (Vec<u64>, Vec<u64>) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Unknown,
        Kernel,
        User,
        Other,
    }
    let mut mode = Mode::Unknown;
    let mut kernel = Vec::new();
    let mut user = Vec::new();
    for &ip in ips {
        if ip >= PERF_CONTEXT_MAX {
            mode = if ip == CONTEXT_KERNEL || ip == CONTEXT_GUEST_KERNEL {
                Mode::Kernel
            } else if ip == CONTEXT_USER || ip == CONTEXT_GUEST_USER {
                Mode::User
            } else {
                Mode::Other
            };
            continue;
        }
        match mode {
            Mode::Kernel => push_kernel(&mut kernel, ip),
            Mode::User => push_user(&mut user, ip),
            Mode::Other => {}
            Mode::Unknown => {
                if ip >= 0xffff_8000_0000_0000 {
                    push_kernel(&mut kernel, ip);
                } else {
                    push_user(&mut user, ip);
                }
            }
        }
    }
    (kernel, user)
}

fn push_kernel(out: &mut Vec<u64>, ip: u64) {
    if (0xffff_8000_0000_0000..PERF_CONTEXT_MAX).contains(&ip) {
        out.push(ip);
    }
}

fn push_user(out: &mut Vec<u64>, ip: u64) {
    if plausible_user_address(ip) {
        out.push(ip);
    }
}

/// Prefer the frame-pointer walk when it recovered a caller.
///
/// The kernel call chain is the fallback when the copied stack does not
/// contain the saved frame. A walk that produced only a leaf does not hide a
/// longer call chain.
pub(crate) fn choose_user_frames(walked: Vec<u64>, callchain: Vec<u64>) -> Vec<u64> {
    let walked_ok = !walked.is_empty() && walked.iter().copied().all(plausible_user_address);
    let call_ok = !callchain.is_empty() && callchain.iter().copied().all(plausible_user_address);
    if walked_ok && walked.len() >= 2 {
        walked
    } else if call_ok && callchain.len() > walked.len() {
        callchain
    } else if walked_ok {
        walked
    } else if call_ok {
        callchain
    } else {
        Vec::new()
    }
}

/// Parse the body of a `PERF_RECORD_SAMPLE`, including its 8-byte header.
///
/// `regs_mask` is `sample_regs_user`. Returns `None` when the record is
/// shorter than the fields it claims to hold.
pub(crate) fn parse_sample(record: &[u8], regs_mask: u64) -> Option<ParsedSample> {
    if record.len() < 8 {
        return None;
    }
    let size = u16::from_le_bytes(record[6..8].try_into().ok()?) as usize;
    if size < 8 || size > record.len() {
        return None;
    }
    let record = &record[..size];
    let mut cursor = 8usize;
    let ip = read_u64(record, &mut cursor)?;
    let pid = read_u32(record, &mut cursor)?;
    let tid = read_u32(record, &mut cursor)?;
    let nr = read_u64(record, &mut cursor)?;
    if nr > 1024 {
        return None;
    }
    let mut callchain = Vec::with_capacity(nr as usize);
    for _ in 0..nr {
        callchain.push(read_u64(record, &mut cursor)?);
    }
    let abi = read_u64(record, &mut cursor)?;
    let (user_fp, user_sp, user_ip) = if abi == ABI_NONE {
        (None, None, None)
    } else {
        let count = regs_mask.count_ones() as usize;
        let mut regs = Vec::with_capacity(count);
        for _ in 0..count {
            regs.push(read_u64(record, &mut cursor)?);
        }
        let fp = reg_value(regs_mask, &regs, 6);
        let sp = reg_value(regs_mask, &regs, 7);
        let ip = reg_value(regs_mask, &regs, 8);
        (fp, sp, ip)
    };
    let stack_size = read_u64(record, &mut cursor)?;
    let stack = if stack_size == 0 {
        Vec::new()
    } else {
        if stack_size > 1024 * 1024 {
            return None;
        }
        let data = read_bytes(record, &mut cursor, stack_size as usize)?;
        let dyn_size = read_u64(record, &mut cursor)?;
        let keep = (dyn_size as usize).min(data.len());
        data[..keep].to_vec()
    };
    Some(ParsedSample {
        ip,
        pid,
        tid,
        callchain,
        user_ip,
        user_sp,
        user_fp,
        stack,
    })
}

/// Value of register `bit` inside the dense register array.
fn reg_value(mask: u64, regs: &[u64], bit: u32) -> Option<u64> {
    if mask & (1 << bit) == 0 {
        return None;
    }
    let index = (mask & ((1 << bit) - 1)).count_ones() as usize;
    regs.get(index).copied()
}

/// Copy `len` bytes from a ring, wrapping at the end.
pub(crate) fn read_ring(data: &[u8], tail: u64, len: usize) -> Option<Vec<u8>> {
    if data.is_empty() || len > data.len() {
        return None;
    }
    let start = (tail % data.len() as u64) as usize;
    let mut out = vec![0u8; len];
    let first = (data.len() - start).min(len);
    out[..first].copy_from_slice(&data[start..start + first]);
    if first < len {
        out[first..].copy_from_slice(&data[..len - first]);
    }
    Some(out)
}

fn read_u64(buf: &[u8], cursor: &mut usize) -> Option<u64> {
    let bytes = read_bytes(buf, cursor, 8)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u32(buf: &[u8], cursor: &mut usize) -> Option<u32> {
    let bytes = read_bytes(buf, cursor, 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_bytes<'a>(buf: &'a [u8], cursor: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(len)?;
    let slice = buf.get(*cursor..end)?;
    *cursor = end;
    Some(slice)
}

#[cfg(test)]
mod tests {
    use super::{
        CONTEXT_KERNEL, CONTEXT_USER, X86_USER_REGS, choose_user_frames, event_flags, parse_sample,
        read_ring, split_callchain,
    };

    #[test]
    fn event_flags_match_the_kernel_bitfield() {
        // disabled | exclude_kernel | exclude_hv | exclude_callchain_kernel.
        assert_eq!(event_flags(false), 0x200061);
        // disabled | exclude_hv. Kernel samples and kernel call chains stay.
        assert_eq!(event_flags(true), 0x41);
    }

    #[test]
    fn a_user_sample_round_trips_through_the_record_layout() {
        let sp = 0x0000_7fff_0000_1000u64;
        let mut stack = vec![0u8; 32];
        stack[0..8].copy_from_slice(&(sp + 0x20).to_le_bytes());
        stack[8..16].copy_from_slice(&0x5555_0000_2000u64.to_le_bytes());
        let mut record = Vec::new();
        record.extend_from_slice(&9u32.to_le_bytes());
        record.extend_from_slice(&2u16.to_le_bytes());
        let size_at = record.len();
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&0x5555_0000_1000u64.to_le_bytes());
        record.extend_from_slice(&42u32.to_le_bytes());
        record.extend_from_slice(&42u32.to_le_bytes());
        record.extend_from_slice(&2u64.to_le_bytes());
        record.extend_from_slice(&CONTEXT_USER.to_le_bytes());
        record.extend_from_slice(&0x5555_0000_1000u64.to_le_bytes());
        record.extend_from_slice(&2u64.to_le_bytes()); // abi 64
        record.extend_from_slice(&sp.to_le_bytes());
        record.extend_from_slice(&sp.to_le_bytes());
        record.extend_from_slice(&0x5555_0000_1000u64.to_le_bytes());
        record.extend_from_slice(&(stack.len() as u64).to_le_bytes());
        record.extend_from_slice(&stack);
        record.extend_from_slice(&(stack.len() as u64).to_le_bytes());
        let size = record.len() as u16;
        record[size_at..size_at + 2].copy_from_slice(&size.to_le_bytes());

        let parsed = parse_sample(&record, X86_USER_REGS).unwrap();
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.user_ip, Some(0x5555_0000_1000));
        assert_eq!(parsed.user_sp, Some(sp));
        assert_eq!(parsed.user_fp, Some(sp));
        assert_eq!(parsed.stack.len(), 32);
        let (kernel, user) = split_callchain(&parsed.callchain);
        assert!(kernel.is_empty());
        assert_eq!(user, vec![0x5555_0000_1000]);
    }

    #[test]
    fn kernel_and_user_markers_split_the_chain() {
        let ips = [
            CONTEXT_KERNEL,
            0xffff_ffff_8100_0010,
            0xffff_ffff_8100_0200,
            CONTEXT_USER,
            0x5555_0000_1000,
            0x5555_0000_2000,
        ];
        let (kernel, user) = split_callchain(&ips);
        assert_eq!(kernel, vec![0xffff_ffff_8100_0010, 0xffff_ffff_8100_0200]);
        assert_eq!(user, vec![0x5555_0000_1000, 0x5555_0000_2000]);
    }

    #[test]
    fn a_longer_call_chain_replaces_a_leaf_only_walk() {
        let leaf = 0x5555_0000_1000;
        let mid = 0x5555_0000_2000;
        let root = 0x5555_0000_3000;
        let chosen = choose_user_frames(vec![leaf], vec![leaf, mid, root]);
        assert_eq!(chosen, vec![leaf, mid, root]);
        let chosen = choose_user_frames(vec![leaf, mid], vec![leaf]);
        assert_eq!(chosen, vec![leaf, mid]);
    }

    #[test]
    fn an_abi_of_zero_omits_registers_and_a_zero_stack_omits_bytes() {
        let mut record = Vec::new();
        record.extend_from_slice(&9u32.to_le_bytes());
        record.extend_from_slice(&0u16.to_le_bytes());
        let size_at = record.len();
        record.extend_from_slice(&0u16.to_le_bytes());
        record.extend_from_slice(&0x1000u64.to_le_bytes());
        record.extend_from_slice(&1u32.to_le_bytes());
        record.extend_from_slice(&1u32.to_le_bytes());
        record.extend_from_slice(&0u64.to_le_bytes());
        record.extend_from_slice(&0u64.to_le_bytes()); // abi none
        record.extend_from_slice(&0u64.to_le_bytes()); // stack size 0
        let size = record.len() as u16;
        record[size_at..size_at + 2].copy_from_slice(&size.to_le_bytes());
        let parsed = parse_sample(&record, X86_USER_REGS).unwrap();
        assert!(parsed.user_ip.is_none());
        assert!(parsed.stack.is_empty());
    }

    #[test]
    fn a_short_record_is_rejected() {
        assert!(parse_sample(&[0; 7], X86_USER_REGS).is_none());
        let mut record = vec![0u8; 8];
        record[6..8].copy_from_slice(&40u16.to_le_bytes());
        assert!(parse_sample(&record, X86_USER_REGS).is_none());
    }

    #[test]
    fn ring_reads_wrap_to_the_front() {
        let data = b"abcdefgh";
        let got = read_ring(data, 6, 4).unwrap();
        assert_eq!(got, b"ghab");
    }
}
