//! HotSpot Java CPU sampling backend.
//!
//! There is no second on-CPU event source. A Java process is profiled by the
//! native backend (Mach on macOS, `StackWalk64` on Windows x64,
//! `perf_event_open` on Linux x86_64). When the leaf program counter is
//! inside a HotSpot code-cache blob and the method can be read, that sampler
//! replaces the user stack with `Class.method` frames. Those frames are
//! [`anyprof_core::Runtime::Java`]. A leaf in another image, such as libc,
//! keeps the native walk. A stub or a method pointer that does not resolve
//! keeps it too. Those frames are not labeled Java. A denied
//! `process_vm_readv` does not: `start` returns an error and no profile is
//! written.
//!
//! Only a thread actually on CPU is recorded. A thread blocked in
//! `Thread.sleep` is not a Java sample. This is not a snapshot of every
//! thread, and it is not a JVM TI or JFR attach.
//!
//! The walk uses the serviceability tables HotSpot exports
//! (`gHotSpotVMStructs`, `gHotSpotVMTypes`, `gHotSpotVMIntConstants`). It
//! accepts JDK 17, 21, and 25, read from
//! `Abstract_VM_Version._vm_major_version`. The interpreter constants have to
//! be `sender_sp = -1` and `last_sp = -2` (method slot `last_sp - 1`). x86_64
//! exports that pair. aarch64 does not in those three releases, and the empty
//! table is used only when the mapped libjvm is aarch64 and the version is
//! one of those three. `HeapBlock` is 16 bytes with `_used` at 8 on 17 and
//! 21, and 8 bytes with `_used` at 4 on 25. The instruction range is the
//! stored `_code_begin`/`_code_end` pointers on 17 and 21, and
//! `this + _code_offset` through `this + _data_offset` on 25. A missing table, a different
//! exported pair, a debug `GrowableArrayBase`, another version, or a 32-bit
//! JVM is an error, not a native profile of `libjvm`. On Linux the frame read is
//! `process_vm_readv`, which needs ptrace attach permission.
//! `perf_event_open` permission is not enough when `kernel.yama.ptrace_scope`
//! is 1 and the target is not a descendant. That refusal names the errno and
//! the current scope. anyprof does not change the sysctl.
//!
//! Other architectures compile. [`JavaBackend::start`] returns not
//! implemented there, with the same platform split as the native sampler.

use std::time::Duration;

use anyprof_core::{
    Backend, Capabilities, Error, Format, LanguageUnwind, RecordOptions, Session, Target,
};
use anyprof_native::NativeBackend;

/// Detail for [`Error::NotImplemented`] on Windows builds that are not x64.
#[cfg(all(windows, not(target_arch = "x86_64")))]
const NOT_IMPLEMENTED: &str = "Java on-CPU sampling uses the native Windows sampler, which is implemented for x64; this build is another architecture (macOS Mach sampling and Linux x86_64/aarch64 sampling are implemented)";

/// Detail for [`Error::NotImplemented`] on Linux builds that are not x86_64 or aarch64.
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
const NOT_IMPLEMENTED: &str = "Java on-CPU sampling uses the native Linux sampler, which is implemented for x86_64 and aarch64; this build is another architecture (macOS Mach sampling and Windows x64 sampling are implemented)";

/// Detail for [`Error::NotImplemented`] on platforms without a sampler.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const NOT_IMPLEMENTED: &str = "Java on-CPU sampling is not built on this platform (macOS Mach sampling, Windows x64 sampling, and Linux x86_64/aarch64 sampling are implemented)";

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
const NOT_IMPLEMENTED: &str = "Java on-CPU sampling is not available in this build";

/// HotSpot sampler. Sampling is the native sampler; this type is the backend
/// name, the detection, and the error prefix.
#[derive(Debug, Default)]
pub struct JavaBackend;

impl Backend for JavaBackend {
    fn name(&self) -> &'static str {
        "java"
    }

    fn detect(&self, pid: u32) -> bool {
        let found = anyprof_native::pid_is_hotspot(pid);
        tracing::debug!(pid, found, "java detect");
        found
    }

    fn capabilities(&self) -> Capabilities {
        let native = NativeBackend.capabilities();
        Capabilities {
            // A stack can contain native frames when the leaf is outside the
            // code cache, and kernel frames on Linux x86_64/aarch64 with `-k`.
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
        // An unsupported JVM, or a Linux read the kernel refuses, stops
        // here. The native sampler is not asked to record libjvm frames
        // under this backend's name.
        anyprof_native::prepare_java(target.pid)?;
        let mut java_opts = opts.clone();
        java_opts.language_unwind = LanguageUnwind::Java;
        let inner = NativeBackend.start(target, &java_opts).map_err(retarget)?;
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
        Error::Backend { message, .. } => Error::backend("java", message),
        Error::NotImplemented { .. } => Error::not_implemented("java", NOT_IMPLEMENTED),
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
    use super::JavaBackend;
    use anyprof_core::{Backend, RecordOptions, Target};
    use std::time::Duration;

    fn options() -> RecordOptions {
        RecordOptions {
            duration: Some(Duration::from_millis(50)),
            ..RecordOptions::default()
        }
    }

    #[test]
    fn this_process_is_not_java() {
        assert!(!JavaBackend.detect(std::process::id()));
        assert!(!JavaBackend.detect(u32::MAX));
        assert!(!JavaBackend.detect(0));
    }

    #[test]
    fn capabilities_follow_the_native_sampler_and_allow_mixed_frames() {
        let caps = JavaBackend.capabilities();
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
            JavaBackend.default_format(),
            anyprof_core::Format::Speedscope
        );
        assert_eq!(JavaBackend.default_sampling_interval().as_micros(), 10_101);
    }

    #[test]
    fn missing_pid_is_rejected_as_java_before_sampling() {
        let Err(err) = JavaBackend.start(&Target { pid: u32::MAX }, &options()) else {
            panic!("a missing pid must not be sampled");
        };
        let message = err.to_string();
        assert!(message.starts_with("java:"), "{message}");
        assert!(message.contains("not running"), "{message}");
        assert!(!message.contains("native:"), "{message}");
    }

    #[test]
    fn zero_duration_is_rejected() {
        let Err(err) = JavaBackend.start(
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
        assert!(message.starts_with("java:"), "{message}");
        assert!(message.contains("greater than zero"), "{message}");
    }

    #[cfg(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(windows, target_arch = "x86_64"),
    ))]
    #[test]
    fn this_process_is_refused_as_not_hotspot_before_sampling() {
        let Err(err) = JavaBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("this process is not a JVM");
        };
        let message = err.to_string();
        assert!(message.starts_with("java:"), "{message}");
        assert!(message.contains("HotSpot"), "{message}");
        assert!(
            !message.contains("task_for_pid") && !message.contains("OpenProcess"),
            "the refusal must happen before attach: {message}"
        );
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
        let Err(err) = JavaBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &options(),
        ) else {
            panic!("java sampling must not run on this platform");
        };
        match err {
            anyprof_core::Error::NotImplemented { backend, detail } => {
                assert_eq!(backend, "java");
                assert!(
                    detail.contains("sampling are implemented")
                        || detail.contains("sampling is implemented"),
                    "{detail}"
                );
            }
            other => panic!("expected not implemented, got {other}"),
        }
    }
}
