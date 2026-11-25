use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::{Child, Command as ProcessCommand, ExitCode, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use anyprof_core::{
    Backend, Profile, RecordOptions, Registry, StopGate, Target, interval_for_frequency,
};
use anyprof_export::SampleCounts;

use crate::cli::{BackendName, ProfileFormat, RecordArgs};
use crate::output::{self, OutputRequest};
use crate::registry::default_registry;
use crate::{interrupt, proc_match, view};

pub fn run(args: RecordArgs, verbose: u8) -> anyhow::Result<ExitCode> {
    validate(&args)?;
    let registry = default_registry();

    let stop = StopGate::new();
    interrupt::route_to(&stop)?;

    let mut child_guard = if !args.command.is_empty() {
        Some(SpawnedTarget::spawn(&args.command)?)
    } else {
        None
    };

    let (backend, pid) = if args.browser {
        let backend = registry
            .get("browser")
            .context("selecting the browser backend")?;
        (backend, 0)
    } else if let Some(ref mut spawned) = child_guard {
        let pid = spawned.pid();
        let backend = select_for_spawn(&registry, spawned, &args, &stop)?;
        spawned.start_exit_watcher(Arc::clone(&stop));
        (backend, pid)
    } else if let Some(ref name) = args.name {
        let pid = proc_match::resolve_pid_by_name(name)?;
        let backend = select(&registry, pid, args.backend.map(BackendName::as_str))?;
        (backend, pid)
    } else {
        let pid = args.pid.ok_or_else(|| anyhow!("--pid is required"))?;
        let backend = select(&registry, pid, args.backend.map(BackendName::as_str))?;
        (backend, pid)
    };

    let requested_interval = requested_interval(&args);
    let interval = requested_interval.unwrap_or_else(|| backend.default_sampling_interval());
    if args.browser {
        eprintln!(
            "using backend 'browser' ({}), {}",
            backend.capabilities().summary(),
            describe_interval(interval)
        );
    } else {
        eprintln!(
            "using backend '{}' for pid {} ({}), {}",
            backend.name(),
            pid,
            backend.capabilities().summary(),
            describe_interval(interval)
        );
    }

    let label = recording_label(backend.name(), &args, pid);
    tracing::info!(
        backend = backend.name(),
        pid,
        browser = args.browser,
        spawned = child_guard.is_some(),
        duration = ?args.duration,
        ?interval,
        include_kernel = args.kernel,
        inspect = args.inspect.as_deref().unwrap_or(""),
        tab = args.tab.as_deref().unwrap_or(""),
        "starting profile"
    );

    let start_res = watch_recording(&label, args.duration, verbose, || {
        backend.start(
            &Target { pid },
            &RecordOptions {
                duration: args.duration,
                sampling_interval: requested_interval,
                include_kernel: args.kernel,
                inspect: args.inspect.clone(),
                tab: args.tab.clone(),
                keep_inspector: args.keep_inspector,
                stop: Arc::clone(&stop),
            },
        )
    });
    let session = match start_res {
        Ok(session) => session,
        Err(err) => {
            if let Some(ref mut spawned) = child_guard
                && let Some(status) = spawned.poll_exit()
                && !status.success()
            {
                return Err(anyhow!(
                    "spawned command exited with {status} before sampling completed ({err})"
                ));
            }
            return Err(err.into());
        }
    };
    // Profiler.stop runs inside the backend. Saving the file is the rest of
    // that phase: a second Ctrl+C here exits immediately.
    stop.begin_stopping();
    let child_exit = child_guard.as_mut().and_then(SpawnedTarget::finish);
    let profile = session.stop()?;
    let counts = anyprof_export::sample_counts(&profile);
    if let Some(warning) = uninformative_reason(&profile, counts) {
        eprintln!("warning: {warning}");
    }
    let (path, format) = output::resolve_output(&OutputRequest {
        output: args.output.as_deref(),
        explicit: args.format,
        default_format: ProfileFormat::from(backend.default_format()),
        backend: backend.name(),
        label: &output::profile_label(args.browser, pid, args.tab.as_deref()),
        stamp: &output::local_stamp(),
        cwd: &std::env::current_dir().context("reading the current directory")?,
        home: output::home_dir().as_deref(),
    })?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let bytes = anyprof_export::export(&profile, format.into())?;
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    let absolute = std::path::absolute(&path).unwrap_or(path);
    eprintln!(
        "wrote {} samples to {} ({})",
        counts.total,
        absolute.display(),
        format.as_str()
    );
    if args.web {
        view::show(&absolute, bytes, &args.viewer)?;
    }
    Ok(exit_code_from_child(child_exit))
}

struct ChildHolder {
    child: Child,
    exited: Option<ExitStatus>,
}

struct SpawnedTarget {
    pid: u32,
    state: Arc<Mutex<ChildHolder>>,
    done: Arc<AtomicBool>,
    watcher: Option<thread::JoinHandle<()>>,
}

impl SpawnedTarget {
    fn spawn(command: &[std::ffi::OsString]) -> anyhow::Result<Self> {
        let program = command
            .first()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| anyhow!("command after -- must not be empty"))?;
        let child = ProcessCommand::new(program)
            .args(&command[1..])
            .spawn()
            .with_context(|| format!("spawning '{}'", Path::new(program).display()))?;
        let pid = child.id();
        Ok(Self {
            pid,
            state: Arc::new(Mutex::new(ChildHolder {
                child,
                exited: None,
            })),
            done: Arc::new(AtomicBool::new(false)),
            watcher: None,
        })
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    fn poll_exit(&self) -> Option<ExitStatus> {
        let mut guard = self.state.lock().unwrap();
        if let Some(status) = guard.exited {
            return Some(status);
        }
        if let Ok(Some(status)) = guard.child.try_wait() {
            guard.exited = Some(status);
            return Some(status);
        }
        None
    }

    fn start_exit_watcher(&mut self, stop: Arc<StopGate>) {
        let state = Arc::clone(&self.state);
        let done = Arc::clone(&self.done);
        self.watcher = Some(thread::spawn(move || {
            while !done.load(Ordering::Relaxed) && !stop.stop_requested() {
                {
                    let mut guard = state.lock().unwrap();
                    if guard.exited.is_some() {
                        stop.request_stop();
                        break;
                    }
                    if let Ok(Some(status)) = guard.child.try_wait() {
                        guard.exited = Some(status);
                        stop.request_stop();
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        }));
    }

    /// Stop the watcher; returns `Some(status)` if the child exited on its own,
    /// or terminates and reaps the child (`None`) when `-d` / `Ctrl+C` ended sampling first.
    fn finish(&mut self) -> Option<ExitStatus> {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.watcher.take() {
            let _ = handle.join();
        }
        let mut guard = self.state.lock().unwrap();
        if let Some(status) = guard.exited {
            return Some(status);
        }
        if let Ok(Some(status)) = guard.child.try_wait() {
            guard.exited = Some(status);
            return Some(status);
        }
        let _ = guard.child.kill();
        let _ = guard.child.wait();
        None
    }
}

impl Drop for SpawnedTarget {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.watcher.take() {
            let _ = handle.join();
        }
        if let Ok(mut guard) = self.state.lock()
            && guard.exited.is_none()
        {
            if let Ok(Some(status)) = guard.child.try_wait() {
                guard.exited = Some(status);
            } else {
                let _ = guard.child.kill();
                let _ = guard.child.wait();
            }
        }
    }
}

fn exit_code_from_child(status: Option<ExitStatus>) -> ExitCode {
    let Some(status) = status else {
        return ExitCode::SUCCESS;
    };
    if status.success() {
        return ExitCode::SUCCESS;
    }
    if let Some(code) = status.code() {
        let clamped = u8::try_from(code.clamp(1, 255)).unwrap_or(1);
        return ExitCode::from(clamped);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            let code = u8::try_from((128 + sig).clamp(1, 255)).unwrap_or(1);
            return ExitCode::from(code);
        }
    }
    ExitCode::from(1)
}

fn spawn_runtime_hint(program: &OsStr) -> Option<&'static str> {
    let raw = Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .to_ascii_lowercase();
    let base = raw.strip_suffix(".exe").unwrap_or(&raw);
    if base == "python"
        || base == "python3"
        || base == "py"
        || base.starts_with("python3.")
        || base.starts_with("python2.")
    {
        Some("python")
    } else if base == "java" || base == "javaw" {
        Some("java")
    } else if base == "node" || base == "nodejs" {
        Some("node")
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn linux_catches_sigusr1(pid: u32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    let Some(hex) = status
        .lines()
        .find_map(|line| line.strip_prefix("SigCgt:").map(str::trim))
    else {
        return false;
    };
    let Ok(mask) = u64::from_str_radix(hex, 16) else {
        return false;
    };
    // SIGUSR1 is signal 10 on x86_64/aarch64 Linux -> bit 9 (`1 << 9`).
    (mask & (1 << 9)) != 0
}

fn select_for_spawn<'a>(
    registry: &'a Registry,
    spawned: &SpawnedTarget,
    args: &RecordArgs,
    stop: &StopGate,
) -> anyhow::Result<&'a dyn Backend> {
    let pid = spawned.pid();
    let explicit = args.backend.map(BackendName::as_str);
    let hint = explicit.or_else(|| args.command.first().and_then(|p| spawn_runtime_hint(p)));
    let started = Instant::now();
    let mut delay = Duration::from_millis(2);

    match hint {
        Some("python") => {
            let budget = Duration::from_millis(500);
            while started.elapsed() < budget && !stop.stop_requested() {
                if spawned.poll_exit().is_some() {
                    break;
                }
                if anyprof_native::prepare_python(pid).is_ok() {
                    return registry.get("python").context("selecting backend 'python'");
                }
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(25));
            }
            if explicit == Some("python") || anyprof_native::pid_is_cpython(pid) {
                return registry.get("python").context("selecting backend 'python'");
            }
            select(registry, pid, None)
        }
        Some("java") => {
            let budget = Duration::from_millis(1500);
            while started.elapsed() < budget && !stop.stop_requested() {
                if spawned.poll_exit().is_some() {
                    break;
                }
                if anyprof_native::prepare_java(pid).is_ok() {
                    return registry.get("java").context("selecting backend 'java'");
                }
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(40));
            }
            if explicit == Some("java") || anyprof_native::pid_is_hotspot(pid) {
                return registry.get("java").context("selecting backend 'java'");
            }
            select(registry, pid, None)
        }
        Some("node") => {
            let node = registry.get("node").context("selecting backend 'node'")?;
            let budget = Duration::from_millis(500);
            while started.elapsed() < budget && !stop.stop_requested() {
                if spawned.poll_exit().is_some() {
                    break;
                }
                if node.detect(pid) {
                    #[cfg(target_os = "linux")]
                    let ready = args.inspect.is_some() || linux_catches_sigusr1(pid);
                    #[cfg(not(target_os = "linux"))]
                    let ready =
                        args.inspect.is_some() || started.elapsed() >= Duration::from_millis(80);
                    if ready {
                        return Ok(node);
                    }
                }
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(25));
            }
            if explicit == Some("node") || node.detect(pid) {
                return Ok(node);
            }
            select(registry, pid, None)
        }
        _ => select(registry, pid, explicit),
    }
}

