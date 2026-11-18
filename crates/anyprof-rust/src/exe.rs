//! Path of a process executable, without attaching a sampler.
//!
//! Linux reads `/proc/<pid>/exe`. macOS asks `proc_pidpath`, which works for
//! the same user without root. Windows opens the process with
//! `PROCESS_QUERY_LIMITED_INFORMATION` and calls `QueryFullProcessImageNameW`.
//! That is a name query, not the sampling `OpenProcess`, and it does not
//! enable `SeDebugPrivilege`.

use std::path::PathBuf;

#[must_use]
pub(crate) fn executable_path(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        anyprof_core::executable_file(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_path(pid)
    }
    #[cfg(windows)]
    {
        windows_path(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "macos")]
fn macos_path(pid: u32) -> Option<PathBuf> {
    // `PROC_PIDPATHINFO_MAXSIZE` is 4 * MAXPATHLEN (1024).
    const MAX: u32 = 4 * 1024;
    let mut buf = vec![0u8; MAX as usize];
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: `buf` is `MAX` bytes and the call writes at most that many,
    // including a trailing NUL. It does not suspend the process.
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr(), MAX) };
    if n <= 0 {
        return None;
    }
    let end = buf.iter().position(|byte| *byte == 0).unwrap_or(n as usize);
    let text = String::from_utf8(buf[..end].to_vec()).ok()?;
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut u8, buffersize: u32) -> i32;
}

#[cfg(windows)]
fn windows_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    // SAFETY: query-only access. The handle is closed on every path below.
    // This is not the sampler's `OpenProcess` (`PROCESS_VM_READ` plus a
    // suspend). `SeDebugPrivilege` is not enabled.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
    unsafe { CloseHandle(handle) };
    if ok == 0 || len == 0 {
        return None;
    }
    let os = std::ffi::OsString::from_wide(&buf[..len as usize]);
    if os.is_empty() {
        None
    } else {
        Some(PathBuf::from(os))
    }
}
