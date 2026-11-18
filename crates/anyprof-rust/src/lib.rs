//! Rust CPU sampling backend.
//!
//! There is no second sampler. A Rust process is profiled by the native
//! backend (Mach on macOS, `StackWalk64` on Windows x64, `perf_event_open` on
//! Linux x86_64). This crate decides that the process is Rust, and the native
//! symbol reader demangles Rust names into [`anyprof_core::Runtime::Rust`]
//! frames. C and kernel frames in the same stack keep their own runtimes.
//!
//! Other architectures compile. [`RustBackend::start`] returns not implemented
//! there, with the same platform split as the native sampler. Linux musl
//! x86_64 is the same sampler as gnu; CI type-checks it and does not live-sample
//! it.

mod exe;

use std::time::Duration;

use anyprof_core::{Backend, Capabilities, Error, Format, RecordOptions, Session, Target};
use anyprof_native::{NativeBackend, file_is_rust};

/// Detail for [`Error::NotImplemented`] on Windows builds that are not x64.
#[cfg(all(windows, not(target_arch = "x86_64")))]
const NOT_IMPLEMENTED: &str = "Rust on-CPU sampling uses the native Windows sampler, which is implemented for x64; this build is another architecture (macOS Mach sampling and Linux x86_64 sampling are implemented)";

/// Detail for [`Error::NotImplemented`] on Linux builds that are not x86_64.
#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
const NOT_IMPLEMENTED: &str = "Rust on-CPU sampling uses the native Linux sampler, which is implemented for x86_64; this build is another architecture (macOS Mach sampling and Windows x64 sampling are implemented)";

/// Detail for [`Error::NotImplemented`] on platforms without a sampler.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const NOT_IMPLEMENTED: &str = "Rust on-CPU sampling is not built on this platform (macOS Mach sampling, Windows x64 sampling, and Linux x86_64 sampling are implemented)";

/// Used only if the native sampler reports not implemented on a platform that
/// has one. The cfg-specific messages above cover the builds that refuse.
#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(windows, target_arch = "x86_64"),
))]
const NOT_IMPLEMENTED: &str = "Rust on-CPU sampling is not available in this build";

/// Rust sampler. Sampling is the native sampler; this type is the backend name,
/// the detection, and the error prefix.
#[derive(Debug, Default)]
pub struct RustBackend;

impl Backend for RustBackend {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn detect(&self, pid: u32) -> bool {
        let Some(path) = exe::executable_path(pid) else {
            tracing::debug!(pid, "rust detect: executable path is not readable");
            return false;
        };
        let found = file_is_rust(&path);
        tracing::debug!(pid, path = %path.display(), found, "rust detect");
        found
    }

    fn capabilities(&self) -> Capabilities {
        let native = NativeBackend.capabilities();
        Capabilities {
            // A Rust stack also contains C frames (the `main` shim, libc) and,
            // on Linux x86_64 with `-k`, kernel frames.
            mixed_stacks: true,
            ..native
        }
    }

    fn default_format(&self) -> Format {
        // Same ecosystem default as native and Go: `go tool pprof`.
        Format::Pprof
    }

    fn default_sampling_interval(&self) -> Duration {
        anyprof_core::PERF_SAMPLING_INTERVAL
    }

    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error> {
        // Missing pids and a zero duration are reported as `rust:` even where
        // sampling exists. Not-implemented stays the platform message below.
        NativeBackend.start(target, opts).map_err(retarget)
    }
}

fn retarget(err: Error) -> Error {
    match err {
        Error::Backend { message, .. } => Error::backend("rust", message),
        Error::NotImplemented { .. } => Error::not_implemented("rust", NOT_IMPLEMENTED),
        other => other,
    }
}

/// Whether this build can sample. Exposed for tests and docs; `start` is the
/// check that matters.
#[must_use]
pub fn sampler_implemented() -> bool {
    cfg!(any(
        target_os = "macos",
        all(target_os = "linux", target_arch = "x86_64"),
        all(windows, target_arch = "x86_64"),
    ))
}

