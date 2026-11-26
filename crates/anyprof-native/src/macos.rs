//! macOS on-CPU sampler.
//!
//! `task_for_pid` yields a Mach task port (root, or a debugger entitlement).
//! Each tick asks `thread_info` which threads are actually running, suspends
//! those threads, reads `ARM_THREAD_STATE64` / `x86_THREAD_STATE64`, and unwinds
//! Compact Unwind / `__eh_frame` (`framehop`) or the frame-pointer chain over
//! cached 4 KiB stack pages via `mach_vm_read_overwrite`. A tick with no running
//! thread adds no samples. Threads are resumed before their send rights are
//! dropped. Symbols come from each image's Mach-O symbol table ([`crate::macho`])
//! and macOS's `dyld_v1` shared cache ([`crate::dyld_cache`]). Signed arm64
//! pointers are stripped with `xpaci` / `xpacd` when the thread-state flags say
//! so; they are not authenticated.
//!
//! Kernel frames are not available through this interface. `-k` is ignored and
//! a warning is logged. Linux records kernel frames with perf events. Windows
//! x64 samples user frames only, the same as macOS.

use std::path::Path;
use std::time::{Duration, Instant};

use anyprof_core::{
    Error, Frame, PERF_SAMPLING_INTERVAL, Profile, RecordOptions, Runtime, Target, inspect,
};
use framehop::Unwinder as _;
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

use crate::java::{self, JavaVm};
use crate::macho::{ImageSymbols, MachCpu, segment_bounds};
use crate::policy::{self, SampleCollector, StripKind};
use crate::python::{self, CountedStack, PythonVm};
use crate::walk::SectionBytes;

type MachUnwinder = framehop::UnwinderNative<SectionBytes, framehop::MustNotAllocateDuringUnwind>;
type MachUnwindCache = framehop::CacheNative<framehop::MustNotAllocateDuringUnwind>;

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

unsafe extern "C" {
    fn thread_info(
        target_act: thread_act_t,
        flavor: u32,
        thread_info_out: *mut i32,
        thread_info_out_cnt: *mut mach_msg_type_number_t,
    ) -> kern_return_t;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> kern_return_t;
}

