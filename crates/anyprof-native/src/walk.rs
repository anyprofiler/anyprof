//! Frame-pointer walk over a stack the caller can already read.
//!
//! macOS walks through Mach and strips pointer-authentication bits in
//! `macos`. Linux walks a `PERF_SAMPLE_STACK_USER` copy with this code.
//! The leaf program counter is the instruction that was running. Each saved
//! slot is a return address, so symbol lookup subtracts one byte later.

#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]

/// Most frames recorded for one sample.
pub(crate) const MAX_FRAMES: usize = 64;

/// A user-space instruction pointer or saved return address.
///
/// The upper half of the canonical x86_64 address space belongs to the
/// kernel. Context markers used by perf sit even higher and are not code.
pub(crate) fn plausible_user_address(addr: u64) -> bool {
    addr > 0x1000 && addr < 0x0000_8000_0000_0000
}

/// A frame pointer is an aligned user address.
pub(crate) fn plausible_frame_pointer(fp: u64) -> bool {
    plausible_user_address(fp) && fp.is_multiple_of(8)
}

/// Walk `fp` while `read` can supply the 16-byte saved frame.
///
/// `read` copies bytes from the target address into `buf` and returns false
/// when that address is not available. `lr` is pushed after the leaf when
/// the walk finds no saved frame and `lr` looks like a user address. x86_64
/// passes 0. The result is leaf-first.
pub(crate) fn walk_frame_pointer(
    pc: u64,
    fp: u64,
    lr: u64,
    mut read: impl FnMut(u64, &mut [u8]) -> bool,
) -> Vec<u64> {
    let mut frames = Vec::new();
    if plausible_user_address(pc) {
        frames.push(pc);
    }
    let mut fp = fp;
    let mut guard = 0;
    while frames.len() < MAX_FRAMES && plausible_frame_pointer(fp) {
        let mut record = [0u8; 16];
        if !read(fp, &mut record) {
            break;
        }
        let next_fp = u64::from_le_bytes(record[0..8].try_into().unwrap());
        let ret = u64::from_le_bytes(record[8..16].try_into().unwrap());
        if !plausible_user_address(ret) {
            break;
        }
        frames.push(ret);
        if next_fp <= fp {
            break;
        }
        fp = next_fp;
        guard += 1;
        if guard >= MAX_FRAMES {
            break;
        }
    }
    if frames.len() == 1 && plausible_user_address(lr) && lr != frames[0] {
        frames.push(lr);
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::{plausible_frame_pointer, plausible_user_address, walk_frame_pointer};

    #[test]
    fn user_and_kernel_addresses_are_split() {
        assert!(plausible_user_address(0x0000_5555_0000_1000));
        assert!(!plausible_user_address(0));
        assert!(!plausible_user_address(0x1000));
        assert!(!plausible_user_address(0xffff_ffff_8100_0000));
        assert!(!plausible_user_address(0xffff_ffff_ffff_fe00));
        assert!(plausible_frame_pointer(0x0000_7fff_0000_1000));
        assert!(!plausible_frame_pointer(0x0000_7fff_0000_1004));
    }

    #[test]
    fn a_saved_chain_returns_leaf_then_callers() {
        // sp == fp is a real leaf frame: the saved record sits at the stack pointer.
        let sp = 0x0000_7fff_0000_2000u64;
        let mut memory = std::collections::HashMap::new();
        memory.insert(sp, (sp + 0x20, 0x5555_0000_2000u64));
        memory.insert(sp + 0x20, (sp + 0x40, 0x5555_0000_3000u64));
        memory.insert(sp + 0x40, (0u64, 0u64));
        let frames = walk_frame_pointer(0x5555_0000_1000, sp, 0, |addr, buf| {
            let Some((next, ret)) = memory.get(&addr) else {
                return false;
            };
            buf[..8].copy_from_slice(&next.to_le_bytes());
            buf[8..].copy_from_slice(&ret.to_le_bytes());
            true
        });
        assert_eq!(
            frames,
            vec![0x5555_0000_1000, 0x5555_0000_2000, 0x5555_0000_3000]
        );
    }

    #[test]
    fn a_frame_outside_the_copy_stops_the_walk() {
        let frames = walk_frame_pointer(
            0x5555_0000_1000,
            0x0000_7fff_0000_2000,
            0x5555_0000_4444,
            |_, _| false,
        );
        assert_eq!(frames, vec![0x5555_0000_1000, 0x5555_0000_4444]);
    }

    #[test]
    fn a_descending_link_stops_the_walk() {
        let fp = 0x0000_7fff_0000_3000u64;
        let frames = walk_frame_pointer(0x5555_0000_1000, fp, 0, |addr, buf| {
            assert_eq!(addr, fp);
            buf[..8].copy_from_slice(&(fp - 0x10).to_le_bytes());
            buf[8..].copy_from_slice(&0x5555_0000_2000u64.to_le_bytes());
            true
        });
        assert_eq!(frames, vec![0x5555_0000_1000, 0x5555_0000_2000]);
    }

    #[test]
    fn a_kernel_pointer_is_not_a_user_frame() {
        let frames = walk_frame_pointer(0xffff_ffff_8100_0010, 0, 0, |_, _| unreachable!());
        assert!(frames.is_empty());
    }
}
