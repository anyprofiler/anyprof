//! CPython CPU sampling backend.
//!
//! There is no second on-CPU event source. A CPython process is profiled by
//! the native backend (Mach on macOS, `StackWalk64` on Windows x64,
//! `perf_event_open` on Linux x86_64). When the leaf program counter is
//! inside the CPython image and the frame chain can be read, that sampler
//! replaces the user stack with the chain. Those frames are
//! [`anyprof_core::Runtime::Python`]. A leaf in another image, such as libc
//! or a C extension, keeps the native walk, so that time is not relabeled.
//! A leaf inside the image whose chain cannot be read keeps the native walk
//! too. Those frames are not labeled Python. A denied `process_vm_readv`
//! does not: `start` returns an error and no profile is written.
//!
//! Only a thread actually on CPU is recorded. A thread blocked in
//! `time.sleep` is not a Python sample. The GIL means one thread runs
//! bytecode at a time; two spinning threads both appear as the lock
//! switches. This is not a snapshot of every thread.
//!
//! The walked layouts are the default GIL builds of CPython 3.11, 3.12, 3.13,
//! and 3.14. An older interpreter, 3.15 or later, and a free-threaded build
//! are detected from `_PyRuntime` and then refused: `start` returns an error
//! instead of a native profile of the eval loop. On Linux the frame read is
//! `process_vm_readv`, which needs ptrace attach permission. `perf_event_open`
//! permission is not enough when `kernel.yama.ptrace_scope` is 1 and the
//! target is not a descendant. That refusal names the errno and the current
//! scope. anyprof does not change the sysctl.
//!
//! Other architectures compile. [`PythonBackend::start`] returns not
//! implemented there, with the same platform split as the native sampler.

use std::time::Duration;

use anyprof_core::{
    Backend, Capabilities, Error, Format, LanguageUnwind, RecordOptions, Session, Target,
};
use anyprof_native::NativeBackend;

/// Detail for [`Error::NotImplemented`] on Windows builds that are not x64.
#[cfg(all(windows, not(target_arch = "x86_64")))]
const NOT_IMPLEMENTED: &str = "Python on-CPU sampling uses the native Windows sampler, which is implemented for x64; this build is another architecture (macOS Mach sampling and Linux x86_64/aarch64 sampling are implemented)";

/// Detail for [`Error::NotImplemented`] on Linux builds that are not x86_64 or aarch64.
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
const NOT_IMPLEMENTED: &str = "Python on-CPU sampling uses the native Linux sampler, which is implemented for x86_64 and aarch64; this build is another architecture (macOS Mach sampling and Windows x64 sampling are implemented)";

/// Detail for [`Error::NotImplemented`] on platforms without a sampler.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const NOT_IMPLEMENTED: &str = "Python on-CPU sampling is not built on this platform (macOS Mach sampling, Windows x64 sampling, and Linux x86_64/aarch64 sampling are implemented)";

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
const NOT_IMPLEMENTED: &str = "Python on-CPU sampling is not available in this build";

/// CPython sampler. Sampling is the native sampler; this type is the backend
/// name, the detection, and the error prefix.
#[derive(Debug, Default)]
pub struct PythonBackend;

impl Backend for PythonBackend {
    fn name(&self) -> &'static str {
        "python"
    }

    fn detect(&self, pid: u32) -> bool {
        let found = anyprof_native::pid_is_cpython(pid);
        tracing::debug!(pid, found, "python detect");
        found
    }

    fn capabilities(&self) -> Capabilities {
        let native = NativeBackend.capabilities();
        Capabilities {
            // A stack can contain native frames when the leaf is outside the
            // interpreter image, and kernel frames on Linux x86_64/aarch64 with `-k`.
            mixed_stacks: true,
            ..native
        }
    }

    fn default_format(&self) -> Format {
        Format::Speedscope
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
        if !sampler_implemented() {
            return Err(Error::not_implemented(self.name(), NOT_IMPLEMENTED));
        }
        if !anyprof_core::process_exists(target.pid) {
            return Err(Error::backend(
                self.name(),
                format!("pid {} is not running", target.pid),
            ));
        }
        // An unsupported build, or a Linux read the kernel refuses, stops
        // here. The native sampler is not asked to record interpreter frames
        // under this backend's name.
        anyprof_native::prepare_python(target.pid)?;
        let mut py_opts = opts.clone();
        py_opts.language_unwind = LanguageUnwind::Python;
        let inner = NativeBackend.start(target, &py_opts).map_err(retarget)?;
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
        Error::Backend { message, .. } => Error::backend("python", message),
        Error::NotImplemented { .. } => Error::not_implemented("python", NOT_IMPLEMENTED),
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
    use super::PythonBackend;
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
    fn missing_pid_is_not_python() {
        assert!(!PythonBackend.detect(u32::MAX));
        assert!(!PythonBackend.detect(0));
    }

    #[test]
    fn capabilities_follow_the_native_sampler_and_allow_mixed_frames() {
        let caps = PythonBackend.capabilities();
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
        assert_eq!(
            PythonBackend.default_format(),
            anyprof_core::Format::Speedscope
        );
        assert_eq!(
            PythonBackend.default_sampling_interval().as_micros(),
            10_101
        );
    }

    #[test]
    fn missing_pid_is_rejected_as_python_before_sampling() {
        let Err(err) = PythonBackend.start(&Target { pid: u32::MAX }, &options()) else {
            panic!("a missing pid must not be sampled");
        };
        let message = err.to_string();
        assert!(message.starts_with("python:"), "{message}");
        assert!(message.contains("not running"), "{message}");
        assert!(!message.contains("native:"), "{message}");
    }

    #[test]
    fn zero_duration_is_rejected() {
        let Err(err) = PythonBackend.start(
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
        assert!(message.starts_with("python:"), "{message}");
        assert!(message.contains("greater than zero"), "{message}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sampling_the_current_process_is_refused_before_attach() {
        let Err(err) = PythonBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("sampling this process must be refused");
        };
        let message = err.to_string();
        assert!(message.starts_with("python:"), "{message}");
        assert!(message.contains("suspending"), "{message}");
        assert!(
            !message.contains("task_for_pid"),
            "the refusal must happen before attach: {message}"
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn sampling_the_current_process_is_refused_before_attach() {
        let Err(err) = PythonBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("sampling this process must be refused");
        };
        let message = err.to_string();
        assert!(message.starts_with("python:"), "{message}");
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
    fn linux_samples_or_explains_perf_event_open_as_python() {
        let result = PythonBackend.start(
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
                assert!(message.starts_with("python:"), "{message}");
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
        let Err(err) = PythonBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("python sampling must not run on this platform");
        };
        match err {
            Error::NotImplemented { backend, detail } => {
                assert_eq!(backend, "python");
                assert!(
                    detail.contains("sampling are implemented")
                        || detail.contains("sampling is implemented"),
                    "{detail}"
                );
                assert!(!detail.starts_with("native"), "{detail}");
            }
            other => panic!("expected not implemented, got {other}"),
        }
    }
}
