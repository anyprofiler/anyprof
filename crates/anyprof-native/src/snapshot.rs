//! Which threads of a Windows process are on CPU.
//!
//! `NtQuerySystemInformation(SystemProcessInformation)` returns this layout on
//! 64-bit Windows from Windows 7 through Windows 11. The sampler only asks for
//! threads whose `ThreadState` is `Running` (2). `Ready` is queued, not on a
//! CPU, and is not sampled. The bytes are parsed here so the choice can be
//! tested without a Windows process.

#![cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]

/// `SystemProcessInformation`.
pub(crate) const SYSTEM_PROCESS_INFORMATION: u32 = 5;

/// `KTHREAD_STATE::Running`.
pub(crate) const THREAD_STATE_RUNNING: u32 = 2;

/// Most threads one process is allowed to contribute before the profile stops.
const SAMPLE_MAX: u32 = 100_000;

/// Why a thread snapshot cannot be sampled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotError {
    /// The buffer ended inside a record.
    Truncated,
    /// The process has more threads than [`SAMPLE_MAX`].
    TooMany { count: u32 },
}

/// Header of `SYSTEM_PROCESS_INFORMATION` up to, and not including, `Threads`.
///
/// Field order matches `ntapi::ntexapi::SYSTEM_PROCESS_INFORMATION`. Sizes are
/// explicit so the layout does not depend on whether `long` is 32 or 64 bits.
#[repr(C)]
struct ProcessHeader {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name_length: u16,
    image_name_maximum: u16,
    _image_name_pad: u32,
    image_name_buffer: u64,
    base_priority: i32,
    _pad_after_priority: u32,
    unique_process_id: u64,
    inherited_from_unique_process_id: u64,
    handle_count: u32,
    session_id: u32,
    unique_process_key: u64,
    peak_virtual_size: u64,
    virtual_size: u64,
    page_fault_count: u32,
    _pad_after_faults: u32,
    peak_working_set_size: u64,
    working_set_size: u64,
    quota_peak_paged_pool_usage: u64,
    quota_paged_pool_usage: u64,
    quota_peak_non_paged_pool_usage: u64,
    quota_non_paged_pool_usage: u64,
    pagefile_usage: u64,
    peak_pagefile_usage: u64,
    private_page_count: u64,
    read_operation_count: i64,
    write_operation_count: i64,
    other_operation_count: i64,
    read_transfer_count: i64,
    write_transfer_count: i64,
    other_transfer_count: i64,
}

/// `SYSTEM_THREAD_INFORMATION` on 64-bit Windows.
#[repr(C)]
struct ThreadInfo {
    kernel_time: i64,
    user_time: i64,
    create_time: i64,
    wait_time: u32,
    _pad_after_wait: u32,
    start_address: u64,
    client_process: u64,
    client_thread: u64,
    priority: i32,
    base_priority: i32,
    context_switches: u32,
    thread_state: u32,
    wait_reason: u32,
    _tail_pad: u32,
}

const PID_OFFSET: usize = std::mem::offset_of!(ProcessHeader, unique_process_id);
const NEXT_OFFSET: usize = std::mem::offset_of!(ProcessHeader, next_entry_offset);
const COUNT_OFFSET: usize = std::mem::offset_of!(ProcessHeader, number_of_threads);
const HEADER_SIZE: usize = std::mem::size_of::<ProcessHeader>();
const THREAD_SIZE: usize = std::mem::size_of::<ThreadInfo>();
const THREAD_ID_OFFSET: usize = std::mem::offset_of!(ThreadInfo, client_thread);
const THREAD_STATE_OFFSET: usize = std::mem::offset_of!(ThreadInfo, thread_state);

const _: () = assert!(PID_OFFSET == 80);
const _: () = assert!(HEADER_SIZE == 256);
const _: () = assert!(THREAD_SIZE == 80);
const _: () = assert!(THREAD_ID_OFFSET == 48);
const _: () = assert!(THREAD_STATE_OFFSET == 68);

