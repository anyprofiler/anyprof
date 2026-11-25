//! Live samples of a Rust process on the platforms that have a native sampler.
//!
//! The fixture is compiled with `-C force-frame-pointers=yes`. rustc 1.99 does
//! not keep frame pointers at `-O0` or `-O2` unless that flag is set, so macOS
//! and Linux would otherwise symbolize the leaf and lose the caller. Windows
//! x64 walks `.pdata` unwind data, which rustc emits without the flag; the
//! flag is still set so the fixture is the same everywhere.
//!
//! `ANYPROF_REQUIRE_NATIVE=1` (set by CI on the sampling jobs) turns a skip
//! into a failure. Linux musl is not a separate live run.

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
mod supported {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyprof_core::{Backend, Format, Runtime};
    use anyprof_rust::RustBackend;

    #[cfg(target_os = "macos")]
    #[test]
    fn samples_a_busy_rust_program() {
        with_root("supported::samples_a_busy_rust_program", sample_busy);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_idle_rust_process_does_not_invent_on_cpu_samples() {
        with_root(
            "supported::an_idle_rust_process_does_not_invent_on_cpu_samples",
            sample_idle,
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn both_running_rust_threads_are_sampled() {
        with_root(
            "supported::both_running_rust_threads_are_sampled",
            sample_threads,
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn samples_a_busy_rust_program() {
        let _lock = LiveLock::acquire();
        sample_busy();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn an_idle_rust_process_does_not_invent_on_cpu_samples() {
        let _lock = LiveLock::acquire();
        sample_idle();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn both_running_rust_threads_are_sampled() {
        let _lock = LiveLock::acquire();
        sample_threads();
    }

    fn sample_busy() {
        let dir = fixture_dir("busy");
        let binary = dir.join(exe("rust-busy"));
        if !compile(&dir.join("busy.rs"), &binary, BUSY_RS) {
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
            .expect("spawn busy rust");
        let pid = child.id();
        if !wait_for_counter(&counter) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let before = read_counter(&counter);
        let started = RustBackend.start(
            &anyprof_core::Target { pid },
            &anyprof_core::RecordOptions {
                duration: Some(Duration::from_secs(1)),
                sampling_interval: Some(Duration::from_millis(10)),
                ..anyprof_core::RecordOptions::default()
            },
        );
        let during = read_counter(&counter);
        let Some(session) = accept_or_skip(started) else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        std::thread::sleep(Duration::from_millis(30));
        let after = read_counter(&counter);
        let still_running = child.try_wait().ok().flatten().is_none();
        let _ = child.kill();
        let _ = child.wait();
        let profile = session.stop().expect("stop");
        assert!(
            still_running && during > before && after >= during,
            "the target must keep running (before {before}, during {during}, after {after}, alive {still_running})"
        );
        assert_spin_profile(
            &profile,
            pid,
            &binary,
            "anyprof_rust_spin",
            "anyprof_rust_caller",
        );
        assert_exports(&profile);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_idle() {
        let dir = fixture_dir("idle");
        let binary = dir.join(exe("rust-idle"));
        if !compile(&dir.join("idle.rs"), &binary, IDLE_RS) {
            return;
        }
        let mut child = Command::new(&binary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn idle rust");
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(100));
        let started = RustBackend.start(
            &anyprof_core::Target { pid },
            &anyprof_core::RecordOptions {
                duration: Some(Duration::from_millis(500)),
                sampling_interval: Some(Duration::from_millis(10)),
                ..anyprof_core::RecordOptions::default()
            },
        );
        let Some(session) = accept_or_skip(started) else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let alive = child.try_wait().ok().flatten().is_none();
        let _ = child.kill();
        let _ = child.wait();
        let profile = session.stop().expect("stop");
        let total = profile.total_weight();
        assert!(
            total <= 2,
            "an idle Rust process must not be recorded as on-CPU (got {total} over {:?})\n{}",
            profile.duration,
            explain(&profile)
        );
        assert!(
            profile.duration >= Duration::from_millis(300),
            "the idle recording ended early: {:?}",
            profile.duration
        );
        assert!(alive, "the idle process exited during the profile");
        assert!(
            profile
                .stacks
                .iter()
                .flat_map(|stack| stack.frames.iter())
                .all(|frame| frame.runtime != Runtime::Kernel),
            "-k was not set"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_threads() {
        let dir = fixture_dir("threads");
        let binary = dir.join(exe("rust-threads"));
        if !compile(&dir.join("threads.rs"), &binary, THREADS_RS) {
            return;
        }
        let mut child = Command::new(&binary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn rust threads");
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(50));
        let started = RustBackend.start(
            &anyprof_core::Target { pid },
            &anyprof_core::RecordOptions {
                duration: Some(Duration::from_secs(1)),
                sampling_interval: Some(Duration::from_millis(10)),
                ..anyprof_core::RecordOptions::default()
            },
        );
        let Some(session) = accept_or_skip(started) else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let alive = child.try_wait().ok().flatten().is_none();
        let _ = child.kill();
        let _ = child.wait();
        let profile = session.stop().expect("stop");
        let a = weight_named(&profile, "anyprof_rust_spin_a");
        let b = weight_named(&profile, "anyprof_rust_spin_b");
        assert!(
            alive && a > 0 && b > 0,
            "both running threads should be sampled (spin_a {a}, spin_b {b}, alive {alive})\n{}",
            explain(&profile)
        );
        assert!(
            profile.stacks.iter().any(|stack| {
                stack.frames.iter().any(|frame| {
                    frame.name.contains("anyprof_rust_spin_a") && frame.runtime == Runtime::Rust
                })
            }),
            "spin_a should be a Rust frame\n{}",
            explain(&profile)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn assert_spin_profile(
        profile: &anyprof_core::Profile,
        pid: u32,
        binary: &Path,
        spin: &str,
        caller: &str,
    ) {
        let total = profile.total_weight();
        let spin_weight = weight_named(profile, spin);
        assert!(
            (20..=1_000).contains(&total),
            "expected on the order of 100 samples at 100 Hz for 1s, got {total}\n{}",
            explain(profile)
        );
        assert!(
            spin_weight * 2 >= total,
            "expected {spin} on at least half of the samples ({spin_weight}/{total})\n{}",
            explain(profile)
        );
        assert!(
            profile.stacks.iter().any(|stack| {
                let names = stack
                    .frames
                    .iter()
                    .map(|frame| frame.name.as_str())
                    .collect::<Vec<_>>();
                names.iter().any(|name| name.contains(spin))
                    && names.iter().any(|name| name.contains(caller))
            }),
            "expected a stack with {spin} and {caller}\n{}",
            explain(profile)
        );
        let module = binary
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        assert!(
            profile.stacks.iter().any(|stack| {
                stack.frames.iter().any(|frame| {
                    frame.name.contains(spin)
                        && frame.runtime == Runtime::Rust
                        && frame.address.is_some()
                        && frame
                            .module
                            .as_deref()
                            .is_some_and(|name| name.contains("rust-busy") || name == module)
                })
            }),
            "spin frame should be Rust, addressed, and in {module}\n{}",
            explain(profile)
        );
        assert!(
            profile
                .stacks
                .iter()
                .flat_map(|stack| stack.frames.iter())
                .filter(|frame| frame.name.contains(spin))
                .all(|frame| !frame.name.contains("::h") && frame.name.contains("::")),
            "Rust names should be demangled paths without the legacy hash\n{}",
            explain(profile)
        );
        assert_eq!(profile.pid, pid);
        assert!(
            profile.duration >= Duration::from_millis(700)
                && profile.duration <= Duration::from_secs(3),
            "recording should last about 1s, was {:?}",
            profile.duration
        );
        assert_eq!(profile.period, Some(Duration::from_millis(10)));
        assert!(
            profile
                .stacks
                .iter()
                .flat_map(|stack| stack.frames.iter())
                .all(|frame| frame.runtime != Runtime::Kernel),
            "-k was not requested"
        );
    }

    fn assert_exports(profile: &anyprof_core::Profile) {
        for format in [Format::Collapsed, Format::Pprof, Format::Speedscope] {
            let bytes = anyprof_export::export(profile, format)
                .unwrap_or_else(|err| panic!("export {format:?}: {err}"));
            let rendered = anyprof_export::import(&bytes, format)
                .unwrap_or_else(|err| panic!("import {format:?}: {err}"));
            assert!(
                rendered.stacks.iter().any(|stack| {
                    stack.frames.iter().any(|frame| {
                        frame.name.contains("anyprof_rust_spin") && frame.runtime == Runtime::Rust
                    })
                }),
                "{format:?} lost the Rust spin frame\n{}",
                String::from_utf8_lossy(&bytes)
            );
        }
        let collapsed = anyprof_export::export(profile, Format::Collapsed).unwrap();
        let text = String::from_utf8(collapsed).unwrap();
        assert!(
            text.contains("[rust] ") && text.contains("anyprof_rust_fixture::anyprof_rust_spin"),
            "{text}"
        );
    }

    fn weight_named(profile: &anyprof_core::Profile, needle: &str) -> u64 {
        profile
            .stacks
            .iter()
            .filter(|stack| {
                stack
                    .frames
                    .iter()
                    .take(6)
                    .any(|frame| frame.name.contains(needle))
            })
            .map(|stack| stack.weight)
            .sum()
    }

    fn explain(profile: &anyprof_core::Profile) -> String {
        let mut lines = Vec::new();
        for stack in profile.stacks.iter().take(12) {
            let frames = stack
                .frames
                .iter()
                .take(8)
                .map(|frame| format!("{}:{}", frame.runtime, frame.name))
                .collect::<Vec<_>>()
                .join(" <- ");
            lines.push(format!("{} {frames}", stack.weight));
        }
        lines.join("\n")
    }

    fn accept_or_skip(
        started: Result<Box<dyn anyprof_core::Session>, anyprof_core::Error>,
    ) -> Option<Box<dyn anyprof_core::Session>> {
        match started {
            Ok(session) => Some(session),
            Err(err) => {
                let message = err.to_string();
                let blocked = message.contains("perf_event_open")
                    || message.contains("task_for_pid")
                    || message.contains("sudo");
                if blocked && !required() {
                    eprintln!("skipping live rust sample: {message}");
                    None
                } else {
                    panic!("rust profile failed: {message}");
                }
            }
        }
    }

    fn compile(source: &Path, binary: &Path, body: &str) -> bool {
        if let Some(parent) = source.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(source, body).expect("write fixture");
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
        match Command::new(&rustc)
            .args([
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
                "-o",
            ])
            .arg(binary)
            .arg(source)
            .output()
        {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                let message = format!(
                    "rustc failed to build the fixture\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if required() {
                    panic!("{message}");
                }
                eprintln!("skipping live rust sample: {message}");
                false
            }
            Err(err) => {
                let message = format!("failed to spawn {rustc}: {err}");
                if required() {
                    panic!("{message}");
                }
                eprintln!("skipping live rust sample: {message}");
                false
            }
        }
    }

    fn wait_for_counter(path: &Path) -> bool {
        let started = Instant::now();
        while read_counter(path) == 0 && started.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if read_counter(path) > 0 {
            return true;
        }
        if required() {
            panic!("the Rust fixture did not increment its counter");
        }
        eprintln!("skipping live rust sample: fixture counter stayed at 0");
        false
    }

    fn read_counter(path: &Path) -> u64 {
        let bytes = std::fs::read(path).unwrap_or_default();
        if bytes.len() < 8 {
            return 0;
        }
        u64::from_le_bytes(bytes[..8].try_into().unwrap())
    }

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anyprof-rust-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        dir
    }

    fn exe(stem: &str) -> String {
        if cfg!(windows) {
            format!("{stem}.exe")
        } else {
            stem.to_string()
        }
    }

    fn required() -> bool {
        std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1")
    }

    #[cfg(target_os = "macos")]
    fn with_root(test_name: &str, body: fn()) {
        if std::env::var_os("ANYPROF_RUST_WORKER").is_none() && unsafe { geteuid() } != 0 {
            relaunch_under_sudo(test_name);
            return;
        }
        if unsafe { geteuid() } != 0 {
            panic!("the rust live sampler was not running as root");
        }
        body();
    }

    #[cfg(target_os = "macos")]
    fn relaunch_under_sudo(test_name: &str) {
        let exe = std::env::current_exe().expect("test executable");
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
        let rustc_binding = format!("RUSTC={rustc}");
        let mut command = Command::new("sudo");
        command.args(["-n", "env", "ANYPROF_RUST_WORKER=1", rustc_binding.as_str()]);
        if required() {
            command.arg("ANYPROF_REQUIRE_NATIVE=1");
        }
        command.arg(exe).args([test_name, "--exact", "--nocapture"]);
        let output = command.output().expect("invoke sudo");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // `--exact` must be the libtest path (`supported::<fn>`). A bare
        // function name filters every test out and still exits 0.
        let ran = stdout.lines().any(|line| {
            let line = line.trim();
            line.starts_with("test ") && line.contains(test_name) && line.contains("... ok")
        });
        if output.status.success() && stdout.contains("running 1 test") && ran {
            return;
        }
        let sudo_missing = stderr.contains("a password is required")
            || stderr.contains("not permitted")
            || stderr.contains("No such file")
            || stderr.contains("command not found");
        if sudo_missing && !required() {
            eprintln!("skipping live rust sample: sudo is not available\n{stderr}");
            return;
        }
        panic!(
            "live rust sample failed (status {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        );
    }

    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    #[cfg(not(target_os = "macos"))]
    struct LiveLock {
        path: PathBuf,
    }

    #[cfg(not(target_os = "macos"))]
    impl LiveLock {
        fn acquire() -> Self {
            // Same file the native live tests use, so perf and dbghelp are
            // not exercised by two crates at once.
            let path = std::env::temp_dir().join("anyprof-native-live.lock");
            let started = Instant::now();
            loop {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(_) => return Self { path },
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
    }

    #[cfg(not(target_os = "macos"))]
    impl Drop for LiveLock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

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

    const IDLE_RS: &str = r#"
fn main() {
    loop {
        std::thread::park_timeout(std::time::Duration::from_secs(3600));
    }
}
"#;

    const THREADS_RS: &str = r#"
#[inline(never)]
fn anyprof_rust_spin_a() {
    let mut acc: u64 = 0;
    loop {
        acc = acc.wrapping_add(1);
        std::hint::black_box(acc);
    }
}

#[inline(never)]
fn anyprof_rust_spin_b() {
    let mut acc: u64 = 0;
    loop {
        acc = acc.wrapping_add(1);
        std::hint::black_box(acc);
    }
}

fn main() {
    let a = std::thread::spawn(anyprof_rust_spin_a);
    let b = std::thread::spawn(anyprof_rust_spin_b);
    let _ = a.join();
    let _ = b.join();
}
"#;
}

#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
)))]
#[test]
fn live_rust_sampler_is_not_executed_on_this_platform() {
    assert!(
        !anyprof_rust::sampler_implemented(),
        "this negative test must not be compiled into a sampling build"
    );
}
