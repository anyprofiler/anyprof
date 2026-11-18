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
//! `-k` is ignored. A program counter in kernel space is not stored as a
//! user frame. `IsWow64Process2` refuses 32-bit and ARM64 targets.
//! `IsWow64Process` alone cannot see ARM64.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::java::{self, JavaVm};
use crate::python::{self, CountedStack, PythonVm};
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
    ReadProcessMemory, STACKFRAME64, SYMBOL_INFO, SYMOPT_DEFERRED_LOADS,
    SYMOPT_DISABLE_SYMSRV_AUTODETECT, SYMOPT_FAIL_CRITICAL_ERRORS, SYMOPT_IGNORE_NT_SYMPATH,
    SYMOPT_NO_PROMPTS, SYMOPT_UNDNAME, StackWalk64, SymCleanup, SymFromAddr,
    SymFunctionTableAccess64, SymGetModuleBase64, SymGetModuleInfo64, SymGetOptions,
    SymInitializeW, SymSetOptions,
};
use windows_sys::Win32::System::ProcessStatus::GetModuleFileNameExW;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessInformation, IsWow64Process2, OpenProcess, OpenProcessToken,
    OpenThread, PROCESS_NAME_WIN32, PROCESS_PROTECTION_LEVEL_INFORMATION,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    ProcessProtectionLevelInfo, QueryFullProcessImageNameW, ResumeThread, SuspendThread,
    THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

use crate::policy::{self, DebugPrivilege};
use crate::snapshot::{self, SnapshotError};
use crate::symbol::{self, frame_runtime};

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
    let mut go = load_go(process.handle);
    let python = python::attach_process(process.handle);
    let java = java::attach_process(process.handle, &mut |addr, buf| {
        read_process(process.handle, addr, buf)
    });

    let started = Instant::now();
    let mut next = started;
    let mut counts: HashMap<CountedStack, u64> = HashMap::new();
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
        match sample_addresses(
            process.handle,
            target.pid,
            go.as_mut(),
            python.as_ref(),
            java.as_ref(),
        ) {
            Ok(stacks) => {
                for stack in stacks {
                    if stack.recordable() {
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
        go.as_ref(),
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

pub(crate) fn with_process<T>(
    pid: u32,
    body: impl FnOnce(HANDLE) -> Result<T, String>,
) -> Result<T, Error> {
    let process = Process::open(pid)?;
    body(process.handle).map_err(|message| Error::backend("native", message))
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
        // rustc's `/PDBALTPATH` often stores only the PDB file name. dbghelp
        // finds that name in this directory. The embedded absolute path, when
        // it still exists, is tried first, which is how a C/C++ PDB next to
        // its exe is loaded. The path is not a symbol server.
        let search = exe_symbol_dir(process);
        let search_ptr = search
            .as_ref()
            .map_or(std::ptr::null(), |path| path.as_ptr());
        // SAFETY: `SymInitializeW` attaches dbghelp to this process handle.
        // `Symbols` calls `SymCleanup`. `search` stays alive for the call.
        let ok = unsafe { SymInitializeW(process, search_ptr, 1) };
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

struct GoProc {
    image: crate::go_sym::GoImage,
    preferred: u64,
    module: String,
}

impl GoProc {
    fn image_for(&self, process: HANDLE, pc: u64) -> Option<crate::go_sym::GoImage> {
        let actual = unsafe { SymGetModuleBase64(process, pc) };
        if actual == 0 {
            return None;
        }
        let relocated = self.image.at_base(self.preferred, actual);
        relocated.contains_pc(pc).then_some(relocated)
    }
}

fn load_go(process: HANDLE) -> Option<GoProc> {
    let path = process_exe(process)?;
    let image = crate::go_sym::open_image(&path)?;
    let preferred = crate::go_sym::pe_image_base(&path).unwrap_or(0);
    let module = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("go")
        .to_string();
    Some(GoProc {
        image,
        preferred,
        module,
    })
}

fn process_exe(process: HANDLE) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) };
    if ok == 0 || len == 0 {
        return None;
    }
    let os = std::ffi::OsString::from_wide(&buf[..len as usize]);
    if os.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(os))
    }
}

