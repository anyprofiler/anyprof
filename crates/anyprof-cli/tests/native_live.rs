//! CLI coverage for the native backend.
//!
//! On macOS the live test records a busy C++ process (sudo) and checks the
//! auto-named pprof file plus collapsed and speedscope conversions. Linux
//! x86_64 and Windows x64 do the same attach without sudo. Other platforms
//! assert `anyprof rec -b native` exits with "not implemented".

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
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
)))]
fn anyprof(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(args)
        .output()
        .expect("run anyprof")
}

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
)))]
#[test]
fn native_record_is_not_implemented_on_this_platform() {
    let pid = std::process::id().to_string();
    let output = anyprof(&["rec", "-b", "native", "-p", &pid, "-d", "200ms"]);
    assert!(
        !output.status.success(),
        "native sampling must fail closed on this platform\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not implemented yet"),
        "expected the not-implemented error, got:\n{stderr}"
    );
    assert!(
        stderr.contains("macOS Mach sampling is implemented"),
        "{stderr}"
    );
    #[cfg(target_os = "linux")]
    assert!(stderr.contains("Linux"), "{stderr}");
    #[cfg(windows)]
    assert!(stderr.contains("Windows"), "{stderr}");
}

#[cfg(target_os = "macos")]
#[test]
fn unprivileged_native_record_explains_sudo() {
    if unsafe { geteuid() } == 0 {
        eprintln!("skipping CLI permission check: already root");
        return;
    }
    let mut child = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id().to_string();
    let output = anyprof(&["rec", "-b", "native", "-p", &pid, "-d", "1s"]);
    let _ = child.kill();
    let _ = child.wait();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "unprivileged native record should fail:\n{stderr}"
    );
    assert!(
        stderr.contains("task_for_pid") && (stderr.contains("sudo") || stderr.contains("root")),
        "{stderr}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn record_native_writes_a_profile_of_a_busy_cpp_program() {
    let dir = std::env::temp_dir().join(format!("anyprof-native-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("busy.cpp");
    let binary = dir.join("busy");
    std::fs::write(&source, BUSY_CPP).unwrap();
    let compile = Command::new("clang++")
        .args([
            "-std=c++17",
            "-O0",
            "-fno-omit-frame-pointer",
            "-mno-omit-leaf-frame-pointer",
            "-o",
        ])
        .arg(&binary)
        .arg(&source)
        .output()
        .expect("clang++");
    assert!(
        compile.status.success(),
        "clang++\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(50));

    let output_dir = dir.join("out");
    std::fs::create_dir_all(&output_dir).unwrap();
    let pid_arg = pid.to_string();
    let mut command = Command::new("sudo");
    command.args([
        "-n",
        env!("CARGO_BIN_EXE_anyprof"),
        "rec",
        "-b",
        "native",
        "-p",
        pid_arg.as_str(),
        "-d",
        "1s",
        "-F",
        "100",
        "-o",
    ]);
    command.arg(&output_dir);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let recorded = run_with_timeout(command, Duration::from_secs(30));
    let _ = child.kill();
    let _ = child.wait();
    // root's umask can create a profile the test user cannot read.
    let _ = Command::new("sudo")
        .args(["-n", "chmod", "-R", "a+rX"])
        .arg(&output_dir)
        .status();
    let stderr = String::from_utf8_lossy(&recorded.stderr);
    let stdout = String::from_utf8_lossy(&recorded.stdout);
    if !recorded.status.success() {
        let sudo_missing = stderr.contains("a password is required")
            || stderr.contains("not permitted")
            || stderr.contains("No such file");
        if sudo_missing && std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() != Some("1") {
            eprintln!("skipping CLI native sample: sudo is not available\n{stderr}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!(
            "anyprof rec failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            recorded.status.code()
        );
    }
    assert!(
        stderr.contains("using backend 'native'"),
        "auto/explicit backend line missing:\n{stderr}"
    );
    let (samples, path) = parse_wrote(&stderr);
    assert!(
        (20..=1_000).contains(&samples),
        "sample count {samples} is not a 1s/100Hz recording\n{stderr}"
    );
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with(&format!("anyprof-native-{pid}-")) && name.ends_with(".pb.gz"),
        "unexpected auto name {name}"
    );
    assert!(path.starts_with(&output_dir), "{}", path.display());
    assert!(path.is_file(), "profile was not written");

    let collapsed = dir.join("cpu.collapsed");
    let speedscope = dir.join("cpu.speedscope.json");
    convert(&path, &collapsed, "collapsed");
    convert(&path, &speedscope, "speedscope");
    let folded = std::fs::read_to_string(&collapsed).unwrap();
    let json = std::fs::read_to_string(&speedscope).unwrap();
    assert!(
        folded.contains("spin") && !folded.contains("[native] "),
        "collapsed stacks:\n{folded}"
    );
    assert!(json.contains("spin"), "speedscope lost the spin frame");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn convert(input: &Path, output: &Path, to: &str) {
    let result = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["conv", "-i"])
        .arg(input)
        .arg("-o")
        .arg(output)
        .args(["-t", to])
        .output()
        .expect("anyprof conv");
    assert!(
        result.status.success(),
        "conv to {to} failed\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
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

#[cfg(target_os = "macos")]
const BUSY_CPP: &str = r#"
namespace anyprof_native {
__attribute__((noinline, optnone)) void spin() {
    volatile unsigned long long x = 0;
    for (;;) {
        x += 1;
    }
}
}

int main() {
    anyprof_native::spin();
    return 0;
}
"#;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn geteuid() -> u32;
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn record_native_writes_a_profile_of_a_busy_cpp_program_on_linux() {
    let _lock = CliLock::acquire();
    let dir = std::env::temp_dir().join(format!(
        "anyprof-native-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("busy.cpp");
    let binary = dir.join("busy");
    std::fs::write(&source, LINUX_BUSY_CPP).unwrap();
    if !compile_unix(&source, &binary) {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(50));
    let output_dir = dir.join("out");
    std::fs::create_dir_all(&output_dir).unwrap();
    let recorded = record_native(pid, &output_dir);
    let _ = child.kill();
    let _ = child.wait();
    assert_native_profile(&recorded, pid, &output_dir, &dir);
}

#[cfg(all(windows, target_arch = "x86_64"))]
#[test]
fn record_native_writes_a_profile_of_a_busy_cpp_program_on_windows() {
    let _lock = CliLock::acquire();
    let dir = std::env::temp_dir().join(format!(
        "anyprof-native-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("busy.cpp");
    let binary = dir.join("busy.exe");
    std::fs::write(&source, WINDOWS_BUSY_CPP).unwrap();
    if !compile_windows(&dir, &source, &binary) {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(50));
    let output_dir = dir.join("out");
    std::fs::create_dir_all(&output_dir).unwrap();
    let recorded = record_native(pid, &output_dir);
    let _ = child.kill();
    let _ = child.wait();
    assert_native_profile(&recorded, pid, &output_dir, &dir);
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn record_native(pid: u32, output_dir: &Path) -> std::process::Output {
    let pid_arg = pid.to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_anyprof"));
    command.args([
        "rec",
        "-b",
        "native",
        "-p",
        pid_arg.as_str(),
        "-d",
        "1s",
        "-F",
        "100",
        "-o",
    ]);
    command.arg(output_dir);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_timeout(command, Duration::from_secs(30))
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
fn assert_native_profile(recorded: &std::process::Output, pid: u32, output_dir: &Path, dir: &Path) {
    let stderr = String::from_utf8_lossy(&recorded.stderr);
    let stdout = String::from_utf8_lossy(&recorded.stdout);
    if !recorded.status.success() {
        let denied = stderr.contains("perf_event_open");
        if denied && std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() != Some("1") {
            eprintln!("skipping CLI native sample: {stderr}");
            let _ = std::fs::remove_dir_all(dir);
            return;
        }
        panic!(
            "anyprof rec failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            recorded.status.code()
        );
    }
    assert!(
        stderr.contains("using backend 'native'"),
        "backend line missing:\n{stderr}"
    );
    let (samples, path) = parse_wrote(&stderr);
    assert!(
        (20..=1_000).contains(&samples),
        "sample count {samples} is not a 1s/100Hz recording\n{stderr}"
    );
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with(&format!("anyprof-native-{pid}-")) && name.ends_with(".pb.gz"),
        "unexpected auto name {name}"
    );
    assert!(path.starts_with(output_dir), "{}", path.display());
    assert!(path.is_file(), "profile was not written");
    let collapsed = dir.join("cpu.collapsed");
    let speedscope = dir.join("cpu.speedscope.json");
    convert(&path, &collapsed, "collapsed");
    convert(&path, &speedscope, "speedscope");
    let folded = std::fs::read_to_string(&collapsed).unwrap();
    let json = std::fs::read_to_string(&speedscope).unwrap();
    assert!(
        folded.contains("spin") && !folded.contains("[native] "),
        "collapsed stacks:\n{folded}"
    );
    assert!(json.contains("spin"), "speedscope lost the spin frame");
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn compile_unix(source: &Path, binary: &Path) -> bool {
    let mut errors = Vec::new();
    for compiler in ["g++", "clang++"] {
        let mut command = Command::new(compiler);
        command.args([
            "-std=c++17",
            "-O0",
            "-fno-omit-frame-pointer",
            "-fno-optimize-sibling-calls",
            "-g",
        ]);
        if compiler == "clang++" {
            command.arg("-mno-omit-leaf-frame-pointer");
        }
        command.arg("-o").arg(binary).arg(source);
        match command.output() {
            Ok(output) if output.status.success() => return true,
            Ok(output) => errors.push(format!(
                "{compiler}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )),
            Err(err) => errors.push(format!("{compiler}: {err}")),
        }
    }
    let message = format!("no C++ compiler built the fixture:\n{}", errors.join("\n"));
    if std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1") {
        panic!("{message}");
    }
    eprintln!("skipping CLI native sample: {message}");
    false
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn compile_windows(dir: &Path, source: &Path, binary: &Path) -> bool {
    let clang = Command::new("clang++")
        .args(["-std=c++17", "-O0", "-g", "-o"])
        .arg(binary)
        .arg(source)
        .output();
    if let Ok(output) = clang
        && output.status.success()
    {
        return true;
    }
    let vswhere =
        Path::new(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe");
    let listed = Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output();
    let Ok(listed) = listed else {
        skip_or_fail("vswhere could not be run");
        return false;
    };
    let install = String::from_utf8_lossy(&listed.stdout).trim().to_string();
    if install.is_empty() {
        skip_or_fail("Visual Studio C++ tools were not found");
        return false;
    }
    let vcvars = Path::new(&install).join(r"VC\Auxiliary\Build\vcvars64.bat");
    let script = dir.join("build.cmd");
    std::fs::write(
        &script,
        format!(
            "@echo off\r\ncall \"{}\"\r\nif errorlevel 1 exit /b 1\r\ncl /nologo /Od /Zi /EHsc /std:c++17 /Fe:\"{}\" \"{}\" /link /DEBUG\r\n",
            vcvars.display(),
            binary.display(),
            source.display()
        ),
    )
    .expect("build.cmd");
    let output = Command::new("cmd")
        .arg("/C")
        .arg(&script)
        .current_dir(dir)
        .output()
        .expect("cmd");
    if output.status.success() && binary.is_file() {
        true
    } else {
        skip_or_fail(&format!(
            "cl failed\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
        false
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn skip_or_fail(message: &str) {
    if std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1") {
        panic!("{message}");
    }
    eprintln!("skipping CLI native sample: {message}");
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
struct CliLock {
    path: std::path::PathBuf,
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
impl CliLock {
    fn acquire() -> Self {
        let path = std::env::temp_dir().join("anyprof-native-live.lock");
        let started = std::time::Instant::now();
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
                    if started.elapsed() > Duration::from_secs(180) {
                        panic!("timed out waiting for the native live-test lock");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
impl Drop for CliLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const LINUX_BUSY_CPP: &str = r#"
namespace anyprof_native {
__attribute__((noinline)) void spin() {
    volatile unsigned long long x = 0;
    for (;;) {
        x += 1;
    }
}
}

int main() {
    anyprof_native::spin();
    return 0;
}
"#;

#[cfg(all(windows, target_arch = "x86_64"))]
const WINDOWS_BUSY_CPP: &str = r#"
namespace anyprof_native {
__declspec(noinline) void spin() {
    volatile unsigned long long x = 0;
    for (;;) {
        x += 1;
    }
}
}

int main() {
    anyprof_native::spin();
    return 0;
}
"#;
