//! Linux x86_64 on-CPU sampler.
//!
//! `perf_event_open` of `PERF_COUNT_SW_TASK_CLOCK` fires only while that
//! thread is scheduled, so an idle thread contributes no samples and the
//! target is never suspended. Each thread gets its own event and its own
//! ring buffer. This kernel rejects `PERF_FLAG_FD_OUTPUT` across threads,
//! because a per-task buffer cannot be shared with another task. User frames
//! come from a frame-pointer walk of `PERF_SAMPLE_STACK_USER`,
//! with the kernel's user call chain as a fallback. `-k` keeps kernel
//! frames from the call chain. Nothing here writes the target's memory or
//! registers, and `ptrace` is not used.
//!
//! Sampling this process is allowed. The event does not stop the sampler,
//! which is why [`crate::policy::reject_self_sample`] is not called.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyprof_core::{
    Error, Frame, PERF_SAMPLING_INTERVAL, Profile, RecordOptions, Runtime, Stack, Target, inspect,
    process_exists,
};

use crate::elf::{self, UserImage};
use crate::kallsyms::{self, Kallsym};
use crate::maps::{self, MapSpan};
use crate::perf_parse::{self, ParsedSample, X86_USER_REGS};
use crate::policy::{self, LinuxDenial};
use crate::walk::{self, walk_frame_pointer};

const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
const SAMPLE_TYPE: u64 = (1 << 0) | (1 << 1) | (1 << 5) | (1 << 12) | (1 << 13);
const ATTR_SIZE: u32 = 96;
const FLAG_FD_CLOEXEC: u64 = 1 << 3;
const IOC_ENABLE: u64 = 0x2400;
const IOC_DISABLE: u64 = 0x2401;
const DATA_HEAD: usize = 1024;
const DATA_TAIL: usize = 1032;
const DATA_OFFSET: usize = 1040;
const DATA_SIZE: usize = 1048;
const RECORD_SAMPLE: u32 = 9;
const RECORD_LOST: u32 = 2;
const RECORD_LOST_SAMPLES: u32 = 13;
const EM_X86_64: u16 = 62;
const ELFCLASS64: u8 = 2;

/// `perf_event_attr` through `PERF_ATTR_SIZE_VER3` (96 bytes).
///
/// The flags word is little-endian, matching the kernel bitfield on x86_64.
/// The event is created disabled and enabled after the ring exists.
#[repr(C)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
}

const _: () = assert!(std::mem::size_of::<PerfEventAttr>() == ATTR_SIZE as usize);

#[derive(Clone, PartialEq, Eq, Hash)]
struct SampleKey {
    frames: Vec<u64>,
    kernel_len: u16,
}

enum AttachFail {
    /// `EINVAL` before any event existed. A smaller stack copy may be accepted.
    Shrink,
    Fatal(Error),
}

