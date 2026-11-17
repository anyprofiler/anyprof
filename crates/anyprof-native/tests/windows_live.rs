//! Live Windows x64 samples of native processes.
//!
//! These tests compile a C++ fixture with `clang++` or the Visual Studio
//! `cl` from `vcvars64.bat`, then attach to that process. They skip when no
//! compiler is on the machine, unless `ANYPROF_REQUIRE_NATIVE=1`, which is
//! what `windows-latest` CI sets. A green run with the variable unset is not
//! evidence that sampling works.
//!
//! Sampling this process is refused before `OpenProcess`. That check does
//! not need a compiler.

#![cfg(all(windows, target_arch = "x86_64"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyprof_core::{Backend, Runtime};

#[test]
fn samples_a_busy_cpp_program() {
    let _lock = LiveLock::acquire();
    let dir = fixture_dir("busy");
    let source = dir.join("busy.cpp");
    let binary = dir.join("busy.exe");
    std::fs::write(&source, BUSY_CPP).unwrap();
    if !compile(&dir, &source, &binary) {
        return;
    }
    let counter = dir.join("counter");
    std::fs::write(&counter, [0u8; 4096]).unwrap();
    let mut child = spawn(&binary, Some(&counter));
    let pid = child.id();
    wait_until_running(&counter);
    let before = read_counter(&counter);

    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_secs(1)),
            sampling_interval: Some(Duration::from_millis(10)),
            // `-k` is ignored. The profile must still be user frames.
            include_kernel: true,
            ..anyprof_core::RecordOptions::default()
        },
    );
    let during = read_counter(&counter);
    std::thread::sleep(Duration::from_millis(50));
    let after = read_counter(&counter);
    let alive = anyprof_core::process_exists(pid);
    let session = started.unwrap_or_else(|err| panic!("native profile failed: {err}"));
    let _ = child.kill();
    let _ = child.wait();

    let profile = session
        .stop()
        .unwrap_or_else(|err| panic!("stop failed: {err}"));
    assert!(
        during > before && after > during,
        "the target must keep running during and after the profile (before {before}, during {during}, after {after})"
    );
    assert!(alive, "the target exited during the profile");

    let spin_weight = weight_named(&profile, "spin");
    let total = profile.total_weight();
    assert!(
        (20..=1_000).contains(&total),
        "expected on the order of 100 samples at 100 Hz for 1s, got {total}\n{}",
        explain(&profile)
    );
    assert!(
        spin_weight * 2 >= total,
        "expected spin on at least half of the samples ({spin_weight}/{total})\n{}",
        explain(&profile)
    );
    assert!(
        profile.stacks.iter().any(|stack| {
            stack.frames.len() >= 2 && stack.frames.iter().any(|frame| is_spin(&frame.name))
        }),
        "expected a stack with spin and its caller\n{}",
        explain(&profile)
    );
    assert!(
        profile.stacks.iter().all(|stack| {
            stack
                .frames
                .iter()
                .all(|frame| frame.runtime != Runtime::Kernel)
        }),
        "Windows ignores -k and must not invent kernel frames\n{}",
        explain(&profile)
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
                && frame.runtime == Runtime::Native
                && frame.address.is_some()
                && frame
                    .module
                    .as_deref()
                    .is_some_and(|module| module.to_ascii_lowercase().contains("busy"))
        })
    }));
    assert_exports_keep_spin(&profile);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_idle_process_does_not_invent_on_cpu_samples() {
    let _lock = LiveLock::acquire();
    let dir = fixture_dir("idle");
    let source = dir.join("idle.cpp");
    let binary = dir.join("idle.exe");
    std::fs::write(&source, IDLE_CPP).unwrap();
    if !compile(&dir, &source, &binary) {
        return;
    }
    let mut child = spawn(&binary, None);
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(100));
    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(500)),
            sampling_interval: Some(Duration::from_millis(10)),
            ..anyprof_core::RecordOptions::default()
        },
    );
    let alive = anyprof_core::process_exists(pid);
    let session = started.unwrap_or_else(|err| panic!("native profile failed: {err}"));
    let still_alive = anyprof_core::process_exists(pid);
    let _ = child.kill();
    let _ = child.wait();
    let profile = session.stop().expect("stop");
    let total = profile.total_weight();
    assert!(
        total <= 2,
        "an idle process must not be recorded as on-CPU (got {total} samples over {:?})\n{}",
        profile.duration,
        explain(&profile)
    );
    assert!(
        profile.duration >= Duration::from_millis(300),
        "the idle recording ended early: {:?}",
        profile.duration
    );
    assert!(
        alive && still_alive,
        "the idle process exited during the profile"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sampling_this_process_is_refused() {
    let Err(err) = anyprof_native::NativeBackend.start(
        &anyprof_core::Target {
            pid: std::process::id(),
        },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(50)),
            ..anyprof_core::RecordOptions::default()
        },
    ) else {
        panic!("sampling this process must be refused");
    };
    let message = err.to_string();
    assert!(message.contains("suspending"), "{message}");
    assert!(
        !message.contains("OpenProcess"),
        "the refusal must happen before OpenProcess: {message}"
    );
}

