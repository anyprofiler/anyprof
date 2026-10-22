use anyhow::{Context, anyhow};
use anyprof_core::{Backend, RecordOptions, Registry, Target};

use crate::cli::RecordArgs;
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

    let session = backend.start(
        &Target { pid },
        &RecordOptions {
            duration: args.duration,
            frequency_hz: args.frequency,
            include_kernel: args.kernel,
            inspect: args.inspect.clone(),
            tab: args.tab.clone(),
        },
    )?;
    let profile = session.stop()?;
    let format = anyprof_export::Format::from(args.format);
    let bytes = anyprof_export::export(&profile, format)?;
    std::fs::write(&args.output, bytes)
        .with_context(|| format!("writing {}", args.output.display()))?;
    eprintln!(
        "wrote {} samples to {} ({})",
        profile.total_weight(),
        args.output.display(),
        format.as_str()
    );
    Ok(())
}

fn validate(args: &RecordArgs) -> anyhow::Result<()> {
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
    use super::validate;
    use crate::cli::RecordArgs;
    use clap::Parser;

    fn record(args: &[&str]) -> RecordArgs {
        let mut full = vec!["anyprof", "record", "-o", "out"];
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

        let mixed = crate::cli::Cli::try_parse_from([
            "anyprof",
            "record",
            "--browser",
            "--pid",
            "1",
            "--backend",
            "node",
            "-o",
            "out",
        ])
        .unwrap();
        let crate::cli::Command::Record(args) = mixed.command else {
            panic!("expected record");
        };
        let message = validate(&args).unwrap_err().to_string();
        assert!(message.contains("omit --pid"), "{message}");

        let backend = crate::cli::Cli::try_parse_from([
            "anyprof",
            "record",
            "--browser",
            "--backend",
            "node",
            "-o",
            "out",
        ])
        .unwrap();
        let crate::cli::Command::Record(args) = backend.command else {
            panic!("expected record");
        };
        let message = validate(&args).unwrap_err().to_string();
        assert!(message.contains("--backend node"), "{message}");
    }
}