struct Mapped {
    fd: i32,
    tid: i32,
    ptr: *mut u8,
    len: usize,
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` came from `mmap` of `fd`, and both are cleared
        // so a second drop is a no-op. Unmap first, then close.
        unsafe {
            if !self.ptr.is_null() && self.len > 0 {
                libc::munmap(self.ptr.cast(), self.len);
                self.ptr = std::ptr::null_mut();
                self.len = 0;
            }
            if self.fd >= 0 {
                libc::close(self.fd);
                self.fd = -1;
            }
        }
    }
}

struct Sampler {
    events: Vec<Mapped>,
    /// Task-clock period, in nanoseconds, used for threads that appear later.
    period: u64,
    /// User-stack copy the kernel accepted, in bytes.
    stack: u32,
    include_kernel: bool,
    /// Threads that existed but could not be mapped under the mlock limit.
    unmapped: u32,
}

pub(crate) fn record(target: &Target, opts: &RecordOptions) -> Result<Profile, Error> {
    let interval = opts.sampling_interval.unwrap_or(PERF_SAMPLING_INTERVAL);
    if interval < Duration::from_micros(1) {
        return Err(Error::backend(
            "native",
            "sampling interval must be at least 1us",
        ));
    }
    let period = u64::try_from(interval.as_nanos()).map_err(|_| {
        Error::backend(
            "native",
            "sampling interval does not fit in a perf sample period",
        )
    })?;
    if period == 0 {
        return Err(Error::backend(
            "native",
            "sampling interval must be at least 1ns",
        ));
    }
    reject_foreign_elf(target.pid)?;

    let (sampler, _attr) = open_events(target.pid, period, opts.include_kernel)?;
    let mut sampler = sampler;
    let images = load_images(target.pid);
    let kernel_symbols = if opts.include_kernel {
        load_kallsyms()
    } else {
        Vec::new()
    };
    tracing::debug!(
        pid = target.pid,
        images = images.len(),
        kernel_symbols = kernel_symbols.len(),
        "native symbol tables loaded"
    );

    let started = Instant::now();
    let mut next = started;
    let mut counts: HashMap<SampleKey, u64> = HashMap::new();
    let mut lost = 0u64;
    loop {
        if opts.stop.stop_requested() {
            break;
        }
        if opts
            .duration
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            break;
        }
        refresh(&mut sampler, target.pid);
        lost = lost.saturating_add(take_samples(
            &sampler,
            target.pid,
            opts.include_kernel,
            &mut counts,
        ));
        next += interval;
        let now = Instant::now();
        if next < now {
            next = now;
        }
        if let Some(limit) = opts.duration {
            let end = started + limit;
            if next > end {
                next = end;
            }
        }
        wait_until(next, opts);
    }
    sampler.disable();
    lost = lost.saturating_add(take_samples(
        &sampler,
        target.pid,
        opts.include_kernel,
        &mut counts,
    ));
    if lost > 0 {
        tracing::warn!(lost, pid = target.pid, "perf ring buffer dropped samples");
        eprintln!(
            "anyprof: the perf ring buffer dropped {lost} samples; the profile is missing those hits"
        );
    }
    if sampler.unmapped > 0 {
        eprintln!(
            "anyprof: {} thread(s) of pid {} had no perf buffer (kernel.perf_event_mlock_kb). Those threads were not sampled. anyprof does not change that sysctl.",
            sampler.unmapped, target.pid
        );
    }

    let comm = inspect(target.pid).and_then(|snapshot| snapshot.comm);
    Ok(finish_profile(
        target.pid,
        comm,
        counts,
        &images,
        &kernel_symbols,
        started.elapsed(),
        interval,
    ))
}

fn open_events(
    pid: u32,
    period: u64,
    include_kernel: bool,
) -> Result<(Sampler, PerfEventAttr), Error> {
    let mut stack = 8192u32;
    loop {
        let attr = event_attr(period, stack, include_kernel);
        match attach(pid, &attr) {
            Ok(sampler) => return Ok((sampler, attr)),
            Err(AttachFail::Shrink) if stack > 2048 => stack /= 2,
            Err(AttachFail::Shrink) if stack > 0 => stack = 0,
            Err(AttachFail::Shrink) => {
                return Err(Error::backend("native", invalid_event(pid, period, stack)));
            }
            Err(AttachFail::Fatal(err)) => return Err(err),
        }
    }
}

fn event_attr(period: u64, stack: u32, include_kernel: bool) -> PerfEventAttr {
    PerfEventAttr {
        type_: PERF_TYPE_SOFTWARE,
        size: ATTR_SIZE,
        config: PERF_COUNT_SW_TASK_CLOCK,
        sample_period: period,
        sample_type: SAMPLE_TYPE,
        read_format: 0,
        flags: perf_parse::event_flags(include_kernel),
        wakeup_events: 1,
        bp_type: 0,
        config1: 0,
        config2: 0,
        branch_sample_type: 0,
        sample_regs_user: X86_USER_REGS,
        sample_stack_user: stack,
        clockid: 0,
    }
}

fn attach(pid: u32, attr: &PerfEventAttr) -> Result<Sampler, AttachFail> {
    let tids =
        list_tids(pid).map_err(|message| AttachFail::Fatal(Error::backend("native", message)))?;
    if tids.is_empty() {
        return Err(AttachFail::Fatal(Error::backend(
            "native",
            format!("pid {pid} has no threads to sample"),
        )));
    }
    let mut sampler = Sampler {
        events: Vec::new(),
        period: attr.sample_period,
        stack: attr.sample_stack_user,
        include_kernel: attr_includes_kernel(attr),
        unmapped: 0,
    };
    let mut denied = None;
    let mut other = None;
    for tid in tids {
        match open_one(tid, attr) {
            Ok(fd) => match map_ring(fd) {
                Ok((ptr, len)) => sampler.events.push(Mapped { fd, tid, ptr, len }),
                Err(err) => {
                    unsafe { libc::close(fd) };
                    if sampler.events.is_empty() {
                        return Err(AttachFail::Fatal(Error::backend("native", map_error(err))));
                    }
                    sampler.unmapped = sampler.unmapped.saturating_add(1);
                }
            },
            Err(OpenError::Denied(errno)) if sampler.events.is_empty() => {
                denied = Some(errno);
                break;
            }
            Err(OpenError::Denied(_)) => {}
            Err(OpenError::Invalid) if sampler.events.is_empty() => {
                return Err(AttachFail::Shrink);
            }
            Err(OpenError::Invalid) => other = Some(22),
            Err(OpenError::Gone) => {}
            Err(OpenError::Other(errno)) => other = Some(errno),
        }
    }
    if sampler.events.is_empty() {
        if let Some(errno) = denied {
            return Err(AttachFail::Fatal(Error::backend(
                "native",
                permission_error(pid, errno, attr_includes_kernel(attr)),
            )));
        }
        if let Some(errno) = other {
            return Err(AttachFail::Fatal(Error::backend(
                "native",
                format!("perf_event_open failed for pid {pid} (errno {errno})"),
            )));
        }
        let why = if process_exists(pid) {
            format!("perf_event_open could not attach to any thread of pid {pid}")
        } else {
            format!("pid {pid} is not running")
        };
        return Err(AttachFail::Fatal(Error::backend("native", why)));
    }
    if let Err(errno) = sampler.enable() {
        return Err(AttachFail::Fatal(Error::backend(
            "native",
            format!("perf events for pid {pid} could not be enabled (errno {errno})"),
        )));
    }
    Ok(sampler)
}

fn attr_includes_kernel(attr: &PerfEventAttr) -> bool {
    attr.flags & (1 << 5) == 0
}

#[derive(Debug)]
enum OpenError {
    Denied(i32),
    Invalid,
    Gone,
    Other(i32),
}

fn open_one(tid: i32, attr: &PerfEventAttr) -> Result<i32, OpenError> {
    // SAFETY: `attr` is a 96-byte `perf_event_attr`. The call creates a
    // read-only sampling event for one thread. It does not write the
    // target's memory or registers and does not stop the thread.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            attr as *const PerfEventAttr,
            tid,
            -1i32,
            -1i32,
            FLAG_FD_CLOEXEC,
        )
    };
    if rc < 0 {
        return Err(classify(errno_now()));
    }
    Ok(rc as i32)
}

fn classify(errno: i32) -> OpenError {
    match errno {
        1 | 13 => OpenError::Denied(errno),
        22 => OpenError::Invalid,
        3 => OpenError::Gone,
        other => OpenError::Other(other),
    }
}

fn map_ring(fd: i32) -> Result<(*mut u8, usize), i32> {
    let page = page_size();
    let mut pages = preferred_data_pages();
    loop {
        let len = page.saturating_mul(pages.saturating_add(1));
        // SAFETY: a shared mapping of `len` bytes so the kernel can publish
        // samples for this thread. `Mapped` unmaps it.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr != libc::MAP_FAILED {
            return Ok((ptr.cast(), len));
        }
        let err = errno_now();
        if pages > 4 {
            pages = (pages / 2).max(4);
            continue;
        }
        return Err(err);
    }
}

fn preferred_data_pages() -> usize {
    // Leave room for several threads under `perf_event_mlock_kb`. Four data
    // pages hold one 8 KiB stack sample; 32 is enough for a single busy thread
    // without crowding the others out.
    let kb = read_sysctl("/proc/sys/kernel/perf_event_mlock_kb").unwrap_or(512);
    let kb = usize::try_from(kb).unwrap_or(512).max(32);
    let per_thread = kb.saturating_mul(1024) / 8;
    let pages = per_thread / page_size();
    pages.clamp(5, 33).saturating_sub(1)
}

fn map_error(errno: i32) -> String {
    format!(
        "cannot map the perf ring buffer (errno {errno}). \
         kernel.perf_event_mlock_kb limits how much an unprivileged process can lock. \
         anyprof does not change that sysctl."
    )
}

fn refresh(sampler: &mut Sampler, pid: u32) {
    let Ok(live) = list_tids(pid) else {
        return;
    };
    let attr = event_attr(sampler.period, sampler.stack, sampler.include_kernel);
    for tid in &live {
        if sampler.has_tid(*tid) {
            continue;
        }
        let Ok(fd) = open_one(*tid, &attr) else {
            continue;
        };
        match map_ring(fd) {
            Ok((ptr, len)) => {
                let mapped = Mapped {
                    fd,
                    tid: *tid,
                    ptr,
                    len,
                };
                if ioctl_req(mapped.fd, IOC_ENABLE) {
                    sampler.events.push(mapped);
                } else {
                    sampler.unmapped = sampler.unmapped.saturating_add(1);
                }
            }
            Err(_) => {
                unsafe { libc::close(fd) };
                sampler.unmapped = sampler.unmapped.saturating_add(1);
            }
        }
    }
    sampler.events.retain(|event| live.contains(&event.tid));
}

impl Sampler {
    fn has_tid(&self, tid: i32) -> bool {
        self.events.iter().any(|event| event.tid == tid)
    }

    fn enable(&self) -> Result<(), i32> {
        for event in &self.events {
            if !ioctl_req(event.fd, IOC_ENABLE) {
                return Err(errno_now());
            }
        }
        Ok(())
    }

    fn disable(&self) {
        for event in &self.events {
            let _ = ioctl_req(event.fd, IOC_DISABLE);
        }
    }

    fn drain(&self) -> (Vec<ParsedSample>, u64) {
        let mut samples = Vec::new();
        let mut lost = 0u64;
        for event in &self.events {
            let (part, part_lost) = drain_mapped(event);
            samples.extend(part);
            lost = lost.saturating_add(part_lost);
        }
        (samples, lost)
    }
}

fn drain_mapped(mapped: &Mapped) -> (Vec<ParsedSample>, u64) {
    if mapped.ptr.is_null() || mapped.len < DATA_SIZE + 8 {
        return (Vec::new(), 0);
    }
    let Some((data_ptr, data_len)) = data_region(mapped.ptr, mapped.len) else {
        return (Vec::new(), 0);
    };
    // SAFETY: `data_head` and `data_tail` sit at fixed offsets in the perf
    // mmap header. The kernel publishes `data_head`; this process publishes
    // `data_tail`. Both are 8-byte aligned because mmap is page aligned.
    // Acquire orders the header read before the record bytes. Release orders
    // those reads before the tail update so the kernel does not overwrite
    // unread samples.
    let head_ptr = unsafe { mapped.ptr.add(DATA_HEAD).cast::<u64>() };
    let tail_ptr = unsafe { mapped.ptr.add(DATA_TAIL).cast::<u64>() };
    let head = unsafe { AtomicU64::from_ptr(head_ptr).load(Ordering::Acquire) };
    let mut tail = unsafe { AtomicU64::from_ptr(tail_ptr).load(Ordering::Relaxed) };
    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
    let mut samples = Vec::new();
    let mut lost = 0u64;
    if head.wrapping_sub(tail) > data_len as u64 {
        lost = lost.saturating_add(1);
        tail = head;
    }
    while tail < head {
        let Some(header) = perf_parse::read_ring(data, tail, 8) else {
            break;
        };
        let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let size = u16::from_le_bytes(header[6..8].try_into().unwrap()) as usize;
        if size < 8 || size > data_len || tail.saturating_add(size as u64) > head {
            lost = lost.saturating_add(1);
            tail = head;
            break;
        }
        let Some(record) = perf_parse::read_ring(data, tail, size) else {
            break;
        };
        if kind == RECORD_SAMPLE {
            if let Some(sample) = perf_parse::parse_sample(&record, X86_USER_REGS) {
                samples.push(sample);
            }
        } else if kind == RECORD_LOST && record.len() >= 24 {
            let count = u64::from_le_bytes(record[16..24].try_into().unwrap());
            lost = lost.saturating_add(count);
        } else if kind == RECORD_LOST_SAMPLES && record.len() >= 16 {
            let count = u64::from_le_bytes(record[8..16].try_into().unwrap());
            lost = lost.saturating_add(count);
        }
        tail = tail.saturating_add(size as u64);
    }
    unsafe { AtomicU64::from_ptr(tail_ptr).store(tail, Ordering::Release) };
    (samples, lost)
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.disable();
        self.events.clear();
    }
}

fn data_region(base: *mut u8, len: usize) -> Option<(*const u8, usize)> {
    let offset = read_header_u64(base, DATA_OFFSET) as usize;
    let size = read_header_u64(base, DATA_SIZE) as usize;
    if offset > 0 && size > 0 && offset.saturating_add(size) <= len {
        return Some((unsafe { base.add(offset) }, size));
    }
    let page = page_size();
    if len > page {
        Some((unsafe { base.add(page) }, len - page))
    } else {
        None
    }
}

fn read_header_u64(base: *mut u8, offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    unsafe {
        std::ptr::copy_nonoverlapping(base.add(offset), buf.as_mut_ptr(), 8);
    }
    u64::from_le_bytes(buf)
}

fn take_samples(
    sampler: &Sampler,
    pid: u32,
    include_kernel: bool,
    counts: &mut HashMap<SampleKey, u64>,
) -> u64 {
    let (samples, lost) = sampler.drain();
    for sample in samples {
        if sample.pid != pid && sample.pid != 0 {
            continue;
        }
        let Some(key) = assemble_addresses(&sample, include_kernel) else {
            continue;
        };
        *counts.entry(key).or_default() += 1;
    }
    lost
}

/// Leaf-first addresses and how many leading frames are kernel addresses.
///
/// The kernel section and the user section each have their own leaf, so
/// symbol lookup subtracts one byte only from the saved frames of that section.
fn assemble_addresses(sample: &ParsedSample, include_kernel: bool) -> Option<SampleKey> {
    let (kernel_chain, user_chain) = perf_parse::split_callchain(&sample.callchain);
    let mut kernel = if include_kernel {
        kernel_chain
    } else {
        Vec::new()
    };
    if include_kernel && kernel.is_empty() && sample.ip >= 0xffff_8000_0000_0000 {
        kernel.push(sample.ip);
    }
    kernel.truncate(walk::MAX_FRAMES);

    let pc = sample
        .user_ip
        .filter(|&ip| walk::plausible_user_address(ip))
        .or_else(|| walk::plausible_user_address(sample.ip).then_some(sample.ip))
        .unwrap_or(0);
    let fp = sample.user_fp.unwrap_or(0);
    let sp = sample.user_sp.unwrap_or(0);
    let stack = sample.stack.as_slice();
    let walked = walk_frame_pointer(pc, fp, 0, |addr, buf| {
        read_copied_stack(stack, sp, addr, buf)
    });
    let mut user = perf_parse::choose_user_frames(walked, user_chain);
    if user.is_empty() && walk::plausible_user_address(sample.ip) {
        user.push(sample.ip);
    }
    user.truncate(walk::MAX_FRAMES);
    if kernel.is_empty() && user.is_empty() {
        return None;
    }
    let kernel_len = u16::try_from(kernel.len()).unwrap_or(u16::MAX);
    let mut frames = kernel;
    frames.extend(user);
    Some(SampleKey { frames, kernel_len })
}

fn read_copied_stack(stack: &[u8], sp: u64, addr: u64, buf: &mut [u8]) -> bool {
    let Some(offset) = addr.checked_sub(sp) else {
        return false;
    };
    let Ok(offset) = usize::try_from(offset) else {
        return false;
    };
    let Some(end) = offset.checked_add(buf.len()) else {
        return false;
    };
    if end > stack.len() {
        return false;
    }
    buf.copy_from_slice(&stack[offset..end]);
    true
}

fn finish_profile(
    pid: u32,
    comm: Option<String>,
    counts: HashMap<SampleKey, u64>,
    images: &[UserImage],
    kernel_symbols: &[Kallsym],
    elapsed: Duration,
    interval: Duration,
) -> Profile {
    let mut stacks = Vec::with_capacity(counts.len());
    for (key, weight) in counts {
        let kernel_len = key.kernel_len as usize;
        let frames = key
            .frames
            .into_iter()
            .enumerate()
            .map(|(index, addr)| {
                let leaf = index == 0 || (kernel_len > 0 && index == kernel_len);
                if index < kernel_len {
                    kernel_frame(kernel_symbols, addr, leaf)
                } else {
                    user_frame(images, addr, leaf)
                }
            })
            .collect();
        stacks.push(Stack { frames, weight });
    }
    stacks.sort_by_key(|stack| std::cmp::Reverse(stack.weight));
    Profile {
        pid,
        comm,
        duration: elapsed,
        period: Some(interval),
        stacks,
        raw_cpuprofile: None,
    }
}

fn user_frame(images: &[UserImage], addr: u64, leaf: bool) -> Frame {
    let lookup = policy::symbol_lookup_address(addr, leaf);
    let named = elf::name_for(images, addr, leaf);
    let module = named.map(|(_, module)| module.to_string()).or_else(|| {
        images
            .iter()
            .find(|image| image.contains(lookup) || image.contains(addr))
            .map(|image| image.module.clone())
    });
    let mut frame = Frame::new(Runtime::Native, named.map(|(name, _)| name).unwrap_or(""));
    frame.address = Some(addr);
    frame.module = module;
    frame
}

fn kernel_frame(symbols: &[Kallsym], addr: u64, leaf: bool) -> Frame {
    let lookup = policy::symbol_lookup_address(addr, leaf);
    let name = kallsyms::name_at(symbols, lookup)
        .or_else(|| kallsyms::name_at(symbols, addr))
        .unwrap_or("");
    let mut frame = Frame::new(Runtime::Kernel, name);
    frame.address = Some(addr);
    frame.module = Some("kernel".to_string());
    frame
}

fn load_images(pid: u32) -> Vec<UserImage> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
    images_from_maps(&text)
}

fn images_from_maps(text: &str) -> Vec<UserImage> {
    let spans = maps::parse_maps(text);
    let mut order = Vec::new();
    let mut groups: HashMap<String, Vec<MapSpan>> = HashMap::new();
    for span in spans {
        let key = maps::live_path(&span.path).to_string();
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(span);
    }
    let mut images = Vec::new();
    for path in order.into_iter().take(512) {
        let Some(group) = groups.remove(&path) else {
            continue;
        };
        images.push(image_from_group(&path, &group));
    }
    images
}

fn image_from_group(path: &str, group: &[MapSpan]) -> UserImage {
    let start = group.iter().map(|span| span.start).min().unwrap_or(0);
    let end = group.iter().map(|span| span.end).max().unwrap_or(start);
    let module = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string();
    let Some(elf) = elf::parse_path(Path::new(path)) else {
        return UserImage::range_only(start, end, module);
    };
    if !elf::is_x86_64(&elf) {
        return UserImage::range_only(start, end, module);
    }
    let anchor = group
        .iter()
        .find(|span| span.executable)
        .unwrap_or(&group[0]);
    let Some(bias) = elf::load_bias(anchor.start, anchor.offset, &elf.loads) else {
        return UserImage::range_only(start, end, module);
    };
    UserImage::from_elf(start, end, module, &elf, bias)
}

fn load_kallsyms() -> Vec<Kallsym> {
    let text = std::fs::read_to_string("/proc/kallsyms").unwrap_or_default();
    let symbols = kallsyms::parse_kallsyms(&text);
    if text.trim().is_empty() {
        tracing::warn!(
            "/proc/kallsyms could not be read; -k frames will keep raw addresses. anyprof will not change kernel.kptr_restrict"
        );
    } else if !kallsyms::visible(&symbols) {
        tracing::warn!(
            "kernel symbol addresses are hidden; -k frames will keep raw addresses. kernel.kptr_restrict controls this, and anyprof will not change it"
        );
    }
    symbols
}

fn reject_foreign_elf(pid: u32) -> Result<(), Error> {
    let mut file = match File::open(format!("/proc/{pid}/exe")) {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };
    let mut bytes = [0u8; 20];
    if file.read(&mut bytes).unwrap_or(0) < 20 || bytes[0..4] != *b"\x7fELF" {
        return Ok(());
    }
    let class = bytes[4];
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if class == ELFCLASS64 && machine == EM_X86_64 {
        return Ok(());
    }
    Err(Error::backend(
        "native",
        format!(
            "cannot sample pid {pid}: native sampling on Linux walks x86_64 frame pointers; this process is ELF class {class} machine {machine}"
        ),
    ))
}

fn list_tids(pid: u32) -> Result<Vec<i32>, String> {
    let dir = std::fs::read_dir(format!("/proc/{pid}/task"))
        .map_err(|err| format!("cannot read /proc/{pid}/task ({err})"))?;
    let mut tids = Vec::new();
    for entry in dir {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(tid) = name.parse::<i32>() else {
            continue;
        };
        if tid > 0 {
            tids.push(tid);
        }
    }
    tids.sort_unstable();
    if let Ok(leader) = i32::try_from(pid)
        && let Some(index) = tids.iter().position(|tid| *tid == leader)
    {
        tids.swap(0, index);
    }
    Ok(tids)
}

fn permission_error(pid: u32, errno: i32, kernel: bool) -> String {
    let mut text = policy::linux_perf_denied(LinuxDenial {
        pid,
        errno,
        paranoid: read_sysctl("/proc/sys/kernel/perf_event_paranoid"),
        ptrace_scope: read_sysctl("/proc/sys/kernel/yama/ptrace_scope"),
        target_uid: target_uid(pid),
        self_uid: unsafe { libc::geteuid() },
        kernel,
    });
    if errno == 22 {
        text.push(' ');
        text.push_str(&invalid_event(pid, 0, 0));
    }
    text
}

fn invalid_event(pid: u32, period: u64, stack: u32) -> String {
    let rate = std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unreadable".to_string());
    format!(
        "perf_event_open rejected the event for pid {pid} (EINVAL, errno 22, period {period} ns, stack {stack} bytes). kernel.perf_event_max_sample_rate is {rate}. A very short interval can exceed that rate. anyprof does not change the sysctl."
    )
}

fn read_sysctl(path: &str) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}

fn target_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("Uid:") else {
            continue;
        };
        return rest.split_whitespace().next()?.parse().ok();
    }
    None
}

fn page_size() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n <= 0 { 4096 } else { n as usize }
}

fn errno_now() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn ioctl_req(fd: i32, request: u64) -> bool {
    // SAFETY: `PERF_EVENT_IOC_ENABLE` / `DISABLE` take no payload. The third
    // argument is ignored for these `_IO` requests.
    unsafe { libc::ioctl(fd, request, 0) == 0 }
}

fn wait_until(deadline: Instant, opts: &RecordOptions) {
    while Instant::now() < deadline {
        if opts.stop.stop_requested() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(20)));
    }
}

#[cfg(test)]
mod tests {
    use super::assemble_addresses;
    use crate::perf_parse::{CONTEXT_KERNEL, CONTEXT_USER, ParsedSample};

    #[test]
    fn a_saved_frame_in_the_copy_is_kept() {
        let sp = 0x7fff_0000_1000u64;
        let next_fp = sp + 0x20;
        let ret = 0x5555_0000_1200u64;
        let mut stack = vec![0u8; 32];
        stack[0..8].copy_from_slice(&next_fp.to_le_bytes());
        stack[8..16].copy_from_slice(&ret.to_le_bytes());
        let sample = ParsedSample {
            ip: 0x5555_0000_1100,
            pid: 1,
            tid: 1,
            callchain: vec![0x5555_0000_1100],
            user_ip: Some(0x5555_0000_1100),
            user_sp: Some(sp),
            user_fp: Some(sp),
            stack,
        };
        let key = assemble_addresses(&sample, false).expect("frames");
        assert_eq!(key.kernel_len, 0);
        assert_eq!(key.frames, vec![0x5555_0000_1100, ret]);
    }

    #[test]
    fn kernel_and_user_sections_keep_separate_leaves() {
        let sample = ParsedSample {
            ip: 0xffff_ffff_8100_0100,
            pid: 4,
            tid: 4,
            callchain: vec![
                CONTEXT_KERNEL,
                0xffff_ffff_8100_0100,
                CONTEXT_USER,
                0x5555_0000_1100,
                0x5555_0000_1000,
            ],
            user_ip: Some(0x5555_0000_1100),
            user_sp: None,
            user_fp: None,
            stack: Vec::new(),
        };
        let with_kernel = assemble_addresses(&sample, true).expect("frames");
        assert_eq!(with_kernel.kernel_len, 1);
        assert_eq!(
            with_kernel.frames,
            vec![0xffff_ffff_8100_0100, 0x5555_0000_1100, 0x5555_0000_1000]
        );
        let user_only = assemble_addresses(&sample, false).expect("user frames");
        assert_eq!(user_only.kernel_len, 0);
        assert_eq!(user_only.frames, vec![0x5555_0000_1100, 0x5555_0000_1000]);
    }
}
