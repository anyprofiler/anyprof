use anyhow::{Context, anyhow};
use anyprof_core::{Backend, Profile, RecordOptions, Registry, Target};

use crate::cli::{ProfileFormat, RecordArgs, infer_format};
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
    if let Some(warning) = uninformative_reason(&profile) {
        eprintln!("warning: {warning}");
    }
    let format = output_format(&args, backend.name());
    let bytes = anyprof_export::export(&profile, format.into())?;
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

fn output_format(args: &RecordArgs, backend: &str) -> ProfileFormat {
    if let Some(format) = args.format {
        return format;
    }
    if let Some(format) = infer_format(&args.output) {
        return format;
    }
    match backend {
        "node" | "browser" => ProfileFormat::Cpuprofile,
        _ => ProfileFormat::Collapsed,
    }
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
        let mut full = vec!["anyprof", "record"];
        if !args.iter().any(|arg| *arg == "-o" || *arg == "--output") {
            full.extend(["-o", "out"]);
        }
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

    #[test]
    fn output_format_uses_the_extension_then_the_backend() {
        let node = record(&["--pid", "1", "-o", "cpu.cpuprofile"]);
        assert_eq!(
            super::output_format(&node, "node"),
            crate::cli::ProfileFormat::Cpuprofile
        );
        let bare = record(&["--pid", "1", "-o", "cpu"]);
        assert_eq!(
            super::output_format(&bare, "node"),
            crate::cli::ProfileFormat::Cpuprofile
        );
        assert_eq!(
            super::output_format(&bare, "browser"),
            crate::cli::ProfileFormat::Cpuprofile
        );
        assert_eq!(
            super::output_format(&bare, "native"),
            crate::cli::ProfileFormat::Collapsed
        );
        let collapsed = record(&["--pid", "1", "-o", "cpu.collapsed"]);
        assert_eq!(
            super::output_format(&collapsed, "node"),
            crate::cli::ProfileFormat::Collapsed
        );
        let explicit = record(&["--pid", "1", "--format", "pprof", "-o", "cpu.cpuprofile"]);
        assert_eq!(
            super::output_format(&explicit, "node"),
            crate::cli::ProfileFormat::Pprof
        );
    }

    #[test]
    fn warns_on_empty_and_idle_only_profiles() {
        use anyprof_core::{Frame, Runtime, Stack};

        assert_eq!(
            super::uninformative_reason(&anyprof_core::Profile::default()),
            Some("profile has 0 samples")
        );
        let idle = anyprof_core::Profile {
            stacks: vec![
                Stack {
                    frames: vec![Frame::new(Runtime::Node, "(idle)")],
                    weight: 10,
                },
                Stack {
                    frames: vec![Frame::new(Runtime::Node, "(program)")],
                    weight: 2,
                },
                Stack {
                    frames: vec![Frame::new(Runtime::Node, "(garbage collector)")],
                    weight: 1,
                },
            ],
            ..anyprof_core::Profile::default()
        };
        assert!(
            super::uninformative_reason(&idle)
                .unwrap()
                .contains("(idle)")
        );
        let busy = anyprof_core::Profile {
            stacks: vec![Stack {
                frames: vec![Frame::new(Runtime::Node, "anyprofSampleWork")],
                weight: 4,
            }],
            ..anyprof_core::Profile::default()
        };
        assert_eq!(super::uninformative_reason(&busy), None);
    }
}
