//! Live samples of a HotSpot process on the platforms that have a native sampler.
//!
//! The fixture is a Java class. Names come from the code cache and the
//! interpreter frame, not from a native unwind of `libjvm`.
//!
//! `ANYPROF_REQUIRE_NATIVE=1` (set by CI on the sampling jobs) turns a skip
//! into a failure. CI installs a JDK 21 and leaves `java` on `PATH`.
//! On Linux, an unrelated same-user process is refused while
//! `kernel.yama.ptrace_scope` is 1 and is sampled once that scope is 0.
//! The MSRV job does not set the variable and does not install a JDK.

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
mod supported {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;
    #[cfg(not(target_os = "macos"))]
    use std::time::Instant;

    use anyprof_core::{Backend, Format, Runtime};
    use anyprof_java::JavaBackend;

    const SOURCE: &str = r#"
public class AnyprofSpin {
  static volatile long sink;
  public static void caller() {
    while (true) {
      spin();
    }
  }
  public static void spin() {
    long y = sink;
    for (int i = 0; i < 64; i++) {
      y += i;
    }
    sink = y;
  }
  public static void spinA() {
    long y = 0;
    while (true) {
      y++;
      sink = y;
    }
  }
  public static void spinB() {
    long y = 0;
    while (true) {
      y++;
      sink = y;
    }
  }
  public static void main(String[] args) throws Exception {
    String mode = args.length > 0 ? args[0] : "busy";
    if (mode.equals("sleep")) {
      Thread.sleep(Long.MAX_VALUE);
      return;
    }
    if (mode.equals("two")) {
      Thread a = new Thread(AnyprofSpin::spinA, "a");
      Thread b = new Thread(AnyprofSpin::spinB, "b");
      a.setDaemon(true);
      b.setDaemon(true);
      a.start();
      b.start();
      Thread.sleep(Long.MAX_VALUE);
      return;
    }
    caller();
  }
}
"#;

