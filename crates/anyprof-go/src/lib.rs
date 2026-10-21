//! Go CPU sampling backend.
//!
//! Lower priority than Node.js, browser pages, and native C++. Detection still
//! scans the process executable for the Go build-info magic (`\xffGo buildinf:`)
//! on Linux so a Go process is not labeled native. It does not ptrace the
//! process. Collecting a CPU profile from an arbitrary live Go runtime is not
//! implemented yet.

mod binary;

use anyprof_core::{executable_file, Backend, Capabilities, Error, RecordOptions, Session, Target};

/// Go sampler.
#[derive(Debug, Default)]
pub struct GoBackend;

impl Backend for GoBackend {
    fn name(&self) -> &'static str {
        "go"
    }

    fn detect(&self, pid: u32) -> bool {
        let Some(path) = executable_file(pid) else {
            tracing::debug!(pid, "go detect: executable is not readable");
            return false;
        };
        match binary::contains_go_build_info(&path) {
            Ok(found) => {
                tracing::debug!(pid, found, "go detect");
                found
            }
            Err(err) => {
                tracing::debug!(pid, %err, "go detect: build-info scan failed");
                false
            }
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            on_cpu: true,
            off_cpu: false,
            mixed_stacks: true,
            kernel_frames: false,
            needs_root: true,
        }
    }

    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error> {
        // TODO(go): lower priority than Node, browser pages, and native C++.
        // Walk a live Go runtime and collect CPU samples. Linux build-info
        // detection stays in place so Go is not classified as native.
        let _ = (target, opts);
        Err(Error::not_implemented(
            self.name(),
            "sampling a live Go runtime is not built yet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::GoBackend;
    use anyprof_core::{Backend, Error, RecordOptions, Target};

    #[test]
    fn missing_pid_is_not_go() {
        assert!(!GoBackend.detect(u32::MAX));
    }

    #[test]
    fn start_reports_not_implemented() {
        let Err(err) = GoBackend.start(&Target { pid: 1 }, &RecordOptions::default()) else {
            panic!("go start should fail until sampling exists");
        };
        assert!(matches!(err, Error::NotImplemented { backend: "go", .. }));
    }
}
