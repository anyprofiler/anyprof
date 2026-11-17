//! Windows x64 on-CPU sampler.
//!
//! A tick asks `NtQuerySystemInformation` which threads are `Running` and
//! suspends only those. `Ready` threads are queued, not on a CPU, and are
//! not sampled. `GetThreadContext` and `StackWalk64` read the suspended
//! thread. `ResumeThread` runs from `Drop`, including on panic, and is
//! retried. A resume that still fails aborts the profile instead of
//! returning stacks for a thread that might still be stopped.
//!
//! The sampler never calls `SetThreadContext` or `WriteProcessMemory`.
//! Sampling this process is refused before `OpenProcess`: suspending the
//! thread that would resume the others can leave the process stuck.
//!
//! `-k` is ignored. This interface does not record kernel frames.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, Instant};

use anyprof_core::{
    Error, Frame, PERF_SAMPLING_INTERVAL, Profile, RecordOptions, Runtime, Stack, Target, inspect,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, FALSE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    ADDRESS64, AddrModeFlat, CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext, IMAGEHLP_MODULE64,
    STACKFRAME64, SYMBOL_INFO_PACKAGE, SYMOPT_DEFERRED_LOADS, SYMOPT_DISABLE_SYMSRV_AUTODETECT,
    SYMOPT_FAIL_CRITICAL_ERRORS, SYMOPT_IGNORE_NT_SYMPATH, SYMOPT_NO_PROMPTS, SYMOPT_UNDNAME,
    StackWalk64, SymCleanup, SymFromAddr, SymFunctionTableAccess64, SymGetModuleBase64,
    SymGetModuleInfo64, SymInitializeW, SymSetOptions,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessInformation, IsWow64Process, OpenProcess, OpenProcessToken,
    OpenThread, PROCESS_PROTECTION_LEVEL_INFORMATION, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ, ProcessProtectionLevelInfo, ResumeThread,
    SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

use crate::elf::present_symbol;
use crate::policy::{self, DebugPrivilege};
use crate::snapshot::{self, SnapshotError};

const MAX_FRAMES: usize = 64;
/// `IMAGE_FILE_MACHINE_AMD64`. `StackWalk64` takes it as a `u32`.
const MACHINE_AMD64: u32 = 0x8664;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
const MAX_SNAPSHOT: usize = 64 * 1024 * 1024;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut u8,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

pub(crate) fn record(target: &Target, opts: &RecordOptions) -> Result<Profile, Error> {
    if opts.include_kernel {
        tracing::warn!(
            "ignoring -k/--kernel: the Windows native sampler collects user frames only"
        );
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

    let process = Process::open(target.pid)?;
    ensure_x64(process.handle, target.pid)?;
    let symbols = Symbols::load(process.handle)?;

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
        match sample_addresses(process.handle, target.pid) {
            Ok(stacks) => {
                for stack in stacks {
                    if !stack.is_empty() {
                        *counts.entry(stack).or_default() += 1;
                    }
                }
            }
            Err(SampleFault::Fatal(err)) => {
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
    }

    let comm = inspect(target.pid).and_then(|snapshot| snapshot.comm);
    let profile = finish_profile(
        target.pid,
        comm,
        counts,
        process.handle,
        started.elapsed(),
        interval,
    );
    drop(symbols);
    Ok(profile)
}

enum SampleFault {
    Fatal(String),
    Tick(String),
}

struct Process {
    handle: HANDLE,
}

impl Process {
    fn open(pid: u32) -> Result<Self, Error> {
        let access = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;
        let handle = unsafe { OpenProcess(access, FALSE, pid) };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            return Ok(Self { handle });
        }
        let os_error = unsafe { GetLastError() };
        if os_error != ERROR_ACCESS_DENIED {
            return Err(Error::backend(
                "native",
                policy::windows_open_denied(pid, os_error, None, DebugPrivilege::NotHeld),
            ));
        }
        let privilege = try_enable_debug_privilege();
        let handle = unsafe { OpenProcess(access, FALSE, pid) };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            return Ok(Self { handle });
        }
        let os_error = unsafe { GetLastError() };
        let protection = limited_protection(pid);
        Err(Error::backend(
            "native",
            policy::windows_open_denied(pid, os_error, protection, privilege),
        ))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

struct Symbols {
    process: HANDLE,
}

impl Symbols {
    fn load(process: HANDLE) -> Result<Self, Error> {
        // Local debug info next to the image is loaded. These options stop
        // dbghelp from prompting, walking `_NT_SYMBOL_PATH`, or contacting a
        // symbol server. `SYMOPT_SECURE` is not set: it also disables
        // unqualified local PDB loads, which is how an unstripped binary
        // built in this directory gets function names.
        let options = SYMOPT_UNDNAME
            | SYMOPT_DEFERRED_LOADS
            | SYMOPT_FAIL_CRITICAL_ERRORS
            | SYMOPT_NO_PROMPTS
            | SYMOPT_IGNORE_NT_SYMPATH
            | SYMOPT_DISABLE_SYMSRV_AUTODETECT;
        unsafe { SymSetOptions(options) };
        // SAFETY: `SymInitializeW` attaches dbghelp to this process handle.
        // `Symbols` calls `SymCleanup`. A null path does not add a search
        // directory; modules already mapped in the process are enumerated.
        let ok = unsafe { SymInitializeW(process, std::ptr::null(), 1) };
        if ok == FALSE {
            let err = unsafe { GetLastError() };
            return Err(Error::backend(
                "native",
                format!("SymInitialize failed (error {err})"),
            ));
        }
        Ok(Self { process })
    }
}

impl Drop for Symbols {
    fn drop(&mut self) {
        unsafe { SymCleanup(self.process) };
    }
}

fn sample_addresses(process: HANDLE, pid: u32) -> Result<Vec<Vec<u64>>, SampleFault> {
    let running = running_threads(pid)?;
    let mut stacks = Vec::with_capacity(running.len());
    for tid in running {
        match unwind_thread(process, tid) {
            Ok(Some(frames)) => stacks.push(frames),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(stacks)
}

fn running_threads(pid: u32) -> Result<Vec<u32>, SampleFault> {
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let mut needed = 0u32;
        // SAFETY: `buf` is writable for `buf.len()` bytes. The class is
        // `SystemProcessInformation`. The kernel only reads the buffer.
        let status = unsafe {
            NtQuerySystemInformation(
                snapshot::SYSTEM_PROCESS_INFORMATION,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut needed,
            )
        };
        if status == 0 {
            return match snapshot::running_tids(&buf, pid) {
                Ok(tids) => Ok(tids),
                Err(err) => Err(snapshot_fault(err)),
            };
        }
        if status != STATUS_INFO_LENGTH_MISMATCH {
            return Err(SampleFault::Tick(format!(
                "NtQuerySystemInformation failed (status {status:#x})"
            )));
        }
        let next = (needed as usize)
            .saturating_add(64 * 1024)
            .max(buf.len().saturating_mul(2));
        if next > MAX_SNAPSHOT || next <= buf.len() {
            return Err(SampleFault::Tick(
                "the thread snapshot did not fit in 64MB".to_string(),
            ));
        }
        buf.resize(next, 0);
    }
}

fn snapshot_fault(err: SnapshotError) -> SampleFault {
    match err {
        SnapshotError::Truncated => {
            SampleFault::Tick("the thread snapshot was truncated".to_string())
        }
        SnapshotError::TooMany { count } => SampleFault::Fatal(snapshot::too_many_threads(count)),
    }
}

fn unwind_thread(process: HANDLE, tid: u32) -> Result<Option<Vec<u64>>, SampleFault> {
    let access = THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION;
    let thread = unsafe { OpenThread(access, FALSE, tid) };
    if thread.is_null() || thread == INVALID_HANDLE_VALUE {
        return Ok(None);
    }
    let thread = Thread(thread);
    let mut resume_failure = None;
    let Some(suspended) = Suspended::new(thread.0, &mut resume_failure) else {
        return Ok(None);
    };
    let frames = walk_suspended(process, thread.0);
    drop(suspended);
    if let Some(code) = resume_failure {
        return Err(SampleFault::Fatal(format!(
            "ResumeThread failed (error {code}); a target thread may still be suspended"
        )));
    }
    Ok(frames)
}

struct Thread(HANDLE);

impl Drop for Thread {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

struct Suspended<'a> {
    thread: HANDLE,
    resumed: bool,
    failure: &'a mut Option<u32>,
}

impl<'a> Suspended<'a> {
    fn new(thread: HANDLE, failure: &'a mut Option<u32>) -> Option<Self> {
        // SAFETY: `SuspendThread` increments the suspend count. `Drop`
        // resumes exactly once, including when this function unwinds.
        let previous = unsafe { SuspendThread(thread) };
        if previous == u32::MAX {
            None
        } else {
            Some(Self {
                thread,
                resumed: false,
                failure,
            })
        }
    }
}

impl Drop for Suspended<'_> {
    fn drop(&mut self) {
        if self.resumed {
            return;
        }
        let thread = self.thread;
        match policy::retry_until_success(3, 0, || resume_code(thread)) {
            Ok(_) => self.resumed = true,
            Err(code) => {
                *self.failure = Some(code as u32);
                tracing::error!(
                    error = code,
                    "ResumeThread failed; the target thread may still be suspended"
                );
                eprintln!(
                    "anyprof: ResumeThread failed (error {code}); a target thread may still be suspended"
                );
            }
        }
    }
}

fn resume_code(thread: HANDLE) -> i32 {
    // SAFETY: one `ResumeThread` balances one `SuspendThread`. A second call
    // is not made after this returns 0.
    let previous = unsafe { ResumeThread(thread) };
    if previous == u32::MAX {
        let err = unsafe { GetLastError() };
        if err == 0 { -1 } else { err as i32 }
    } else {
        // The return is the previous suspend count, not a status. Any value
        // other than `u32::MAX` resumed the thread. Do not call this again.
        0
    }
}

/// `GetThreadContext` requires the x64 `CONTEXT` to be 16-byte aligned.
#[repr(C, align(16))]
struct AlignedContext {
    context: CONTEXT,
}

fn walk_suspended(process: HANDLE, thread: HANDLE) -> Option<Vec<u64>> {
    let mut aligned = AlignedContext {
        context: unsafe { std::mem::zeroed() },
    };
    aligned.context.ContextFlags = CONTEXT_FULL_AMD64;
    // SAFETY: `context` is an aligned x64 `CONTEXT`. The thread is suspended.
    // `GetThreadContext` reads registers. It does not write the thread.
    let ok = unsafe { GetThreadContext(thread, &mut aligned.context) };
    if ok == FALSE {
        return None;
    }
    let mut frame: STACKFRAME64 = unsafe { std::mem::zeroed() };
    frame.AddrPC = flat(aligned.context.Rip);
    // x64 unwind uses `Rsp` as both the frame and stack addresses. Microsoft's
    // `StackWalk64` sample does this; `Rbp` is not the frame pointer there.
    frame.AddrFrame = flat(aligned.context.Rsp);
    frame.AddrStack = flat(aligned.context.Rsp);
    let mut frames = Vec::new();
    let mut previous = 0u64;
    while frames.len() < MAX_FRAMES {
        // SAFETY: `StackWalk64` reads the suspended thread through the
        // process handle. The read callback is null, so dbghelp uses
        // `ReadProcessMemory`. Nothing is written back.
        let ok = unsafe {
            StackWalk64(
                MACHINE_AMD64,
                process,
                thread,
                &mut frame,
                &mut aligned.context as *mut CONTEXT as *mut c_void,
                None,
                Some(SymFunctionTableAccess64),
                Some(SymGetModuleBase64),
                None,
            )
        };
        if ok == FALSE {
            break;
        }
        let pc = frame.AddrPC.Offset;
        if pc == 0 || pc == previous {
            break;
        }
        frames.push(pc);
        previous = pc;
    }
    if frames.is_empty() {
        None
    } else {
        Some(frames)
    }
}

fn flat(offset: u64) -> ADDRESS64 {
    ADDRESS64 {
        Offset: offset,
        Segment: 0,
        Mode: AddrModeFlat,
    }
}

fn finish_profile(
    pid: u32,
    comm: Option<String>,
    counts: HashMap<Vec<u64>, u64>,
    process: HANDLE,
    elapsed: Duration,
    interval: Duration,
) -> Profile {
    let mut stacks = Vec::with_capacity(counts.len());
    for (addresses, weight) in counts {
        let frames = addresses
            .iter()
            .enumerate()
            .map(|(index, addr)| frame_for(process, *addr, index == 0))
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

fn frame_for(process: HANDLE, addr: u64, leaf: bool) -> Frame {
    let lookup = policy::symbol_lookup_address(addr, leaf);
    let (name, module) = symbol_at(process, lookup)
        .or_else(|| symbol_at(process, addr))
        .unwrap_or_else(|| {
            (
                String::new(),
                module_only(process, lookup)
                    .or_else(|| module_only(process, addr))
                    .unwrap_or_default(),
            )
        });
    let module = if module.is_empty() {
        None
    } else {
        Some(module)
    };
    let mut frame = Frame::new(Runtime::Native, name);
    frame.address = Some(addr);
    frame.module = module;
    frame
}

fn symbol_at(process: HANDLE, addr: u64) -> Option<(String, String)> {
    let name = symbol_name(process, addr)?;
    let module = module_only(process, addr).unwrap_or_default();
    Some((name, module))
}

fn symbol_name(process: HANDLE, addr: u64) -> Option<String> {
    let mut package: SYMBOL_INFO_PACKAGE = unsafe { std::mem::zeroed() };
    package.si.SizeOfStruct = std::mem::size_of_val(&package.si) as u32;
    // `Name` is one byte inside `SYMBOL_INFO`, and `name` continues it.
    package.si.MaxNameLen = 1 + package.name.len() as u32;
    let mut displacement = 0u64;
    let ok = unsafe { SymFromAddr(process, addr, &mut displacement, &mut package.si) };
    if ok == FALSE {
        return None;
    }
    let len = package.si.NameLen as usize;
    let bytes =
        unsafe { std::slice::from_raw_parts(package.si.Name.as_ptr().cast::<u8>(), len.min(1024)) };
    let text = String::from_utf8_lossy(bytes)
        .trim_end_matches('\0')
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(present_symbol(&text))
    }
}

fn module_only(process: HANDLE, addr: u64) -> Option<String> {
    let mut info: IMAGEHLP_MODULE64 = unsafe { std::mem::zeroed() };
    info.SizeOfStruct = std::mem::size_of::<IMAGEHLP_MODULE64>() as u32;
    let ok = unsafe { SymGetModuleInfo64(process, addr, &mut info) };
    if ok == FALSE {
        return None;
    }
    let path = c_string(&info.LoadedImageName)
        .or_else(|| c_string(&info.ImageName))
        .or_else(|| c_string(&info.ModuleName))?;
    let name = Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path.as_str());
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn c_string(bytes: &[i8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let text: Vec<u8> = bytes[..end].iter().map(|byte| *byte as u8).collect();
    let text = String::from_utf8_lossy(&text).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn ensure_x64(process: HANDLE, pid: u32) -> Result<(), Error> {
    let mut wow = FALSE;
    let ok = unsafe { IsWow64Process(process, &mut wow) };
    if ok == FALSE {
        let err = unsafe { GetLastError() };
        return Err(Error::backend(
            "native",
            format!("IsWow64Process failed for pid {pid} (error {err})"),
        ));
    }
    if wow != FALSE {
        return Err(Error::backend(
            "native",
            format!(
                "cannot sample pid {pid}: native sampling on Windows walks x64 stacks; this process is 32-bit"
            ),
        ));
    }
    Ok(())
}

fn limited_protection(pid: u32) -> Option<u32> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = PROCESS_PROTECTION_LEVEL_INFORMATION { ProtectionLevel: 0 };
    let ok = unsafe {
        GetProcessInformation(
            handle,
            ProcessProtectionLevelInfo,
            &mut info as *mut PROCESS_PROTECTION_LEVEL_INFORMATION as *mut c_void,
            std::mem::size_of::<PROCESS_PROTECTION_LEVEL_INFORMATION>() as u32,
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == FALSE {
        None
    } else {
        Some(info.ProtectionLevel)
    }
}

fn try_enable_debug_privilege() -> DebugPrivilege {
    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    };
    if opened == FALSE || token.is_null() {
        return DebugPrivilege::NotHeld;
    }
    let name: Vec<u16> = "SeDebugPrivilege\0".encode_utf16().collect();
    let mut luid = windows_sys::Win32::Foundation::LUID {
        LowPart: 0,
        HighPart: 0,
    };
    let looked = unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) };
    if looked == FALSE {
        unsafe { CloseHandle(token) };
        return DebugPrivilege::NotHeld;
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    let adjusted = unsafe {
        AdjustTokenPrivileges(
            token,
            FALSE,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    let err = unsafe { GetLastError() };
    unsafe { CloseHandle(token) };
    if adjusted == FALSE || err == ERROR_NOT_ALL_ASSIGNED {
        DebugPrivilege::NotHeld
    } else {
        DebugPrivilege::Enabled
    }
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
