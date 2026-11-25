//! Live Linux `x86_64` and `aarch64` samples of native processes.
//!
//! These tests attach with `perf_event_open` to programs this test compiled.
//! They skip when that syscall is refused, unless `ANYPROF_REQUIRE_NATIVE=1`,
//! which is what Ubuntu CI sets after lowering `kernel.perf_event_paranoid`.
//! A green run with the variable unset is not evidence that sampling works.

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyprof_core::{Backend, Runtime};

#[test]
fn samples_a_busy_cpp_program() {
    let _lock = LiveLock::acquire();
    let dir = fixture_dir("busy");
    let source = dir.join("busy.cpp");
    let binary = dir.join("busy");
    std::fs::write(&source, BUSY_CPP).unwrap();
    if !compile(&source, &binary, &[]) {
        return;
    }
    let counter = dir.join("counter");
    std::fs::write(&counter, [0u8; 4096]).unwrap();
    let mut child = Command::new(&binary)
        .arg(&counter)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busy");
    let pid = child.id();
    wait_until_running(&counter);
    let before = read_counter(&counter);

    let started = anyprof_native::NativeBackend.start(
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
        return;
    };
    let during = read_counter(&counter);
    std::thread::sleep(Duration::from_millis(50));
    let after = read_counter(&counter);
    let state = proc_state(pid);
    let _ = child.kill();
    let _ = child.wait();

    let profile = session
        .stop()
        .unwrap_or_else(|err| panic!("stop failed: {err}\nsymbols:\n{}", nm_symbols(&binary)));
    assert!(
        during > before && after > during,
        "the target must keep running during and after the profile (before {before}, during {during}, after {after}, state {state:?})"
    );
    assert!(
        state.is_some_and(|state| state != 'T' && state != 't'),
        "the target was left stopped (state {state:?})"
    );

    let spin_weight = weight_named(&profile, "spin");
    let total = profile.total_weight();
    assert!(
        (20..=1_000).contains(&total),
        "expected on the order of 100 samples at 100 Hz for 1s, got {total}\n{}",
        explain(&profile, &binary)
    );
    assert!(
        spin_weight * 2 >= total,
        "expected spin on at least half of the samples ({spin_weight}/{total})\n{}",
        explain(&profile, &binary)
    );
    assert!(
        profile.stacks.iter().any(|stack| {
            stack.frames.len() >= 2 && stack.frames.iter().any(|frame| is_spin(&frame.name))
        }),
        "expected a stack with spin and its caller\n{}",
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
                && frame.runtime == Runtime::Native
                && frame.address.is_some()
                && frame.module.as_deref() == Some("busy")
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
    let binary = dir.join("idle");
    std::fs::write(&source, IDLE_CPP).unwrap();
    if !compile(&source, &binary, &[]) {
        return;
    }
    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn idle");
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
    let state = proc_state(pid);
    let alive = anyprof_core::process_exists(pid);
    let Some(session) = accept_or_skip(started) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let _ = child.kill();
    let _ = child.wait();
    let profile = session.stop().expect("stop");
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
    assert!(alive, "the idle process exited during the profile");
    assert!(
        state.is_some_and(|state| state != 'T' && state != 't'),
        "the idle process was left stopped (state {state:?})"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn both_running_threads_are_sampled() {
    let _lock = LiveLock::acquire();
    let dir = fixture_dir("threads");
    let source = dir.join("threads.cpp");
    let binary = dir.join("threads");
    std::fs::write(&source, THREADS_CPP).unwrap();
    if !compile(&source, &binary, &["-pthread"]) {
        return;
    }
    let counter = dir.join("counter");
    std::fs::write(&counter, [0u8; 4096]).unwrap();
    let mut child = Command::new(&binary)
        .arg(&counter)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn threads");
    let pid = child.id();
    wait_until_running(&counter);
    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_secs(1)),
            sampling_interval: Some(Duration::from_millis(10)),
            ..anyprof_core::RecordOptions::default()
        },
    );
    let state = proc_state(pid);
    let Some(session) = accept_or_skip(started) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let _ = child.kill();
    let _ = child.wait();
    let profile = session.stop().expect("stop");
    let a = weight_named(&profile, "spin_a");
    let b = weight_named(&profile, "spin_b");
    assert!(
        a > 0 && b > 0,
        "both running threads should be sampled (spin_a {a}, spin_b {b}, state {state:?})\n{}",
        explain(&profile, &binary)
    );
    assert!(
        state.is_some_and(|state| state != 'T' && state != 't'),
        "a thread was left stopped (state {state:?})"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kernel_frames_are_recorded_when_the_thread_is_in_the_kernel() {
    let _lock = LiveLock::acquire();
    let dir = fixture_dir("kread");
    let source = dir.join("kread.cpp");
    let binary = dir.join("kread");
    std::fs::write(&source, KREAD_CPP).unwrap();
    if !compile(&source, &binary, &[]) {
        return;
    }
    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kread");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(50));
    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(500)),
            sampling_interval: Some(Duration::from_millis(10)),
            include_kernel: true,
            ..anyprof_core::RecordOptions::default()
        },
    );
    let _ = child.kill();
    let _ = child.wait();
    let session = match started {
        Ok(session) => session,
        Err(err) => {
            let message = err.to_string();
            let blocked = message.contains("perf_event_open")
                && (message.contains("paranoid") || message.contains("-k"));
            if blocked && !native_required() {
                eprintln!("skipping -k live sample: {message}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            panic!("-k profile failed: {message}");
        }
    };
    let profile = session.stop().expect("stop");
    let kernel: Vec<_> = profile
        .stacks
        .iter()
        .flat_map(|stack| stack.frames.iter())
        .filter(|frame| frame.runtime == Runtime::Kernel)
        .collect();
    assert!(
        !kernel.is_empty(),
        "a read loop with -k should record kernel frames\n{}",
        explain(&profile, &binary)
    );
    assert!(
        kernel
            .iter()
            .any(|frame| frame.module.as_deref() == Some("kernel")),
        "kernel frames should name the kernel module\n{}",
        explain(&profile, &binary)
    );
    if native_required() {
        assert!(
            kernel.iter().any(|frame| !frame.name.is_empty()),
            "CI sets kernel.kptr_restrict=0, so -k frames should be named\n{}",
            explain(&profile, &binary)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sampling_this_process_completes() {
    let result = anyprof_native::NativeBackend.start(
        &anyprof_core::Target {
            pid: std::process::id(),
        },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(100)),
            sampling_interval: Some(Duration::from_millis(10)),
            ..anyprof_core::RecordOptions::default()
        },
    );
    match result {
        Ok(session) => {
            let profile = session.stop().expect("stop");
            assert_eq!(profile.pid, std::process::id());
            assert!(profile.duration >= Duration::from_millis(50));
        }
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("perf_event_open"),
                "self-sample failed for an unexpected reason: {message}"
            );
            assert!(
                !message.contains("suspending"),
                "perf sampling must not refuse self-sample as a suspend: {message}"
            );
            if native_required() {
                panic!("CI should be allowed to sample this process: {message}");
            }
            eprintln!("skipping self-sample: {message}");
        }
    }
}

