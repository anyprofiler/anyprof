//! macOS on-CPU sampler.
//!
//! `task_for_pid` yields a Mach task port (root, or a debugger entitlement).
//! Each tick asks `thread_info` which threads are actually running, suspends
//! those threads, reads `ARM_THREAD_STATE64` / `x86_THREAD_STATE64`, and walks
//! the frame-pointer chain with `mach_vm_read_overwrite`. A tick with no
//! running thread adds no samples. Threads are resumed before their send
//! rights are dropped. Symbols come from each image's Mach-O symbol table
//! ([`crate::macho`]). Signed arm64 pointers are stripped with `xpaci` /
//! `xpacd` when the thread-state flags say so; they are not authenticated.
//!
//! Kernel frames are not available through this interface. `-k` is ignored and
//! a warning is logged. Linux x86_64 records kernel frames with perf events.
//! Windows x64 samples user frames only, the same as macOS.

use std::collections::HashMap;
use std::ffi::c_char;
use std::path::Path;
use std::time::{Duration, Instant};

use anyprof_core::{
    Error, Frame, PERF_SAMPLING_INTERVAL, Profile, RecordOptions, Runtime, Stack, Target, inspect,
};
use mach2::kern_return::{KERN_SUCCESS, kern_return_t};
use mach2::mach_types::{task_t, thread_act_array_t, thread_act_t};
use mach2::message::mach_msg_type_number_t;
use mach2::port::mach_port_t;
use mach2::task::{task_info, task_threads};
use mach2::task_info::TASK_DYLD_INFO;
use mach2::thread_act::{thread_get_state, thread_resume, thread_suspend};
use mach2::traps::{mach_task_self, task_for_pid};
use mach2::vm::mach_vm_deallocate;
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};

use crate::macho::{ImageSymbols, MachCpu, segment_bounds};
use crate::policy::{self, StripKind};

const MAX_FRAMES: usize = 64;
const THREAD_BASIC_INFO: u32 = 3;
const TH_STATE_RUNNING: i32 = 1;

#[cfg(target_arch = "aarch64")]
use mach2::structs::arm_thread_state64_t as ThreadState;
#[cfg(target_arch = "x86_64")]
use mach2::structs::x86_thread_state64_t as ThreadState;

#[cfg(target_arch = "aarch64")]
use mach2::thread_status::ARM_THREAD_STATE64 as THREAD_STATE;
#[cfg(target_arch = "x86_64")]
use mach2::thread_status::x86_THREAD_STATE64 as THREAD_STATE;

#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_char, buffersize: u32) -> i32;
}

unsafe extern "C" {
    fn thread_info(
        target_act: thread_act_t,
        flavor: u32,
        thread_info_out: *mut i32,
        thread_info_out_cnt: *mut mach_msg_type_number_t,
    ) -> kern_return_t;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> kern_return_t;
}

pub fn record(target: &Target, opts: &RecordOptions) -> Result<Profile, Error> {
    if opts.include_kernel {
        tracing::warn!("ignoring -k/--kernel: the macOS native sampler collects user frames only");
    }
    let interval = opts.sampling_interval.unwrap_or(PERF_SAMPLING_INTERVAL);
    if interval < Duration::from_micros(1) {
        return Err(Error::backend(
            "native",
            "sampling interval must be at least 1us",
        ));
    }
    if let Err(message) = policy::reject_self_sample(target.pid, std::process::id()) {
        return Err(Error::backend("native", message));
    }

    let task = Task::attach(target.pid)?;
    let symbols = load_symbols(task.port, target.pid);
    tracing::debug!(
        pid = target.pid,
        images = symbols.len(),
        "native symbol tables loaded"
    );

    let started = Instant::now();
    let mut next = started;
    let mut counts: HashMap<Vec<u64>, u64> = HashMap::new();
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
        match task.sample_addresses() {
            Ok(stacks) => {
                for stack in stacks {
                    if !stack.is_empty() {
                        *counts.entry(stack).or_default() += 1;
                    }
                }
            }
            Err(SampleFault::Fatal(err)) => {
                // A failed resume can leave a target thread suspended, and an
                // unbounded `task_threads` count can leak a send right. Stop
                // instead of turning that into a partial profile.
                tracing::error!(pid = target.pid, %err, "native sample stopped");
                return Err(Error::backend("native", err));
            }
            Err(SampleFault::Tick(err)) => {
                tracing::debug!(pid = target.pid, %err, "native sample failed");
                if counts.is_empty() && started.elapsed() < Duration::from_millis(50) {
                    return Err(Error::backend("native", err));
                }
                break;
            }
        }
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
        if opts
            .duration
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            break;
        }
    }

    let comm = inspect(target.pid).and_then(|snapshot| snapshot.comm);
    Ok(finish_profile(
        target.pid,
        comm,
        counts,
        &symbols,
        started.elapsed(),
        interval,
    ))
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

