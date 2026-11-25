//! Go CPU sampling backend.
//!
//! There is no second on-CPU event source. A Go process is profiled by the
//! native backend (Mach on macOS, `StackWalk64` on Windows x64,
//! `perf_event_open` on Linux x86_64). When the executable contains a Go
//! 1.18+ pclntab, that sampler unwinds with the pcsp table instead of frame
//! pointers: a Go leaf often has no frame pointer, and the goroutine return
//! PC is not in a saved `%rbp` slot. Frames from that table are
//! [`anyprof_core::Runtime::Go`]. A leaf that is not a Go function keeps the
//! native walk.
//!
//! Parked goroutines are not sampled. The PC and SP are the OS thread's,
//! which is the goroutine that thread is running. This is not a walk of
//! `runtime.allgs`.
//!
//! Other architectures compile. [`GoBackend::start`] returns not implemented
//! there, with the same platform split as the native sampler.

mod binary;

use std::time::Duration;

use anyprof_core::{
    Backend, Capabilities, Error, Format, LanguageUnwind, RecordOptions, Session, Target,
    executable_file,
};
use anyprof_native::NativeBackend;

/// Detail for [`Error::NotImplemented`] on Windows builds that are not x64.
#[cfg(all(windows, not(target_arch = "x86_64")))]
const NOT_IMPLEMENTED: &str = "Go on-CPU sampling uses the native Windows sampler, which is implemented for x64; this build is another architecture (macOS Mach sampling and Linux x86_64/aarch64 sampling are implemented)";

/// Detail for [`Error::NotImplemented`] on Linux builds that are not x86_64 or aarch64.
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
const NOT_IMPLEMENTED: &str = "Go on-CPU sampling uses the native Linux sampler, which is implemented for x86_64 and aarch64; this build is another architecture (macOS Mach sampling and Windows x64 sampling are implemented)";

/// Detail for [`Error::NotImplemented`] on platforms without a sampler.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const NOT_IMPLEMENTED: &str = "Go on-CPU sampling is not built on this platform (macOS Mach sampling, Windows x64 sampling, and Linux x86_64/aarch64 sampling are implemented)";

/// Used only if the native sampler reports not implemented on a platform that
/// has one. The cfg-specific messages above cover the builds that refuse.
#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(windows, target_arch = "x86_64"),
))]
const NOT_IMPLEMENTED: &str = "Go on-CPU sampling is not available in this build";

/// Go sampler. Sampling is the native sampler; this type is the backend name,
/// the detection, and the error prefix. Stacks come from pclntab.
#[derive(Debug, Default)]
pub struct GoBackend;

impl Backend for GoBackend {
    fn name(&self) -> &'static str {
        "go"
    }

    fn detect(&self, pid: u32) -> bool {
        let Some(path) = executable_file(pid) else {
            tracing::debug!(pid, "go detect: executable path is not readable");
            return false;
        };
        match binary::contains_go_build_info(&path) {
            Ok(found) => {
                tracing::debug!(pid, path = %path.display(), found, "go detect");
                found
            }
            Err(err) => {
                tracing::debug!(pid, %err, "go detect: build-info scan failed");
                false
            }
        }
    }

    fn capabilities(&self) -> Capabilities {
        let native = NativeBackend.capabilities();
        Capabilities {
            // A Go stack can also contain a C return (cgo) and, on Linux
            // x86_64/aarch64 with `-k`, kernel frames.
            mixed_stacks: true,
            ..native
        }
    }

    fn default_format(&self) -> Format {
        Format::Pprof
    }

    fn default_sampling_interval(&self) -> Duration {
        anyprof_core::PERF_SAMPLING_INTERVAL
    }

    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error> {
        let mut go_opts = opts.clone();
        go_opts.language_unwind = LanguageUnwind::Go;
        let inner = NativeBackend.start(target, &go_opts).map_err(retarget)?;
        Ok(Box::new(RetargetSession { inner }))
    }
}

struct RetargetSession {
    inner: Box<dyn Session>,
}

impl Session for RetargetSession {
    fn stop(self: Box<Self>) -> Result<anyprof_core::Profile, Error> {
        self.inner.stop().map_err(retarget)
    }
}

