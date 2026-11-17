//! Real on-CPU samples of a busy C++ process, and a check that an idle
//! process is not recorded as on-CPU.
//!
//! macOS CI sets `ANYPROF_REQUIRE_NATIVE=1` and must not skip: the test
//! re-executes itself under `sudo` and checks stacks, timing, sample counts,
//! and collapsed / pprof / speedscope output. The idle fixture is a
//! user-compiled `pause()` loop, not `/bin/sleep`, because System Integrity
//! Protection can refuse `task_for_pid` on platform binaries. Windows and
//! Linux compile this file but do not sample. Their assertion is that the
//! Mach sampler is absent; [`NativeBackend::start`] returning "not
//! implemented" is covered by the library tests.

#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
use anyprof_core::Backend;

#[cfg(not(target_os = "macos"))]
#[test]
fn live_mach_sampler_is_not_executed_on_this_platform() {
    // Compilation of the native crate on this OS is covered by `cargo test`.
    // The sampler itself is macOS-only; do not treat a green run here as
    // evidence that C++ sampling works.
    assert_ne!(
        std::env::consts::OS,
        "macos",
        "this negative test must not be compiled into the macOS live run"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn unprivileged_attach_explains_that_root_is_required() {
    if unsafe { geteuid() } == 0 {
        eprintln!("skipping permission denial check: the test process is already root");
        return;
    }
    let mut child = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(200)),
            ..anyprof_core::RecordOptions::default()
        },
    );
    let _ = child.kill();
    let _ = child.wait();
    let Err(err) = started else {
        panic!("task_for_pid succeeded without root; the permission error was not tested");
    };
    let message = err.to_string();
    assert!(
        message.contains("task_for_pid"),
        "expected the Mach permission error, got: {message}"
    );
    assert!(
        message.contains("sudo") || message.contains("root"),
        "expected a root hint, got: {message}"
    );
    assert!(
        message.contains(&pid.to_string()),
        "expected the pid in the error, got: {message}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn samples_a_busy_cpp_program() {
    with_root("samples_a_busy_cpp_program", sample_busy_program);
}

#[cfg(target_os = "macos")]
#[test]
fn an_idle_process_does_not_invent_on_cpu_samples() {
    with_root(
        "an_idle_process_does_not_invent_on_cpu_samples",
        sample_idle_program,
    );
}

#[cfg(target_os = "macos")]
fn with_root(test_name: &str, body: fn()) {
    if std::env::var_os("ANYPROF_NATIVE_WORKER").is_none() && unsafe { geteuid() } != 0 {
        relaunch_under_sudo(test_name);
        return;
    }
    if unsafe { geteuid() } != 0 {
        panic!("the native live sampler was not running as root");
    }
    body();
}

