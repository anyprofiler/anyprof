//! CLI coverage for the Go backend.
//!
//! Auto-detection (no `-b`) must select `go` for a Go pid and write
//! `anyprof-go-<pid>-*.pb.gz`. Collapsed stacks prefix pclntab names with
//! `[go]`.

use std::process::Command;

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
use std::path::Path;
#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
use std::process::Stdio;
#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
use std::time::Duration;

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
)))]
#[test]
fn go_record_is_not_implemented_on_this_platform() {
    let pid = std::process::id().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["rec", "-b", "go", "-p", &pid, "-d", "200ms"])
        .output()
        .expect("run anyprof");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "go sampling must fail closed on this platform\n{stderr}"
    );
    assert!(
        stderr.contains("not implemented yet"),
        "expected the not-implemented error, got:\n{stderr}"
    );
    assert!(
        stderr.contains("go backend"),
        "the refusal should name the go backend:\n{stderr}"
    );
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
#[test]
fn record_auto_detects_a_busy_go_program() {
    #[cfg(not(target_os = "macos"))]
    let _lock = live_lock();
    let dir = std::env::temp_dir().join(format!(
        "anyprof-go-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // macOS temp is /var/folders, and /var is a symlink to /private/var.
    // Go's module check compares the lexical package path with the module
    // root it sees after that canonicalization, so the directory used for
    // `go build` is the canonical one.
    let dir = canonical_module_dir(&dir);
    let binary = dir.join(if cfg!(windows) {
        "go-busy.exe"
    } else {
        "go-busy"
    });
    if !compile(&dir, &binary) {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let counter = dir.join("counter");
    std::fs::write(&counter, [0u8; 8]).unwrap();
    let mut child = Command::new(&binary)
        .arg(&counter)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy go");
    let pid = child.id();
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        let bytes = std::fs::read(&counter).unwrap_or_default();
        let value = if bytes.len() >= 8 {
            u64::from_le_bytes(bytes[..8].try_into().unwrap())
        } else {
            0
        };
        if value > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
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
        let denied = stderr.contains("perf_event_open")
            || stderr.contains("task_for_pid")
            || stderr.contains("a password is required")
            || stderr.contains("not permitted");
        if denied && std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() != Some("1") {
            eprintln!("skipping CLI go sample:\n{stderr}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!(
            "anyprof rec failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            recorded.status.code()
        );
    }
    assert!(
        stderr.contains("using backend 'go'"),
        "auto-detection should select go:\n{stderr}"
    );
    let (samples, path) = parse_wrote(&stderr);
    assert!(
        (20..=1_000).contains(&samples),
        "sample count {samples} is not a 1s/100Hz recording\n{stderr}"
    );
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with(&format!("anyprof-go-{pid}-")) && name.ends_with(".pb.gz"),
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
        folded.contains("[go] main.spin") && folded.contains("[go] main.caller"),
        "collapsed stacks:\n{folded}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn compile(dir: &Path, binary: &Path) -> bool {
    std::fs::write(
        dir.join("main.go"),
        r#"package main
import (
	"encoding/binary"
	"os"
)
func main() {
	f, err := os.OpenFile(os.Args[1], os.O_RDWR, 0)
	if err != nil { panic(err) }
	caller(f)
}
//go:noinline
func caller(f *os.File) { spin(f) }
//go:noinline
func spin(f *os.File) {
	var n uint64
	var buf [8]byte
	for {
		n++
		if n%1000000 == 0 {
			binary.LittleEndian.PutUint64(buf[:], n)
			_, _ = f.WriteAt(buf[:], 0)
		}
	}
}
"#,
    )
    .unwrap();
    let go = std::env::var("GO_BIN").unwrap_or_else(|_| "go".to_string());
    let init = Command::new(&go)
        .args(["mod", "init", "anyprof.cli"])
        .current_dir(dir)
        .env("GO111MODULE", "on")
        .output();
    let Ok(init) = init else {
        return skip_or_panic("go is not installed");
    };
    if !init.status.success() {
        return skip_or_panic(&String::from_utf8_lossy(&init.stderr));
    }
    // `.` stays inside the module. An absolute temp path on macOS
    // canonicalizes through `/private` and Go rejects it as outside the module.
    let build = Command::new(&go)
        .args(["build", "-ldflags=-s -w", "-o"])
        .arg(binary)
        .arg(".")
        .current_dir(dir)
        .env("CGO_ENABLED", "0")
        .env("GO111MODULE", "on")
        .output();
    match build {
        Ok(output) if output.status.success() => true,
        Ok(output) => skip_or_panic(&String::from_utf8_lossy(&output.stderr)),
        Err(err) => skip_or_panic(&err.to_string()),
    }
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn canonical_module_dir(dir: &Path) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
    }
    #[cfg(not(unix))]
    {
        dir.to_path_buf()
    }
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn skip_or_panic(message: &str) -> bool {
    if std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1") {
        panic!("go fixture: {message}");
    }
    eprintln!("skipping CLI go sample: {message}");
    false
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
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
    all(target_os = "linux", target_arch = "x86_64"),
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
        .strip_suffix(" (pprof)")
        .unwrap_or_else(|| panic!("expected pprof in: {line}"))
        .trim();
    (
        count.trim().parse().expect("sample count"),
        std::path::PathBuf::from(path),
    )
}

#[cfg(all(
    not(target_os = "macos"),
    any(target_os = "linux", windows),
    any(target_arch = "x86_64", not(target_os = "linux")),
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
                if start.elapsed() > Duration::from_secs(180) {
                    panic!("timed out waiting for {path:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("lock {path:?}: {err}"),
        }
    }
}

#[cfg(all(
    not(target_os = "macos"),
    any(target_os = "linux", windows),
    any(target_arch = "x86_64", not(target_os = "linux")),
))]
struct LiveLock {
    path: std::path::PathBuf,
}

#[cfg(all(
    not(target_os = "macos"),
    any(target_os = "linux", windows),
    any(target_arch = "x86_64", not(target_os = "linux")),
))]
impl Drop for LiveLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
