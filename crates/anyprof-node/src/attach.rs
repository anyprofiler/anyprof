//! Turn on the V8 inspector and find its WebSocket debugger URL.
//!
//! macOS and Linux deliver `SIGUSR1`, which Node treats as "start the inspector"
//! and which the kernel treats as terminate for every other process. This module
//! is only called after the process has already been recognized as Node.
//! Windows has no `SIGUSR1`; Node's documented equivalent is
//! `process._debugProcess(pid)`, run with `node -e`.

use std::thread;
use std::time::{Duration, Instant};

use anyprof_cdp::{InspectEndpoint, choose_node, list_targets, parse_inspect};
use anyprof_core::Error;

use crate::ports::{listening_ports, prefer_inspector_port};
use crate::script_hints;

const WAIT: Duration = Duration::from_secs(5);

pub(crate) fn inspector_websocket(
    pid: u32,
    cmdline: &[String],
    inspect: Option<&str>,
) -> Result<String, Error> {
    let hints = script_hints(cmdline);
    if let Some(raw) = inspect {
        let endpoint = parse_inspect(raw).map_err(fail)?;
        if let Some(url) = endpoint.websocket_url.clone() {
            tracing::info!(pid, %url, "using the inspector websocket from --inspect");
            return Ok(url);
        }
        if let Some(url) = try_endpoint(&endpoint, &hints) {
            return Ok(url);
        }
        activate(pid)?;
        return poll_endpoint(&endpoint, &hints).ok_or_else(|| {
            fail(format!(
                "inspector did not answer at {}:{} after activation",
                endpoint.host, endpoint.port
            ))
        });
    }

    if let Some(url) = try_discovered(pid, &hints) {
        return Ok(url);
    }
    activate(pid)?;
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(url) = try_discovered(pid, &hints) {
            return Ok(url);
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let fallback = InspectEndpoint {
        host: "127.0.0.1".to_string(),
        port: 9229,
        websocket_url: None,
    };
    try_endpoint(&fallback, &hints).ok_or_else(|| {
        fail(
            "inspector did not open a node target on a port this process is listening on, or on 127.0.0.1:9229. Pass --inspect with the port or websocket URL",
        )
    })
}

fn try_discovered(pid: u32, hints: &[String]) -> Option<String> {
    for port in prefer_inspector_port(listening_ports(pid)) {
        let endpoint = InspectEndpoint {
            host: "127.0.0.1".to_string(),
            port,
            websocket_url: None,
        };
        if let Some(url) = try_endpoint(&endpoint, hints) {
            return Some(url);
        }
    }
    None
}

fn try_endpoint(endpoint: &InspectEndpoint, hints: &[String]) -> Option<String> {
    match list_targets(endpoint) {
        Ok(targets) => match choose_node(&targets, hints) {
            Ok(target) => {
                tracing::info!(
                    url = %target.websocket_url,
                    target = %target.url,
                    "selected node inspector target"
                );
                Some(target.websocket_url.clone())
            }
            Err(err) => {
                tracing::debug!(%err, host = %endpoint.host, port = endpoint.port, "no node target");
                None
            }
        },
        Err(err) => {
            tracing::debug!(%err, host = %endpoint.host, port = endpoint.port, "inspector not ready");
            None
        }
    }
}

fn poll_endpoint(endpoint: &InspectEndpoint, hints: &[String]) -> Option<String> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(url) = try_endpoint(endpoint, hints) {
            return Some(url);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn activate(pid: u32) -> Result<(), Error> {
    tracing::info!(pid, "activating the Node.js inspector");
    activate_inspector(pid).map_err(fail)?;
    if !anyprof_core::process_exists(pid) {
        return Err(fail(format!(
            "pid {pid} exited when the inspector was activated. On macOS and Linux, SIGUSR1 terminates a process that has not installed Node's handler yet"
        )));
    }
    Ok(())
}

fn activate_inspector(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        signal_sigusr1(pid)
    }
    #[cfg(windows)]
    {
        signal_debug_process(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err("activating the Node.js inspector is not supported on this platform".to_string())
    }
}

#[cfg(unix)]
fn signal_sigusr1(pid: u32) -> Result<(), String> {
    let status = std::process::Command::new("kill")
        .args(["-USR1", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|err| format!("running kill -USR1: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill -USR1 {pid} failed ({status})"))
    }
}

#[cfg(windows)]
fn signal_debug_process(pid: u32) -> Result<(), String> {
    let code = format!("process._debugProcess({pid})");
    let status = std::process::Command::new("node")
        .args(["-e", &code])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|err| {
            format!("running node -e process._debugProcess: {err} (node must be on PATH)")
        })?;
    if status.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&status.stderr);
        Err(format!(
            "process._debugProcess({pid}) failed ({}): {stderr}",
            status.status
        ))
    }
}

fn fail(message: impl Into<String>) -> Error {
    Error::backend("node", message)
}