fn recording_label(backend: &str, args: &RecordArgs, pid: u32) -> String {
    if args.browser {
        match args.tab.as_deref() {
            Some(tab) => format!("browser tab {tab}"),
            None => "browser tab".to_string(),
        }
    } else {
        format!("{backend} pid {pid}")
    }
}

fn watch_recording<T>(
    label: &str,
    limit: Option<Duration>,
    verbose: u8,
    sample: impl FnOnce() -> T,
) -> T {
    let tty = std::io::stderr().is_terminal();
    let rust_log = std::env::var("RUST_LOG").ok();
    let redraw = should_redraw_status(tty, verbose, rust_log.as_deref())
        && !tracing::enabled!(tracing::Level::INFO);
    if !redraw {
        eprintln!("Recording {label}... (Ctrl+C to stop)");
        return sample();
    }
    let done = Arc::new(AtomicBool::new(false));
    let done_flag = Arc::clone(&done);
    let label = label.to_string();
    let painter = thread::spawn(move || {
        let started = Instant::now();
        while !done_flag.load(Ordering::Relaxed) {
            paint(&status_line(&label, started.elapsed(), limit));
            thread::sleep(Duration::from_millis(200));
        }
    });
    let result = sample();
    done.store(true, Ordering::Relaxed);
    let _ = painter.join();
    paint("");
    result
}

