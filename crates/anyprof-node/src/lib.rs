//! Node.js CPU sampling backend.
//!
//! Attaches to a running `node` or `nodejs` process by pid. The process does
//! not need `--inspect` and does not need a restart: on macOS and Linux anyprof
//! sends `SIGUSR1`, and on Windows it runs `node -e "process._debugProcess(pid)"`.
//! The V8 inspector then samples via the Chrome DevTools Protocol
//! (`Profiler.enable`, `Profiler.setSamplingInterval`, `Profiler.start`,
//! `Profiler.stop`). The `.cpuprofile` document is converted into the unified
//! [`anyprof_core::Profile`].
//!
//! `SIGUSR1`'s default action is to terminate the process. This backend sends
//! it only after the pid looks like Node. Pass `--inspect ws://...` when the
//! inspector is already open; that path does not signal.
//!
//! macOS is the primary target. The same code path runs on Windows and Linux.

mod attach;
mod ports;

use std::time::Duration;

use anyprof_cdp::{capture_cpu_profile, sampling_interval_us};
use anyprof_core::{
    Backend, Capabilities, Error, ProcessSnapshot, Profile, RecordOptions, Runtime, Session,
    Target, inspect,
};
use anyprof_export::import_cpuprofile;

/// Node.js sampler.
#[derive(Debug, Default)]
pub struct NodeBackend;

impl Backend for NodeBackend {
    fn name(&self) -> &'static str {
        "node"
    }

    fn detect(&self, pid: u32) -> bool {
        let Some(snapshot) = inspect(pid) else {
            tracing::debug!(pid, "node detect: process is not visible");
            return false;
        };
        let matched = looks_like_node(&snapshot);
        tracing::debug!(
            pid,
            comm = snapshot.comm.as_deref().unwrap_or(""),
            matched,
            "node detect"
        );
        matched
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            on_cpu: true,
            off_cpu: false,
            mixed_stacks: true,
            kernel_frames: false,
            needs_root: false,
        }
    }

    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error> {
        if opts.duration.is_zero() {
            return Err(Error::backend(
                self.name(),
                "profile duration must be greater than zero",
            ));
        }
        let snapshot = inspect(target.pid).ok_or_else(|| {
            Error::backend(
                self.name(),
                format!(
                    "refusing to activate the inspector: pid {} does not look like node",
                    target.pid
                ),
            )
        })?;
        if !looks_like_node(&snapshot) {
            return Err(Error::backend(
                self.name(),
                format!(
                    "refusing to activate the inspector: pid {} does not look like node",
                    target.pid
                ),
            ));
        }

        let websocket =
            attach::inspector_websocket(target.pid, &snapshot.cmdline, opts.inspect.as_deref())?;
        let interval_us = sampling_interval_us(opts.frequency_hz);
        let bytes = capture_cpu_profile(&websocket, opts.duration, interval_us)
            .map_err(|err| Error::backend(self.name(), err.to_string()))?;
        let imported = import_cpuprofile(&bytes, Runtime::Node)
            .map_err(|err| Error::backend(self.name(), err.to_string()))?;
        if imported.stacks.is_empty() {
            return Err(Error::backend(self.name(), "profiler returned no samples"));
        }
        let profile = finish_profile(imported, target.pid, snapshot.comm, opts, interval_us);
        tracing::info!(
            pid = target.pid,
            samples = profile.total_weight(),
            stacks = profile.stacks.len(),
            "node profile complete"
        );
        Ok(Box::new(ReadySession { profile }))
    }
}

pub(crate) fn script_hints(cmdline: &[String]) -> Vec<String> {
    cmdline
        .iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-') && !is_node_name(arg))
        .cloned()
        .collect()
}

fn finish_profile(
    mut profile: Profile,
    pid: u32,
    comm: Option<String>,
    opts: &RecordOptions,
    interval_us: u32,
) -> Profile {
    profile.pid = pid;
    if profile.comm.is_none() {
        profile.comm = comm;
    }
    if profile.duration.is_zero() {
        profile.duration = opts.duration;
    }
    if profile.period.is_none() {
        profile.period = Some(Duration::from_micros(u64::from(interval_us)));
    }
    profile
}

