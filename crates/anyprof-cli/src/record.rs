use anyhow::Context;
use anyprof_core::{Backend, RecordOptions, Registry, Target};

use crate::cli::RecordArgs;
use crate::registry::default_registry;

pub fn run(args: RecordArgs) -> anyhow::Result<()> {
    let registry = default_registry();
    let backend = select(&registry, args.pid, args.backend.as_deref())?;
    eprintln!(
        "using backend '{}' for pid {} ({})",
        backend.name(),
        args.pid,
        backend.capabilities().summary()
    );
    tracing::info!(
        backend = backend.name(),
        pid = args.pid,
        duration = ?args.duration,
        frequency_hz = args.frequency,
        include_kernel = args.kernel,
        "starting profile"
    );

    let session = backend.start(
        &Target { pid: args.pid },
        &RecordOptions {
            duration: args.duration,
            frequency_hz: args.frequency,
            include_kernel: args.kernel,
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
