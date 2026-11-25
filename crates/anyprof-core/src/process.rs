use std::path::PathBuf;

#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(windows)]
use std::process::{Command, Stdio};

/// A cheap look at a process that does not attach a sampler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSnapshot {
    /// Process id that was inspected.
    pub pid: u32,
    /// Short process name (`comm` on Linux, `proc_name` on macOS, image name on Windows).
    pub comm: Option<String>,
    /// Argument vector. Empty when it could not be read.
    pub cmdline: Vec<String>,
    /// Resolved executable path (`/proc/<pid>/exe`, `proc_pidpath`, or `QueryFullProcessImageNameW`).
    pub exe: Option<PathBuf>,
}

/// Read a process name, command line, and executable path without attaching a sampler.
///
/// Linux reads `/proc/<pid>`, macOS queries `libproc` and `sysctl(KERN_PROCARGS2)`,
/// and Windows queries `QueryFullProcessImageNameW` (with `tasklist` fallback for
/// protected system processes). Returns `None` when the process does not exist.
#[must_use]
pub fn inspect(pid: u32) -> Option<ProcessSnapshot> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        inspect_linux(pid)
    }
    #[cfg(target_os = "macos")]
    {
        inspect_macos(pid)
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        if process_exists(pid) {
            Some(ProcessSnapshot {
                pid,
                comm: None,
                cmdline: Vec::new(),
                exe: None,
            })
        } else {
            None
        }
    }
    #[cfg(windows)]
    {
        inspect_windows(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        None
    }
}

/// Path that can be opened to read the process executable without attaching a sampler.
///
/// Linux returns `/proc/<pid>/exe` (valid across mount namespaces and deleted binaries),
/// macOS calls `proc_pidpath`, and Windows calls `QueryFullProcessImageNameW` with
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
#[must_use]
pub fn executable_file(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from(format!("/proc/{pid}/exe"));
        if path.exists() { Some(path) } else { None }
    }
    #[cfg(target_os = "macos")]
    {
        macos_executable_path(pid)
    }
    #[cfg(windows)]
    {
        windows_executable_path(pid).ok().flatten()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

/// Whether `pid` currently refers to a live process.
///
/// Linux checks `/proc/<pid>`; macOS checks `proc_pidinfo` (excluding zombies) and
/// `kill(pid, 0)`; Windows checks `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
/// and `GetExitCodeProcess`.
#[must_use]
pub fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).is_dir()
    }
    #[cfg(target_os = "macos")]
    {
        macos_process_exists(pid)
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let Ok(raw) = i32::try_from(pid) else {
            return false;
        };
        if raw <= 0 {
            return false;
        }
        // SAFETY: signal 0 performs existence and permission checks without delivering a signal.
        let rc = unsafe { libc::kill(raw, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        windows_process_exists(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Deliver `SIGUSR1` to a positive process id via `libc::kill` without spawning `/bin/kill`.
#[cfg(unix)]
pub fn send_sigusr1(pid: u32) -> Result<(), String> {
    let Ok(raw) = i32::try_from(pid) else {
        return Err(format!("pid {pid} is out of range for kill(2)"));
    };
    if raw <= 0 {
        return Err(format!("refusing to signal non-positive pid {pid}"));
    }
    // SAFETY: `raw > 0` targets a single process rather than a process group.
    let rc = unsafe { libc::kill(raw, libc::SIGUSR1) };
    if rc == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        Err(format!("kill({pid}, SIGUSR1) failed: {err}"))
    }
}

#[cfg(target_os = "linux")]
fn inspect_linux(pid: u32) -> Option<ProcessSnapshot> {
    let root = PathBuf::from(format!("/proc/{pid}"));
    if !root.is_dir() {
        return None;
    }
    let comm = std::fs::read_to_string(root.join("comm"))
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|comm| !comm.is_empty());
    let cmdline = std::fs::read(root.join("cmdline"))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect()
        })
        .unwrap_or_default();
    let exe = std::fs::read_link(root.join("exe")).ok();
    Some(ProcessSnapshot {
        pid,
        comm,
        cmdline,
        exe,
    })
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, buffersize: u32) -> i32;
    fn proc_name(pid: i32, buffer: *mut std::ffi::c_void, buffersize: u32) -> i32;
    fn proc_pidinfo(
        pid: i32,
        flavor: i32,
        arg: u64,
        buffer: *mut std::ffi::c_void,
        buffersize: i32,
    ) -> i32;
}

