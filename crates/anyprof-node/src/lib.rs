//! Node.js CPU sampling backend.
//!
//! Detection looks at the process name and executable (`node` or `nodejs`) and
//! does not open the inspector. The intended sampler attaches to the V8
//! inspector and collects a CPU profile over the Chrome DevTools Protocol.
//! [`NodeBackend::start`] is still a stub.

use anyprof_core::{
    inspect, Backend, Capabilities, Error, ProcessSnapshot, RecordOptions, Session, Target,
};

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
        // TODO(node): drive the V8 inspector CPU profiler over CDP.
        let _ = (target, opts);
        Err(Error::not_implemented(
            self.name(),
            "V8 inspector CPU profiles via the Chrome DevTools Protocol are not built yet",
        ))
    }
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
    base == "node" || base == "nodejs"
}

#[cfg(test)]
mod tests {
    use super::looks_like_node;
    use anyprof_core::{Backend, Error, ProcessSnapshot, RecordOptions, Target};
    use std::path::PathBuf;

    use crate::NodeBackend;

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
    }

    #[test]
    fn missing_pid_is_not_node() {
        assert!(!NodeBackend.detect(u32::MAX));
    }

    #[test]
    fn start_reports_not_implemented() {
        let Err(err) = NodeBackend.start(&Target { pid: 1 }, &RecordOptions::default()) else {
            panic!("node start should fail until the inspector sampler exists");
        };
        assert!(matches!(
            err,
            Error::NotImplemented {
                backend: "node",
                ..
            }
        ));
    }
}