#[cfg(test)]
mod tests {
    use super::RustBackend;
    use anyprof_core::{Backend, RecordOptions, Target};
    // macOS and Windows x64 tests only format `Error` through `Display`.
    #[cfg(not(any(target_os = "macos", all(windows, target_arch = "x86_64"))))]
    use anyprof_core::Error;
    use std::time::Duration;

    fn options() -> RecordOptions {
        RecordOptions {
            duration: Some(Duration::from_millis(50)),
            ..RecordOptions::default()
        }
    }

    #[test]
    fn missing_pid_is_not_rust() {
        assert!(!RustBackend.detect(u32::MAX));
        assert!(!RustBackend.detect(0));
    }

    #[test]
    fn this_test_binary_is_a_rust_image() {
        assert!(
            RustBackend.detect(std::process::id()),
            "the cargo test binary is a Rust executable and must auto-detect"
        );
    }

    #[test]
    fn capabilities_follow_the_native_sampler_and_allow_mixed_frames() {
        let caps = RustBackend.capabilities();
        assert!(caps.on_cpu);
        assert!(!caps.off_cpu);
        assert!(caps.mixed_stacks);
        assert_eq!(caps.needs_root, cfg!(target_os = "macos"));
        assert_eq!(
            caps.kernel_frames,
            cfg!(all(target_os = "linux", target_arch = "x86_64"))
        );
        assert_eq!(RustBackend.default_format(), anyprof_core::Format::Pprof);
        assert_eq!(RustBackend.default_sampling_interval().as_micros(), 10_101);
    }

    #[test]
    fn missing_pid_is_rejected_as_rust_before_sampling() {
        let Err(err) = RustBackend.start(&Target { pid: u32::MAX }, &options()) else {
            panic!("a missing pid must not be sampled");
        };
        let message = err.to_string();
        assert!(message.starts_with("rust:"), "{message}");
        assert!(message.contains("not running"), "{message}");
        assert!(!message.contains("native:"), "{message}");
    }

    #[test]
    fn zero_duration_is_rejected_as_rust() {
        let Err(err) = RustBackend.start(
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
        assert!(message.starts_with("rust:"), "{message}");
        assert!(message.contains("greater than zero"), "{message}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sampling_the_current_process_is_refused_before_attach() {
        let Err(err) = RustBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("sampling this process must be refused");
        };
        let message = err.to_string();
        assert!(message.starts_with("rust:"), "{message}");
        assert!(message.contains("suspending"), "{message}");
        assert!(
            !message.contains("task_for_pid"),
            "the refusal must happen before attach: {message}"
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn sampling_the_current_process_is_refused_before_attach() {
        let Err(err) = RustBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("sampling this process must be refused");
        };
        let message = err.to_string();
        assert!(message.starts_with("rust:"), "{message}");
        assert!(message.contains("suspending"), "{message}");
        assert!(
            !message.contains("OpenProcess"),
            "the refusal must happen before OpenProcess: {message}"
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn linux_samples_or_explains_perf_event_open_as_rust() {
        let result = RustBackend.start(
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
                assert!(message.starts_with("rust:"), "{message}");
                assert!(message.contains("perf_event_open"), "{message}");
                assert!(!matches!(err, Error::NotImplemented { .. }), "{message}");
            }
        }
    }

    #[cfg(not(any(
        target_os = "macos",
        all(target_os = "linux", target_arch = "x86_64"),
        all(windows, target_arch = "x86_64"),
    )))]
    #[test]
    fn start_reports_not_implemented_on_this_platform() {
        let Err(err) = RustBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("rust sampling must not run on this platform");
        };
        match err {
            Error::NotImplemented { backend, detail } => {
                assert_eq!(backend, "rust");
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
