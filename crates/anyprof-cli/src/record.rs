use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use anyprof_core::{Backend, Profile, RecordOptions, Registry, SignalAction, StopGate, Target};

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
    if args.browser {
        eprintln!(
            "using backend 'browser' ({})",
            backend.capabilities().summary()
        );
    } else {
        eprintln!(
            "using backend '{}' for pid {} ({})",
            backend.name(),
            pid,
            backend.capabilities().summary()
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
        frequency_hz = args.frequency,
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
                frequency_hz: args.frequency,
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
    if let Some(warning) = uninformative_reason(&profile) {
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
        profile.total_weight(),
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

fn uninformative_reason(profile: &Profile) -> Option<&'static str> {
    let has_sample = profile
        .stacks
        .iter()
        .any(|stack| stack.weight > 0 && !stack.frames.is_empty());
    if !has_sample {
        return Some("profile has 0 samples");
    }
    let useful = profile
        .stacks
        .iter()
        .any(|stack| stack.frames.iter().any(|frame| !is_meta_frame(&frame.name)));
    if useful {
        None
    } else {
        Some("profile only contains (idle), (program), or (garbage collector)")
    }
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
    use super::{format_clock, status_line, validate};
    use crate::cli::RecordArgs;
    use clap::Parser;
    use std::time::Duration;

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
}