fn looks_like_node(snapshot: &ProcessSnapshot) -> bool {
    if snapshot.comm.as_deref().is_some_and(is_node_name) {
        return true;
    }
    if snapshot
        .exe
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .is_some_and(is_node_name)
    {
        return true;
    }
    snapshot
        .cmdline
        .first()
        .is_some_and(|argv0| is_node_name(argv0))
}

fn is_node_name(value: &str) -> bool {
    let base = value.rsplit(['/', '\\']).next().unwrap_or(value);
    let stem = match base.len().checked_sub(4) {
        Some(split) if base[split..].eq_ignore_ascii_case(".exe") => &base[..split],
        _ => base,
    };
    stem.eq_ignore_ascii_case("node") || stem.eq_ignore_ascii_case("nodejs")
}

struct ReadySession {
    profile: Profile,
}

impl Session for ReadySession {
    fn stop(self: Box<Self>) -> Result<Profile, Error> {
        Ok(self.profile)
    }
}

#[cfg(test)]
mod tests {
    use super::{is_node_name, looks_like_node, script_hints};
    use crate::NodeBackend;
    use anyprof_core::{Backend, ProcessSnapshot, RecordOptions, Target};
    use std::path::PathBuf;
    use std::time::Duration;

    fn snap(comm: Option<&str>, cmdline: &[&str], exe: Option<&str>) -> ProcessSnapshot {
        ProcessSnapshot {
            pid: 10,
            comm: comm.map(str::to_string),
            cmdline: cmdline.iter().map(|arg| (*arg).to_string()).collect(),
            exe: exe.map(PathBuf::from),
        }
    }

    #[test]
    fn detects_node_by_comm_exe_or_argv0() {
        assert!(looks_like_node(&snap(Some("node"), &[], None)));
        assert!(looks_like_node(&snap(Some("nodejs"), &[], None)));
        assert!(looks_like_node(&snap(
            None,
            &[],
            Some("/usr/local/bin/node")
        )));
        assert!(looks_like_node(&snap(
            Some("MainThread"),
            &["/usr/bin/node", "server.js"],
            None
        )));
        assert!(is_node_name(r"C:\Program Files\nodejs\node.EXE"));
        assert!(is_node_name("nodejs.exe"));
    }

    #[test]
    fn ignores_names_that_only_contain_node() {
        assert!(!looks_like_node(&snap(Some("nodemon"), &[], None)));
        assert!(!looks_like_node(&snap(
            Some("rustc"),
            &["rustc", "main.rs"],
            None
        )));
        assert!(!looks_like_node(&snap(None, &[], None)));
        assert!(!is_node_name("node.exe.bak"));
    }

    #[test]
    fn script_hints_skip_the_binary_and_flags() {
        let hints = script_hints(&[
            "node".to_string(),
            "--inspect-port=9229".to_string(),
            "server.js".to_string(),
        ]);
        assert_eq!(hints, vec!["server.js".to_string()]);
    }

    #[test]
    fn missing_pid_is_not_node() {
        assert!(!NodeBackend.detect(u32::MAX));
    }

    #[test]
    fn refuses_to_signal_a_process_that_is_not_node() {
        let Err(err) = NodeBackend.start(
            &Target { pid: u32::MAX },
            &RecordOptions {
                duration: Duration::from_secs(1),
                ..RecordOptions::default()
            },
        ) else {
            panic!("a missing pid must not be signaled");
        };
        let message = err.to_string();
        assert!(message.contains("does not look like node"), "{message}");
        assert!(message.contains("refusing"), "{message}");
    }

    #[test]
    fn refuses_to_signal_this_test_process() {
        let Err(err) = NodeBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &RecordOptions {
                duration: Duration::from_millis(200),
                ..RecordOptions::default()
            },
        ) else {
            panic!("the test binary is not node and must not be signaled");
        };
        let message = err.to_string();
        assert!(message.contains("refusing"), "{message}");
        assert!(message.contains("does not look like node"), "{message}");
    }
}
