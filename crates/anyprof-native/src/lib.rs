//! Native (C and C++) CPU sampling backend.
//!
//! macOS samples on-CPU stacks through Mach (`task_for_pid`, frame pointers,
//! Mach-O symbols) and needs root. Linux x86_64 samples through
//! `perf_event_open` task-clock events and can include kernel frames. Windows
//! x64 suspends threads that are actually running and walks them with
//! `StackWalk64`. Other architectures compile and `start` returns not
//! implemented.
//!
//! Rust sampling reuses this sampler. [`file_is_rust`] recognizes a Rust
//! image, and symbol lookup demangles Rust names into
//! [`anyprof_core::Runtime::Rust`] frames. The `anyprof-rust` backend selects
//! that path; this backend remains the fallback for every other live pid.
//!
//! Go sampling reuses the same on-CPU events. When the executable has a Go
//! pclntab, stacks are unwound from that table’s pcsp values instead of from
//! frame pointers, and the frames are [`anyprof_core::Runtime::Go`]. A leaf
//! that is not a Go function still uses the frame-pointer or `StackWalk64` walk.
//!
//! Python sampling reuses the same on-CPU events. When the leaf is inside
//! the CPython image, the user stack is the interpreter frame chain and the
//! frames are [`anyprof_core::Runtime::Python`]. A leaf in another image
//! keeps the native walk.
//!
//! Register this backend last. [`NativeBackend::detect`] is true for any live
//! pid so it acts as the fallback when no language runtime matched.

mod elf;
mod go_sym;
mod kallsyms;
mod macho;
mod maps;
mod pdb_syms;
mod perf_parse;
mod policy;
mod python;
mod python_sym;
mod rust_image;
mod snapshot;
mod symbol;
mod walk;

pub use python::pid_is_cpython;
pub use rust_image::file_is_rust;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

#[cfg(all(windows, target_arch = "x86_64"))]
mod windows;

use std::time::Duration;

use anyprof_core::{
    Backend, Capabilities, Error, Format, RecordOptions, Session, Target, process_exists,
};

/// Detail for [`Error::NotImplemented`] on Windows builds that are not x64.
#[cfg(all(windows, not(target_arch = "x86_64")))]
const NOT_IMPLEMENTED: &str = "native on-CPU sampling on Windows is implemented for x64; this build is another architecture (macOS Mach sampling is implemented)";

/// Detail for [`Error::NotImplemented`] on Linux builds that are not x86_64.
#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
const NOT_IMPLEMENTED: &str = "native on-CPU sampling on Linux is implemented for x86_64; this build is another architecture (macOS Mach sampling is implemented)";

/// Detail for [`Error::NotImplemented`] on platforms without a sampler yet.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const NOT_IMPLEMENTED: &str = "native on-CPU sampling is not built on this platform (macOS Mach sampling is implemented; Windows x64 and Linux x86_64 sampling are implemented)";

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
            // Kernel stacks exist only in the Linux x86_64 perf sampler.
            // Other Linux architectures return not implemented from `start`
            // and do not advertise `-k`.
            kernel_frames: cfg!(all(target_os = "linux", target_arch = "x86_64")),
            // macOS `task_for_pid` needs root for an ordinary process.
            // Windows same-user sampling does not. Linux same-user user-space
            // sampling does not when `perf_event_paranoid` is 2 or lower;
            // the permission error names the case that does.
            needs_root: cfg!(target_os = "macos"),
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
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let profile = linux::record(target, opts)?;
            tracing::info!(
                pid = target.pid,
                samples = profile.total_weight(),
                stacks = profile.stacks.len(),
                "native profile complete"
            );
            Ok(Box::new(ReadySession { profile }))
        }
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let profile = windows::record(target, opts)?;
            tracing::info!(
                pid = target.pid,
                samples = profile.total_weight(),
                stacks = profile.stacks.len(),
                "native profile complete"
            );
            Ok(Box::new(ReadySession { profile }))
        }
        #[cfg(not(any(
            target_os = "macos",
            all(target_os = "linux", target_arch = "x86_64"),
            all(windows, target_arch = "x86_64"),
        )))]
        {
            Err(Error::not_implemented(self.name(), NOT_IMPLEMENTED))
        }
    }
}

struct ReadySession {
    profile: anyprof_core::Profile,
}

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
        assert_eq!(caps.needs_root, cfg!(target_os = "macos"));
        assert_eq!(
            caps.kernel_frames,
            cfg!(all(target_os = "linux", target_arch = "x86_64"))
        );
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

    /// Linux x86_64 either samples this process or explains `perf_event_open`.
    /// Task-clock events do not suspend the target, so self-sampling is safe.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn linux_samples_or_explains_perf_event_open() {
        let result = NativeBackend.start(
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
                assert!(
                    message.contains("perf_event_open"),
                    "expected a perf error, got {message}"
                );
                assert!(!matches!(err, Error::NotImplemented { .. }), "{message}");
            }
        }
    }

    /// `-k` is a real request on Linux x86_64. Paranoid 2 refuses kernel samples.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn kernel_flag_is_a_real_perf_request() {
        let result = NativeBackend.start(
            &Target {
                pid: std::process::id(),
            },
            &RecordOptions {
                duration: Some(Duration::from_millis(50)),
                include_kernel: true,
                ..RecordOptions::default()
            },
        );
        match result {
            Ok(session) => {
                let _ = session.stop();
            }
            Err(err) => {
                let message = err.to_string();
                assert!(
                    message.contains("perf_event_open") || message.contains("paranoid"),
                    "{message}"
                );
                assert!(!matches!(err, Error::NotImplemented { .. }), "{message}");
            }
        }
    }

    /// Suspending this process can leave the sampler stuck. The refusal has
    /// to happen before `OpenProcess`.
    #[cfg(all(windows, target_arch = "x86_64"))]
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
            !message.contains("OpenProcess"),
            "the refusal must happen before OpenProcess: {message}"
        );
    }

    /// Platforms without a sampler still compile and refuse.
    #[cfg(not(any(
        target_os = "macos",
        all(target_os = "linux", target_arch = "x86_64"),
        all(windows, target_arch = "x86_64"),
    )))]
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

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn kernel_flag_still_refuses_to_sample_this_process() {
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
            panic!("-k must not sample this process");
        };
        let message = err.to_string();
        assert!(message.contains("suspending"), "{message}");
        assert!(!matches!(err, Error::NotImplemented { .. }), "{message}");
    }

    #[cfg(not(any(
        target_os = "macos",
        all(target_os = "linux", target_arch = "x86_64"),
        all(windows, target_arch = "x86_64"),
    )))]
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