/// Whether stderr should continuously overwrite the recording status line.
///
/// Disabled when stderr is not a terminal or when `-v` / `RUST_LOG` enables
/// `info`, `debug`, or `trace` output that `\r\x1b[2K` would erase.
fn should_redraw_status(is_tty: bool, verbose: u8, rust_log: Option<&str>) -> bool {
    if !is_tty || verbose > 0 {
        return false;
    }
    !rust_log.is_some_and(rust_log_enables_verbose)
}

fn rust_log_enables_verbose(raw: &str) -> bool {
    for directive in raw
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let level = directive
            .rsplit_once('=')
            .map_or(directive, |(_, rhs)| rhs.trim());
        if matches!(
            level.to_ascii_lowercase().as_str(),
            "info" | "debug" | "trace"
        ) {
            return true;
        }
    }
    false
}

fn status_line(label: &str, elapsed: Duration, limit: Option<Duration>) -> String {
    let clock = format_clock(elapsed);
    match limit {
        Some(limit) => {
            let left = limit.saturating_sub(elapsed);
            format!(
                "Recording {label}... {clock} elapsed, {} left (Ctrl+C to stop)",
                format_clock(left)
            )
        }
        None => format!("Recording {label}... {clock} elapsed (Ctrl+C to stop)"),
    }
}