    #[cfg(target_os = "macos")]
    #[test]
    fn samples_a_busy_java_program() {
        with_root("supported::samples_a_busy_java_program", || {
            sample_busy(false)
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn samples_an_interpreted_java_program() {
        with_root("supported::samples_an_interpreted_java_program", || {
            sample_busy(true)
        });
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
    fn samples_a_busy_java_program() {
        let _lock = LiveLock::acquire();
        sample_busy(false);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn samples_an_interpreted_java_program() {
        let _lock = LiveLock::acquire();
        sample_busy(true);
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

    /// A double-forked JVM is not a descendant of this test.
    /// `process_vm_readv` then follows `kernel.yama.ptrace_scope`.
    #[cfg(target_os = "linux")]
    #[test]
    fn unrelated_same_user_read_follows_ptrace_scope() {
        let _lock = LiveLock::acquire();
        let Some(java) = java_bin() else {
            return;
        };
        let Some(dir) = compile_fixture("orphan") else {
            return;
        };
        let scope = ptrace_scope();
        let root = euid() == 0;
        let refused = scope.is_some_and(|scope| scope >= 1) && !root;
        let python = python_bin();
        let Some(python) = python else {
            if required() {
                panic!("python3 is required to double-fork the Java fixture");
            }
            eprintln!("skipping unrelated Java sample: python3 was not found");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let script = r#"
import os, sys
java, classpath = sys.argv[1], sys.argv[2]
if os.fork() > 0:
    os._exit(0)
pid = os.fork()
if pid > 0:
    sys.stdout.write(str(pid) + "\n")
    sys.stdout.flush()
    os._exit(0)
os.execv(java, [java, "-XX:CompileCommand=dontinline,AnyprofSpin.spin", "-cp", classpath, "AnyprofSpin", "busy"])
"#;
        let mut child = Command::new(&python)
            .arg("-c")
            .arg(script)
            .arg(&java)
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|err| panic!("spawn python: {err}"));
        let launcher = child.id();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().expect("stdout")),
            &mut line,
        )
        .expect("grandchild pid");
        let _ = child.wait();
        let pid: u32 = line
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("grandchild pid from {line:?}"));
        std::thread::sleep(Duration::from_millis(800));
        let mut parent = ppid(pid);
        for _ in 0..50 {
            if parent != Some(std::process::id()) && parent != Some(launcher) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            parent = ppid(pid);
        }
        assert_ne!(
            parent,
            Some(std::process::id()),
            "still parented to the test"
        );
        let started = JavaBackend.start(
            &anyprof_core::Target { pid },
            &record(Duration::from_secs(1)),
        );
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        if refused {
            let message = match started {
                Err(err) => err.to_string(),
                Ok(_) => panic!(
                    "ptrace_scope {scope:?} must refuse process_vm_readv of unrelated java and must not record native frames"
                ),
            };
            assert!(message.starts_with("java:"), "{message}");
            assert!(message.contains("process_vm_readv"), "{message}");
            assert!(message.contains("ptrace_scope"), "{message}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let session = started.unwrap_or_else(|err| panic!("unrelated java sample failed: {err}"));
        let profile = session.stop().expect("stop");
        assert_spin_stack(&profile);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_busy(interpret: bool) {
        let Some(java) = java_bin() else {
            return;
        };
        let Some(dir) = compile_fixture(if interpret { "xint" } else { "busy" }) else {
            return;
        };
        let mut command = Command::new(&java);
        command.arg("-XX:CompileCommand=dontinline,AnyprofSpin.spin");
        if interpret {
            command.arg("-Xint");
        }
        command
            .arg("-cp")
            .arg(&dir)
            .arg("AnyprofSpin")
            .arg("busy")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .unwrap_or_else(|err| panic!("spawn {}: {err}", java.display()));
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(800));
        if child.try_wait().ok().flatten().is_some() {
            let _ = std::fs::remove_dir_all(&dir);
            if required() {
                panic!("the Java fixture exited before sampling");
            }
            eprintln!("skipping live java sample: the fixture exited");
            return;
        }
        assert!(
            JavaBackend.detect(pid),
            "{} must be detected as HotSpot",
            java.display()
        );
        let started = JavaBackend.start(
            &anyprof_core::Target { pid },
            &record(Duration::from_secs(1)),
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
        assert!(alive, "the Java process exited during the profile");
        assert_spin_stack(&profile);
        assert_exports(&profile);
        assert!(
            profile.duration >= Duration::from_millis(700)
                && profile.duration <= Duration::from_secs(4),
            "recording should last about 1s, was {:?}",
            profile.duration
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_idle() {
        let Some(java) = java_bin() else {
            return;
        };
        let Some(dir) = compile_fixture("idle") else {
            return;
        };
        let mut child = Command::new(&java)
            .arg("-cp")
            .arg(&dir)
            .arg("AnyprofSpin")
            .arg("sleep")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|err| panic!("spawn {}: {err}", java.display()));
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(400));
        let started = JavaBackend.start(
            &anyprof_core::Target { pid },
            &record(Duration::from_millis(500)),
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
        let fixture = weight_named(&profile, "AnyprofSpin.");
        assert!(
            fixture == 0,
            "a thread blocked in Thread.sleep is not the spinning method (weight {fixture})\n{}",
            explain(&profile)
        );
        assert!(alive, "the idle process exited during the profile");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_threads() {
        let Some(java) = java_bin() else {
            return;
        };
        let Some(dir) = compile_fixture("threads") else {
            return;
        };
        let mut child = Command::new(&java)
            .arg("-cp")
            .arg(&dir)
            .arg("AnyprofSpin")
            .arg("two")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|err| panic!("spawn {}: {err}", java.display()));
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(800));
        let started = JavaBackend.start(
            &anyprof_core::Target { pid },
            &record(Duration::from_secs(2)),
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
        let a = weight_named(&profile, "AnyprofSpin.spinA");
        let b = weight_named(&profile, "AnyprofSpin.spinB");
        assert!(
            alive && a > 0 && b > 0,
            "both spinning threads should be sampled (spinA {a}, spinB {b}, alive {alive})\n{}",
            explain(&profile)
        );
        assert!(
            profile.stacks.iter().any(|stack| {
                stack.frames.iter().any(|frame| {
                    frame.name == "AnyprofSpin.spinA" && frame.runtime == Runtime::Java
                })
            }),
            "spinA must be a Java frame\n{}",
            explain(&profile)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn record(duration: Duration) -> anyprof_core::RecordOptions {
        anyprof_core::RecordOptions {
            duration: Some(duration),
            sampling_interval: Some(Duration::from_millis(10)),
            ..anyprof_core::RecordOptions::default()
        }
    }

    fn assert_spin_stack(profile: &anyprof_core::Profile) {
        let found = profile.stacks.iter().any(|stack| {
            let spin = stack
                .frames
                .iter()
                .position(|frame| frame.name == "AnyprofSpin.spin");
            let caller = stack
                .frames
                .iter()
                .position(|frame| frame.name == "AnyprofSpin.caller");
            match (spin, caller) {
                (Some(spin), Some(caller)) => {
                    spin < caller
                        && stack.frames[spin].runtime == Runtime::Java
                        && stack.frames[caller].runtime == Runtime::Java
                }
                _ => false,
            }
        });
        assert!(
            found,
            "a stack should be leaf-first AnyprofSpin.spin then AnyprofSpin.caller\n{}",
            explain(profile)
        );
        assert!(
            profile
                .stacks
                .iter()
                .flat_map(|stack| stack.frames.iter())
                .all(|frame| frame.name != "AnyprofSpin.spin" || frame.runtime == Runtime::Java),
            "spin must not be a native JVM frame\n{}",
            explain(profile)
        );
    }

    fn assert_exports(profile: &anyprof_core::Profile) {
        let speedscope = anyprof_export::export(profile, Format::Speedscope).expect("export");
        let rendered = anyprof_export::import(&speedscope, Format::Speedscope).expect("import");
        assert!(
            rendered.stacks.iter().any(|stack| {
                stack
                    .frames
                    .iter()
                    .any(|frame| frame.name == "AnyprofSpin.spin" && frame.runtime == Runtime::Java)
            }),
            "speedscope lost the Java method"
        );
        let collapsed = anyprof_export::export(profile, Format::Collapsed).unwrap();
        let text = String::from_utf8(collapsed).unwrap();
        assert!(
            text.contains("[java] AnyprofSpin.spin") && text.contains("[java] AnyprofSpin.caller"),
            "{text}"
        );
    }

    fn weight_named(profile: &anyprof_core::Profile, needle: &str) -> u64 {
        profile
            .stacks
            .iter()
            .filter(|stack| stack.frames.iter().any(|frame| frame.name.contains(needle)))
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
            lines.push(format!("{} {names}", stack.weight));
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
                let denied = !message.contains("process_vm_readv")
                    && (message.contains("perf_event_open")
                        || message.contains("task_for_pid")
                        || message.contains("not permitted")
                        || message.contains("Operation not permitted"));
                if denied && !required() {
                    eprintln!("skipping live java sample: {message}");
                    return None;
                }
                panic!("java sampling failed: {message}");
            }
        }
    }

    fn java_bin() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("ANYPROF_JAVA")
            && !path.is_empty()
            && Path::new(&path).is_file()
        {
            return Some(PathBuf::from(path));
        }
        if let Ok(home) = std::env::var("JAVA_HOME") {
            let name = if cfg!(windows) { "java.exe" } else { "java" };
            let bin = PathBuf::from(home).join("bin").join(name);
            if bin.is_file() {
                return Some(bin);
            }
        }
        let found = if cfg!(windows) {
            Command::new("where").arg("java").output().ok()
        } else {
            Command::new("which").arg("java").output().ok()
        };
        if let Some(output) = found.filter(|output| output.status.success()) {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = text.lines().find(|line| !line.trim().is_empty()) {
                let path = PathBuf::from(line.trim());
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        if required() {
            panic!("no java executable was found");
        }
        eprintln!("skipping live java sample: java was not found");
        None
    }

    fn javac_for(java: &Path) -> Option<PathBuf> {
        let name = if cfg!(windows) { "javac.exe" } else { "javac" };
        let sibling = java.parent().map(|dir| dir.join(name))?;
        if sibling.is_file() {
            return Some(sibling);
        }
        let found = if cfg!(windows) {
            Command::new("where").arg("javac").output().ok()
        } else {
            Command::new("which").arg("javac").output().ok()
        }?;
        if !found.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&found.stdout);
        text.lines()
            .find(|line| !line.trim().is_empty())
            .map(|line| PathBuf::from(line.trim()))
            .filter(|path| path.is_file())
    }

    fn compile_fixture(name: &str) -> Option<PathBuf> {
        let java = java_bin()?;
        let Some(javac) = javac_for(&java) else {
            if required() {
                panic!("javac was not found next to {}", java.display());
            }
            eprintln!("skipping live java sample: javac was not found");
            return None;
        };
        let dir = fixture_dir(name);
        let source = dir.join("AnyprofSpin.java");
        std::fs::write(&source, SOURCE).unwrap();
        let output = Command::new(javac)
            .arg("-d")
            .arg(&dir)
            .arg(&source)
            .output()
            .unwrap_or_else(|err| panic!("javac: {err}"));
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_dir_all(&dir);
            panic!("javac failed\n{stderr}");
        }
        Some(dir)
    }

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anyprof-java-{name}-{}-{}",
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

    #[cfg(target_os = "linux")]
    fn ptrace_scope() -> Option<i32> {
        std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
            .ok()
            .and_then(|text| text.trim().parse().ok())
    }

    #[cfg(target_os = "linux")]
    fn euid() -> u32 {
        unsafe { libc::geteuid() }
    }

    #[cfg(target_os = "linux")]
    fn ppid(pid: u32) -> Option<u32> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("PPid:") {
                return rest.trim().parse().ok();
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    fn python_bin() -> Option<PathBuf> {
        for name in ["python3", "python"] {
            if Command::new(name)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
            {
                return Some(PathBuf::from(name));
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    fn with_root(test_name: &str, body: fn()) {
        if std::env::var_os("ANYPROF_JAVA_WORKER").is_none() && unsafe { geteuid() } != 0 {
            relaunch_under_sudo(test_name);
            return;
        }
        if unsafe { geteuid() } != 0 {
            panic!("the java live sampler was not running as root");
        }
        body();
    }

    #[cfg(target_os = "macos")]
    fn relaunch_under_sudo(test_name: &str) {
        let exe = std::env::current_exe().expect("test executable");
        let mut command = Command::new("sudo");
        command.args(["-n", "env", "ANYPROF_JAVA_WORKER=1"]);
        for key in ["ANYPROF_JAVA", "JAVA_HOME", "PATH"] {
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
            eprintln!("skipping live java sample: sudo is not available\n{stderr}");
            return;
        }
        panic!(
            "live java sample failed (status {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
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
fn live_java_sampler_is_not_executed_on_this_platform() {
    assert!(!anyprof_java::sampler_implemented());
}
