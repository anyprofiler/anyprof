//! CLI coverage for the Python backend.
//!
//! Auto-detection (no `-b`) must select `python` for a CPython pid and write
//! `anyprof-python-<pid>-*.speedscope.json`. Collapsed stacks prefix
//! interpreter names with `[python]`.

use std::process::Command;

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
use std::path::Path;
#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
use std::process::Stdio;
#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
use std::time::Duration;

#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
)))]
#[test]
fn python_record_is_not_implemented_on_this_platform() {
    let pid = std::process::id().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["rec", "-b", "python", "-p", &pid, "-d", "200ms"])
        .output()
        .expect("run anyprof");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "python sampling must fail closed on this platform\n{stderr}"
    );
    assert!(
        stderr.contains("not implemented yet"),
        "expected the not-implemented error, got:\n{stderr}"
    );
    assert!(
        stderr.contains("python backend"),
        "the refusal should name the python backend:\n{stderr}"
    );
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
#[test]
fn record_auto_detects_a_busy_python_program() {
    #[cfg(not(target_os = "macos"))]
    let _lock = live_lock();
    let pythons = interpreters();
    if pythons.is_empty() {
        return;
    }
    for python in pythons {
        record_one(&python);
    }
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
fn record_one(python: &Path) {
    let dir = std::env::temp_dir().join(format!(
        "anyprof-python-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("spin.py");
    std::fs::write(
        &script,
        r#"
import sys
path = sys.argv[1]
def caller():
    with open(path, "wb") as handle:
        handle.write((1).to_bytes(8, "little"))
    spin_é()
def spin_é():
    x = 0
    while True:
        x = x + 1
caller()
"#,
    )
    .unwrap();
    let counter = dir.join("counter");
    std::fs::write(&counter, [0u8; 8]).unwrap();
    let mut child = Command::new(python)
        .arg(&script)
        .arg(&counter)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy python");
    let pid = child.id();
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if read_counter(&counter) > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if read_counter(&counter) == 0 {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        if std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1") {
            panic!("the Python fixture did not increment its counter");
        }
        eprintln!("skipping CLI python sample: fixture counter stayed at 0");
        return;
    }
    let output_dir = dir.join("out");
    std::fs::create_dir_all(&output_dir).unwrap();
    let recorded = record(pid, &output_dir);
    let _ = child.kill();
    let _ = child.wait();
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("sudo")
            .args(["-n", "chmod", "-R", "a+rX"])
            .arg(&output_dir)
            .status();
    }
    let stderr = String::from_utf8_lossy(&recorded.stderr);
    let stdout = String::from_utf8_lossy(&recorded.stdout);
    if !recorded.status.success() {
        // A refused frame read is the product error under test. It must not
        // be reported as a skipped native profile.
        let denied = !stderr.contains("process_vm_readv")
            && (stderr.contains("perf_event_open")
                || stderr.contains("task_for_pid")
                || stderr.contains("a password is required")
                || stderr.contains("not permitted"));
        if denied && std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() != Some("1") {
            eprintln!("skipping CLI python sample:\n{stderr}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!(
            "anyprof rec failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            recorded.status.code()
        );
    }
    assert!(
        stderr.contains("using backend 'python'"),
        "auto-detection should select python:\n{stderr}"
    );
    let (samples, path) = parse_wrote(&stderr);
    assert!(
        (20..=1_000).contains(&samples),
        "sample count {samples} is not a 1s/100Hz recording\n{stderr}"
    );
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with(&format!("anyprof-python-{pid}-")) && name.ends_with(".speedscope.json"),
        "unexpected auto name {name}"
    );
    let collapsed = dir.join("cpu.collapsed");
    let convert = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["conv", "-i"])
        .arg(&path)
        .arg("-o")
        .arg(&collapsed)
        .args(["-t", "collapsed"])
        .output()
        .expect("anyprof conv");
    assert!(
        convert.status.success(),
        "conv failed\n{}",
        String::from_utf8_lossy(&convert.stderr)
    );
    let folded = std::fs::read_to_string(&collapsed).unwrap();
    assert!(
        folded.contains("[python] spin_é") && folded.contains("[python] caller"),
        "collapsed stacks:\n{folded}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
fn interpreters() -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for key in [
        "ANYPROF_PYTHON_311",
        "ANYPROF_PYTHON_312",
        "ANYPROF_PYTHON_313",
        "ANYPROF_PYTHON_314",
        "ANYPROF_PYTHON",
    ] {
        if let Ok(path) = std::env::var(key)
            && !path.is_empty()
        {
            let path = std::path::PathBuf::from(path);
            if !found.contains(&path) {
                found.push(path);
            }
        }
    }
    if found.is_empty() {
        for name in [
            "python3.11",
            "python3.12",
            "python3.13",
            "python3.14",
            "python3",
        ] {
            let Ok(output) = Command::new(name)
                .args(["-c", "import sys; print(sys.executable)"])
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if path.is_empty() {
                continue;
            }
            let path = std::path::PathBuf::from(path);
            if !found.contains(&path) {
                found.push(path);
            }
        }
    }
    found.retain(|path| {
        let Ok(output) = Command::new(path)
            .args([
                "-c",
                "import sys; print(sys.version_info[0], sys.version_info[1])",
            ])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.split_whitespace();
        matches!(
            (parts.next(), parts.next()),
            (Some("3"), Some("11") | Some("12") | Some("13") | Some("14"))
        )
    });
    if found.is_empty() {
        if std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1") {
            panic!("no CPython 3.11, 3.12, 3.13, or 3.14 interpreter was found");
        }
        eprintln!("skipping CLI python sample: no interpreter");
    }
    found
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
fn read_counter(path: &Path) -> u64 {
    let bytes = std::fs::read(path).unwrap_or_default();
    if bytes.len() < 8 {
        0
    } else {
        u64::from_le_bytes(bytes[..8].try_into().unwrap())
    }
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
fn record(pid: u32, output_dir: &Path) -> std::process::Output {
    let pid_arg = pid.to_string();
    let anyprof = env!("CARGO_BIN_EXE_anyprof");
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("sudo");
        command.args(["-n", anyprof]);
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new(anyprof);
    command.args(["rec", "-p", pid_arg.as_str(), "-d", "1s", "-F", "100", "-o"]);
    command.arg(output_dir);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().expect("run anyprof")
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
fn parse_wrote(stderr: &str) -> (u64, std::path::PathBuf) {
    let line = stderr
        .lines()
        .find(|line| line.contains("wrote ") && line.contains(" samples to "))
        .unwrap_or_else(|| panic!("missing wrote line:\n{stderr}"));
    let rest = line
        .split_once("wrote ")
        .unwrap_or_else(|| panic!("wrote line: {line}"))
        .1;
    let (count, after) = rest
        .split_once(" samples to ")
        .unwrap_or_else(|| panic!("wrote line: {line}"));
    let path = after
        .strip_suffix(" (speedscope)")
        .unwrap_or_else(|| panic!("expected speedscope in: {line}"))
        .trim();
    (
        count.trim().parse().expect("sample count"),
        std::path::PathBuf::from(path),
    )
}

#[cfg(all(
    not(target_os = "macos"),
    any(target_os = "linux", windows),
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        not(target_os = "linux")
    ),
))]
fn live_lock() -> LiveLock {
    let path = std::env::temp_dir().join("anyprof-native-live.lock");
    let start = std::time::Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return LiveLock { path },
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if start.elapsed() > std::time::Duration::from_secs(180) {
                    panic!("timed out waiting for {path:?}");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(err) => panic!("lock {path:?}: {err}"),
        }
    }
}

#[cfg(all(
    not(target_os = "macos"),
    any(target_os = "linux", windows),
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        not(target_os = "linux")
    ),
))]
struct LiveLock {
    path: std::path::PathBuf,
}

#[cfg(all(
    not(target_os = "macos"),
    any(target_os = "linux", windows),
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        not(target_os = "linux")
    ),
))]
impl Drop for LiveLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