fn format_clock(duration: Duration) -> String {
    let total = duration.as_secs();
    let minutes = total / 60;
    let seconds = total % 60;
    if minutes >= 60 {
        format!("{}:{:02}:{:02}", minutes / 60, minutes % 60, seconds)
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn paint(line: &str) {
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\r\x1b[2K{line}");
    let _ = err.flush();
}

/// Share of `(idle)` plus `(program)` samples above which a recording is
/// reported as mostly idle.
const IDLE_WARNING_PERCENT: u64 = 95;

/// A mostly idle recording with at least this many JavaScript, native, or GC
/// samples still shows the work that happened, so it does not warn.
const IDLE_WARNING_MAX_WORK_SAMPLES: u64 = 100;

fn uninformative_reason(profile: &Profile, counts: SampleCounts) -> Option<String> {
    let has_sample = profile
        .stacks
        .iter()
        .any(|stack| stack.weight > 0 && !stack.frames.is_empty());
    if !has_sample || counts.total == 0 {
        return Some("profile has 0 samples".to_string());
    }
    if counts.idle_above(IDLE_WARNING_PERCENT) && counts.non_idle() < IDLE_WARNING_MAX_WORK_SAMPLES
    {
        return Some(format!(
            "{} of {} samples ({}) are (idle) or (program) and only {} caught work, so the process was mostly waiting. \
             To profile request handling, record while it is under load, for example by sending it requests during the recording",
            counts.idle,
            counts.total,
            idle_percent(counts),
            counts.non_idle()
        ));
    }
    let useful = profile
        .stacks
        .iter()
        .any(|stack| stack.frames.iter().any(|frame| !is_meta_frame(&frame.name)));
    if useful {
        None
    } else {
        Some("profile only contains (idle), (program), or (garbage collector)".to_string())
    }
}

/// Idle share with one decimal for a profile that is above
/// [`IDLE_WARNING_PERCENT`].
///
/// Rounds down, so `100.0%` means every sample was idle, except that a share
/// just above the threshold is shown as `95.1%` rather than the `95.0%` it
/// exceeded.
fn idle_percent(counts: SampleCounts) -> String {
    let total = u128::from(counts.total.max(1));
    let floor = u128::from(counts.idle) * 1000 / total;
    let threshold = u128::from(IDLE_WARNING_PERCENT) * 10;
    let tenths = if floor <= threshold && counts.idle_above(IDLE_WARNING_PERCENT) {
        threshold + 1
    } else {
        floor
    };
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// `--interval`, or `--frequency` converted to an interval. `None` leaves the
/// choice to [`Backend::default_sampling_interval`].
fn requested_interval(args: &RecordArgs) -> Option<Duration> {
    args.interval
        .or_else(|| args.frequency.map(interval_for_frequency))
}

fn describe_interval(interval: Duration) -> String {
    let hertz = 1.0 / interval.as_secs_f64();
    let rate = if hertz >= 10.0 {
        format!("{hertz:.0} Hz")
    } else {
        format!("{hertz:.2} Hz")
    };
    format!("sampling every {}us ({rate})", interval.as_micros())
}

fn is_meta_frame(name: &str) -> bool {
    matches!(
        name,
        "(idle)" | "(program)" | "(garbage collector)" | "(root)" | ""
    )
}

fn validate(args: &RecordArgs) -> anyhow::Result<()> {
    if args.duration.is_some_and(|duration| duration.is_zero()) {
        return Err(anyhow!("--duration must be greater than zero"));
    }
    if args.keep_inspector && args.browser {
        return Err(anyhow!(
            "--keep-inspector applies to Node.js and cannot be combined with --browser"
        ));
    }
    if args
        .interval
        .is_some_and(|interval| interval < Duration::from_micros(1))
    {
        return Err(anyhow!("--interval must be at least 1us"));
    }
    if !args.web
        && let Some(flag) = args.viewer.first_flag_set()
    {
        return Err(anyhow!("{flag} configures the viewer and requires --web"));
    }
    view::validate(&args.viewer)?;
    if args.tab.is_some() && !args.browser {
        return Err(anyhow!(
            "--tab selects a browser page and requires --browser"
        ));
    }
    if args.browser {
        if args.pid.is_some() {
            return Err(anyhow!("--browser profiles a tab; omit --pid"));
        }
        if args.name.is_some() {
            return Err(anyhow!("--browser profiles a tab; omit --name"));
        }
        if !args.command.is_empty() {
            return Err(anyhow!(
                "--browser profiles a tab and cannot be combined with -- <COMMAND>"
            ));
        }
        if let Some(backend) = args.backend
            && backend != BackendName::Browser
        {
            return Err(anyhow!(
                "--browser cannot be combined with --backend {}",
                backend.as_str()
            ));
        }
    }
    let selectors = usize::from(args.pid.is_some())
        + usize::from(args.name.is_some())
        + usize::from(args.browser)
        + usize::from(!args.command.is_empty());
    if selectors == 0 {
        return Err(anyhow!(
            "one of --pid, --name, --browser, or -- <COMMAND> is required"
        ));
    }
    if selectors > 1 {
        return Err(anyhow!(
            "--pid, --name, --browser, and -- <COMMAND> are mutually exclusive"
        ));
    }
    if let Some(name) = &args.name
        && name.trim().is_empty()
    {
        return Err(anyhow!("--name must not be empty"));
    }
    if let Some(first) = args.command.first()
        && first.is_empty()
    {
        return Err(anyhow!("command after -- must not be empty"));
    }
    Ok(())
}

fn select<'a>(
    registry: &'a Registry,
    pid: u32,
    explicit: Option<&str>,
) -> anyhow::Result<&'a dyn Backend> {
    match explicit {
        Some(name) => registry
            .get(name)
            .with_context(|| format!("selecting backend '{name}'")),
        None => registry
            .detect(pid)
            .with_context(|| format!("auto-detecting a backend for pid {pid}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        describe_interval, format_clock, idle_percent, requested_interval, should_redraw_status,
        spawn_runtime_hint, status_line, uninformative_reason, validate,
    };
    use crate::cli::RecordArgs;
    use anyprof_core::{Frame, Profile, Runtime, Stack};
    use anyprof_export::{SampleCounts, sample_counts};
    use clap::Parser;
    use std::ffi::OsStr;
    use std::time::Duration;

    fn profile(stacks: &[(&[&str], u64)]) -> Profile {
        Profile {
            stacks: stacks
                .iter()
                .map(|(names, weight)| Stack {
                    frames: names
                        .iter()
                        .map(|name| Frame::new(Runtime::Node, *name))
                        .collect(),
                    weight: *weight,
                })
                .collect(),
            ..Profile::default()
        }
    }

    fn reason(profile: &Profile) -> Option<String> {
        uninformative_reason(profile, sample_counts(profile))
    }

    fn record(args: &[&str]) -> RecordArgs {
        let mut full = vec!["anyprof", "record"];
        full.extend_from_slice(args);
        let cli = crate::cli::Cli::try_parse_from(full).unwrap();
        let crate::cli::Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        args
    }

    #[test]
    fn tab_requires_browser_and_browser_rejects_other_backends() {
        let tab_only = record(&["--pid", "1", "--tab", "example.com"]);
        let message = validate(&tab_only).unwrap_err().to_string();
        assert!(message.contains("requires --browser"), "{message}");

        let mixed = record(&["--browser", "--pid", "1", "--backend", "node"]);
        let message = validate(&mixed).unwrap_err().to_string();
        assert!(message.contains("omit --pid"), "{message}");

        let backend = record(&["--browser", "--backend", "node"]);
        let message = validate(&backend).unwrap_err().to_string();
        assert!(message.contains("--backend node"), "{message}");

        let zero = record(&["--pid", "1", "-d", "0s"]);
        let message = validate(&zero).unwrap_err().to_string();
        assert!(message.contains("greater than zero"), "{message}");
    }

    #[test]
    fn target_selectors_are_mutually_exclusive_and_runtime_hints_match() {
        assert!(validate(&record(&["-n", "my-app"])).is_ok());
        assert!(validate(&record(&["--", "python3", "spin.py"])).is_ok());

        for bad in [
            &["-p", "1", "-n", "app"][..],
            &["-p", "1", "--", "python3"][..],
            &["-n", "app", "--", "python3"][..],
            &["-B", "--", "python3"][..],
        ] {
            let err = validate(&record(bad)).unwrap_err().to_string();
            assert!(
                err.contains("mutually exclusive") || err.contains("cannot be combined"),
                "{bad:?}: {err}"
            );
        }

        assert_eq!(
            spawn_runtime_hint(OsStr::new("/usr/bin/python3.12")),
            Some("python")
        );
        assert_eq!(spawn_runtime_hint(OsStr::new("python.exe")), Some("python"));
        assert_eq!(spawn_runtime_hint(OsStr::new("java")), Some("java"));
        assert_eq!(spawn_runtime_hint(OsStr::new("node.exe")), Some("node"));
        assert_eq!(spawn_runtime_hint(OsStr::new("./my_bench")), None);
    }

    #[test]
    fn viewer_flags_require_web() {
        for flag in [
            &["--no-open"][..],
            &["--port", "8123"][..],
            &["--web-timeout", "1m"][..],
        ] {
            let mut args = vec!["--pid", "1"];
            args.extend_from_slice(flag);
            let message = validate(&record(&args)).unwrap_err().to_string();
            assert!(message.contains("requires --web"), "{message}");
            assert!(message.contains(flag[0]), "{message}");

            args.push("--web");
            assert!(validate(&record(&args)).is_ok(), "{args:?}");
        }
        let zero = record(&["--pid", "1", "-w", "--web-timeout", "0s"]);
        let message = validate(&zero).unwrap_err().to_string();
        assert!(message.contains("--web-timeout"), "{message}");
    }

    #[test]
    fn keep_inspector_rejects_browser_and_accepts_pid() {
        let bad = record(&["--browser", "--keep-inspector"]);
        let message = validate(&bad).unwrap_err().to_string();
        assert!(message.contains("--keep-inspector"), "{message}");
        assert!(validate(&record(&["--pid", "1", "--keep-inspector"])).is_ok());
    }

    #[test]
    fn status_line_counts_up_and_down() {
        assert_eq!(format_clock(Duration::from_secs(12)), "00:12");
        assert_eq!(format_clock(Duration::from_secs(3_661)), "1:01:01");
        assert_eq!(
            status_line("node pid 1234", Duration::from_secs(12), None),
            "Recording node pid 1234... 00:12 elapsed (Ctrl+C to stop)"
        );
        assert_eq!(
            status_line(
                "node pid 1234",
                Duration::from_secs(12),
                Some(Duration::from_secs(30))
            ),
            "Recording node pid 1234... 00:12 elapsed, 00:18 left (Ctrl+C to stop)"
        );
    }

    #[test]
    fn frequency_and_interval_override_the_backend_default() {
        assert_eq!(requested_interval(&record(&["-p", "1"])), None);
        assert_eq!(
            requested_interval(&record(&["-p", "1", "-F", "1000"])),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            requested_interval(&record(&["-p", "1", "--interval", "250us"])),
            Some(Duration::from_micros(250))
        );

        let zero = record(&["-p", "1", "--interval", "0s"]);
        let message = validate(&zero).unwrap_err().to_string();
        assert!(message.contains("at least 1us"), "{message}");
        assert!(validate(&record(&["-p", "1", "--interval", "1us"])).is_ok());
    }

    #[test]
    fn interval_is_shown_in_microseconds_and_hertz() {
        assert_eq!(
            describe_interval(Duration::from_micros(100)),
            "sampling every 100us (10000 Hz)"
        );
        assert_eq!(
            describe_interval(anyprof_core::PERF_SAMPLING_INTERVAL),
            "sampling every 10101us (99 Hz)"
        );
        assert_eq!(
            describe_interval(Duration::from_secs(2)),
            "sampling every 2000000us (0.50 Hz)"
        );
    }

    #[test]
    fn mostly_idle_recordings_suggest_recording_under_load() {
        let idle = profile(&[(&["(idle)"], 848), (&["(program)"], 1), (&["handle"], 1)]);
        let message = reason(&idle).unwrap();
        assert!(message.contains("849 of 850 samples (99.8%)"), "{message}");
        assert!(message.contains("(idle) or (program)"), "{message}");
        assert!(message.contains("only 1 caught work"), "{message}");
        assert!(message.contains("under load"), "{message}");

        let busy = profile(&[(&["(idle)"], 90), (&["handle", "main"], 10)]);
        assert_eq!(reason(&busy), None);

        let boundary = profile(&[(&["(idle)"], 95), (&["handle"], 5)]);
        assert_eq!(reason(&boundary), None);

        // Beace's DevTools recording: 99.6% idle, but 335 JS samples of request handling.
        let light_load = profile(&[(&["(idle)"], 93_773), (&["parserOnHeadersComplete"], 335)]);
        assert_eq!(reason(&light_load), None);

        let just_under = profile(&[
            (&["(idle)"], 50_000),
            (&["handle"], 60),
            (&["(garbage collector)"], 39),
        ]);
        let message = reason(&just_under).unwrap();
        assert!(message.contains("only 99 caught work"), "{message}");

        let enough_work = profile(&[
            (&["(idle)"], 50_000),
            (&["handle"], 60),
            (&["(garbage collector)"], 40),
        ]);
        assert_eq!(reason(&enough_work), None);
    }

    #[test]
    fn empty_and_meta_only_profiles_keep_their_warnings() {
        assert_eq!(
            reason(&Profile::default()).as_deref(),
            Some("profile has 0 samples")
        );
        let gc = profile(&[(&["(garbage collector)"], 3), (&["(idle)"], 1)]);
        assert_eq!(
            reason(&gc).as_deref(),
            Some("profile only contains (idle), (program), or (garbage collector)")
        );
        assert_eq!(
            uninformative_reason(&gc, SampleCounts { total: 0, idle: 0 }).as_deref(),
            Some("profile has 0 samples")
        );
    }

    #[test]
    fn idle_percent_never_rounds_down_to_the_threshold() {
        let percent = |idle, total| idle_percent(SampleCounts { total, idle });
        assert_eq!(percent(96, 101), "95.1%");
        assert_eq!(percent(95_001, 100_000), "95.1%");
        assert_eq!(percent(951, 1000), "95.1%");
        assert_eq!(percent(959, 1000), "95.9%");
        assert_eq!(percent(32_530, 32_531), "99.9%");
        assert_eq!(percent(19_585, 19_585), "100.0%");

        let just_over = profile(&[(&["(idle)"], 96), (&["handle"], 5)]);
        let message = reason(&just_over).unwrap();
        assert!(message.contains("96 of 101 samples (95.1%)"), "{message}");
    }

    #[test]
    fn status_redraw_stops_when_verbose_or_rust_log_writes_to_stderr() {
        assert!(should_redraw_status(true, 0, None));
        assert!(should_redraw_status(true, 0, Some("")));
        assert!(should_redraw_status(true, 0, Some("warn")));
        assert!(should_redraw_status(true, 0, Some("error,anyprof=warn")));
        assert!(!should_redraw_status(false, 0, None));
        assert!(!should_redraw_status(true, 1, None));
        assert!(!should_redraw_status(true, 2, None));
        assert!(!should_redraw_status(true, 0, Some("info")));
        assert!(!should_redraw_status(true, 0, Some("debug")));
        assert!(!should_redraw_status(
            true,
            0,
            Some("warn,anyprof_native=trace")
        ));
    }
}
