//! CLI integration coverage for `anyprof rec -- <COMMAND>` and `anyprof rec -n <NAME>`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static LIVE_LOCK: Mutex<()> = Mutex::new(());

fn live_lock() -> MutexGuard<'static, ()> {
    LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "anyprof-spawn-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn required() -> bool {
    std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1")
}

#[allow(dead_code)]
fn run_with_timeout(mut command: Command, limit: Duration) -> std::process::Output {
    let mut child = command.spawn().expect("spawn command");
    let stdout = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut buf);
            buf
        })
    });
    let stderr = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut err, &mut buf);
            buf
        })
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("command timed out after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    std::process::Output {
        status,
        stdout: stdout.and_then(|h| h.join().ok()).unwrap_or_default(),
        stderr: stderr.and_then(|h| h.join().ok()).unwrap_or_default(),
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
const SHORT_RUST_SPIN: &str = r#"
use std::hint::black_box;
use std::time::{Duration, Instant};

#[inline(never)]
pub fn anyprof_spawn_spin(ms: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut acc = 1u64;
    while Instant::now() < deadline {
        for i in 0..4096u64 {
            acc = black_box(acc.wrapping_mul(6364136223846793005).wrapping_add(i));
        }
    }
    acc
}

fn main() {
    let mut args = std::env::args().skip(1);
    let code: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(400);
    let val = anyprof_spawn_spin(ms);
    if val == 0 {
        std::process::exit(99);
    }
    std::process::exit(code);
}
"#;

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
fn compile_rust_fixture(dir: &Path, stem: &str) -> PathBuf {
    let source = dir.join(format!("{stem}.rs"));
    let binary = dir.join(if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    });
    std::fs::write(&source, SHORT_RUST_SPIN).unwrap();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = Command::new(&rustc)
        .args([
            "--edition",
            "2024",
            "--crate-name",
            "anyprof_spawn_fixture",
            "-C",
            "opt-level=0",
            "-C",
            "force-frame-pointers=yes",
            "-C",
            "debuginfo=2",
            "-o",
        ])
        .arg(&binary)
        .arg(&source)
        .output()
        .unwrap_or_else(|err| panic!("spawn {rustc}: {err}"));
    assert!(
        out.status.success(),
        "rustc failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    binary
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
fn anyprof_cmd() -> Command {
    let anyprof = env!("CARGO_BIN_EXE_anyprof");
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("sudo");
        cmd.args(["-n", anyprof]);
        cmd
    }
    #[cfg(not(target_os = "macos"))]
    {
        Command::new(anyprof)
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
fn skip_if_unprivileged(stderr: &str) -> bool {
    let denied = stderr.contains("perf_event_open")
        || stderr.contains("task_for_pid")
        || stderr.contains("a password is required")
        || stderr.contains("not permitted");
    if denied && !required() {
        eprintln!("skipping spawn_live check:\n{stderr}");
        return true;
    }
    false
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
fn spawn_short_lived_fixture_captures_profile_and_propagates_exit_code() {
    let _guard = live_lock();
    let dir = scratch_dir("short");
    let binary = compile_rust_fixture(&dir, "short-spin");
    let out_pb = dir.join("out.pb.gz");

    let mut cmd = anyprof_cmd();
    cmd.args(["rec", "-F", "200", "-o"])
        .arg(&out_pb)
        .arg("--")
        .arg(&binary)
        .args(["0", "1200"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_with_timeout(cmd, Duration::from_secs(25));
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("sudo")
            .args(["-n", "chmod", "-R", "a+rX"])
            .arg(&dir)
            .status();
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if skip_if_unprivileged(&stderr) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!("spawn rec failed ({:?}):\n{stderr}", output.status.code());
    }
    assert!(out_pb.is_file(), "expected profile at {}", out_pb.display());
    let collapsed = dir.join("out.collapsed");
    let conv = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["conv", "-i"])
        .arg(&out_pb)
        .arg("-o")
        .arg(&collapsed)
        .args(["-t", "collapsed"])
        .output()
        .expect("conv");
    assert!(
        conv.status.success(),
        "{}",
        String::from_utf8_lossy(&conv.stderr)
    );
    let folded = std::fs::read_to_string(&collapsed).unwrap();
    assert!(
        folded.contains("anyprof_spawn_spin"),
        "expected anyprof_spawn_spin in spawned profile:\n{folded}"
    );

    // Non-zero exit code from the spawned child is propagated after saving the profile.
    let out_nonzero = dir.join("nonzero.pb.gz");
    let mut cmd2 = anyprof_cmd();
    cmd2.args(["rec", "-F", "200", "-o"])
        .arg(&out_nonzero)
        .arg("--")
        .arg(&binary)
        .args(["7", "900"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output2 = run_with_timeout(cmd2, Duration::from_secs(25));
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("sudo")
            .args(["-n", "chmod", "-R", "a+rX"])
            .arg(&dir)
            .status();
    }
    assert_eq!(
        output2.status.code(),
        Some(7),
        "expected child exit code 7 to propagate, stderr:\n{}",
        String::from_utf8_lossy(&output2.stderr)
    );
    assert!(
        out_nonzero.is_file(),
        "profile should still be written when child exits non-zero"
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
#[test]
fn name_flag_attaches_unique_target_and_rejects_ambiguous_names() {
    let _guard = live_lock();
    let dir = scratch_dir("byname");
    let unique_stem = format!("aptarget{}", std::process::id());
    let binary = compile_rust_fixture(&dir, &unique_stem);

    let mut child1 = Command::new(&binary)
        .args(["0", "15000"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child1");
    std::thread::sleep(Duration::from_millis(60));

    let out_pb = dir.join("named.pb.gz");
    let mut cmd = anyprof_cmd();
    cmd.args(["rec", "-n", &unique_stem, "-d", "350ms", "-F", "200", "-o"])
        .arg(&out_pb)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let res = run_with_timeout(cmd, Duration::from_secs(20));
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("sudo")
            .args(["-n", "chmod", "-R", "a+rX"])
            .arg(&dir)
            .status();
    }
    let stderr = String::from_utf8_lossy(&res.stderr);
    if !res.status.success() {
        let _ = child1.kill();
        let _ = child1.wait();
        if skip_if_unprivileged(&stderr) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!("--name rec failed:\n{stderr}");
    }
    assert!(out_pb.is_file());

    // Spawn a second process with the same binary name and verify ambiguity is refused.
    let mut child2 = Command::new(&binary)
        .args(["0", "15000"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child2");
    std::thread::sleep(Duration::from_millis(60));

    let dup = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["rec", "-n", &unique_stem, "-d", "200ms"])
        .output()
        .expect("run duplicate --name check");
    let _ = child1.kill();
    let _ = child1.wait();
    let _ = child2.kill();
    let _ = child2.wait();

    assert!(!dup.status.success());
    let dup_err = String::from_utf8_lossy(&dup.stderr);
    assert!(
        dup_err.contains("multiple processes") && dup_err.contains(&unique_stem),
        "expected ambiguity error, got:\n{dup_err}"
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
fn find_python() -> Option<PathBuf> {
    for key in [
        "ANYPROF_PYTHON_312",
        "ANYPROF_PYTHON_313",
        "ANYPROF_PYTHON_311",
        "ANYPROF_PYTHON",
    ] {
        if let Ok(val) = std::env::var(key) {
            let p = PathBuf::from(val.trim());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for candidate in [
        "python3.12",
        "python3.13",
        "python3.11",
        "python3",
        "python",
    ] {
        let out = Command::new(candidate)
            .args([
                "-c",
                "import sys; print(sys.executable if (3, 11) <= sys.version_info[:2] <= (3, 13) else '')",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        if let Ok(out) = out
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !text.is_empty() {
                return Some(PathBuf::from(text));
            }
        }
    }
    None
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
fn spawn_python_captures_interpreter_frames_as_parent() {
    let _guard = live_lock();
    let Some(python) = find_python() else {
        if required() {
            panic!("supported CPython 3.11..3.13 was not found");
        }
        eprintln!("skipping spawn_python test: no CPython 3.11..3.13 found");
        return;
    };

    let dir = scratch_dir("py");
    let pid_file = dir.join("child.pid");
    let script = dir.join("spin_child.py");
    std::fs::write(
        &script,
        r#"import os, sys
with open(sys.argv[1], "w") as f:
    f.write(str(os.getpid()))

def spawned_py_work():
    x = 0
    while True:
        x = (x + 1) ^ 3

def entry():
    spawned_py_work()

entry()
"#,
    )
    .unwrap();

    let out_json = dir.join("py.speedscope.json");
    let mut cmd = anyprof_cmd();
    cmd.args(["rec", "-d", "1s", "-F", "200", "-o"])
        .arg(&out_json)
        .arg("--")
        .arg(&python)
        .arg(&script)
        .arg(&pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_with_timeout(cmd, Duration::from_secs(45));
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("sudo")
            .args(["-n", "chmod", "-R", "a+rX"])
            .arg(&dir)
            .status();
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if skip_if_unprivileged(&stderr) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!(
            "anyprof rec -- python failed ({:?}):\n{stderr}",
            output.status.code()
        );
    }
    assert!(
        stderr.contains("using backend 'python'"),
        "expected python backend for spawned script:\n{stderr}"
    );
    if let Ok(raw_pid) = std::fs::read_to_string(&pid_file)
        && let Ok(child_pid) = raw_pid.trim().parse::<u32>()
    {
        let mut gone = !anyprof_core::process_exists(child_pid);
        for _ in 0..20 {
            if gone {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
            gone = !anyprof_core::process_exists(child_pid);
        }
        assert!(gone, "spawned child pid {child_pid} was left running");
    }
    let collapsed = dir.join("py.collapsed");
    let conv = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["conv", "-i"])
        .arg(&out_json)
        .arg("-o")
        .arg(&collapsed)
        .args(["-t", "collapsed"])
        .output()
        .expect("conv");
    assert!(conv.status.success());
    let folded = std::fs::read_to_string(&collapsed).unwrap();
    assert!(
        folded.contains("[python] spawned_py_work"),
        "expected [python] spawned_py_work in:\n{folded}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