#[test]
fn another_users_process_explains_the_denial() {
    if euid() == 0 {
        eprintln!("skipping cross-user denial: this test is root");
        return;
    }
    let Err(err) = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid: 1 },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(200)),
            ..anyprof_core::RecordOptions::default()
        },
    ) else {
        panic!("pid 1 was sampled without root");
    };
    let message = err.to_string();
    assert!(message.contains("perf_event_open"), "{message}");
    assert!(message.contains("pid 1"), "{message}");
    assert!(
        !message.contains("suspending"),
        "a perf denial is not a suspend: {message}"
    );
}

#[test]
fn an_unrelated_same_user_process_follows_perf_not_ptrace_scope() {
    let _lock = LiveLock::acquire();
    let dir = fixture_dir("orphan");
    let source = dir.join("orphan.cpp");
    let binary = dir.join("orphan");
    std::fs::write(&source, ORPHAN_CPP).unwrap();
    if !compile(&source, &binary, &[]) {
        return;
    }
    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn orphan");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("read grandchild pid");
    let _ = child.wait();
    let pid: u32 = line
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("grandchild pid from {line:?}"));
    std::thread::sleep(Duration::from_millis(50));
    let parent = ppid(pid);
    assert_ne!(
        parent,
        Some(std::process::id()),
        "the double-fork was reparented to this test, so it is not an unrelated process"
    );
    // `perf_event_open` uses PTRACE_MODE_READ_REALCREDS. Yama applies
    // ptrace_scope to PTRACE_MODE_ATTACH, which this sampler does not
    // request, so the sample is allowed at scope 0 and at scope 1 once
    // perf_event_paranoid is low enough. The Python frame read is the
    // attach check; this native sample is not.
    let started = anyprof_native::NativeBackend.start(
        &anyprof_core::Target { pid },
        &anyprof_core::RecordOptions {
            duration: Some(Duration::from_millis(100)),
            ..anyprof_core::RecordOptions::default()
        },
    );
    let state = proc_state(pid);
    let scope = read_i32("/proc/sys/kernel/yama/ptrace_scope");
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    match started {
        Ok(session) => {
            let profile = session.stop().expect("stop");
            assert_eq!(profile.pid, pid);
            assert!(
                profile.duration >= Duration::from_millis(50),
                "the unrelated sample ended early: {:?}",
                profile.duration
            );
            assert!(
                state.is_none_or(|state| state != 'T' && state != 't'),
                "the unrelated process was left stopped (state {state:?}, ppid {parent:?}, ptrace_scope {scope:?})"
            );
        }
        Err(err) => {
            let message = err.to_string();
            if message.contains("perf_event_open") && !native_required() {
                eprintln!(
                    "skipping unrelated same-user sample (ptrace_scope {scope:?}): {message}"
                );
            } else {
                panic!(
                    "same-user sampling failed (ppid {parent:?}, ptrace_scope {scope:?}): {message}"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn ppid(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("PPid:")?;
        rest.trim().parse().ok()
    })
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

fn explain(profile: &anyprof_core::Profile, binary: &Path) -> String {
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
        "duration {:?} period {:?} weight {}\nTop stacks:\n{top}\nnm:\n{}",
        profile.duration,
        profile.period,
        profile.total_weight(),
        nm_symbols(binary)
    )
}

fn nm_symbols(binary: &Path) -> String {
    match Command::new("nm").arg(binary).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| {
                line.contains("spin") || line.contains("anyprof") || line.contains("main")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Err(err) => format!("nm failed: {err}"),
    }
}

fn compile(source: &Path, binary: &Path, extra: &[&str]) -> bool {
    let mut errors = Vec::new();
    for compiler in ["g++", "clang++"] {
        let mut command = Command::new(compiler);
        command.args([
            "-std=c++17",
            "-O0",
            "-fno-omit-frame-pointer",
            "-mno-omit-leaf-frame-pointer",
            "-fno-optimize-sibling-calls",
            "-g",
        ]);
        command.args(extra).arg("-o").arg(binary).arg(source);
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
    if native_required() {
        panic!("{message}");
    }
    eprintln!("skipping live native sample: {message}");
    false
}

/// `None` means the test skipped because perf refused the attach.
fn accept_or_skip(
    started: Result<Box<dyn anyprof_core::Session>, anyprof_core::Error>,
) -> Option<Box<dyn anyprof_core::Session>> {
    match started {
        Ok(session) => Some(session),
        Err(err) => {
            let message = err.to_string();
            if message.contains("perf_event_open") && !native_required() {
                eprintln!("skipping live native sample: {message}");
                None
            } else {
                panic!("native profile failed: {message}");
            }
        }
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

fn proc_state(pid: u32) -> Option<char> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.rsplit_once(')')?.1.trim_start();
    rest.chars().next()
}

fn read_i32(path: &str) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn euid() -> u32 {
    unsafe { geteuid() }
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

unsafe extern "C" {
    fn geteuid() -> u32;
}

const BUSY_CPP: &str = r#"
#include <fcntl.h>
#include <stdint.h>
#include <sys/mman.h>
#include <unistd.h>

namespace anyprof_native {
__attribute__((noinline)) void spin(volatile uint64_t *slot) {
    for (;;) {
        *slot += 1;
    }
}
}

int main(int argc, char **argv) {
    int fd = open(argv[1], O_RDWR);
    auto *slot = static_cast<volatile uint64_t *>(
        mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    anyprof_native::spin(slot);
}
"#;

const IDLE_CPP: &str = r#"
#include <unistd.h>
int main() {
    for (;;) {
        pause();
    }
}
"#;

const THREADS_CPP: &str = r#"
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <sys/mman.h>
#include <unistd.h>

namespace anyprof_native {
__attribute__((noinline)) void spin_a(volatile uint64_t *slot) {
    for (;;) {
        *slot += 1;
    }
}
__attribute__((noinline)) void spin_b(volatile uint64_t *slot) {
    for (;;) {
        *slot += 1;
    }
}
}

struct Slot {
    volatile uint64_t *value;
};

static void *start_b(void *arg) {
    auto *slot = static_cast<Slot *>(arg);
    anyprof_native::spin_b(slot->value);
    return nullptr;
}

int main(int argc, char **argv) {
    int fd = open(argv[1], O_RDWR);
    auto *base = static_cast<volatile uint64_t *>(
        mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    Slot slot{base + 1};
    pthread_t thread;
    pthread_create(&thread, nullptr, start_b, &slot);
    anyprof_native::spin_a(base);
}
"#;

const KREAD_CPP: &str = r#"
#include <fcntl.h>
#include <unistd.h>
int main() {
    int fd = open("/dev/zero", O_RDONLY);
    char buf[4096];
    for (;;) {
        if (read(fd, buf, sizeof buf) < 0) {
            return 1;
        }
    }
}
"#;

const ORPHAN_CPP: &str = r#"
#include <stdio.h>
#include <unistd.h>
int main() {
    if (fork() > 0) {
        _exit(0);
    }
    printf("%d\n", static_cast<int>(getpid()));
    fflush(stdout);
    for (;;) {
        pause();
    }
}
"#;