#[cfg(target_os = "macos")]
fn relaunch_under_sudo(test_name: &str) {
    let exe = std::env::current_exe().expect("test executable");
    // `env` sets the variable. `sudo VAR=cmd` is rejected when sudoers
    // does not grant SETENV, which is the macOS default.
    let output = Command::new("sudo")
        .args([
            "-n",
            "env",
            "ANYPROF_NATIVE_WORKER=1",
            exe.to_str().expect("test path is utf-8"),
            test_name,
            "--exact",
            "--nocapture",
        ])
        .output()
        .expect("invoke sudo");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() && stdout.contains(test_name) && stdout.contains("ok") {
        return;
    }
    let sudo_missing = stderr.contains("a password is required")
        || stderr.contains("not permitted")
        || stderr.contains("No such file")
        || stderr.contains("command not found");
    if sudo_missing && !native_required() {
        eprintln!("skipping live native sample: sudo is not available\n{stderr}");
        return;
    }
    panic!(
        "live native sample failed (status {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
}

#[cfg(target_os = "macos")]
fn native_required() -> bool {
    std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1")
}

#[cfg(target_os = "macos")]
fn sample_busy_program() {
    let dir = std::env::temp_dir().join(format!("anyprof-native-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let source = dir.join("busy.cpp");
    let binary = dir.join("busy");
    std::fs::write(&source, BUSY_CPP).expect("write fixture");
    let compile = Command::new("clang++")
        .args([
            "-std=c++17",
            "-O0",
            "-fno-omit-frame-pointer",
            "-mno-omit-leaf-frame-pointer",
            "-fno-optimize-sibling-calls",
            "-o",
        ])
        .arg(&binary)
        .arg(&source)
        .output()
        .unwrap_or_else(|err| panic!("clang++ failed to launch: {err}"));
    if !compile.status.success() {
        panic!(
            "clang++ failed to build the fixture\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }

    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy C++ process");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(50));

    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_secs(1)),
            sampling_interval: Some(Duration::from_millis(10)),
            ..anyprof_core::RecordOptions::default()
        },
    );
    let _ = child.kill();
    let _ = child.wait();
    let session = started.unwrap_or_else(|err| {
        panic!(
            "native profile failed: {err}\nsymbols:\n{}",
            nm_symbols(&binary)
        )
    });
    let profile = session
        .stop()
        .unwrap_or_else(|err| panic!("stop failed: {err}"));

    let spin_weight = profile
        .stacks
        .iter()
        .filter(|stack| {
            stack
                .frames
                .iter()
                .take(4)
                .any(|frame| is_spin(&frame.name))
        })
        .map(|stack| stack.weight)
        .sum::<u64>();
    let total = profile.total_weight();
    assert!(
        (20..=1_000).contains(&total),
        "expected on the order of 100 samples at 100 Hz for 1s, got {total}\n{}",
        explain(&profile, &binary)
    );
    assert!(
        spin_weight * 2 >= total,
        "expected anyprof_native::spin on at least half of the samples ({spin_weight}/{total})\n{}",
        explain(&profile, &binary)
    );
    assert!(
        profile.stacks.iter().any(|stack| stack.frames.len() >= 2
            && stack.frames.iter().any(|frame| is_spin(&frame.name))),
        "expected a stack with the spin frame and its caller\n{}",
        explain(&profile, &binary)
    );
    assert_eq!(profile.pid, pid);
    assert!(
        profile.duration >= Duration::from_millis(700)
            && profile.duration <= Duration::from_secs(3),
        "recording should last about 1s, was {:?}",
        profile.duration
    );
    assert_eq!(profile.period, Some(Duration::from_millis(10)));
    assert!(profile.stacks.iter().any(|stack| {
        stack.frames.iter().any(|frame| {
            is_spin(&frame.name)
                && frame.runtime == anyprof_core::Runtime::Native
                && frame.address.is_some()
                && frame.module.as_deref() == Some("busy")
        })
    }));

    for format in [
        anyprof_core::Format::Collapsed,
        anyprof_core::Format::Pprof,
        anyprof_core::Format::Speedscope,
    ] {
        let bytes = anyprof_export::export(&profile, format)
            .unwrap_or_else(|err| panic!("export {format:?}: {err}"));
        assert!(!bytes.is_empty());
        let rendered = anyprof_export::import(&bytes, format)
            .unwrap_or_else(|err| panic!("import {format:?}: {err}"));
        let rendered_spin = rendered.stacks.iter().any(|stack| {
            stack
                .frames
                .iter()
                .any(|frame| frame.name.contains("spin") || frame.name.contains("anyprof_native"))
        });
        assert!(
            rendered_spin,
            "{format:?} output lost the spin frame\n{}",
            String::from_utf8_lossy(&bytes)
        );
    }
    let collapsed = anyprof_export::export(&profile, anyprof_core::Format::Collapsed).unwrap();
    let text = String::from_utf8(collapsed).unwrap();
    assert!(text.contains("spin"), "{text}");
    assert!(
        !text.contains("[native] "),
        "native frames are bare symbols, not a runtime prefix:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "macos")]
fn sample_idle_program() {
    let dir = std::env::temp_dir().join(format!("anyprof-native-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let source = dir.join("idle.c");
    let binary = dir.join("idle");
    std::fs::write(&source, IDLE_C).expect("write fixture");
    let compile = Command::new("clang")
        .args(["-std=c11", "-O0", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .unwrap_or_else(|err| panic!("clang failed to launch: {err}"));
    if !compile.status.success() {
        panic!(
            "clang failed to build the idle fixture\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }
    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn idle process");
    let pid = child.id();
    // Let the process reach pause() before the first sample.
    std::thread::sleep(Duration::from_millis(100));
    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(500)),
            sampling_interval: Some(Duration::from_millis(10)),
            ..anyprof_core::RecordOptions::default()
        },
    );
    let _ = child.kill();
    let _ = child.wait();
    let session = started.unwrap_or_else(|err| panic!("idle profile failed: {err}"));
    let profile = session
        .stop()
        .unwrap_or_else(|err| panic!("stop failed: {err}"));
    let total = profile.total_weight();
    assert!(
        total <= 2,
        "an idle process must not be recorded as on-CPU (got {total} samples over {:?})\n{}",
        profile.duration,
        explain(&profile, &binary)
    );
    assert!(
        profile.duration >= Duration::from_millis(300),
        "the idle recording ended early: {:?}",
        profile.duration
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "macos")]
fn is_spin(name: &str) -> bool {
    name.contains("anyprof_native") && name.contains("spin")
}

#[cfg(target_os = "macos")]
fn explain(profile: &anyprof_core::Profile, binary: &std::path::Path) -> String {
    format!(
        "duration {:?} period {:?} weight {}\nTop stacks:\n{}\nnm:\n{}",
        profile.duration,
        profile.period,
        profile.total_weight(),
        top_stacks(profile),
        nm_symbols(binary)
    )
}

#[cfg(target_os = "macos")]
fn top_stacks(profile: &anyprof_core::Profile) -> String {
    let mut stacks: Vec<_> = profile.stacks.iter().collect();
    stacks.sort_by_key(|stack| std::cmp::Reverse(stack.weight));
    stacks
        .into_iter()
        .take(8)
        .map(|stack| {
            let frames: Vec<_> = stack
                .frames
                .iter()
                .map(|frame| {
                    format!(
                        "{}@{:x}{}",
                        if frame.name.is_empty() {
                            "?"
                        } else {
                            frame.name.as_str()
                        },
                        frame.address.unwrap_or(0),
                        frame
                            .module
                            .as_deref()
                            .map(|module| format!(" ({module})"))
                            .unwrap_or_default()
                    )
                })
                .collect();
            format!("  {} x {}", stack.weight, frames.join(" <- "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(target_os = "macos")]
fn nm_symbols(binary: &std::path::Path) -> String {
    match Command::new("nm").arg(binary).output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .filter(|line| {
                    line.contains("spin")
                        || line.contains("anyprof_native")
                        || line.contains(" main")
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Err(err) => format!("nm failed: {err}"),
    }
}

#[cfg(target_os = "macos")]
const IDLE_C: &str = r#"
#include <unistd.h>
int main(void) {
    for (;;) {
        pause();
    }
}
"#;

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
