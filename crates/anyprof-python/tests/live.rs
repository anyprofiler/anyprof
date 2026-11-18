//! Live samples of a CPython process on the platforms that have a native sampler.
//!
//! The fixture is a script. Names come from the interpreter frame chain, not
//! from a native unwind of `_PyEval_EvalFrameDefault`.
//!
//! `ANYPROF_REQUIRE_NATIVE=1` (set by CI on the sampling jobs) turns a skip
//! into a failure. CI installs CPython 3.12 and 3.13 and passes them as
//! `ANYPROF_PYTHON_312` and `ANYPROF_PYTHON_313`. The MSRV job does not set
//! the variable.

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
    use anyprof_python::PythonBackend;

    const BUSY: &str = r#"
import sys
path = sys.argv[1]
def caller():
    with open(path, "wb") as handle:
        handle.write((1).to_bytes(8, "little"))
    spin()
def spin():
    x = 0
    while True:
        x = x + 1
caller()
"#;

    const IDLE: &str = r#"
import time
def parked():
    while True:
        time.sleep(3600)
parked()
"#;

    const TWO: &str = r#"
import threading
def spin_a():
    x = 0
    while True:
        x += 1
def spin_b():
    x = 0
    while True:
        x += 1
threading.Thread(target=spin_a, daemon=True).start()
threading.Thread(target=spin_b, daemon=True).start()
x = 0
while True:
    x += 1