fn retarget(err: Error) -> Error {
    match err {
        Error::Backend { message, .. } => Error::backend("go", message),
        Error::NotImplemented { .. } => Error::not_implemented("go", NOT_IMPLEMENTED),
        other => other,
    }
}

/// Whether this build can sample. Exposed for tests and docs; `start` is the
/// check that matters.
#[must_use]
pub fn sampler_implemented() -> bool {
    cfg!(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(windows, target_arch = "x86_64"),
    ))
}

#[cfg(test)]
mod tests {
    use super::GoBackend;
    #[cfg(not(any(target_os = "macos", all(windows, target_arch = "x86_64"))))]
    use anyprof_core::Error;
    use anyprof_core::{Backend, RecordOptions, Target};
    use std::time::Duration;

    fn options() -> RecordOptions {
        RecordOptions {
            duration: Some(Duration::from_millis(50)),
            ..RecordOptions::default()
        }
    }

    #[test]
    fn missing_pid_is_not_go() {
        assert!(!GoBackend.detect(u32::MAX));
        assert!(!GoBackend.detect(0));
    }

    #[test]
    fn capabilities_follow_the_native_sampler_and_allow_mixed_frames() {
        let caps = GoBackend.capabilities();
        assert!(caps.on_cpu);
        assert!(!caps.off_cpu);
        assert!(caps.mixed_stacks);
        assert_eq!(caps.needs_root, cfg!(target_os = "macos"));
        assert_eq!(
            caps.kernel_frames,
            cfg!(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))
        );
        assert_eq!(GoBackend.default_format(), anyprof_core::Format::Pprof);
        assert_eq!(GoBackend.default_sampling_interval().as_micros(), 10_101);
    }

    #[test]
    fn missing_pid_is_rejected_as_go_before_sampling() {
        let Err(err) = GoBackend.start(&Target { pid: u32::MAX }, &options()) else {
            panic!("a missing pid must not be sampled");
        };
        let message = err.to_string();
        assert!(message.starts_with("go:"), "{message}");
        assert!(message.contains("not running"), "{message}");
        assert!(!message.contains("native:"), "{message}");
    }

    #[test]
    fn zero_duration_is_rejected_as_go() {
        let Err(err) = GoBackend.start(
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
        let message = err.to_string();
        assert!(message.starts_with("go:"), "{message}");
        assert!(message.contains("greater than zero"), "{message}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sampling_the_current_process_is_refused_before_attach() {
        let Err(err) = GoBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("sampling this process must be refused");
        };
        let message = err.to_string();
        assert!(message.starts_with("go:"), "{message}");
        assert!(message.contains("suspending"), "{message}");
        assert!(
            !message.contains("task_for_pid"),
            "the refusal must happen before attach: {message}"
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn sampling_the_current_process_is_refused_before_attach() {
        let Err(err) = GoBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("sampling this process must be refused");
        };
        let message = err.to_string();
        assert!(message.starts_with("go:"), "{message}");
        assert!(message.contains("suspending"), "{message}");
        assert!(
            !message.contains("OpenProcess"),
            "the refusal must happen before OpenProcess: {message}"
        );
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn linux_samples_or_explains_perf_event_open_as_go() {
        let result = GoBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        );
        match result {
            Ok(session) => {
                let profile = session.stop().expect("stop");
                assert_eq!(profile.pid, std::process::id());
            }
            Err(err) => {
                let message = err.to_string();
                assert!(message.starts_with("go:"), "{message}");
                assert!(message.contains("perf_event_open"), "{message}");
                assert!(!matches!(err, Error::NotImplemented { .. }), "{message}");
            }
        }
    }

    #[cfg(not(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(windows, target_arch = "x86_64"),
    )))]
    #[test]
    fn start_reports_not_implemented_on_this_platform() {
        let Err(err) = GoBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("go sampling must not run on this platform");
        };
        match err {
            Error::NotImplemented { backend, detail } => {
                assert_eq!(backend, "go");
                assert!(
                    detail.contains("sampling are implemented")
                        || detail.contains("sampling is implemented"),
                    "{detail}"
                );
                assert!(!detail.starts_with("native"), "{detail}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
