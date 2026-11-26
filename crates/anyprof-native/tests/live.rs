//! Real on-CPU samples of a busy C++ process, and a check that an idle
//! process is not recorded as on-CPU.
//!
//! macOS CI sets `ANYPROF_REQUIRE_NATIVE=1` and must not skip: the test
//! re-executes itself under `sudo` and checks stacks, timing, sample counts,
//! and collapsed / pprof / speedscope output. The idle fixture is a
//! user-compiled `pause()` loop, not `/bin/sleep`, because System Integrity
//! Protection can refuse `task_for_pid` on platform binaries. Windows x64
//! and Linux x86_64 sample in `windows_live.rs` and `linux_live.rs`. This
//! file only checks that the host is not macOS.

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
    if std::env::var_os("ANYPROF_DISABLE_XCTRACE").is_none() {
        let exe = std::env::current_exe().expect("test binary path");
        let out = Command::new(exe)
            .env("ANYPROF_DISABLE_XCTRACE", "1")
            .args([
                "--exact",
                "unprivileged_attach_explains_that_root_is_required",
                "--nocapture",
            ])
            .output()
            .expect("re-exec with ANYPROF_DISABLE_XCTRACE=1");
        assert!(
            out.status.success(),
            "unprivileged_attach_explains_that_root_is_required failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
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
        message.contains("anyprof setup") && (message.contains("sudo") || message.contains("root")),
        "expected setup/root hint, got: {message}"
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
#[test]
fn samples_an_optimized_fp_less_cpp_program_and_dyld_cache_symbols() {
    with_root(
        "samples_an_optimized_fp_less_cpp_program_and_dyld_cache_symbols",
        sample_nofp_and_dyld_cache,
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
    let session = match started {
        Ok(session) => session,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "native profile failed: {err}\nsymbols:\n{}",
                nm_symbols(&binary)
            );
        }
    };
    std::thread::sleep(Duration::from_secs(1));
    let profile = session.stop().unwrap_or_else(|err| {
        let _ = child.kill();
        let _ = child.wait();
        panic!("stop failed: {err}")
    });
    let _ = child.kill();
    let _ = child.wait();

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
    let session = match started {
        Ok(session) => session,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("idle profile failed: {err}");
        }
    };
    std::thread::sleep(Duration::from_millis(500));
    let profile = session.stop().unwrap_or_else(|err| {
        let _ = child.kill();
        let _ = child.wait();
        panic!("stop failed: {err}")
    });
    let _ = child.kill();
    let _ = child.wait();
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
fn sample_nofp_and_dyld_cache() {
    let dir = std::env::temp_dir().join(format!("anyprof-native-nofp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let source = dir.join("nofp.cpp");
    let binary = dir.join("nofp");
    std::fs::write(&source, NOFP_MACOS_CPP).expect("write fixture");
    let compile = Command::new("clang++")
        .args([
            "-std=c++17",
            "-O2",
            "-fomit-frame-pointer",
            "-fno-optimize-sibling-calls",
            "-o",
        ])
        .arg(&binary)
        .arg(&source)
        .output()
        .unwrap_or_else(|err| panic!("clang++ failed to launch: {err}"));
    if !compile.status.success() {
        panic!(
            "clang++ failed to build the nofp fixture\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }

    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nofp C++ process");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(50));

    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(800)),
            sampling_interval: Some(Duration::from_millis(10)),
            ..anyprof_core::RecordOptions::default()
        },
    );
    let session = match started {
        Ok(session) => session,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("nofp profile failed: {err}");
        }
    };
    std::thread::sleep(Duration::from_millis(800));
    let profile = session.stop().unwrap_or_else(|err| {
        let _ = child.kill();
        let _ = child.wait();
        panic!("stop failed: {err}")
    });
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        profile.stacks.iter().any(|stack| {
            let has_leaf = stack.frames.iter().any(|f| f.name.contains("nofp_leaf"));
            let has_mid = stack.frames.iter().any(|f| f.name.contains("nofp_mid"));
            let has_top = stack.frames.iter().any(|f| f.name.contains("nofp_top"));
            has_leaf && has_mid && has_top
        }),
        "expected Compact Unwind / __eh_frame chain through nofp_leaf <- nofp_mid <- nofp_top\n{}",
        explain(&profile, &binary)
    );
    assert!(
        profile
            .stacks
            .iter()
            .flat_map(|stack| stack.frames.iter())
            .any(|frame| {
                !frame.name.is_empty()
                    && frame.module.as_deref().is_some_and(|m| {
                        m.starts_with("libsystem_") || m == "dyld" || m == "libdyld.dylib"
                    })
            }),
        "expected dyld_shared_cache / dyld system symbols to resolve non-empty function names\n{}",
        explain(&profile, &binary)
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
                        || line.contains("nofp")
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
const NOFP_MACOS_CPP: &str = r#"
#include <stdint.h>
#include <stdio.h>

static volatile uint64_t keep_running = 1;

namespace anyprof_native {
__attribute__((noinline)) uint64_t nofp_leaf(uint64_t seed) {
    char buf[64];
    uint64_t acc = seed;
    while (keep_running) {
        int n = snprintf(buf, sizeof(buf), "%llu", static_cast<unsigned long long>(acc));
        acc += static_cast<uint64_t>(n);
    }
    return acc;
}

__attribute__((noinline)) uint64_t nofp_mid(uint64_t seed) {
    volatile uint64_t scratch[8];
    scratch[0] = 3;
    uint64_t res = nofp_leaf(seed + scratch[0]);
    return res + scratch[0];
}

__attribute__((noinline)) uint64_t nofp_top(uint64_t seed) {
    volatile uint64_t scratch[8];
    scratch[1] = 5;
    uint64_t res = nofp_mid(seed + scratch[1]);
    return res + scratch[1];
}
}

int main() {
    return static_cast<int>(anyprof_native::nofp_top(1));
}
"#;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn geteuid() -> u32;
}