#[cfg(target_os = "macos")]
fn macos_process_exists(pid: u32) -> bool {
    const PROC_PIDTBSDINFO: i32 = 3;
    const SZOMB: u32 = 5;

    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    if raw <= 0 {
        return false;
    }
    let mut bsd = [0u8; 136];
    // SAFETY: `bsd` holds `sizeof(struct proc_bsdinfo)` (136 bytes on 64-bit Darwin).
    let wrote = unsafe {
        proc_pidinfo(
            raw,
            PROC_PIDTBSDINFO,
            0,
            bsd.as_mut_ptr().cast(),
            bsd.len() as i32,
        )
    };
    if wrote >= 12 {
        let status = u32::from_le_bytes(bsd[4..8].try_into().unwrap_or([0; 4]));
        return status != SZOMB;
    }
    // SAFETY: signal 0 checks process existence without delivering a signal.
    let rc = unsafe { libc::kill(raw, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
fn macos_executable_path(pid: u32) -> Option<PathBuf> {
    const MAX: u32 = 4 * 1024;
    let raw = i32::try_from(pid).ok()?;
    if raw <= 0 {
        return None;
    }
    let mut buf = vec![0u8; MAX as usize];
    // SAFETY: `buf` is `MAX` bytes (`PROC_PIDPATHINFO_MAXSIZE`); does not suspend the target.
    let n = unsafe { proc_pidpath(raw, buf.as_mut_ptr().cast(), MAX) };
    if n <= 0 {
        return None;
    }
    let end = buf
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or((n as usize).min(buf.len()));
    let text = String::from_utf8(buf[..end].to_vec()).ok()?;
    (!text.is_empty()).then(|| PathBuf::from(text))
}

#[cfg(target_os = "macos")]
fn macos_proc_name(pid: u32) -> Option<String> {
    let raw = i32::try_from(pid).ok()?;
    if raw <= 0 {
        return None;
    }
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is 256 bytes and `proc_name` writes a NUL-terminated UTF-8 name.
    let n = unsafe { proc_name(raw, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let end = buf
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or((n as usize).min(buf.len()));
    let text = std::str::from_utf8(&buf[..end]).ok()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(target_os = "macos")]
fn macos_procargs2(pid: u32) -> Option<Vec<String>> {
    const MAX_PROCARGS: usize = 512 * 1024;
    let raw = i32::try_from(pid).ok()?;
    if raw <= 0 {
        return None;
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, raw];
    let mut size: libc::size_t = 0;
    // SAFETY: querying required buffer size with a null output buffer.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size == 0
    {
        return None;
    }
    let alloc = size.saturating_add(4096).min(MAX_PROCARGS);
    let mut buf = vec![0u8; alloc];
    let mut actual = buf.len();
    // SAFETY: `buf` has `actual` writable bytes.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut actual,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || actual < 5
    {
        return None;
    }
    buf.truncate(actual.min(buf.len()));
    parse_kern_procargs2(&buf)
}

#[cfg(any(target_os = "macos", test))]
fn parse_kern_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    if buf.len() < 5 {
        return None;
    }
    let argc = i32::from_le_bytes(buf[0..4].try_into().ok()?);
    if argc <= 0 || argc > 65_536 {
        return None;
    }
    // Skip `exec_path\0` and any NUL alignment padding before `argv[0]`.
    let exec_nul = buf[4..].iter().position(|&b| b == 0)?;
    let mut pos = 4 + exec_nul + 1;
    while pos < buf.len() && buf[pos] == 0 {
        pos += 1;
    }
    let mut args = Vec::with_capacity((argc as usize).min(256));
    for _ in 0..argc {
        if pos >= buf.len() {
            break;
        }
        let end = buf[pos..]
            .iter()
            .position(|&b| b == 0)
            .map_or(buf.len(), |off| pos + off);
        let arg = String::from_utf8_lossy(&buf[pos..end]).into_owned();
        if !arg.is_empty() {
            args.push(arg);
        }
        pos = end.saturating_add(1);
    }
    (!args.is_empty()).then_some(args)
}

#[cfg(target_os = "macos")]
fn inspect_macos(pid: u32) -> Option<ProcessSnapshot> {
    if !macos_process_exists(pid) {
        return None;
    }
    let exe = macos_executable_path(pid);
    let mut cmdline = macos_procargs2(pid).unwrap_or_default();
    let comm = macos_proc_name(pid).or_else(|| {
        exe.as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
    });
    if cmdline.is_empty()
        && let Some(ref fallback) = exe
            .as_ref()
            .and_then(|p| p.to_str().map(str::to_string))
            .or_else(|| comm.clone())
    {
        cmdline.push(fallback.clone());
    }
    Some(ProcessSnapshot {
        pid,
        comm,
        cmdline,
        exe,
    })
}

#[cfg(windows)]
const STILL_ACTIVE_CODE: u32 = 259;

#[cfg(windows)]
fn windows_process_exists(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: query-only access; handle is closed before returning.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    unsafe { CloseHandle(handle) };
    ok != 0 && code == STILL_ACTIVE_CODE
}

/// Returns `Ok(Some(path))` for an accessible live process, `Ok(None)` if not running,
/// and `Err(())` when `ERROR_ACCESS_DENIED` prevented opening a protected process.
#[cfg(windows)]
fn windows_executable_path(pid: u32) -> Result<Option<PathBuf>, ()> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    // SAFETY: query-only access without enabling `SeDebugPrivilege`; handle is always closed.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
            return Err(());
        }
        return Ok(None);
    }
    let mut code = 0u32;
    let alive = unsafe { GetExitCodeProcess(handle, &mut code) } != 0 && code == STILL_ACTIVE_CODE;
    if !alive {
        unsafe { CloseHandle(handle) };
        return Ok(None);
    }
    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
    unsafe { CloseHandle(handle) };
    if ok == 0 || len == 0 {
        return Ok(None);
    }
    let os = std::ffi::OsString::from_wide(&buf[..len as usize]);
    if os.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(os)))
    }
}

