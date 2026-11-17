//! CLI coverage for the native backend.
//!
//! On macOS the live test records a busy C++ process (sudo) and checks the
//! auto-named pprof file plus collapsed and speedscope conversions. Windows
//! and Linux do not sample: they assert `anyprof rec -b native` exits with
//! the platform's "not implemented" error. That is a compilation-and-CLI
//! check, not evidence the sampler works there.

use std::process::Command;

#[cfg(target_os = "macos")]
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::time::Duration;

fn anyprof(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(args)
        .output()
        .expect("run anyprof")
}

#[cfg(not(target_os = "macos"))]
#[test]
fn native_record_is_not_implemented_on_this_platform() {
    let pid = std::process::id().to_string();
    let output = anyprof(&["rec", "-b", "native", "-p", &pid, "-d", "200ms"]);
    assert!(
        !output.status.success(),
        "native sampling must fail closed off macOS\n{}",
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
    assert!(
        stderr.contains("Linux"),
        "Linux CI must name this platform as unimplemented:\n{stderr}"
    );
    #[cfg(windows)]
    assert!(
        stderr.contains("Windows"),
        "Windows CI must name this platform as unimplemented:\n{stderr}"
    );
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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
