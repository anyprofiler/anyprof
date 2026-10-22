use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use anyprof_core::{
    Backend, Profile, RecordOptions, Registry, SignalAction, StopGate, Target,
    interval_for_frequency,
};
use anyprof_export::SampleCounts;

use crate::cli::{ProfileFormat, RecordArgs};
use crate::output::{self, OutputRequest};
use crate::registry::default_registry;

pub fn run(args: RecordArgs) -> anyhow::Result<()> {
    validate(&args)?;
    let registry = default_registry();
    let (backend, pid) = if args.browser {
        let backend = registry
            .get("browser")
            .context("selecting the browser backend")?;
        (backend, 0)
    } else {
        let pid = args.pid.ok_or_else(|| anyhow!("--pid is required"))?;
        let backend = select(&registry, pid, args.backend.as_deref())?;
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

    let stop = StopGate::new();
    install_stop_handler(&stop)?;
    let label = recording_label(backend.name(), &args, pid);
    tracing::info!(
        backend = backend.name(),
        pid,
        browser = args.browser,
        duration = ?args.duration,
        ?interval,
        include_kernel = args.kernel,
        inspect = args.inspect.as_deref().unwrap_or(""),
        tab = args.tab.as_deref().unwrap_or(""),
        "starting profile"
    );

    let session = watch_recording(&label, args.duration, || {
        backend.start(
            &Target { pid },
            &RecordOptions {
                duration: args.duration,
                sampling_interval: requested_interval,
                include_kernel: args.kernel,
                inspect: args.inspect.clone(),
                tab: args.tab.clone(),
                stop: Arc::clone(&stop),
            },
        )
    })?;
    // Profiler.stop runs inside the backend. Saving the file is the rest of
    // that phase: a second Ctrl+C here exits immediately.
    stop.begin_stopping();
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
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    let absolute = std::path::absolute(&path).unwrap_or(path);
    eprintln!(
        "wrote {} samples to {} ({})",
        counts.total,
        absolute.display(),
        format.as_str()
    );
    Ok(())
}

fn install_stop_handler(gate: &Arc<StopGate>) -> anyhow::Result<()> {
    let gate = Arc::clone(gate);
    ctrlc::set_handler(move || match gate.note_signal() {
        SignalAction::StopSampling => {}
        SignalAction::ForceExit => {
            eprintln!("\nanyprof: second interrupt, exiting immediately");
            let _ = std::io::stderr().flush();
            std::process::exit(130);
        }
    })
    .context("installing the Ctrl+C handler")?;
    Ok(())
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

fn watch_recording<T>(label: &str, limit: Option<Duration>, sample: impl FnOnce() -> T) -> T {
    let tty = std::io::stderr().is_terminal();
    if !tty {
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
            "{} of {} samples ({}%) are (idle) or (program) and only {} caught work, so the process was mostly waiting. \
             To profile request handling, record while it is under load, for example by sending it requests during the recording",
            counts.idle,
            counts.total,
            u128::from(counts.idle) * 100 / u128::from(counts.total),
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
    if args
        .interval
        .is_some_and(|interval| interval < Duration::from_micros(1))
    {
        return Err(anyhow!("--interval must be at least 1us"));
    }
    if args.tab.is_some() && !args.browser {
        return Err(anyhow!(
            "--tab selects a browser page and requires --browser"
        ));
    }
    if args.browser {
        if args.pid.is_some() {
            return Err(anyhow!("--browser profiles a tab; omit --pid"));
        }
        if let Some(name) = args.backend.as_deref() {
            if name != "browser" {
                return Err(anyhow!(
                    "--browser cannot be combined with --backend {name}"
                ));
            }
        }
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
        describe_interval, format_clock, requested_interval, status_line, uninformative_reason,
        validate,
    };
    use crate::cli::RecordArgs;
    use anyprof_core::{Frame, Profile, Runtime, Stack};
    use anyprof_export::{SampleCounts, sample_counts};
    use clap::Parser;
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
        assert!(message.contains("849 of 850 samples (99%)"), "{message}");
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
}