#[cfg(windows)]
fn inspect_windows(pid: u32) -> Option<ProcessSnapshot> {
    match windows_executable_path(pid) {
        Ok(Some(exe)) => {
            let comm = exe
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            let cmdline = comm.iter().cloned().collect();
            return Some(ProcessSnapshot {
                pid,
                comm,
                cmdline,
                exe: Some(exe),
            });
        }
        Ok(None) => {
            if !windows_process_exists(pid) {
                return None;
            }
        }
        Err(()) => {}
    }
    inspect_windows_tasklist(pid)
}

#[cfg(windows)]
fn inspect_windows_tasklist(pid: u32) -> Option<ProcessSnapshot> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if line.starts_with("INFO:") {
        return None;
    }
    let name = csv_first_field(line)?.to_string();
    Some(ProcessSnapshot {
        pid,
        comm: Some(name.clone()),
        cmdline: vec![name],
        exe: None,
    })
}

#[cfg(any(windows, test))]
fn csv_first_field(line: &str) -> Option<&str> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(&rest[..end])
    } else {
        Some(line.split(',').next()?.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::{executable_file, inspect, parse_kern_procargs2, process_exists};

    #[test]
    fn zero_pid_is_not_a_process() {
        assert!(!process_exists(0));
        assert!(inspect(0).is_none());
        assert!(executable_file(0).is_none());
    }

    #[test]
    fn csv_first_field_reads_tasklist_names() {
        assert_eq!(
            super::csv_first_field(r#""node.exe","1234","Console","1","12 K""#),
            Some("node.exe")
        );
        assert_eq!(super::csv_first_field("node,1234"), Some("node"));
    }

    #[test]
    fn missing_pid_is_absent() {
        assert!(!process_exists(u32::MAX));
        assert!(inspect(u32::MAX).is_none());
        assert!(executable_file(u32::MAX).is_none());
    }

    #[test]
    fn inspect_and_executable_file_read_current_process() {
        let pid = std::process::id();
        assert!(process_exists(pid));
        let snap = inspect(pid).expect("current process is visible");
        assert!(snap.comm.is_some() || !snap.cmdline.is_empty());
        let exe = executable_file(pid).expect("current process executable path is readable");
        assert!(exe.exists(), "expected {} to exist", exe.display());
    }

    #[test]
    fn kern_procargs2_parses_argc_exec_path_padding_and_argv() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3i32.to_le_bytes());
        buf.extend_from_slice(b"/usr/local/bin/node\0\0\0\0");
        buf.extend_from_slice(b"/usr/local/bin/node\0--inspect\0server.js\0HOME=/tmp\0");
        assert_eq!(
            parse_kern_procargs2(&buf),
            Some(vec![
                "/usr/local/bin/node".to_string(),
                "--inspect".to_string(),
                "server.js".to_string(),
            ])
        );
        assert!(parse_kern_procargs2(&[]).is_none());
        assert!(parse_kern_procargs2(&0i32.to_le_bytes()).is_none());
    }
}