"#;

    #[cfg(target_os = "macos")]
    #[test]
    fn samples_a_busy_python_program() {
        with_root("supported::samples_a_busy_python_program", sample_busy);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_sleeping_thread_is_not_an_on_cpu_sample() {
        with_root(
            "supported::a_sleeping_thread_is_not_an_on_cpu_sample",
            sample_idle,
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn both_spinning_threads_are_sampled() {
        with_root(
            "supported::both_spinning_threads_are_sampled",
            sample_threads,
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn samples_a_busy_python_program() {
        let _lock = LiveLock::acquire();
        sample_busy();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_sleeping_thread_is_not_an_on_cpu_sample() {
        let _lock = LiveLock::acquire();
        sample_idle();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn both_spinning_threads_are_sampled() {
        let _lock = LiveLock::acquire();
        sample_threads();
    }

    fn sample_busy() {
        for python in interpreters() {
            let dir = fixture_dir("busy");
            let script = dir.join("spin.py");
            std::fs::write(&script, BUSY).unwrap();
            let counter = dir.join("counter");
            std::fs::write(&counter, [0u8; 8]).unwrap();
            let mut child = Command::new(&python)
                .arg(&script)
                .arg(&counter)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap_or_else(|err| panic!("spawn {}: {err}", python.display()));
            let pid = child.id();
            if !wait_for_counter(&counter) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            assert!(
                PythonBackend.detect(pid),
                "{} must be detected as CPython",
                python.display()
            );
            let started = PythonBackend.start(
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
            let still_running = child.try_wait().ok().flatten().is_none();
            let _ = child.kill();
            let _ = child.wait();
            let profile = session.stop().expect("stop");
            assert!(
                still_running,
                "{} exited during the profile",
                python.display()
            );
            assert_spin_profile(&profile, pid, &script, &python);
            assert_exports(&profile);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn sample_idle() {
        for python in interpreters() {
            let dir = fixture_dir("idle");
            let script = dir.join("idle.py");
            std::fs::write(&script, IDLE).unwrap();
            let mut child = Command::new(&python)
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap_or_else(|err| panic!("spawn {}: {err}", python.display()));
            let pid = child.id();
            std::thread::sleep(Duration::from_millis(200));
            let started = PythonBackend.start(
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
            let parked = weight_named(&profile, "parked");
            let total = profile.total_weight();
            assert!(
                parked == 0,
                "a thread blocked in time.sleep is not on CPU (parked weight {parked}, {})\n{}",
                python.display(),
                explain(&profile)
            );
            assert!(
                total <= 15,
                "an idle Python process should not look like a spin loop ({total} samples, {})\n{}",
                python.display(),
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
    }

    fn sample_threads() {
        for python in interpreters() {
            let dir = fixture_dir("threads");
            let script = dir.join("two.py");
            std::fs::write(&script, TWO).unwrap();
            let mut child = Command::new(&python)
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap_or_else(|err| panic!("spawn {}: {err}", python.display()));
            let pid = child.id();
            std::thread::sleep(Duration::from_millis(100));
            let started = PythonBackend.start(
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
            let a = weight_named(&profile, "spin_a");
            let b = weight_named(&profile, "spin_b");
            assert!(
                alive && a > 0 && b > 0,
                "both spinning threads should be sampled (spin_a {a}, spin_b {b}, alive {alive}, {})\n{}",
                python.display(),
                explain(&profile)
            );
            assert!(
                profile.stacks.iter().any(|stack| {
                    stack.frames.iter().any(|frame| {
                        frame.name.contains("spin_a") && frame.runtime == Runtime::Python
                    })
                }),
                "spin_a should be a Python frame\n{}",
                explain(&profile)
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn assert_spin_profile(
        profile: &anyprof_core::Profile,
        pid: u32,
        script: &Path,
        python: &Path,
    ) {
        let total = profile.total_weight();
        let spin_weight = weight_named(profile, "spin");
        assert!(
            (20..=1_000).contains(&total),
            "expected on the order of 100 samples at 100 Hz for 1s, got {total} ({})\n{}",
            python.display(),
            explain(profile)
        );
        assert!(
            spin_weight * 2 >= total,
            "expected spin on at least half of the samples ({spin_weight}/{total}, {})\n{}",
            python.display(),
            explain(profile)
        );
        let script_name = script
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("spin.py");
        assert!(
            profile.stacks.iter().any(|stack| {
                let names: Vec<_> = stack
                    .frames
                    .iter()
                    .map(|frame| frame.name.as_str())
                    .collect();
                names.iter().any(|name| name.contains("spin"))
                    && names.iter().any(|name| name.contains("caller"))
                    && stack.frames.iter().any(|frame| {
                        frame.runtime == Runtime::Python
                            && frame.name.contains("spin")
                            && frame.line.is_some_and(|line| line > 0)
                            && frame
                                .file
                                .as_deref()
                                .is_some_and(|file| file.ends_with(script_name))
                    })
            }),
            "expected a Python stack with spin and caller in {script_name} ({})\n{}",
            python.display(),
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
        let speedscope = anyprof_export::export(profile, Format::Speedscope).expect("export");
        let rendered = anyprof_export::import(&speedscope, Format::Speedscope).expect("import");
        assert!(
            rendered.stacks.iter().any(|stack| {
                stack.frames.iter().any(|frame| {
                    frame.name.contains("spin")
                        && frame.runtime == Runtime::Python
                        && frame.line.is_some()
                        && frame
                            .file
                            .as_deref()
                            .is_some_and(|file| file.ends_with("spin.py"))
                })
            }),
            "speedscope lost the Python file and line"
        );
        let collapsed = anyprof_export::export(profile, Format::Collapsed).unwrap();
        let text = String::from_utf8(collapsed).unwrap();
        assert!(
            text.contains("[python] spin") && text.contains("[python] caller"),
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
                    eprintln!("skipping live python sample: {message}");
                    return None;
                }
                panic!("python sampling failed: {message}");
            }
        }
    }

    fn interpreters() -> Vec<PathBuf> {
        let mut found = Vec::new();
        for key in ["ANYPROF_PYTHON_312", "ANYPROF_PYTHON_313", "ANYPROF_PYTHON"] {
            if let Ok(path) = std::env::var(key)
                && !path.is_empty()
            {
                push_unique(&mut found, PathBuf::from(path));
            }
        }
        if found.is_empty() {
            for name in ["python3.12", "python3.13", "python3.11", "python3"] {
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
                push_unique(&mut found, PathBuf::from(path));
            }
        }
        found.retain(|path| supported_version(path));
        if found.is_empty() {
            if required() {
                panic!("no CPython 3.11, 3.12, or 3.13 interpreter was found");
            }
            eprintln!("skipping live python sample: no supported interpreter");
        }
        found
    }

    fn push_unique(found: &mut Vec<PathBuf>, path: PathBuf) {
        if !found.iter().any(|have| have == &path) {
            found.push(path);
        }
    }

    fn supported_version(path: &Path) -> bool {
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
        let major = parts.next();
        let minor = parts.next();
        matches!(
            (major, minor),
            (Some("3"), Some("11") | Some("12") | Some("13"))
        )
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
            panic!("the Python fixture did not increment its counter");
        }
        eprintln!("skipping live python sample: fixture counter stayed at 0");
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
            "anyprof-python-{name}-{}-{}",
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

    fn required() -> bool {
        std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1")
    }

    #[cfg(target_os = "macos")]
    fn with_root(test_name: &str, body: fn()) {
        if std::env::var_os("ANYPROF_PYTHON_WORKER").is_none() && unsafe { geteuid() } != 0 {
            relaunch_under_sudo(test_name);
            return;
        }
        if unsafe { geteuid() } != 0 {
            panic!("the python live sampler was not running as root");
        }
        body();
    }

    #[cfg(target_os = "macos")]
    fn relaunch_under_sudo(test_name: &str) {
        let exe = std::env::current_exe().expect("test executable");
        let mut command = Command::new("sudo");
        command.args(["-n", "env", "ANYPROF_PYTHON_WORKER=1"]);
        for key in ["ANYPROF_PYTHON_312", "ANYPROF_PYTHON_313", "ANYPROF_PYTHON"] {
            if let Ok(value) = std::env::var(key)
                && !value.is_empty()
            {
                command.arg(format!("{key}={value}"));
            }
        }
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
            eprintln!("skipping live python sample: sudo is not available\n{stderr}");
            return;
        }
        panic!(
            "live python sample failed (status {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
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
}

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
)))]
#[test]
fn live_python_sampler_is_not_executed_on_this_platform() {
    assert!(!anyprof_python::sampler_implemented());
}