fn finish_profile(
    pid: u32,
    comm: Option<String>,
    counts: HashMap<Vec<u64>, u64>,
    symbols: &[ImageSymbols],
    elapsed: Duration,
    interval: Duration,
) -> Profile {
    let mut stacks = Vec::with_capacity(counts.len());
    for (addresses, weight) in counts {
        let frames = addresses
            .into_iter()
            .enumerate()
            .map(|(index, addr)| frame_for(addr, symbols, index == 0))
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

fn frame_for(addr: u64, symbols: &[ImageSymbols], leaf: bool) -> Frame {
    let lookup = policy::symbol_lookup_address(addr, leaf);
    let image = symbols
        .iter()
        .find(|image| image.contains(lookup))
        .or_else(|| symbols.iter().find(|image| image.contains(addr)));
    let name = image.and_then(|image| image.name_at(lookup)).unwrap_or("");
    let mut frame = Frame::new(Runtime::Native, name);
    frame.address = Some(addr);
    frame.module = image.map(|image| image.module.clone());
    frame
}

struct Task {
    port: mach_port_t,
}

impl Task {
    fn attach(pid: u32) -> Result<Self, Error> {
        let pid_i = i32::try_from(pid).map_err(|_| {
            Error::backend("native", format!("pid {pid} does not fit in a Mach pid"))
        })?;
        let mut port: mach_port_t = 0;
        // SAFETY: `task_for_pid` writes one send right into `port`. The kernel
        // rejects the call (typically kern return 5) unless we are root or
        // hold a debugger entitlement. The port is deallocated in `Drop`.
        let kr = unsafe { task_for_pid(mach_task_self(), pid_i, &mut port) };
        if kr != KERN_SUCCESS || port == 0 {
            return Err(Error::backend("native", permission_message(pid, kr)));
        }
        Ok(Self { port })
    }

    fn sample_addresses(&self) -> Result<Vec<Vec<u64>>, SampleFault> {
        let threads = self.threads()?;
        let running: Vec<thread_act_t> = threads
            .ports
            .iter()
            .copied()
            .filter(|thread| thread_is_running(*thread))
            .collect();
        // Sleeping threads are not on CPU. An idle tick contributes no samples
        // instead of a weight-1 stack for every waiter.
        let chosen = policy::on_cpu_threads(running.as_slice(), threads.ports.as_slice());
        let mut stacks = Vec::with_capacity(chosen.len());
        for thread in chosen {
            // `threads` stays alive until this function returns, so each
            // resume in `unwind_thread` happens before `ThreadList` drops the
            // send rights.
            match unwind_thread(self.port, *thread) {
                Ok(Some(frames)) => stacks.push(frames),
                Ok(None) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(stacks)
    }

    fn threads(&self) -> Result<ThreadList, SampleFault> {
        let mut list: thread_act_array_t = std::ptr::null_mut();
        let mut count: mach_msg_type_number_t = 0;
        // SAFETY: on success the kernel allocates `list` in our task. A
        // failed call does not hand that array back (the stub returns before
        // publishing it, and deallocating a pointer it did not publish would
        // be a double free). The outs start as null and zero.
        let kr = unsafe { task_threads(self.port, &mut list, &mut count) };
        if kr != KERN_SUCCESS {
            return Err(SampleFault::Tick(format!(
                "task_threads failed (kern return {kr})"
            )));
        }
        let plan = policy::plan_thread_array(
            list.is_null(),
            count,
            std::mem::size_of::<thread_act_t>() as u32,
        );
        match plan {
            policy::ThreadArrayPlan::Sample { count } => {
                let ports = unsafe { std::slice::from_raw_parts(list, count as usize) }.to_vec();
                release_thread_array(list, plan, false);
                Ok(ThreadList { ports })
            }
            policy::ThreadArrayPlan::Release { count } => {
                release_thread_array(list, plan, true);
                Err(SampleFault::Tick(policy::too_many_threads(count)))
            }
            policy::ThreadArrayPlan::Nothing => {
                // Null, or a zero count. A zero count does not prove how many
                // bytes were allocated, so the array is not unmapped.
                Err(SampleFault::Tick(
                    "task_threads returned no threads".to_string(),
                ))
            }
            policy::ThreadArrayPlan::Unprovable { count } => {
                tracing::error!(
                    count,
                    "task_threads count is not a bounded allocation; leaving it untouched"
                );
                eprintln!(
                    "anyprof: task_threads returned an unbounded count ({count}); the array was not walked or unmapped"
                );
                Err(SampleFault::Fatal(format!(
                    "task_threads returned an unsafe count {count}; the array was not walked or unmapped"
                )))
            }
        }
    }
}

/// A sample tick failed, or the recording has to stop.
enum SampleFault {
    /// The target may still be suspended, or a Mach allocation was left in
    /// place because its size could not be proved. Do not return a profile.
    Fatal(String),
    /// This tick produced nothing. Keep samples already collected.
    Tick(String),
}

/// Send rights returned by `task_threads`. Dropped after the threads have been
/// resumed, so the rights stay valid for the whole sample.
struct ThreadList {
    ports: Vec<thread_act_t>,
}

impl Drop for ThreadList {
    fn drop(&mut self) {
        for port in &self.ports {
            if policy::should_deallocate_send_right(*port) {
                unsafe { mach_port_deallocate(mach_task_self(), *port) };
            }
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if self.port != 0 {
            unsafe { mach_port_deallocate(mach_task_self(), self.port) };
        }
    }
}

/// Release the vm allocation `task_threads` returned.
///
/// `drop_ports` also drops each send right. The sample path copies the ports
/// out first and then calls this with `drop_ports == false`.
fn release_thread_array(list: thread_act_array_t, plan: policy::ThreadArrayPlan, drop_ports: bool) {
    let count = match plan {
        policy::ThreadArrayPlan::Sample { count } | policy::ThreadArrayPlan::Release { count } => {
            count
        }
        policy::ThreadArrayPlan::Nothing | policy::ThreadArrayPlan::Unprovable { .. } => return,
    };
    if list.is_null() || count == 0 {
        return;
    }
    if drop_ports {
        let ports = unsafe { std::slice::from_raw_parts(list, count as usize) };
        for port in ports {
            if policy::should_deallocate_send_right(*port) {
                unsafe { mach_port_deallocate(mach_task_self(), *port) };
            }
        }
    }
    deallocate_thread_array(list, count);
}

fn deallocate_thread_array(list: thread_act_array_t, count: u32) {
    let Some(bytes) = u64::from(count).checked_mul(std::mem::size_of::<thread_act_t>() as u64)
    else {
        return;
    };
    if list.is_null() || bytes == 0 {
        return;
    }
    unsafe {
        mach_vm_deallocate(mach_task_self(), list as mach_vm_address_t, bytes);
    }
}

fn thread_is_running(thread: thread_act_t) -> bool {
    let mut info = [0i32; 16];
    let mut count: mach_msg_type_number_t = 10;
    // SAFETY: `thread_info` writes `count` integers into `info`. 10 is
    // THREAD_BASIC_INFO_COUNT; `run_state` is the 7th field.
    let kr = unsafe { thread_info(thread, THREAD_BASIC_INFO, info.as_mut_ptr(), &mut count) };
    kr == KERN_SUCCESS && count > 6 && info[6] == TH_STATE_RUNNING
}

struct Suspended<'a> {
    thread: thread_act_t,
    resumed: bool,
    failure: &'a mut Option<kern_return_t>,
}

impl<'a> Suspended<'a> {
    fn new(thread: thread_act_t, failure: &'a mut Option<kern_return_t>) -> Option<Self> {
        // SAFETY: suspend increments the thread's suspend count. `Drop`
        // resumes exactly once, including when unwinding this function.
        let kr = unsafe { thread_suspend(thread) };
        if kr == KERN_SUCCESS {
            Some(Self {
                thread,
                resumed: false,
                failure,
            })
        } else {
            None
        }
    }
}

impl Drop for Suspended<'_> {
    fn drop(&mut self) {
        if self.resumed {
            return;
        }
        // Three immediate attempts and no sleep: the thread is still
        // suspended while this runs. A second `thread_resume` after success
        // returns an error, so a success is recorded and not repeated.
        match policy::retry_until_success(3, KERN_SUCCESS, || unsafe { thread_resume(self.thread) })
        {
            Ok(_) => self.resumed = true,
            Err(kr) => {
                *self.failure = Some(kr);
                tracing::error!(
                    kern = kr,
                    thread = self.thread,
                    "thread_resume failed; the target thread may still be suspended"
                );
                eprintln!(
                    "anyprof: thread_resume failed (kern return {kr}); a target thread may still be suspended"
                );
            }
        }
    }
}

fn unwind_thread(task: task_t, thread: thread_act_t) -> Result<Option<Vec<u64>>, SampleFault> {
    let mut resume_failure = None;
    let Some(suspended) = Suspended::new(thread, &mut resume_failure) else {
        return Ok(None);
    };
    let mut state = ThreadState::new();
    let mut count = ThreadState::count();
    // SAFETY: `thread_get_state` fills `state`, which is the register struct
    // the flavor names. The thread is suspended for the read.
    let kr = unsafe {
        thread_get_state(
            thread,
            THREAD_STATE,
            std::ptr::from_mut(&mut state).cast::<mach2::vm_types::natural_t>(),
            &mut count,
        )
    };
    let frames = if kr == KERN_SUCCESS {
        let (pc, fp, lr) = registers(&state);
        let frames = walk_frames(task, pc, fp, lr);
        if frames.is_empty() {
            None
        } else {
            Some(frames)
        }
    } else {
        None
    };
    drop(suspended);
    if let Some(resume_kr) = resume_failure {
        return Err(SampleFault::Fatal(format!(
            "thread_resume failed (kern return {resume_kr}); a target thread may still be suspended"
        )));
    }
    Ok(frames)
}

#[cfg(target_arch = "aarch64")]
fn registers(state: &ThreadState) -> (u64, u64, u64) {
    let flags = state.__flags;
    (
        apply_strip(state.__pc, flags, StripKind::Code, false),
        apply_strip(state.__fp, flags, StripKind::Data, false),
        apply_strip(state.__lr, flags, StripKind::Code, true),
    )
}

#[cfg(target_arch = "x86_64")]
fn registers(state: &ThreadState) -> (u64, u64, u64) {
    // x86_64 pointers are not authenticated.
    (state.__rip, state.__rbp, 0)
}

fn walk_frames(task: task_t, pc: u64, fp: u64, lr: u64) -> Vec<u64> {
    let mut frames = Vec::new();
    if plausible_code(pc) {
        frames.push(pc);
    }
    let mut fp = fp;
    let mut guard = 0;
    while frames.len() < MAX_FRAMES && plausible_frame(fp) {
        let mut record = [0u8; 16];
        if !read_task(task, fp, &mut record) {
            break;
        }
        // `flags` applies to the registers already stripped above. A stack
        // slot keeps its own signature: an arm64 process can be inside an
        // arm64e callee. `stack_strip_flags` ignores `NO_PTRAUTH` so `xpacd`
        // and `xpaci` still run. Both are the identity on an unsigned pointer.
        let stack_flags = policy::stack_strip_flags();
        let next_fp = apply_strip(
            u64::from_le_bytes(record[0..8].try_into().unwrap()),
            stack_flags,
            StripKind::Data,
            false,
        );
        let ret = apply_strip(
            u64::from_le_bytes(record[8..16].try_into().unwrap()),
            stack_flags,
            StripKind::Code,
            false,
        );
        if !plausible_code(ret) {
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
    if frames.len() == 1 && plausible_code(lr) && lr != frames[0] {
        frames.push(lr);
    }
    frames
}

fn plausible_code(addr: u64) -> bool {
    addr > 0x1000 && addr < 0x0000_8000_0000_0000
}

fn plausible_frame(fp: u64) -> bool {
    fp > 0x1000 && fp < 0x0000_8000_0000_0000 && fp & 0x7 == 0
}

/// Strip a signed pointer the way Darwin's `ptrauth_strip` macro does.
///
/// `xpaci` clears an instruction-key signature and `xpacd` clears a data-key
/// signature. Neither authenticates, so a bad signature cannot trap. The
/// instructions are only emitted on Apple Silicon. Intel macOS and every
/// other build pass the address through when the flags say it is signed;
/// they do not apply a fixed bit mask. `NO_PTRAUTH` leaves the bits alone on
/// every architecture.
fn apply_strip(addr: u64, flags: u32, kind: StripKind, is_lr: bool) -> u64 {
    policy::strip_pointer(addr, flags, kind, is_lr, hardware_strip)
}

#[cfg(target_arch = "aarch64")]
fn hardware_strip(addr: u64, kind: StripKind) -> u64 {
    match kind {
        StripKind::Code => xpaci(addr),
        StripKind::Data => xpacd(addr),
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn hardware_strip(addr: u64, kind: StripKind) -> u64 {
    let _ = kind;
    addr
}

/// `XPACI`: strip an instruction signature. It does not authenticate, so a bad
/// signature cannot trap. The instruction is not compiled for Intel macOS.
#[cfg(target_arch = "aarch64")]
fn xpaci(addr: u64) -> u64 {
    let stripped;
    // SAFETY: `xpaci` only clears the instruction-key signature bits. Apple
    // Silicon implements it. This is not built for x86_64.
    unsafe {
        std::arch::asm!(
            "xpaci {p}",
            p = inout(reg) addr => stripped,
            options(nomem, nostack, preserves_flags)
        );
    }
    stripped
}

/// `XPACD`: strip a data signature. Same trap rule as [`xpaci`].
#[cfg(target_arch = "aarch64")]
fn xpacd(addr: u64) -> u64 {
    let stripped;
    // SAFETY: `xpacd` only clears the data-key signature bits.
    unsafe {
        std::arch::asm!(
            "xpacd {p}",
            p = inout(reg) addr => stripped,
            options(nomem, nostack, preserves_flags)
        );
    }
    stripped
}

fn read_task(task: task_t, addr: u64, buf: &mut [u8]) -> bool {
    if addr == 0 || buf.is_empty() {
        return false;
    }
    let mut copied: mach_vm_size_t = 0;
    // SAFETY: `mach_vm_read_overwrite` copies `buf.len()` bytes from the
    // target into `buf`. A bad address returns an error and does not trap.
    let kr = unsafe {
        mach2::vm::mach_vm_read_overwrite(
            task,
            addr,
            buf.len() as mach_vm_size_t,
            buf.as_mut_ptr() as mach_vm_address_t,
            &mut copied,
        )
    };
    kr == KERN_SUCCESS && copied == buf.len() as u64
}

fn load_symbols(task: task_t, pid: u32) -> Vec<ImageSymbols> {
    let cpu = MachCpu::host();
    let mut images = Vec::new();
    for remote in remote_images(task) {
        let module = module_name(&remote.path);
        let file_symbols =
            ImageSymbols::parse_file(Path::new(&remote.path), remote.load, &module, cpu);
        if let Some(parsed) = file_symbols {
            images.push(parsed);
            continue;
        }
        if remote.end > remote.load {
            images.push(ImageSymbols::range_only(remote.load, remote.end, module));
        }
    }
    if images.is_empty()
        && let Some(path) = pid_path(pid)
        && let Some(parsed) =
            ImageSymbols::parse_file(Path::new(&path), 0, &module_name(&path), cpu)
    {
        // No dyld list. A slide of 0 is wrong for a PIE binary, so this only
        // helps when the image was linked at a fixed address. The dyld path
        // above is the one the sampler relies on.
        tracing::debug!(pid, path, "native symbol load fell back to proc_pidpath");
        images.push(parsed);
    }
    images
}

struct RemoteImage {
    load: u64,
    end: u64,
    path: String,
}

fn remote_images(task: task_t) -> Vec<RemoteImage> {
    let Some(info_addr) = dyld_info_addr(task) else {
        return Vec::new();
    };
    let mut header = [0u8; 16];
    if !read_task(task, info_addr, &mut header) {
        return Vec::new();
    }
    let count = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let array = u64::from_le_bytes(header[8..16].try_into().unwrap());
    if count == 0 || count > 4096 || array == 0 {
        return Vec::new();
    }
    // `dyld_image_info` is three pointers: header, path, mtime.
    let mut raw = vec![0u8; count.saturating_mul(24)];
    if !read_remote(task, array, &mut raw) {
        return Vec::new();
    }
    let mut images = Vec::with_capacity(count);
    let (rows, _) = raw.as_chunks::<24>();
    for chunk in rows {
        let load = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let path_ptr = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        if load == 0 {
            continue;
        }
        let path = read_remote_cstr(task, path_ptr).unwrap_or_default();
        let end = image_end(task, load).unwrap_or(load.saturating_add(0x1000));
        images.push(RemoteImage { load, end, path });
    }
    images
}

fn dyld_info_addr(task: task_t) -> Option<u64> {
    // Modern kernels want TASK_DYLD_INFO_COUNT == 6 (the struct is 24 bytes
    // with tail padding). Older ones want 5. Try the larger count first and
    // accept either, then read the address from the first 8 bytes.
    for count_in in [6u32, 5] {
        let mut raw = [0i32; 8];
        let mut count = count_in;
        let kr = unsafe { task_info(task, TASK_DYLD_INFO, raw.as_mut_ptr(), &mut count) };
        if kr == KERN_SUCCESS && count >= 2 {
            let addr = (raw[0] as u32 as u64) | ((raw[1] as u32 as u64) << 32);
            if addr > 0x1000 {
                return Some(addr);
            }
        }
    }
    None
}

fn image_end(task: task_t, load: u64) -> Option<u64> {
    let mut header = [0u8; 32];
    if !read_task(task, load, &mut header) {
        return None;
    }
    let sizeofcmds = u32::from_le_bytes(header[20..24].try_into().ok()?) as usize;
    if sizeofcmds == 0 || sizeofcmds > 1024 * 1024 {
        return None;
    }
    let mut bytes = vec![0u8; 32 + sizeofcmds];
    bytes[..32].copy_from_slice(&header);
    if !read_remote(task, load.saturating_add(32), &mut bytes[32..]) {
        return None;
    }
    let (text_vmaddr, end_vmaddr) = segment_bounds(&bytes)?;
    let slide = load.wrapping_sub(text_vmaddr);
    let end = end_vmaddr.wrapping_add(slide);
    (end > load).then_some(end)
}

fn read_remote(task: task_t, mut addr: u64, buf: &mut [u8]) -> bool {
    let mut offset = 0;
    while offset < buf.len() {
        let chunk = (buf.len() - offset).min(128);
        if !read_task(task, addr, &mut buf[offset..offset + chunk]) {
            return false;
        }
        offset += chunk;
        addr = addr.saturating_add(chunk as u64);
    }
    true
}

fn read_remote_cstr(task: task_t, addr: u64) -> Option<String> {
    if addr == 0 {
        return None;
    }
    let mut collected = Vec::new();
    let mut cursor = addr;
    for _ in 0..8 {
        let mut buf = [0u8; 128];
        if !read_task(task, cursor, &mut buf) {
            break;
        }
        if let Some(end) = buf.iter().position(|byte| *byte == 0) {
            collected.extend_from_slice(&buf[..end]);
            break;
        }
        collected.extend_from_slice(&buf);
        cursor = cursor.saturating_add(buf.len() as u64);
        if collected.len() >= 1024 {
            break;
        }
    }
    if collected.is_empty() {
        return None;
    }
    String::from_utf8(collected).ok()
}

fn pid_path(pid: u32) -> Option<String> {
    let mut buf = [0u8; 4096];
    let n = unsafe {
        proc_pidpath(
            pid as i32,
            buf.as_mut_ptr() as *mut c_char,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    let end = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    let text = std::str::from_utf8(&buf[..end]).ok()?;
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn module_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn permission_message(pid: u32, kr: kern_return_t) -> String {
    policy::task_for_pid_denied(pid, kr)
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn xpaci_and_xpacd_keep_an_unsigned_low_pointer() {
        // Apple Silicon implements these instructions. This job is arm64, not
        // arm64e userspace and not Intel. A green run here does not cover
        // x86_64 and does not feed a kernel-signed arm64e pointer through.
        // A raw user pointer has a zero signature, so both strips are the
        // identity. This does not feed a kernel-signed arm64e pointer through
        // the instructions; no CI image runs arm64e userspace.
        let addr = 0x0000_0001_0000_1000u64;
        assert_eq!(super::xpaci(addr), addr);
        assert_eq!(super::xpacd(addr), addr);
    }
}
