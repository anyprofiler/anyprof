//! Native (C, C++, and later Rust) CPU sampling backend.
//!
//! Roadmap order is macOS, then Windows, then Linux. macOS samples on-CPU
//! stacks through Mach (`task_for_pid`, frame pointers, Mach-O symbols) and
//! needs root. Windows is not built yet. The Linux sampler will be
//! `perf_event_open` plus user-space unwinding, including kernel frames when
//! requested.
//!
//! C++ is the native target after Node.js and browser pages. A Rust-specific
//! sampler comes after that; until then Rust processes fall through to this
//! backend and, on macOS, get the same Mach stacks.
//!
//! Register this backend last. [`NativeBackend::detect`] is true for any live
//! pid so it acts as the fallback when no language runtime matched.

mod macho;
mod policy;

#[cfg(target_os = "macos")]
mod macos;

use std::time::Duration;

use anyprof_core::{
    Backend, Capabilities, Error, Format, RecordOptions, Session, Target, process_exists,
};

/// Detail for [`Error::NotImplemented`] on platforms without a sampler yet.
#[cfg(windows)]
const NOT_IMPLEMENTED: &str = "native on-CPU sampling is not built on Windows yet (macOS Mach sampling is implemented; Windows is next, then Linux perf events)";

/// Detail for [`Error::NotImplemented`] on platforms without a sampler yet.
#[cfg(target_os = "linux")]
const NOT_IMPLEMENTED: &str = "native on-CPU sampling is not built on Linux yet (macOS Mach sampling is implemented; Linux perf events come after Windows)";

/// Detail for [`Error::NotImplemented`] on platforms without a sampler yet.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const NOT_IMPLEMENTED: &str = "native on-CPU sampling is not built on this platform (macOS Mach sampling is implemented; Windows is next, then Linux perf events)";

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
            // Kernel stacks are a Linux perf feature. macOS samples user
            // frames only and does not claim kernel frames.
            kernel_frames: cfg!(target_os = "linux"),
            needs_root: true,
        }
    }

    fn default_format(&self) -> Format {
        // `go tool pprof` and the pprof web UI. `.pb.gz` is the auto-named extension.
        Format::Pprof
    }

    fn default_sampling_interval(&self) -> Duration {
        anyprof_core::PERF_SAMPLING_INTERVAL
    }

    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error> {
        if opts.duration.is_some_and(|duration| duration.is_zero()) {
            return Err(Error::backend(
                self.name(),
                "profile duration must be greater than zero",
            ));
        }
        if !process_exists(target.pid) {
            return Err(Error::backend(
                self.name(),
                format!("pid {} is not running", target.pid),
            ));
        }
        #[cfg(target_os = "macos")]
        {
            let profile = macos::record(target, opts)?;
            tracing::info!(
                pid = target.pid,
                samples = profile.total_weight(),
                stacks = profile.stacks.len(),
                "native profile complete"
            );
            Ok(Box::new(ReadySession { profile }))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(Error::not_implemented(self.name(), NOT_IMPLEMENTED))
        }
    }
}

#[cfg(target_os = "macos")]
struct ReadySession {
    profile: anyprof_core::Profile,
}

#[cfg(target_os = "macos")]
impl Session for ReadySession {
    fn stop(self: Box<Self>) -> Result<anyprof_core::Profile, Error> {
        Ok(self.profile)
    }
}

#[cfg(test)]
mod tests {
    use super::NativeBackend;
    use anyprof_core::{Backend, Error, RecordOptions, Target};
    use std::time::Duration;

    fn options() -> RecordOptions {
        RecordOptions {
            duration: Some(Duration::from_millis(50)),
            ..RecordOptions::default()
        }
    }

    #[test]
    fn missing_pid_is_not_native() {
        assert!(!NativeBackend.detect(u32::MAX));
    }

    #[test]
    fn missing_pid_is_rejected_before_sampling() {
        let Err(err) = NativeBackend.start(&Target { pid: u32::MAX }, &options()) else {
            panic!("a missing pid must not be sampled");
        };
        let message = err.to_string();
        assert!(message.contains("not running"), "{message}");
        assert!(
            !matches!(err, Error::NotImplemented { .. }),
            "a missing pid is an error even where sampling exists: {message}"
        );
    }

    #[test]
    fn zero_duration_is_rejected() {
        let Err(err) = NativeBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &RecordOptions {
                duration: Some(Duration::ZERO),
                ..RecordOptions::default()
            },
        ) else {
            panic!("zero duration must be rejected");
        };
        assert!(err.to_string().contains("greater than zero"), "{err}");
    }

    #[test]
    fn capabilities_match_the_platform_plan() {
        let caps = NativeBackend.capabilities();
        assert!(caps.on_cpu);
        assert!(!caps.off_cpu);
        assert!(!caps.mixed_stacks);
        assert!(caps.needs_root);
        assert_eq!(caps.kernel_frames, cfg!(target_os = "linux"));
        assert_eq!(
            NativeBackend.default_sampling_interval().as_micros(),
            10_101
        );
    }

    #[cfg(unix)]
    #[test]
    fn current_process_is_detected() {
        assert!(NativeBackend.detect(std::process::id()));
    }

    /// The refusal has to happen before `task_for_pid`. Attaching to this
    /// process and then suspending it can leave the sampler stuck.
    #[cfg(target_os = "macos")]
    #[test]
    fn sampling_the_current_process_is_refused_before_attach() {
        let Err(err) = NativeBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("sampling this process must be refused");
        };
        let message = err.to_string();
        assert!(message.contains("suspending"), "{message}");
        assert!(
            !message.contains("task_for_pid"),
            "the refusal must happen before attach: {message}"
        );
    }

    /// Windows and Linux compile this crate and refuse to sample. macOS is the
    /// platform that actually records; its live test is `tests/live.rs`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn start_reports_not_implemented_on_this_platform() {
        let Err(err) = NativeBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("native sampling must not run on this platform");
        };
        match err {
            Error::NotImplemented { backend, detail } => {
                assert_eq!(backend, "native");
                assert!(
                    detail.contains("macOS Mach sampling is implemented"),
                    "{detail}"
                );
                #[cfg(target_os = "linux")]
                assert!(detail.contains("Linux"), "{detail}");
                #[cfg(windows)]
                assert!(detail.contains("Windows"), "{detail}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn kernel_flag_does_not_pretend_to_sample() {
        let Err(err) = NativeBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &RecordOptions {
                duration: Some(Duration::from_millis(50)),
                include_kernel: true,
                ..RecordOptions::default()
            },
        ) else {
            panic!("-k must not start a sampler that does not exist here");
        };
        assert!(matches!(
            err,
            Error::NotImplemented {
                backend: "native",
                ..
            }
        ));
    }
}
