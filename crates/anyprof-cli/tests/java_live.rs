//! CLI coverage for the Java backend.
//!
//! Auto-detection (no `-b`) must select `java` for a HotSpot pid and write
//! `anyprof-java-<pid>-*.speedscope.json`. Collapsed stacks prefix method
//! names with `[java]`.

use std::process::Command;

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
use std::path::{Path, PathBuf};
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
  public static void main(String[] args) {
    caller();
  }
}
"#;

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
)))]
#[test]
fn java_record_is_not_implemented_on_this_platform() {
    let pid = std::process::id().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["rec", "-b", "java", "-p", &pid, "-d", "200ms"])
        .output()
        .expect("run anyprof");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "java sampling must fail closed on this platform\n{stderr}"
    );
    assert!(
        stderr.contains("not implemented yet"),
        "expected the not-implemented error, got:\n{stderr}"
    );
    assert!(
        stderr.contains("java backend"),
        "the refusal should name the java backend:\n{stderr}"
    );
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
#[test]
fn record_auto_detects_a_busy_java_program() {
    #[cfg(not(target_os = "macos"))]
    let _lock = live_lock();
    let jdks = java_bins();
    if jdks.is_empty() {
        return;
    }
    for java in jdks {
        record_one(&java);
    }
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn record_one(java: &Path) {
    let dir = std::env::temp_dir().join(format!(
        "anyprof-java-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("AnyprofSpin.java");
    std::fs::write(&source, SOURCE).unwrap();
    let javac = java
        .parent()
        .map(|parent| parent.join(if cfg!(windows) { "javac.exe" } else { "javac" }))
        .filter(|path| path.is_file());
    let Some(javac) = javac else {
        let _ = std::fs::remove_dir_all(&dir);
        if required() {
            panic!("javac was not found next to {}", java.display());
        }
        eprintln!("skipping CLI java sample: javac was not found");
        return;
    };
    let compiled = Command::new(javac)
        .arg("-d")
        .arg(&dir)
        .arg(&source)
        .output()
        .expect("javac");
    assert!(
        compiled.status.success(),
        "javac failed\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let mut child = Command::new(java)
        .arg("-XX:CompileCommand=dontinline,AnyprofSpin.spin")
        .arg("-cp")
        .arg(&dir)
        .arg("AnyprofSpin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy java");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(800));
    if child.try_wait().ok().flatten().is_some() {
        let _ = std::fs::remove_dir_all(&dir);
        if required() {
            panic!("the Java fixture exited before sampling");
        }
        eprintln!("skipping CLI java sample: the fixture exited");
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
        let denied = stderr.contains("process_vm_readv")
            || stderr.contains("perf_event_open")
            || stderr.contains("task_for_pid")
            || stderr.contains("a password is required")
            || stderr.contains("not permitted");
        if denied && !required() {
            eprintln!("skipping CLI java sample:\n{stderr}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!(
            "anyprof rec failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            recorded.status.code()
        );
    }
    assert!(
        stderr.contains("using backend 'java'"),
        "auto-detection should select java:\n{stderr}"
    );
    let (samples, path) = parse_wrote(&stderr);
    assert!(
        (20..=1_000).contains(&samples),
        "sample count {samples} is not a 1s/100Hz recording\n{stderr}"
    );
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with(&format!("anyprof-java-{pid}-")) && name.ends_with(".speedscope.json"),
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
        folded.contains("[java] AnyprofSpin.spin") && folded.contains("[java] AnyprofSpin.caller"),
        "collapsed stacks:\n{folded}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
/// Homes or binaries from `ANYPROF_JAVA_17`, `ANYPROF_JAVA_21`, and
/// `ANYPROF_JAVA_25` when the sampling matrix requires them. Otherwise one
/// JDK from `ANYPROF_JAVA`, `JAVA_HOME`, or `PATH`.
fn java_bins() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for (key, label) in [
        ("ANYPROF_JAVA_17", "17"),
        ("ANYPROF_JAVA_21", "21"),
        ("ANYPROF_JAVA_25", "25"),
    ] {
        match resolve_java(&std::env::var(key).unwrap_or_default()) {
            Some(java) => found.push(java),
            None if required() => panic!("{key} must be a JDK {label} home or java binary"),
            None => {}
        }
    }
    if !found.is_empty() {
        return found;
    }
    if let Some(java) = resolve_java(&std::env::var("ANYPROF_JAVA").unwrap_or_default())
        .or_else(|| resolve_java(&std::env::var("JAVA_HOME").unwrap_or_default()))
        .or_else(java_on_path)
    {
        return vec![java];
    }
    if required() {
        panic!("no java executable was found");
    }
    eprintln!("skipping CLI java sample: java was not found");
    Vec::new()
}

fn resolve_java(value: &str) -> Option<PathBuf> {
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if path.is_file() {
        return Some(path);
    }
    let name = if cfg!(windows) { "java.exe" } else { "java" };
    let bin = path.join("bin").join(name);
    bin.is_file().then_some(bin)
}

fn java_on_path() -> Option<PathBuf> {
    let found = if cfg!(windows) {
        Command::new("where").arg("java").output().ok()
    } else {
        Command::new("which").arg("java").output().ok()
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

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn required() -> bool {
    std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1")
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
fn parse_wrote(stderr: &str) -> (u64, PathBuf) {
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
        PathBuf::from(path),
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
    path: PathBuf,
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
