use std::path::PathBuf;

#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(any(windows, all(unix, not(target_os = "linux"))))]
use std::process::{Command, Stdio};

/// A cheap look at a process that does not attach a sampler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSnapshot {
    /// Process id that was inspected.
    pub pid: u32,
    /// `comm` (Linux) or the process name reported by `ps`.
    pub comm: Option<String>,
    /// Argument vector. Empty when it could not be read.
    pub cmdline: Vec<String>,
    /// Resolved executable path. On Linux this is the target of `/proc/<pid>/exe`.
    pub exe: Option<PathBuf>,
}

/// Read a process name and command line without attaching to it.
///
/// On Linux this reads `/proc/<pid>`. On other Unix systems it asks `ps` for the
/// process name. On Windows it asks `tasklist`. Returns `None` when the process
/// does not exist.
#[must_use]
pub fn inspect(pid: u32) -> Option<ProcessSnapshot> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        inspect_linux(pid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        inspect_ps(pid)
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

/// Path that can be opened to read the process executable.
///
/// On Linux this is `/proc/<pid>/exe`, which stays valid even if the binary was
/// deleted. Other platforms return `None`.
#[must_use]
pub fn executable_file(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from(format!("/proc/{pid}/exe"));
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Whether `pid` currently refers to a process.
///
/// This does not send a signal. Linux checks `/proc`; other Unix systems use `ps`;
/// Windows uses `tasklist`.
#[must_use]
pub fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).is_dir()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        inspect_windows(pid).is_some()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
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

#[cfg(all(unix, not(target_os = "linux")))]
fn inspect_ps(pid: u32) -> Option<ProcessSnapshot> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let comm = String::from_utf8(output.stdout).ok()?;
    let comm = comm.trim();
    if comm.is_empty() {
        return None;
    }
    let cmdline = ps_args(pid).unwrap_or_else(|| vec![comm.to_string()]);
    Some(ProcessSnapshot {
        pid,
        comm: Some(comm.to_string()),
        cmdline,
        exe: None,
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_args(pid: u32) -> Option<Vec<String>> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let args: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

#[cfg(windows)]
fn inspect_windows(pid: u32) -> Option<ProcessSnapshot> {
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
    use super::{inspect, process_exists};

    #[test]
    fn zero_pid_is_not_a_process() {
        assert!(!process_exists(0));
        assert!(inspect(0).is_none());
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
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inspect_self_reads_proc() {
        let snap = inspect(std::process::id()).expect("current process is visible in /proc");
        assert!(snap.comm.is_some() || !snap.cmdline.is_empty());
        assert!(process_exists(std::process::id()));
    }
}
