//! Linux native CPU sampling backend.
//!
//! The intended sampler is `perf_event_open` plus user-space unwinding (frame
//! pointers and DWARF CFI), including kernel frames when requested. Nothing
//! attaches to a process yet: [`NativeBackend::start`] returns a clear error.
//!
//! Register this backend last. [`NativeBackend::detect`] is true for any live
//! pid so it acts as the fallback when no language runtime matched.

use anyprof_core::{process_exists, Backend, Capabilities, Error, RecordOptions, Session, Target};

/// Native (C, C++, Rust, and other non-language-runtime) sampler.
#[derive(Debug, Default)]
pub struct NativeBackend;

impl Backend for NativeBackend {
    fn name(&self) -> &'static str {
        "native"
    }

    fn detect(&self, pid: u32) -> bool {
        let exists = process_exists(pid);
        tracing::debug!(pid, exists, "native detect");
        exists
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            on_cpu: true,
            off_cpu: false,
            mixed_stacks: false,
            // Kernel stacks are a Linux perf feature. macOS still builds this
            // crate, but the sampler will not claim kernel frames there.
            kernel_frames: cfg!(target_os = "linux"),
            needs_root: true,
        }
    }

    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error> {
        // TODO(native): sample with perf_event_open and unwind user (and optional kernel) stacks.
        let _ = (target, opts);
        Err(Error::not_implemented(
            self.name(),
            "Linux on-CPU sampling via perf events is not built yet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::NativeBackend;
    use anyprof_core::{Backend, Error, RecordOptions, Target};

    #[test]
    fn missing_pid_is_not_native() {
        assert!(!NativeBackend.detect(u32::MAX));
    }

    #[test]
    fn start_reports_not_implemented() {
        let Err(err) = NativeBackend.start(&Target { pid: 1 }, &RecordOptions::default()) else {
            panic!("native start should fail until sampling exists");
        };
        match err {
            Error::NotImplemented { backend, .. } => assert_eq!(backend, "native"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn current_process_is_detected() {
        assert!(NativeBackend.detect(std::process::id()));
    }
}
