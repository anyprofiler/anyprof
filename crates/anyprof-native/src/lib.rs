//! Native (C, C++, and later Rust) CPU sampling backend.
//!
//! Roadmap order is macOS, then Windows, then Linux. The Linux sampler will be
//! `perf_event_open` plus user-space unwinding (frame pointers and DWARF CFI),
//! including kernel frames when requested. macOS and Windows samplers come
//! first and are not built yet. [`NativeBackend::start`] returns a clear error
//! on every platform.
//!
//! C++ is the next native target after Node.js and browser pages. A
//! Rust-specific sampler comes after that; until then Rust processes fall
//! through to this backend.
//!
//! Register this backend last. [`NativeBackend::detect`] is true for any live
//! pid so it acts as the fallback when no language runtime matched.

use anyprof_core::{Backend, Capabilities, Error, RecordOptions, Session, Target, process_exists};

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
        // TODO(native): C++ on-CPU sampling, macOS first, then Windows, then Linux
        // perf_event_open. Rust-specific stacks come after the C++ sampler.
        // Keep this backend as the pid fallback until those samplers exist.
        let _ = (target, opts);
        Err(Error::not_implemented(
            self.name(),
            "native on-CPU sampling is not built yet (planned order: macOS, Windows, then Linux perf events)",
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