fn spawn(binary: &Path, counter: Option<&Path>) -> std::process::Child {
    let mut command = Command::new(binary);
    if let Some(counter) = counter {
        command.arg(counter);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fixture")
}

fn weight_named(profile: &anyprof_core::Profile, needle: &str) -> u64 {
    profile
        .stacks
        .iter()
        .filter(|stack| {
            stack
                .frames
                .iter()
                .take(4)
                .any(|frame| frame.name.contains(needle))
        })
        .map(|stack| stack.weight)
        .sum()
}

fn is_spin(name: &str) -> bool {
    name.contains("anyprof_native") && name.contains("spin")
}

fn assert_exports_keep_spin(profile: &anyprof_core::Profile) {
    for format in [
        anyprof_core::Format::Collapsed,
        anyprof_core::Format::Pprof,
        anyprof_core::Format::Speedscope,
    ] {
        let bytes = anyprof_export::export(profile, format).expect("export");
        assert!(!bytes.is_empty());
        let rendered = anyprof_export::import(&bytes, format).expect("import");
        assert!(
            rendered.stacks.iter().any(|stack| {
                stack.frames.iter().any(|frame| {
                    frame.name.contains("spin") || frame.name.contains("anyprof_native")
                })
            }),
            "{format:?} lost the spin frame"
        );
    }
    let collapsed = anyprof_export::export(profile, anyprof_core::Format::Collapsed).unwrap();
    let text = String::from_utf8(collapsed).unwrap();
    assert!(text.contains("spin"), "{text}");
    assert!(
        !text.contains("[native] "),
        "native frames are bare symbols:\n{text}"
    );
}

fn explain(profile: &anyprof_core::Profile) -> String {
    let mut stacks: Vec<_> = profile.stacks.iter().collect();
    stacks.sort_by_key(|stack| std::cmp::Reverse(stack.weight));
    let top = stacks
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
        .join("\n");
    format!(
        "duration {:?} period {:?} weight {}\nTop stacks:\n{top}",
        profile.duration,
        profile.period,
        profile.total_weight(),
    )
}

fn compile(dir: &Path, source: &Path, binary: &Path) -> bool {
    let mut errors = Vec::new();
    match compile_clang(source, binary) {
        Ok(()) => return true,
        Err(err) => errors.push(err),
    }
    match compile_msvc(dir, source, binary) {
        Ok(()) => return true,
        Err(err) => errors.push(err),
    }
    let message = format!("no C++ compiler built the fixture:\n{}", errors.join("\n"));
    if native_required() {
        panic!("{message}");
    }
    eprintln!("skipping live native sample: {message}");
    false
}

fn compile_clang(source: &Path, binary: &Path) -> Result<(), String> {
    let output = Command::new("clang++")
        .args(["-std=c++17", "-O0", "-fno-omit-frame-pointer", "-g", "-o"])
        .arg(binary)
        .arg(source)
        .output()
        .map_err(|err| format!("clang++: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "clang++\n{}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn compile_msvc(dir: &Path, source: &Path, binary: &Path) -> Result<(), String> {
    let vswhere =
        Path::new(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe");
    if !vswhere.exists() {
        return Err("vswhere.exe is not installed".to_string());
    }
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
        .output()
        .map_err(|err| format!("vswhere: {err}"))?;
    if !listed.status.success() {
        return Err(format!(
            "vswhere\n{}",
            String::from_utf8_lossy(&listed.stderr)
        ));
    }
    let install = String::from_utf8_lossy(&listed.stdout).trim().to_string();
    if install.is_empty() {
        return Err("vswhere did not report a Visual Studio installation".to_string());
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
    .map_err(|err| format!("writing build.cmd: {err}"))?;
    let output = Command::new("cmd")
        .arg("/C")
        .arg(&script)
        .current_dir(dir)
        .output()
        .map_err(|err| format!("cmd: {err}"))?;
    if output.status.success() && binary.is_file() {
        Ok(())
    } else {
        Err(format!(
            "cl\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn wait_until_running(counter: &Path) {
    let started = Instant::now();
    while read_counter(counter) == 0 && started.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        read_counter(counter) > 0,
        "the fixture did not increment its counter"
    );
}

fn read_counter(path: &Path) -> u64 {
    let bytes = std::fs::read(path).unwrap_or_default();
    if bytes.len() < 8 {
        return 0;
    }
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

fn native_required() -> bool {
    std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1")
}

fn fixture_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "anyprof-native-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

struct LiveLock {
    path: PathBuf,
}

impl LiveLock {
    fn acquire() -> Self {
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
                    if started.elapsed() > Duration::from_secs(180) {
                        panic!("timed out waiting for the native live-test lock");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

impl Drop for LiveLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

const BUSY_CPP: &str = r#"
#include <stdint.h>
#include <windows.h>

namespace anyprof_native {
__declspec(noinline) void spin(volatile uint64_t *slot) {
    for (;;) {
        *slot += 1;
    }
}
}

int main(int argc, char **argv) {
    HANDLE file = CreateFileA(
        argv[1], GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
        nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    HANDLE mapping = CreateFileMappingA(file, nullptr, PAGE_READWRITE, 0, 4096, nullptr);
    auto *slot = static_cast<volatile uint64_t *>(
        MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 4096));
    anyprof_native::spin(slot);
}
"#;

const IDLE_CPP: &str = r#"
#include <windows.h>
int main() {
    Sleep(INFINITE);
    return 0;
}
"#;
