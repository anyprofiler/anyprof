//! CLI coverage for the Rust backend.
//!
//! Auto-detection (no `-b`) must select `rust` for a Rust pid and write
//! `anyprof-rust-<pid>-*.pb.gz`. Collapsed stacks prefix demangled Rust
//! frames with `[rust]`. Platforms without a sampler refuse `-b rust`.

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
fn rust_record_is_not_implemented_on_this_platform() {
    let pid = std::process::id().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["rec", "-b", "rust", "-p", &pid, "-d", "200ms"])
        .output()
        .expect("run anyprof");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "rust sampling must fail closed on this platform\n{stderr}"
    );
    assert!(
        stderr.contains("not implemented yet"),
        "expected the not-implemented error, got:\n{stderr}"
    );
    assert!(
        stderr.contains("rust backend"),
        "the refusal should name the rust backend:\n{stderr}"
    );
    assert!(
        !stderr.contains("native backend is not implemented"),
        "the rust backend should not report itself as native:\n{stderr}"
    );
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
#[test]
fn record_auto_detects_a_busy_rust_program() {
    #[cfg(not(target_os = "macos"))]
    let _lock = live_lock();
    let dir = std::env::temp_dir().join(format!(
        "anyprof-rust-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("busy.rs");
    let binary = dir.join(if cfg!(windows) {
        "rust-busy.exe"
    } else {
        "rust-busy"
    });
    std::fs::write(&source, BUSY_RS).unwrap();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let mut rustc_cmd = Command::new(&rustc);
    rustc_cmd.args([
        "--edition",
        "2024",
        "--crate-name",
        "anyprof_rust_fixture",
        "-C",
        "opt-level=0",
        "-C",
        "force-frame-pointers=yes",
        "-C",
        "debuginfo=2",
    ]);
    #[cfg(target_os = "macos")]
    rustc_cmd.args(["-C", "split-debuginfo=packed"]);
    let compile = rustc_cmd
        .arg("-o")
        .arg(&binary)
        .arg(&source)
        .output()
        .unwrap_or_else(|err| panic!("spawn {rustc}: {err}"));
    assert!(
        compile.status.success(),
        "rustc\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let counter = dir.join("counter");
    std::fs::write(&counter, [0u8; 8]).unwrap();
    let mut child = Command::new(&binary)
        .arg(&counter)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy rust");
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
            eprintln!("skipping CLI rust sample:\n{stderr}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!(
            "anyprof rec failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            recorded.status.code()
        );
    }
    assert!(
        stderr.contains("using backend 'rust'"),
        "auto-detection should select rust:\n{stderr}"
    );
    assert!(
        stderr.contains("mixed-stacks"),
        "a Rust stack mixes Rust and native frames:\n{stderr}"
    );
    let (samples, path) = parse_wrote(&stderr);
    assert!(
        (20..=1_000).contains(&samples),
        "sample count {samples} is not a 1s/100Hz recording\n{stderr}"
    );
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with(&format!("anyprof-rust-{pid}-")) && name.ends_with(".pb.gz"),
        "unexpected auto name {name}"
    );
    assert!(path.starts_with(&output_dir), "{}", path.display());

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
        folded.contains("[rust] ")
            && folded.contains("anyprof_rust_fixture::anyprof_rust_spin")
            && folded.contains("anyprof_rust_caller"),
        "collapsed stacks:\n{folded}"
    );
    let speedscope = dir.join("cpu.speedscope.json");
    let convert_ss = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["conv", "-i"])
        .arg(&path)
        .arg("-o")
        .arg(&speedscope)
        .args(["-t", "speedscope"])
        .output()
        .expect("anyprof conv speedscope");
    assert!(
        convert_ss.status.success(),
        "conv to speedscope failed\n{}",
        String::from_utf8_lossy(&convert_ss.stderr)
    );
    let json = std::fs::read_to_string(&speedscope).unwrap();
    assert!(
        json.contains("busy.rs") && json.contains("\"line\":"),
        "speedscope missing Rust file:line metadata:\n{json}"
    );
    let _ = std::fs::remove_dir_all(&dir);
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
    // No `-b`: auto-detection has to choose rust over native.
    command.args(["rec", "-p", pid_arg.as_str(), "-d", "1s", "-F", "100", "-o"]);
    command.arg(output_dir);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_timeout(command, Duration::from_secs(30))
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
        .unwrap_or_else(|| panic!("no wrote line in:\n{stderr}"));
    let rest = line.split_once("wrote ").unwrap().1;
    let (count, after) = rest.split_once(" samples to ").unwrap();
    let path = after
        .strip_suffix(" (pprof)")
        .unwrap_or_else(|| panic!("expected pprof in: {line}"))
        .trim();
    (
        count.trim().parse().expect("sample count"),
        std::path::PathBuf::from(path),
    )
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn run_with_timeout(mut command: Command, limit: Duration) -> std::process::Output {
    let mut child = command.spawn().expect("spawn anyprof");
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_end(&mut stdout);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_end(&mut stderr);
                }
                return std::process::Output {
                    status,
                    stdout,
                    stderr,
                };
            }
            Ok(None) if started.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("anyprof did not finish within {}s", limit.as_secs());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => panic!("wait anyprof: {err}"),
        }
    }
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
struct LiveLock {
    path: std::path::PathBuf,
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn live_lock() -> LiveLock {
    let path = std::env::temp_dir().join("anyprof-native-live.lock");
    let started = std::time::Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return LiveLock { path },
            Err(_) => {
                let stale = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(180));
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if started.elapsed() > Duration::from_secs(240) {
                    panic!("timed out waiting for the native live-test lock");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
impl Drop for LiveLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
const BUSY_RS: &str = r#"
use std::io::{Seek, SeekFrom, Write};

#[inline(never)]
fn anyprof_rust_spin(slot: &mut std::fs::File) {
    let mut acc: u64 = 0;
    loop {
        acc = acc.wrapping_add(1);
        if acc % 1_000_000 == 0 {
            let _ = slot.seek(SeekFrom::Start(0));
            let _ = slot.write_all(&acc.to_le_bytes());
        }
        std::hint::black_box(acc);
    }
}

#[inline(never)]
fn anyprof_rust_caller(slot: &mut std::fs::File) {
    anyprof_rust_spin(slot);
}

fn main() {
    let mut slot = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(std::env::args().nth(1).expect("counter path"))
        .expect("open counter");
    anyprof_rust_caller(&mut slot);
}
"#;