pub fn record(
    target: &Target,
    opts: &RecordOptions,
    session_stop: &anyprof_core::StopGate,
    ready_tx: &std::sync::mpsc::SyncSender<Result<(), Error>>,
) -> Result<Profile, Error> {
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

    let mut task = Task::attach(target.pid)?;
    let mut unwinder = MachUnwinder::new();
    let mut unwind_cache = MachUnwindCache::new_in();
    let mut go_images = Vec::new();
    let mut python = None;
    let mut java = None;
    let mut symbols = SymbolTable::new(target.pid, opts.language_unwind);
    symbols.sync_from_task(
        task.port,
        &mut unwinder,
        &mut go_images,
        &mut python,
        &mut java,
    );
    tracing::debug!(
        pid = target.pid,
        images = symbols.images.len(),
        "native unwind tables prepared"
    );
    let _ = ready_tx.send(Ok(()));

    let started = Instant::now();
    let mut next = started;
    let mut collector = SampleCollector::new();
    let mut reattaches = 0u32;
    loop {
        if stop_requested(opts, session_stop) {
            break;
        }
        if opts
            .duration
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            break;
        }
        symbols.sync_from_task(
            task.port,
            &mut unwinder,
            &mut go_images,
            &mut python,
            &mut java,
        );
        match task.sample_addresses(
            &unwinder,
            &mut unwind_cache,
            &go_images,
            python.as_ref(),
            java.as_ref(),
        ) {
            Ok(stacks) => {
                if !stop_requested(opts, session_stop) {
                    symbols.activate_shared_cache_for_hits(&mut unwinder, &stacks);
                }
                let offset_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
                for hit in stacks {
                    if hit.stack.recordable() {
                        collector.record(hit.tid, offset_ns, hit.stack, |_| hit.name);
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
                // When sampling a freshly spawned child (`anyprof rec -- <cmd>`),
                // XNU can replace the child's Mach `task_t` port as `execve` /
                // `amfid` completes, invalidating an early send right (or
                // transiently failing `task_for_pid`) while `pid` is still alive.
                // Retry and re-acquire the task port so sampling continues.
                if reattaches < 60
                    && !stop_requested(opts, session_stop)
                    && anyprof_core::process_exists(target.pid)
                {
                    reattaches += 1;
                    if let Ok(new_task) = Task::attach(target.pid) {
                        let port_changed = new_task.port != task.port;
                        tracing::debug!(
                            pid = target.pid,
                            reattaches,
                            port_changed,
                            %err,
                            "re-attached Mach task port after tick fault on live pid"
                        );
                        task = new_task;
                        if port_changed {
                            unwinder = MachUnwinder::new();
                            unwind_cache = MachUnwindCache::new_in();
                            go_images.clear();
                            python = None;
                            java = None;
                            symbols = SymbolTable::new(target.pid, opts.language_unwind);
                        }
                    }
                    std::thread::sleep(interval.min(Duration::from_millis(5)));
                    continue;
                }
                tracing::debug!(pid = target.pid, %err, "native sample failed");
                if collector.is_empty()
                    && started.elapsed() < Duration::from_millis(50)
                    && !stop_requested(opts, session_stop)
                    && anyprof_core::process_exists(target.pid)
                {
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
        wait_until(next, opts, session_stop);
        if opts
            .duration
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            break;
        }
    }
    let elapsed = started.elapsed();

    symbols.sync_from_task(
        task.port,
        &mut unwinder,
        &mut go_images,
        &mut python,
        &mut java,
    );
    symbols.finish_fallback_if_empty();

    let comm = inspect(target.pid).and_then(|snapshot| snapshot.comm);
    Ok(finish_profile(
        target.pid,
        comm,
        collector,
        &mut symbols,
        elapsed,
        interval,
    ))
}

fn stop_requested(opts: &RecordOptions, session_stop: &anyprof_core::StopGate) -> bool {
    opts.stop.stop_requested() || session_stop.stop_requested()
}

fn wait_until(deadline: Instant, opts: &RecordOptions, session_stop: &anyprof_core::StopGate) {
    while Instant::now() < deadline {
        if stop_requested(opts, session_stop) {
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
    collector: SampleCollector,
    symbols: &mut SymbolTable,
    elapsed: Duration,
    interval: Duration,
) -> Profile {
    let (stacks, samples) = collector.finish(|stack| match stack {
        CountedStack::Addresses { frames, .. } => frames
            .into_iter()
            .enumerate()
            .map(|(index, addr)| frame_for(addr, symbols, index == 0))
            .collect(),
        CountedStack::Python { native, frames, .. } => {
            let mut out = Vec::with_capacity(native.len() + frames.len());
            for (index, addr) in native.into_iter().enumerate() {
                out.push(frame_for(addr, symbols, index == 0));
            }
            out.extend(python::python_profile_frames(&frames));
            out
        }
        CountedStack::Java { native, frames, .. } => {
            let mut out = Vec::with_capacity(native.len() + frames.len());
            for (index, addr) in native.into_iter().enumerate() {
                out.push(frame_for(addr, symbols, index == 0));
            }
            out.extend(java::java_profile_frames(&frames));
            out
        }
    });
    Profile {
        pid,
        comm,
        duration: elapsed,
        period: Some(interval),
        stacks,
        samples,
        raw_cpuprofile: None,
    }
}

fn frame_for(addr: u64, table: &mut SymbolTable, leaf: bool) -> Frame {
    let lookup = policy::symbol_lookup_address(addr, leaf);
    if let Some(frame) = go_frame(&table.images, lookup, addr) {
        return frame;
    }
    if lookup != addr
        && let Some(frame) = go_frame(&table.images, addr, addr)
    {
        return frame;
    }
    let idx = table
        .images
        .iter()
        .position(|entry| entry.symbols.contains(lookup))
        .or_else(|| {
            table
                .images
                .iter()
                .position(|entry| entry.symbols.contains(addr))
        });
    if let Some(idx) = idx {
        table.ensure_symbols_loaded(idx);
    }
    let image = idx.map(|i| &table.images[i].symbols);
    let (name, runtime) = image
        .and_then(|image| {
            image
                .name_at(lookup)
                .or_else(|| image.name_at(addr))
                .map(|(name, symbol_runtime)| {
                    let runtime =
                        crate::symbol::frame_runtime(symbol_runtime, name, false, image.rust_image);
                    (name.to_string(), runtime)
                })
        })
        .unwrap_or_else(|| (String::new(), Runtime::Native));
    let mut frame = Frame::new(runtime, name);
    frame.address = Some(addr);
    frame.module = image.map(|image| image.module.clone());
    if let Some((file, line)) = image.and_then(|image| {
        image
            .location_at(lookup)
            .or_else(|| image.location_at(addr))
    }) {
        frame.file = Some(file.to_string());
        frame.line = Some(line);
    }
    frame
}

fn go_frame(images: &[TrackedImage], lookup: u64, addr: u64) -> Option<Frame> {
    for entry in images {
        let Some(go) = &entry.symbols.go else {
            continue;
        };
        let Some(symbol) = go.symbol(lookup) else {
            continue;
        };
        let mut frame = Frame::new(Runtime::Go, symbol.name);
        frame.file = symbol.file;
        frame.line = symbol.line;
        frame.address = Some(addr);
        frame.module = Some(entry.symbols.module.clone());
        return Some(frame);
    }
    None
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

    fn sample_addresses(
        &self,
        unwinder: &MachUnwinder,
        unwind_cache: &mut MachUnwindCache,
        go_images: &[crate::go_sym::GoImage],
        python: Option<&PythonVm>,
        java: Option<&JavaVm>,
    ) -> Result<Vec<ThreadHit>, SampleFault> {
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
            match unwind_thread(
                self.port,
                *thread,
                unwinder,
                unwind_cache,
                go_images,
                python,
                java,
            ) {
                Ok(Some(hit)) => stacks.push(hit),
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

struct ThreadHit {
    tid: u64,
    name: Option<String>,
    stack: CountedStack,
}

enum PendingStack {
    Native(usize),
    Language(CountedStack),
}

fn unwind_thread(
    task: task_t,
    thread: thread_act_t,
    unwinder: &MachUnwinder,
    unwind_cache: &mut MachUnwindCache,
    go_images: &[crate::go_sym::GoImage],
    python: Option<&PythonVm>,
    java: Option<&JavaVm>,
) -> Result<Option<ThreadHit>, SampleFault> {
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
    let mut raw_addrs = [0u64; MAX_FRAMES];
    let mut tid = 0u64;
    let pending = if kr == KERN_SUCCESS {
        let (pc, sp, fp, lr) = registers(&state);
        tid = thread_identifier(thread).unwrap_or(0);
        // `frames` is `None` for a leaf outside the code cache and for a
        // blob whose method cannot be read. Both keep the native walk.
        if let Some(vm) = java
            && let Some(java_frames) =
                vm.frames(pc, sp, fp, &mut |addr, buf| read_task(task, addr, buf))
        {
            Some(PendingStack::Language(CountedStack::Java {
                kernel: Vec::new(),
                native: Vec::new(),
                frames: java_frames,
            }))
        } else if let Some(vm) = python
            && let Some(py_frames) = vm.frames(tid, pc, &mut |addr, buf| read_task(task, addr, buf))
        {
            Some(PendingStack::Language(CountedStack::Python {
                kernel: Vec::new(),
                native: Vec::new(),
                frames: py_frames,
            }))
        } else if !go_images.is_empty()
            && let Some(go_frames) = walk_go_frames(task, pc, sp, lr, go_images)
        {
            (!go_frames.is_empty()).then_some(PendingStack::Language(CountedStack::Addresses {
                frames: go_frames,
                kernel_len: 0,
            }))
        } else {
            let n =
                walk_native_frames(task, unwinder, unwind_cache, pc, sp, fp, lr, &mut raw_addrs);
            let mut read = |addr: u64, buf: &mut [u8]| read_task(task, addr, buf);
            if let Some(vm) = java
                && let Some(java_frames) = vm.anchor_frames(tid, sp, &mut read)
            {
                let native = vm.slice_external_native(&raw_addrs[..n], &mut read);
                Some(PendingStack::Language(CountedStack::Java {
                    kernel: Vec::new(),
                    native,
                    frames: java_frames,
                }))
            } else if let Some(vm) = python
                && !vm.in_image(pc)
                && let Some(py_frames) = vm.thread_frames_any_pc(tid, &mut read)
            {
                let native = vm.slice_external_native(&raw_addrs[..n]);
                Some(PendingStack::Language(CountedStack::Python {
                    kernel: Vec::new(),
                    native,
                    frames: py_frames,
                }))
            } else {
                (n > 0).then_some(PendingStack::Native(n))
            }
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
    let Some(pending) = pending else {
        return Ok(None);
    };
    let name = thread_name(thread);
    let stack = match pending {
        PendingStack::Native(len) => CountedStack::Addresses {
            frames: raw_addrs[..len].to_vec(),
            kernel_len: 0,
        },
        PendingStack::Language(stack) => stack,
    };
    Ok(Some(ThreadHit { tid, name, stack }))
}

/// `thread_identifier_info.thread_id`, which CPython stores from `pthread_threadid_np`.
fn thread_identifier(thread: thread_act_t) -> Option<u64> {
    const THREAD_IDENTIFIER_INFO: u32 = 4;
    let mut info = [0i32; 6];
    let mut count: mach_msg_type_number_t = 6;
    let kr = unsafe {
        thread_info(
            thread,
            THREAD_IDENTIFIER_INFO,
            info.as_mut_ptr(),
            &mut count,
        )
    };
    if kr != KERN_SUCCESS || count < 2 {
        return None;
    }
    Some(u64::from(info[0] as u32) | (u64::from(info[1] as u32) << 32))
}

/// `thread_extended_info.pth_name` (64-byte NUL-terminated UTF-8 name at byte offset 48).
fn thread_name(thread: thread_act_t) -> Option<String> {
    const THREAD_EXTENDED_INFO: u32 = 5;
    let mut info = [0i32; 28];
    let mut count: mach_msg_type_number_t = 28;
    let kr = unsafe { thread_info(thread, THREAD_EXTENDED_INFO, info.as_mut_ptr(), &mut count) };
    if kr != KERN_SUCCESS || count < 28 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(info.as_ptr().cast::<u8>().add(48), 64) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let name = std::str::from_utf8(&bytes[..end]).ok()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(target_arch = "aarch64")]
fn registers(state: &ThreadState) -> (u64, u64, u64, u64) {
    let flags = state.__flags;
    (
        apply_strip(state.__pc, flags, StripKind::Code, false),
        apply_strip(state.__sp, flags, StripKind::Data, false),
        apply_strip(state.__fp, flags, StripKind::Data, false),
        apply_strip(state.__lr, flags, StripKind::Code, true),
    )
}

#[cfg(target_arch = "x86_64")]
fn registers(state: &ThreadState) -> (u64, u64, u64, u64) {
    // x86_64 pointers are not authenticated.
    (state.__rip, state.__rsp, state.__rbp, 0)
}

fn walk_go_frames(
    task: task_t,
    pc: u64,
    sp: u64,
    lr: u64,
    go_images: &[crate::go_sym::GoImage],
) -> Option<Vec<u64>> {
    let machine = if cfg!(target_arch = "aarch64") {
        crate::go_sym::GoMachine::Arm64
    } else {
        crate::go_sym::GoMachine::X86_64
    };
    let mut reader = CachedTaskStackReader::new(task);
    crate::go_sym::unwind(
        go_images,
        pc,
        sp,
        lr,
        machine,
        |addr, buf| reader.read_slice(addr, buf),
        |addr| apply_strip(addr, policy::stack_strip_flags(), StripKind::Code, false),
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_native_frames(
    task: task_t,
    unwinder: &MachUnwinder,
    unwind_cache: &mut MachUnwindCache,
    pc: u64,
    sp: u64,
    fp: u64,
    lr: u64,
    out: &mut [u64; MAX_FRAMES],
) -> usize {
    if !plausible_code(pc) {
        return 0;
    }
    let mut reader = CachedTaskStackReader::new(task);
    let stack_flags = policy::stack_strip_flags();
    #[cfg(target_arch = "x86_64")]
    let regs = framehop::x86_64::UnwindRegsX86_64::new(pc, sp, fp);
    #[cfg(target_arch = "aarch64")]
    let regs = framehop::aarch64::UnwindRegsAarch64::new_with_ptr_auth_mask(
        crate::walk::aarch64_ptr_auth_mask(
            unwinder.max_known_code_address(),
            0x0000_8000_0000_0000,
        ),
        lr,
        sp,
        fp,
    );
    let mut len = 0usize;
    let mut ended_at_root = false;
    {
        let mut read_word = |addr: u64| reader.read_u64(addr);
        let mut iter = unwinder.iter_frames(pc, regs, unwind_cache, &mut read_word);
        while len < MAX_FRAMES {
            match iter.next() {
                Ok(Some(frame)) => {
                    let addr = apply_strip(frame.address(), stack_flags, StripKind::Code, false);
                    if !plausible_code(addr) {
                        break;
                    }
                    out[len] = addr;
                    len += 1;
                }
                Ok(None) | Err(framehop::Error::ReturnAddressIsNull) => {
                    ended_at_root = true;
                    break;
                }
                Err(_) => break,
            }
        }
    }
    if len <= 1 && !ended_at_root {
        return walk_fp_into(&mut reader, pc, fp, lr, out);
    }
    len
}

fn walk_fp_into(
    reader: &mut CachedTaskStackReader,
    pc: u64,
    mut fp: u64,
    lr: u64,
    out: &mut [u64; MAX_FRAMES],
) -> usize {
    let mut len = 0usize;
    if plausible_code(pc) {
        out[0] = pc;
        len = 1;
    }
    let stack_flags = policy::stack_strip_flags();
    let mut guard = 0usize;
    while len < MAX_FRAMES && plausible_frame(fp) {
        let mut record = [0u8; 16];
        if !reader.read_slice(fp, &mut record) {
            break;
        }
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
        out[len] = ret;
        len += 1;
        if next_fp <= fp {
            break;
        }
        fp = next_fp;
        guard += 1;
        if guard >= MAX_FRAMES {
            break;
        }
    }
    if len == 1 && plausible_code(lr) && lr != out[0] {
        out[1] = lr;
        len = 2;
    }
    len
}

struct CachedTaskStackReader {
    task: task_t,
    page_base: u64,
    valid: bool,
    page: [u8; 4096],
}

impl CachedTaskStackReader {
    fn new(task: task_t) -> Self {
        Self {
            task,
            page_base: u64::MAX,
            valid: false,
            page: [0u8; 4096],
        }
    }

    fn read_u64(&mut self, addr: u64) -> Result<u64, ()> {
        let mut buf = [0u8; 8];
        if self.read_slice(addr, &mut buf) {
            Ok(u64::from_le_bytes(buf))
        } else {
            Err(())
        }
    }

    fn read_slice(&mut self, addr: u64, buf: &mut [u8]) -> bool {
        if addr == 0 || buf.is_empty() {
            return false;
        }
        let off = (addr & 0xfff) as usize;
        if buf.len() <= 4096 - off {
            let aligned = addr & !0xfff;
            if !self.valid || self.page_base != aligned {
                if read_task(self.task, aligned, &mut self.page) {
                    self.page_base = aligned;
                    self.valid = true;
                } else {
                    self.valid = false;
                    return read_task(self.task, addr, buf);
                }
            }
            buf.copy_from_slice(&self.page[off..off + buf.len()]);
            return true;
        }
        read_task(self.task, addr, buf)
    }
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

/// Attach, list images, and run `body` before the port is dropped.
pub(crate) fn with_task<T>(
    pid: u32,
    body: impl FnOnce(task_t, &[(String, u64, u64)]) -> Result<T, String>,
) -> Result<T, Error> {
    let task = Task::attach(pid)?;
    let images = remote_images(task.port);
    let listed: Vec<(String, u64, u64)> = images
        .iter()
        .map(|image| (image.path.clone(), image.load, image.end))
        .collect();
    body(task.port, &listed).map_err(|message| Error::backend("native", message))
}

pub(crate) fn read_memory(task: task_t, addr: u64, buf: &mut [u8]) -> bool {
    read_task(task, addr, buf)
}

enum ImageSource {
    File(String),
    PendingCache(String),
    SharedCache(String),
    None,
}

struct TrackedImage {
    symbols: ImageSymbols,
    source: ImageSource,
    symbols_loaded: bool,
}

struct SymbolTable {
    pid: u32,
    cpu: MachCpu,
    language_unwind: anyprof_core::LanguageUnwind,
    shared_cache: Option<Option<&'static crate::dyld_cache::DyldSharedCache>>,
    info_addr: Option<u64>,
    seen_dyld_count: Option<u32>,
    seen_loads: std::collections::HashSet<u64>,
    raw_images: Vec<(String, u64, u64)>,
    images: Vec<TrackedImage>,
}

impl SymbolTable {
    fn new(pid: u32, language_unwind: anyprof_core::LanguageUnwind) -> Self {
        Self {
            pid,
            cpu: MachCpu::host(),
            language_unwind,
            shared_cache: None,
            info_addr: None,
            seen_dyld_count: None,
            seen_loads: std::collections::HashSet::new(),
            raw_images: Vec::new(),
            images: Vec::new(),
        }
    }

    fn sync_from_task(
        &mut self,
        task: task_t,
        unwinder: &mut MachUnwinder,
        go_images: &mut Vec<crate::go_sym::GoImage>,
        python: &mut Option<PythonVm>,
        java: &mut Option<JavaVm>,
    ) {
        let info_addr = match self.info_addr {
            Some(addr) => addr,
            None => {
                let Some(addr) = dyld_info_addr(task) else {
                    return;
                };
                self.info_addr = Some(addr);
                addr
            }
        };
        let mut count_buf = [0u8; 4];
        if !read_task(task, info_addr.saturating_add(4), &mut count_buf) {
            return;
        }
        let current_count = u32::from_le_bytes(count_buf);
        if self.seen_dyld_count == Some(current_count) {
            return;
        }
        let listed = remote_images(task);
        if listed.is_empty() && current_count == 0 {
            return;
        }
        self.seen_dyld_count = Some(current_count);

        let mut added_any = false;
        for remote in listed {
            if !self.seen_loads.insert(remote.load) {
                continue;
            }
            added_any = true;
            self.raw_images
                .push((remote.path.clone(), remote.load, remote.end));
            let tracked = self.load_tracked_unwind(remote);
            if let Some(tracked) = tracked {
                tracked.symbols.register_unwind_module(unwinder);
                if let Some(go) = &tracked.symbols.go {
                    go_images.push(go.clone());
                }
                self.images.push(tracked);
            }
        }

        if added_any {
            if self.language_unwind.allows_python() && python.is_none() {
                *python = python::attach_mach(&self.raw_images);
            }
            if self.language_unwind.allows_java() && java.is_none() {
                *java = java::attach_mach(&self.raw_images, &mut |addr, buf| {
                    read_task(task, addr, buf)
                });
            }
        }
    }

    fn activate_shared_cache_for_hits(&mut self, unwinder: &mut MachUnwinder, hits: &[ThreadHit]) {
        let mut hit_indices = Vec::new();
        for hit in hits {
            let frames = match &hit.stack {
                CountedStack::Addresses { frames, .. } => frames.as_slice(),
                CountedStack::Python { native, .. } | CountedStack::Java { native, .. } => {
                    native.as_slice()
                }
            };
            for (frame_idx, &addr) in frames.iter().enumerate() {
                let probe = policy::symbol_lookup_address(addr, frame_idx == 0);
                if let Some(img_idx) = self.images.iter().position(|entry| {
                    matches!(entry.source, ImageSource::PendingCache(_))
                        && (entry.symbols.contains(probe) || entry.symbols.contains(addr))
                }) && !hit_indices.contains(&img_idx)
                {
                    hit_indices.push(img_idx);
                }
            }
        }
        if hit_indices.is_empty() {
            return;
        }
        let cpu = self.cpu;
        let Some(cache) = *self
            .shared_cache
            .get_or_insert_with(|| crate::dyld_cache::DyldSharedCache::host(cpu))
        else {
            for idx in hit_indices {
                self.images[idx].source = ImageSource::None;
                self.images[idx].symbols_loaded = true;
            }
            return;
        };
        for idx in hit_indices {
            let entry = &mut self.images[idx];
            let ImageSource::PendingCache(path) = &entry.source else {
                continue;
            };
            let path = path.clone();
            if let Some(parsed) =
                cache.load_unwind(&path, entry.symbols.start, &entry.symbols.module)
            {
                parsed.register_unwind_module(unwinder);
                entry.symbols = parsed;
                entry.source = ImageSource::SharedCache(path);
                entry.symbols_loaded = false;
            } else {
                entry.source = ImageSource::None;
                entry.symbols_loaded = true;
            }
        }
    }

    fn load_tracked_unwind(&self, remote: RemoteImage) -> Option<TrackedImage> {
        let module = module_name(&remote.path);
        let path = Path::new(&remote.path);
        if path.is_file() {
            let file_unwind = ImageSymbols::parse_unwind_file(path, remote.load, &module, self.cpu);
            let rust_image = crate::rust_image::file_is_rust(path);
            let go = self
                .language_unwind
                .allows_go()
                .then(|| attach_go(&remote.path, remote.load))
                .flatten();
            if let Some(mut parsed) = file_unwind {
                parsed.rust_image = rust_image;
                parsed.go = go;
                return Some(TrackedImage {
                    symbols: parsed,
                    source: ImageSource::File(remote.path),
                    symbols_loaded: false,
                });
            }
            if remote.end > remote.load {
                let mut image = ImageSymbols::range_only(remote.load, remote.end, module);
                image.rust_image = rust_image;
                image.go = go;
                return Some(TrackedImage {
                    symbols: image,
                    source: ImageSource::PendingCache(remote.path),
                    symbols_loaded: false,
                });
            }
            return None;
        }

        if remote.end > remote.load {
            return Some(TrackedImage {
                symbols: ImageSymbols::range_only(remote.load, remote.end, module),
                source: ImageSource::PendingCache(remote.path),
                symbols_loaded: false,
            });
        }
        None
    }

    fn finish_fallback_if_empty(&mut self) {
        if self.images.is_empty()
            && let Some(path) = pid_path(self.pid)
            && let Some(mut parsed) =
                ImageSymbols::parse_file(Path::new(&path), 0, &module_name(&path), self.cpu)
        {
            // No dyld list. A slide of 0 is wrong for a PIE binary, so this only
            // helps when the image was linked at a fixed address.
            tracing::debug!(
                pid = self.pid,
                path,
                "native symbol load fell back to proc_pidpath"
            );
            parsed.rust_image = crate::rust_image::file_is_rust(Path::new(&path));
            self.images.push(TrackedImage {
                symbols: parsed,
                source: ImageSource::None,
                symbols_loaded: true,
            });
        }
    }

    fn ensure_symbols_loaded(&mut self, idx: usize) {
        let cpu = self.cpu;
        let shared_cache = &mut self.shared_cache;
        let Some(entry) = self.images.get_mut(idx) else {
            return;
        };
        if entry.symbols_loaded {
            return;
        }
        entry.symbols_loaded = true;
        match &entry.source {
            ImageSource::File(path) => {
                entry.symbols.populate_file_symbols(Path::new(path), cpu);
            }
            ImageSource::PendingCache(path) => {
                let path = path.clone();
                if let Some(cache) = *shared_cache
                    .get_or_insert_with(|| crate::dyld_cache::DyldSharedCache::host(cpu))
                {
                    if let Some(mut parsed) =
                        cache.load_unwind(&path, entry.symbols.start, &entry.symbols.module)
                    {
                        cache.populate_symbols(&path, &mut parsed);
                        entry.symbols = parsed;
                        entry.source = ImageSource::SharedCache(path);
                    } else {
                        cache.populate_symbols(&path, &mut entry.symbols);
                    }
                }
            }
            ImageSource::SharedCache(path) => {
                if let Some(cache) = *shared_cache
                    .get_or_insert_with(|| crate::dyld_cache::DyldSharedCache::host(cpu))
                {
                    cache.populate_symbols(path, &mut entry.symbols);
                }
            }
            ImageSource::None => {}
        }
    }
}

fn attach_go(path: &str, load_addr: u64) -> Option<crate::go_sym::GoImage> {
    let bias = crate::go_sym::macho_bias(Path::new(path), load_addr)?;
    crate::go_sym::open_image_at(Path::new(path), bias)
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
    let mut header = [0u8; 40];
    let has_dyld_slot = if read_task(task, info_addr, &mut header) {
        true
    } else if read_task(task, info_addr, &mut header[..16]) {
        false
    } else {
        return Vec::new();
    };
    let version = u32::from_le_bytes(header[0..4].try_into().unwrap());
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
    let mut images = Vec::with_capacity(count + 1);
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
    // `dyldImageLoadAddress` sits at offset 32 of `dyld_all_image_infos` (version >= 2)
    // and is not listed in `infoArray`, so include `/usr/lib/dyld` (`start` above `main`).
    if has_dyld_slot && version >= 2 {
        let dyld_load = u64::from_le_bytes(header[32..40].try_into().unwrap());
        if dyld_load != 0
            && !images.iter().any(|img| img.load == dyld_load)
            && let Some(end) = image_end(task, dyld_load)
        {
            images.push(RemoteImage {
                load: dyld_load,
                end,
                path: "/usr/lib/dyld".to_string(),
            });
        }
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
    anyprof_core::executable_file(pid).and_then(|path| path.into_os_string().into_string().ok())
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