fn go_frames(process: HANDLE, pc: u64, sp: u64, go: Option<&mut GoProc>) -> Option<Vec<u64>> {
    let go = go?;
    let image = go.image_for(process, pc)?;
    crate::go_sym::unwind(
        std::slice::from_ref(&image),
        pc,
        sp,
        0,
        crate::go_sym::GoMachine::X86_64,
        |addr, buf| read_process(process, addr, buf),
        |addr| addr,
    )
}

fn go_named(process: HANDLE, go: &GoProc, lookup: u64, addr: u64) -> Option<Frame> {
    let image = go.image_for(process, lookup)?;
    let symbol = image.symbol(lookup)?;
    let mut frame = Frame::new(Runtime::Go, symbol.name);
    frame.file = symbol.file;
    frame.line = symbol.line;
    frame.address = Some(addr);
    frame.module = Some(go.module.clone());
    Some(frame)
}

fn read_process(process: HANDLE, addr: u64, buf: &mut [u8]) -> bool {
    if addr == 0 || buf.is_empty() {
        return false;
    }
    let mut read = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            process,
            addr as *const core::ffi::c_void,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut read,
        )
    };
    ok != FALSE && read == buf.len()
}

fn sample_addresses(
    process: HANDLE,
    pid: u32,
    mut go: Option<&mut GoProc>,
    python: Option<&PythonVm>,
    java: Option<&JavaVm>,
) -> Result<Vec<CountedStack>, SampleFault> {
    let running = running_threads(pid)?;
    let mut stacks = Vec::with_capacity(running.len());
    for tid in running {
        match unwind_thread(process, tid, go.as_deref_mut(), python, java) {
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

fn unwind_thread(
    process: HANDLE,
    tid: u32,
    go: Option<&mut GoProc>,
    python: Option<&PythonVm>,
    java: Option<&JavaVm>,
) -> Result<Option<CountedStack>, SampleFault> {
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
    let frames = walk_suspended(process, thread.0, tid, go, python, java);
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

fn walk_suspended(
    process: HANDLE,
    thread: HANDLE,
    tid: u32,
    go: Option<&mut GoProc>,
    python: Option<&PythonVm>,
    java: Option<&JavaVm>,
) -> Option<CountedStack> {
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
    // `frames` is `None` for a leaf outside the code cache and for a blob
    // whose method cannot be read. Both keep the native walk.
    if let Some(vm) = java
        && let Some(frames) = vm.frames(
            aligned.context.Rip,
            aligned.context.Rsp,
            aligned.context.Rbp,
            &mut |addr, buf| read_process(process, addr, buf),
        )
    {
        return Some(CountedStack::Java {
            kernel: Vec::new(),
            frames,
        });
    }
    if let Some(vm) = python
        && let Some(frames) = vm.frames(u64::from(tid), aligned.context.Rip, &mut |addr, buf| {
            read_process(process, addr, buf)
        })
    {
        return Some(CountedStack::Python {
            kernel: Vec::new(),
            frames,
        });
    }
    if let Some(frames) = go_frames(process, aligned.context.Rip, aligned.context.Rsp, go) {
        return Some(CountedStack::Addresses {
            frames,
            kernel_len: 0,
        });
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
    let frames = crate::walk::user_instruction_pointers(frames);
    if frames.is_empty() {
        None
    } else {
        Some(CountedStack::Addresses {
            frames,
            kernel_len: 0,
        })
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
    counts: HashMap<CountedStack, u64>,
    process: HANDLE,
    go: Option<&GoProc>,
    elapsed: Duration,
    interval: Duration,
) -> Profile {
    let mut rust_files = HashMap::<String, bool>::new();
    let mut pdbs = HashMap::<String, crate::pdb_syms::ImageSymbols>::new();
    let mut stacks = Vec::with_capacity(counts.len());
    for (stack, weight) in counts {
        let frames = match stack {
            CountedStack::Addresses { frames, .. } => frames
                .iter()
                .enumerate()
                .map(|(index, addr)| {
                    frame_for(process, *addr, index == 0, go, &mut rust_files, &mut pdbs)
                })
                .collect(),
            CountedStack::Python { frames, .. } => python::python_profile_frames(&frames),
            CountedStack::Java { frames, .. } => java::java_profile_frames(&frames),
        };
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

fn frame_for(
    process: HANDLE,
    addr: u64,
    leaf: bool,
    go: Option<&GoProc>,
    rust_files: &mut HashMap<String, bool>,
    pdbs: &mut HashMap<String, crate::pdb_syms::ImageSymbols>,
) -> Frame {
    let lookup = policy::symbol_lookup_address(addr, leaf);
    if let Some(frame) = go.and_then(|go| go_named(process, go, lookup, addr)) {
        return frame;
    }
    if lookup != addr
        && let Some(frame) = go.and_then(|go| go_named(process, go, addr, addr))
    {
        return frame;
    }
    let hit = symbol_at(process, lookup, pdbs).or_else(|| symbol_at(process, addr, pdbs));
    let (name, module, runtime) = match hit {
        Some((name, module, path, symbol_runtime, from_cpp)) => {
            let image_rust = path.is_some_and(|path| {
                *rust_files
                    .entry(path.clone())
                    .or_insert_with(|| crate::rust_image::file_is_rust(Path::new(&path)))
            });
            let runtime = frame_runtime(symbol_runtime, &name, from_cpp, image_rust);
            (name, module, runtime)
        }
        None => (
            String::new(),
            module_only(process, lookup)
                .or_else(|| module_only(process, addr))
                .unwrap_or_default(),
            Runtime::Native,
        ),
    };
    let module = if module.is_empty() {
        None
    } else {
        Some(module)
    };
    let mut frame = Frame::new(runtime, name);
    frame.address = Some(addr);
    frame.module = module;
    frame
}

fn symbol_at(
    process: HANDLE,
    addr: u64,
    pdbs: &mut HashMap<String, crate::pdb_syms::ImageSymbols>,
) -> Option<(String, String, Option<String>, Runtime, bool)> {
    let (module, dbg_path) = module_path(process, addr).unwrap_or_else(|| (String::new(), None));
    let file = module_file(process, addr).or_else(|| dbg_path.map(std::path::PathBuf::from));
    let path = file
        .as_ref()
        .map(|file| file.to_string_lossy().into_owned());
    if let Some((name, runtime, from_cpp)) = symbol_name(process, addr) {
        return Some((name, module, path, runtime, from_cpp));
    }
    let file = file?;
    let (name, runtime, from_cpp) = pdb_name(process, addr, &file, pdbs)?;
    let module = if module.is_empty() {
        file.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string()
    } else {
        module
    };
    Some((name, module, path, runtime, from_cpp))
}

/// `SymFromAddr` first. A rustc PDB often yields no name here even when the
/// file sits next to the exe and the C++ path through this function works.
/// The raw name is read with `SYMOPT_UNDNAME` off so an MSVC `?` symbol stays
/// distinguishable from an undecorated Rust path, then undecorated for display.
fn symbol_name(process: HANDLE, addr: u64) -> Option<(String, Runtime, bool)> {
    let raw = read_symbol(process, addr, false).or_else(|| read_symbol(process, addr, true))?;
    if raw.starts_with('?') {
        let pretty = read_symbol(process, addr, true).unwrap_or(raw);
        return Some((pretty, Runtime::Native, true));
    }
    let presented = symbol::present_elf_symbol(&raw);
    Some((presented.name, presented.runtime, presented.from_cpp))
}

/// Bytes reserved for `SYMBOL_INFO::Name` past the one byte inside the struct.
const SYMBOL_NAME_EXTRA: usize = 8191;

#[repr(C)]
struct SymbolBuf {
    info: SYMBOL_INFO,
    extra: [u8; SYMBOL_NAME_EXTRA],
}

fn read_symbol(process: HANDLE, addr: u64, undname: bool) -> Option<String> {
    let previous = unsafe { SymGetOptions() };
    let next = if undname {
        previous | SYMOPT_UNDNAME
    } else {
        previous & !SYMOPT_UNDNAME
    };
    unsafe { SymSetOptions(next) };
    let _restore = RestoreOptions(previous);
    let mut buf: Box<SymbolBuf> = unsafe { Box::new(std::mem::zeroed()) };
    buf.info.SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
    buf.info.MaxNameLen = (SYMBOL_NAME_EXTRA + 1) as u32;
    let mut displacement = 0u64;
    let ok = unsafe { SymFromAddr(process, addr, &mut displacement, &mut buf.info) };
    if ok == FALSE {
        return None;
    }
    let cap = SYMBOL_NAME_EXTRA + 1;
    let len = (buf.info.NameLen as usize).min(cap);
    let bytes = unsafe { std::slice::from_raw_parts(buf.info.Name.as_ptr().cast::<u8>(), len) };
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

struct RestoreOptions(u32);

impl Drop for RestoreOptions {
    fn drop(&mut self) {
        unsafe { SymSetOptions(self.0) };
    }
}

fn pdb_name(
    process: HANDLE,
    addr: u64,
    file: &Path,
    pdbs: &mut HashMap<String, crate::pdb_syms::ImageSymbols>,
) -> Option<(String, Runtime, bool)> {
    let base = unsafe { SymGetModuleBase64(process, addr) };
    if base == 0 || addr < base {
        return None;
    }
    let key = file.to_string_lossy().into_owned();
    let symbols = pdbs
        .entry(key)
        .or_insert_with(|| crate::pdb_syms::load(file));
    let (name, runtime, from_cpp) = symbols.name_at(addr - base)?;
    Some((name.to_string(), runtime, from_cpp))
}

/// Full path of the module containing `addr`. `IMAGEHLP_MODULE64` stores only
/// 256 characters, which is enough for the CI fixture and short for a long
/// temp path. `GetModuleFileNameExW` is the path the PDB is opened from.
fn module_file(process: HANDLE, addr: u64) -> Option<std::path::PathBuf> {
    let base = unsafe { SymGetModuleBase64(process, addr) };
    if base == 0 {
        return None;
    }
    let mut buf = vec![0u16; 32768];
    let n = unsafe {
        GetModuleFileNameExW(
            process,
            base as *mut std::ffi::c_void,
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    let text = String::from_utf16_lossy(&buf[..n as usize]);
    if text.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(text))
    }
}

fn exe_symbol_dir(process: HANDLE) -> Option<Vec<u16>> {
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
    };
    if ok == FALSE || len == 0 {
        return None;
    }
    let path = &buf[..len as usize];
    let slash = path
        .iter()
        .rposition(|unit| *unit == u16::from(b'\\') || *unit == u16::from(b'/'))?;
    if slash == 0 {
        return None;
    }
    let mut dir = path[..slash].to_vec();
    dir.push(0);
    Some(dir)
}

fn module_only(process: HANDLE, addr: u64) -> Option<String> {
    module_path(process, addr).map(|(name, _)| name)
}

/// File name shown on the frame, and the full path used to recognize a Rust image.
fn module_path(process: HANDLE, addr: u64) -> Option<(String, Option<String>)> {
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
        .unwrap_or(path.as_str())
        .to_string();
    if name.is_empty() {
        return None;
    }
    let full = if path == name { None } else { Some(path) };
    Some((name, full))
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
    let mut process_machine = 0u16;
    let mut native_machine = 0u16;
    // SAFETY: `IsWow64Process2` reads the process and host machine types.
    // It does not attach, suspend, or write. `IsWow64Process` alone reports
    // only a 32-bit WOW64 process, so an ARM64 target looks like x64.
    let ok = unsafe { IsWow64Process2(process, &mut process_machine, &mut native_machine) };
    if ok == FALSE {
        let err = unsafe { GetLastError() };
        return Err(Error::backend(
            "native",
            format!("IsWow64Process2 failed for pid {pid} (error {err})"),
        ));
    }
    if policy::windows_process_is_x64(process_machine, native_machine) {
        return Ok(());
    }
    let kind = policy::windows_target_kind(process_machine, native_machine);
    Err(Error::backend(
        "native",
        format!(
            "cannot sample pid {pid}: native sampling on Windows walks x64 stacks; this process is {kind}"
        ),
    ))
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
