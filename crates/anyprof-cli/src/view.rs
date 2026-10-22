use std::path::Path;

use anyhow::{Context, anyhow};
use anyprof_core::StopGate;
use anyprof_web::{Launch, StopReason, Viewer};

use crate::cli::{ViewArgs, ViewerArgs};
use crate::interrupt;

pub fn run(args: ViewArgs) -> anyhow::Result<()> {
    validate(&args.viewer)?;
    let path = std::path::absolute(&args.file).unwrap_or_else(|_| args.file.clone());
    if path.is_dir() {
        return Err(anyhow!("{} is a directory, not a profile", path.display()));
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    show(&path, bytes, &args.viewer)
}

pub fn validate(viewer: &ViewerArgs) -> anyhow::Result<()> {
    if viewer.web_timeout.is_some_and(|timeout| timeout.is_zero()) {
        return Err(anyhow!("--web-timeout must be greater than zero"));
    }
    Ok(())
}

/// Serve `bytes` (the contents of `path`) in speedscope until Ctrl+C or the
/// timeout.
pub fn show(path: &Path, bytes: Vec<u8>, viewer: &ViewerArgs) -> anyhow::Result<()> {
    if bytes.is_empty() {
        return Err(anyhow!("{} is empty", path.display()));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "profile".to_string());
    let server = Viewer::bind(&name, bytes, viewer.port.unwrap_or(0))?;
    let url = server.url();
    let stop = StopGate::new();
    interrupt::route_to(&stop)?;

    eprintln!("Viewing {} in speedscope at {url}", path.display());
    match anyprof_web::open_browser(&url, viewer.no_open) {
        Launch::Opened => eprintln!("Opened your default browser."),
        Launch::Skipped(reason) => print_fallback(&url, server.port(), reason),
        Launch::Failed(err) => {
            print_fallback(&url, server.port(), &format!("opening failed: {err}"))
        }
    }
    match viewer.web_timeout {
        Some(timeout) => eprintln!(
            "Serving until Ctrl+C or {} from now.",
            humantime::format_duration(timeout)
        ),
        None => eprintln!("Serving until Ctrl+C."),
    }

    let reason = server.serve(|| stop.stop_requested(), viewer.web_timeout)?;
    match reason {
        StopReason::Stopped => eprintln!("viewer stopped"),
        StopReason::TimedOut => eprintln!("viewer stopped (--web-timeout reached)"),
    }
    Ok(())
}

fn print_fallback(url: &str, port: u16, reason: &str) {
    eprintln!("Not opening a browser: {reason}.");
    eprintln!("Open {url} in a browser on this machine.");
    eprintln!(
        "From another machine, forward the port first: {}",
        anyprof_web::ssh_forward_hint(port)
    );
}