/// Thread ids of `pid` whose state is [`THREAD_STATE_RUNNING`].
///
/// Other states are not on CPU. An idle process therefore contributes an empty
/// list, and the caller records no samples for that tick.
pub(crate) fn running_tids(buf: &[u8], pid: u32) -> Result<Vec<u32>, SnapshotError> {
    let mut offset = 0usize;
    let mut running = Vec::new();
    while offset + HEADER_SIZE <= buf.len() {
        let next = read_u32(buf, offset + NEXT_OFFSET);
        let count = read_u32(buf, offset + COUNT_OFFSET);
        let process = read_u64(buf, offset + PID_OFFSET);
        let threads_end = offset
            .saturating_add(HEADER_SIZE)
            .saturating_add((count as usize).saturating_mul(THREAD_SIZE));
        if process == u64::from(pid) && count > SAMPLE_MAX {
            // The header is complete, so the count is known. Stop before
            // walking a thread array that is too large to sample, even when
            // the buffer does not hold every thread record.
            return Err(SnapshotError::TooMany { count });
        }
        if count > 0 && threads_end > buf.len() {
            return Err(SnapshotError::Truncated);
        }
        if process == u64::from(pid) {
            for index in 0..count {
                let base = offset + HEADER_SIZE + index as usize * THREAD_SIZE;
                let tid = read_u64(buf, base + THREAD_ID_OFFSET);
                let state = read_u32(buf, base + THREAD_STATE_OFFSET);
                if state == THREAD_STATE_RUNNING {
                    running.push(tid as u32);
                }
            }
        }
        if next == 0 {
            break;
        }
        let next = next as usize;
        if next < HEADER_SIZE {
            return Err(SnapshotError::Truncated);
        }
        offset = offset.saturating_add(next);
    }
    Ok(running)
}

/// Message when a process has too many threads to sample safely.
pub(crate) fn too_many_threads(count: u32) -> String {
    format!("the process has {count} threads; the profile stopped instead of walking all of them")
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap_or([0; 4]))
}

fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap_or([0; 8]))
}

#[cfg(test)]
mod tests {
    use super::{
        HEADER_SIZE, SnapshotError, THREAD_ID_OFFSET, THREAD_SIZE, THREAD_STATE_OFFSET,
        THREAD_STATE_RUNNING, running_tids, too_many_threads,
    };

    fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(buf: &mut [u8], offset: usize, value: u64) {
        buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn process(pid: u32, threads: &[(u32, u32)], next: u32) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_SIZE + threads.len() * THREAD_SIZE];
        put_u32(&mut buf, 0, next);
        put_u32(&mut buf, 4, threads.len() as u32);
        put_u64(&mut buf, 80, u64::from(pid));
        for (index, (tid, state)) in threads.iter().enumerate() {
            let base = HEADER_SIZE + index * THREAD_SIZE;
            put_u64(&mut buf, base + THREAD_ID_OFFSET, u64::from(*tid));
            put_u32(&mut buf, base + THREAD_STATE_OFFSET, *state);
        }
        buf
    }

    #[test]
    fn only_running_threads_of_the_target_are_returned() {
        let mut buf = process(7, &[(1, 5), (2, THREAD_STATE_RUNNING), (3, 1)], 0);
        // A second process is not consulted when the first matches and next is 0.
        let other = process(9, &[(9, THREAD_STATE_RUNNING)], 0);
        buf.extend_from_slice(&other);
        // next was 0, so the second record is not walked.
        assert_eq!(running_tids(&buf, 7).unwrap(), vec![2]);
    }

    #[test]
    fn a_later_process_is_reached_through_next_entry_offset() {
        let first = process(7, &[(1, THREAD_STATE_RUNNING)], 0);
        let second = process(8, &[(4, 5), (5, THREAD_STATE_RUNNING)], 0);
        let mut buf = first.clone();
        let next = buf.len() as u32;
        buf[..4].copy_from_slice(&next.to_le_bytes());
        buf.extend_from_slice(&second);
        assert_eq!(running_tids(&buf, 8).unwrap(), vec![5]);
        assert!(running_tids(&buf, 99).unwrap().is_empty());
    }

    #[test]
    fn an_idle_process_contributes_no_threads() {
        let buf = process(4, &[(10, 5), (11, 5)], 0);
        assert!(running_tids(&buf, 4).unwrap().is_empty());
    }

    #[test]
    fn a_truncated_thread_array_is_rejected() {
        let mut buf = process(4, &[(1, THREAD_STATE_RUNNING)], 0);
        buf.truncate(HEADER_SIZE + 4);
        assert_eq!(running_tids(&buf, 4), Err(SnapshotError::Truncated));
    }

    #[test]
    fn too_many_threads_stops_rather_than_sampling() {
        let mut buf = vec![0u8; HEADER_SIZE];
        put_u32(&mut buf, 4, 100_001);
        put_u64(&mut buf, 80, 4);
        assert_eq!(
            running_tids(&buf, 4),
            Err(SnapshotError::TooMany { count: 100_001 })
        );
        let text = too_many_threads(100_001);
        assert!(text.contains("100001"), "{text}");
        assert!(text.contains("profile stopped"), "{text}");
    }
}
