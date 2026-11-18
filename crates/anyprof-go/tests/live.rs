//! Live samples of a Go process on the platforms that have a native sampler.
//!
//! The fixture is built with `CGO_ENABLED=0` and `-ldflags=-s -w`, so the
//! ELF/Mach-O/PE symbol table is gone and names have to come from pclntab.
//! Go leaf functions do not keep a frame pointer; the caller is recovered
//! from the pcsp table.
//!
//! `ANYPROF_REQUIRE_NATIVE=1` (set by CI on the sampling jobs) turns a skip
//! into a failure. CI installs Go 1.22. The MSRV job does not set the
//! variable and does not need the toolchain.

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
mod supported {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyprof_core::{Backend, Format, Runtime};
    use anyprof_go::GoBackend;

    #[cfg(target_os = "macos")]
    #[test]
    fn samples_a_busy_go_program() {
        with_root("supported::samples_a_busy_go_program", sample_busy);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_parked_goroutine_is_not_an_on_cpu_sample() {
        with_root(
            "supported::a_parked_goroutine_is_not_an_on_cpu_sample",
            sample_idle,
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn both_running_goroutines_are_sampled() {
        with_root(
            "supported::both_running_goroutines_are_sampled",
            sample_goroutines,
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn samples_a_busy_go_program() {
        let _lock = LiveLock::acquire();
        sample_busy();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_parked_goroutine_is_not_an_on_cpu_sample() {
        let _lock = LiveLock::acquire();
        sample_idle();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn both_running_goroutines_are_sampled() {
        let _lock = LiveLock::acquire();
        sample_goroutines();
    }

    fn sample_busy() {
        let dir = fixture_dir("busy");
        let binary = dir.join(exe("go-busy"));
        if !compile(&dir, &binary, BUSY_GO) {
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
        if !wait_for_counter(&counter) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert!(
            GoBackend.detect(pid),
            "a running Go process must be detected from its build info"
        );
        let before = read_counter(&counter);
        let started = GoBackend.start(
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
        assert_spin_profile(&profile, pid, &binary);
        assert_exports(&profile);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_idle() {
        let dir = fixture_dir("idle");
        let binary = dir.join(exe("go-idle"));
        if !compile(&dir, &binary, IDLE_GO) {
            return;
        }
        let mut child = Command::new(&binary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn idle go");
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(200));
        let started = GoBackend.start(
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
        let parked = weight_named(&profile, "main.parked");
        let total = profile.total_weight();
        assert!(
            parked == 0,
            "a goroutine blocked in time.Sleep is not on CPU (main.parked weight {parked})\n{}",
            explain(&profile)
        );
        assert!(
            total <= 15,
            "an idle Go process should not look like a spin loop ({total} samples)\n{}",
            explain(&profile)
        );
        assert!(
            profile.duration >= Duration::from_millis(300),
            "the idle recording ended early: {:?}",
            profile.duration
        );
        assert!(alive, "the idle process exited during the profile");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_goroutines() {
        let dir = fixture_dir("goroutines");
        let binary = dir.join(exe("go-two"));
        if !compile(&dir, &binary, TWO_GO) {
            return;
        }
        let mut child = Command::new(&binary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn go goroutines");
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(50));
        let started = GoBackend.start(
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
        let a = weight_named(&profile, "main.spinA");
        let b = weight_named(&profile, "main.spinB");
        assert!(
            alive && a > 0 && b > 0,
            "both running goroutines should be sampled (spinA {a}, spinB {b}, alive {alive})\n{}",
            explain(&profile)
        );
        assert!(
            profile.stacks.iter().any(|stack| {
                stack
                    .frames
                    .iter()
                    .any(|frame| frame.name.contains("main.spinA") && frame.runtime == Runtime::Go)
            }),
            "spinA should be a Go frame\n{}",
            explain(&profile)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn assert_spin_profile(profile: &anyprof_core::Profile, pid: u32, binary: &Path) {
        let total = profile.total_weight();
        let spin_weight = weight_named(profile, "main.spin");
        assert!(
            (20..=1_000).contains(&total),
            "expected on the order of 100 samples at 100 Hz for 1s, got {total}\n{}",
            explain(profile)
        );
        assert!(
            spin_weight * 2 >= total,
            "expected main.spin on at least half of the samples ({spin_weight}/{total})\n{}",
            explain(profile)
        );
        assert!(
            profile.stacks.iter().any(|stack| {
                let names = stack
                    .frames
                    .iter()
                    .map(|frame| frame.name.as_str())
                    .collect::<Vec<_>>();
                names.iter().any(|name| name.contains("main.spin"))
                    && names.iter().any(|name| name.contains("main.caller"))
            }),
            "expected a stack with main.spin and main.caller\n{}",
            explain(profile)
        );
        let module = binary
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        assert!(
            profile.stacks.iter().any(|stack| {
                stack.frames.iter().any(|frame| {
                    frame.name.contains("main.spin")
                        && frame.runtime == Runtime::Go
                        && frame.address.is_some()
                        && frame.line.is_some()
                        && frame
                            .file
                            .as_deref()
                            .is_some_and(|file| file.ends_with(".go"))
                        && frame
                            .module
                            .as_deref()
                            .is_some_and(|name| name.contains("go-busy") || name == module)
                })
            }),
            "spin frame should be Go, addressed, and in {module}\n{}",
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
                        frame.name.contains("main.spin") && frame.runtime == Runtime::Go
                    })
                }),
                "{format:?} lost the Go spin frame"
            );
        }
        let collapsed = anyprof_export::export(profile, Format::Collapsed).unwrap();
        let text = String::from_utf8(collapsed).unwrap();
        assert!(
            text.contains("[go] main.spin") && text.contains("[go] main.caller"),
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
                    .take(8)
                    .any(|frame| frame.name.contains(needle))
            })
            .map(|stack| stack.weight)
            .sum()
    }

    fn explain(profile: &anyprof_core::Profile) -> String {
        let mut lines = Vec::new();
        for stack in profile.stacks.iter().take(12) {
            let names = stack
                .frames
                .iter()
                .map(|frame| {
                    if frame.name.is_empty() {
                        format!("0x{:x}", frame.address.unwrap_or(0))
                    } else {
                        format!("{}:{}", frame.runtime, frame.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(";");
            lines.push(format!("{} {}", stack.weight, names));
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
                let denied = message.contains("perf_event_open")
                    || message.contains("task_for_pid")
                    || message.contains("not permitted")
                    || message.contains("Operation not permitted");
                if denied && !required() {
                    eprintln!("skipping live go sample: {message}");
                    return None;
                }
                panic!("go sampling failed: {message}");
            }
        }
    }

    fn compile(dir: &Path, binary: &Path, source: &str) -> bool {
        let file = dir.join("main.go");
        std::fs::write(&file, source).expect("write fixture");
        let go = go_bin();
        let init = Command::new(&go)
            .args(["mod", "init", "anyprof.test"])
            .current_dir(dir)
            .env("GO111MODULE", "on")
            .output();
        let init = match init {
            Ok(output) => output,
            Err(err) => return compile_missing(&go, &err.to_string()),
        };
        if !init.status.success()
            && !String::from_utf8_lossy(&init.stderr).contains("already exists")
        {
            return compile_missing(&go, &String::from_utf8_lossy(&init.stderr));
        }
        match Command::new(&go)
            .args(["build", "-ldflags=-s -w", "-o"])
            .arg(binary)
            .arg(dir)
            .current_dir(dir)
            .env("CGO_ENABLED", "0")
            .env("GO111MODULE", "on")
            .output()
        {
            Ok(output) if output.status.success() => true,
            Ok(output) => compile_missing(&go, &String::from_utf8_lossy(&output.stderr)),
            Err(err) => compile_missing(&go, &err.to_string()),
        }
    }

    fn compile_missing(go: &str, message: &str) -> bool {
        if required() {
            panic!("go ({go}) failed to build the fixture: {message}");
        }
        eprintln!("skipping live go sample: {go}: {message}");
        false
    }

    fn go_bin() -> String {
        if let Ok(path) = std::env::var("GO_BIN")
            && !path.is_empty()
        {
            return path;
        }
        "go".to_string()
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
            panic!("the Go fixture did not increment its counter");
        }
        eprintln!("skipping live go sample: fixture counter stayed at 0");
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
            "anyprof-go-{name}-{}-{}",
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
        if std::env::var_os("ANYPROF_GO_WORKER").is_none() && unsafe { geteuid() } != 0 {
            relaunch_under_sudo(test_name);
            return;
        }
        if unsafe { geteuid() } != 0 {
            panic!("the go live sampler was not running as root");
        }
        body();
    }

    #[cfg(target_os = "macos")]
    fn relaunch_under_sudo(test_name: &str) {
        let exe = std::env::current_exe().expect("test executable");
        let go = resolve_go();
        let go_binding = format!("GO_BIN={go}");
        let mut command = Command::new("sudo");
        command.args(["-n", "env", "ANYPROF_GO_WORKER=1", go_binding.as_str()]);
        if required() {
            command.arg("ANYPROF_REQUIRE_NATIVE=1");
        }
        command.arg(exe).args([test_name, "--exact", "--nocapture"]);
        let output = command.output().expect("invoke sudo");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
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
            eprintln!("skipping live go sample: sudo is not available\n{stderr}");
            return;
        }
        panic!(
            "live go sample failed (status {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        );
    }

    #[cfg(target_os = "macos")]
    fn resolve_go() -> String {
        if let Ok(path) = std::env::var("GO_BIN")
            && !path.is_empty()
        {
            return path;
        }
        let output = Command::new("which").arg("go").output().expect("which go");
        let text = String::from_utf8_lossy(&output.stdout);
        let path = text.lines().next().unwrap_or("go").trim();
        if path.is_empty() {
            "go".to_string()
        } else {
            path.to_string()
        }
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
            let path = std::env::temp_dir().join("anyprof-native-live.lock");
            let start = Instant::now();
            loop {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(_) => return Self { path },
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
    }

    #[cfg(not(target_os = "macos"))]
    impl Drop for LiveLock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    const BUSY_GO: &str = r#"
package main

import (
	"encoding/binary"
	"os"
)

func main() {
	f, err := os.OpenFile(os.Args[1], os.O_RDWR, 0)
	if err != nil {
		panic(err)
	}
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
"#;

    const IDLE_GO: &str = r#"
package main

import "time"

func main() { parked() }

//go:noinline
func parked() {
	for {
		time.Sleep(time.Hour)
	}
}
"#;

    const TWO_GO: &str = r#"
package main

import "runtime"

var sink uint64

func main() {
	runtime.GOMAXPROCS(2)
	go spinA()
	spinB()
}

//go:noinline
func spinA() {
	runtime.LockOSThread()
	var n uint64
	for {
		n++
		sink = n
	}
}

//go:noinline
func spinB() {
	runtime.LockOSThread()
	var n uint64
	for {
		n++
		sink = n
	}
}
"#;
}

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
)))]
#[test]
fn live_go_sampler_is_not_executed_on_this_platform() {
    assert!(!anyprof_go::sampler_implemented());
}
