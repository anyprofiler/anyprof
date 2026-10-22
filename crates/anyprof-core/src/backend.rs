use std::sync::Arc;
use std::time::Duration;

use crate::error::Error;
use crate::format::Format;
use crate::profile::Profile;
use crate::stop::StopGate;

/// What a backend is able to collect.
///
/// These flags describe the sampler the backend is for. A stub may report the
/// intended capabilities and still return [`Error::NotImplemented`] from
/// [`Backend::start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// On-CPU samples (the thread was running on a CPU).
    pub on_cpu: bool,
    /// Off-CPU samples (the thread was blocked).
    pub off_cpu: bool,
    /// One stack can contain frames from more than one runtime.
    pub mixed_stacks: bool,
    /// Kernel frames can appear in stacks.
    pub kernel_frames: bool,
    /// Opening a session typically requires root or an equivalent capability.
    pub needs_root: bool,
}

impl Capabilities {
    /// A backend that declares no sampling features.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            on_cpu: false,
            off_cpu: false,
            mixed_stacks: false,
            kernel_frames: false,
            needs_root: false,
        }
    }

    /// Short comma-separated summary for logs and the CLI.
    #[must_use]
    pub fn summary(self) -> String {
        let mut parts = Vec::new();
        if self.on_cpu {
            parts.push("on-cpu");
        }
        if self.off_cpu {
            parts.push("off-cpu");
        }
        if self.mixed_stacks {
            parts.push("mixed-stacks");
        }
        if self.kernel_frames {
            parts.push("kernel-frames");
        }
        if self.needs_root {
            parts.push("needs-root");
        }
        if parts.is_empty() {
            "no declared capabilities".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Process to profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// Operating-system process id.
    pub pid: u32,
}

/// Sampling interval Chrome DevTools' Performance panel records with (10 kHz).
///
/// Node.js and browser tabs default to this so a mostly idle server still
/// catches sub-millisecond request handling.
pub const DEVTOOLS_SAMPLING_INTERVAL: Duration = Duration::from_micros(100);

/// 99 Hz, the `perf record -F 99` convention native and Go samplers default to.
/// The odd rate keeps samples from lining up with periodic work.
pub const PERF_SAMPLING_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 99);

/// Interval between samples at `hertz` samples per second.
///
/// Zero is treated as one sample per second.
#[must_use]
pub fn interval_for_frequency(hertz: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(hertz.max(1)))
}

/// Options for one recording session.
///
/// [`RecordOptions::default`] samples for 30 seconds at the backend's default
/// interval. The CLI does not use that duration: omitting `-d` passes [`None`]
/// and the recording runs until [`StopGate`] is signaled.
#[derive(Debug, Clone)]
pub struct RecordOptions {
    /// How long to sample.
    ///
    /// `None` means sample until [`Self::stop`] is signaled. `Some(0)` is rejected.
    pub duration: Option<Duration>,
    /// Time between samples.
    ///
    /// `None` uses [`Backend::default_sampling_interval`]. Backends round to
    /// what their sampler supports (V8 takes whole microseconds).
    pub sampling_interval: Option<Duration>,
    /// Ask for kernel frames when [`Capabilities::kernel_frames`] is set.
    pub include_kernel: bool,
    /// Inspector or Chrome remote-debugging endpoint.
    ///
    /// A port (`9229`), `host:port`, `http://` URL, or `ws://` debugger URL.
    /// `None` means the backend's default (Node: port 9229, or a port the
    /// process is already listening on; browser: `127.0.0.1:9222`).
    pub inspect: Option<String>,
    /// Browser tab selector: target id, or a substring of the page URL or title.
    pub tab: Option<String>,
    /// Set when the user asks to end sampling. The sampler still returns a profile.
    pub stop: Arc<StopGate>,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            duration: Some(Duration::from_secs(30)),
            sampling_interval: None,
            include_kernel: false,
            inspect: None,
            tab: None,
            stop: StopGate::new(),
        }
    }
}

/// An in-progress profile.
///
/// [`Session::stop`] finishes the session and returns the samples collected so far.
pub trait Session: Send {
    /// Stop sampling and return the profile.
    fn stop(self: Box<Self>) -> Result<Profile, Error>;
}

/// One runtime's sampler.
///
/// Backends are stateless and cheap to construct. Register more specific
/// runtimes before the native fallback: the registry uses the first backend
/// whose [`Backend::detect`] returns true. Node.js is the first-class pid
/// backend. The browser backend is selected explicitly and does not match pids.
pub trait Backend: Send + Sync {
    /// Stable name used by `--backend` (`node`, `browser`, `native`, `go`, ...).
    fn name(&self) -> &'static str;

    /// Whether this backend should profile `pid`.
    ///
    /// Detection must not attach to the process. Return false when the runtime
    /// does not match or the process cannot be inspected.
    fn detect(&self, pid: u32) -> bool;

    /// Features this backend is built to provide.
    fn capabilities(&self) -> Capabilities;

    /// Format written when the user does not pass `--format` or a recognized
    /// output-file extension.
    ///
    /// Node.js and browser tabs return [`Format::Cpuprofile`]. Go and native
    /// return [`Format::Pprof`]. A future Python, Ruby, or PHP backend should
    /// return [`Format::Speedscope`].
    fn default_format(&self) -> Format;

    /// Interval used when [`RecordOptions::sampling_interval`] is `None`.
    ///
    /// Node.js and browser tabs return [`DEVTOOLS_SAMPLING_INTERVAL`] (100 µs)
    /// to match a DevTools recording. Go and native return
    /// [`PERF_SAMPLING_INTERVAL`] (99 Hz).
    fn default_sampling_interval(&self) -> Duration;

    /// Begin sampling. The returned session is stopped to obtain a [`Profile`].
    ///
    /// When [`RecordOptions::duration`] is `None`, or [`RecordOptions::stop`] is
    /// signaled early, sampling ends and the session still yields the samples
    /// collected so far. The target profiler must be stopped on every path,
    /// including errors, so the process is not left with a profiler attached.
    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error>;
}

/// Ordered list of backends used for auto-detection.
pub struct Registry {
    backends: Vec<Box<dyn Backend>>,
}

impl Registry {
    /// Register backends. The first match in `backends` wins.
    #[must_use]
    pub fn new(backends: Vec<Box<dyn Backend>>) -> Self {
        Self { backends }
    }

    /// Names in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|backend| backend.name()).collect()
    }

    /// Look up a backend by [`Backend::name`].
    pub fn get(&self, name: &str) -> Result<&dyn Backend, Error> {
        self.backends
            .iter()
            .find(|backend| backend.name() == name)
            .map(std::convert::AsRef::as_ref)
            .ok_or_else(|| Error::UnknownBackend {
                name: name.to_string(),
                available: self.names().join(", "),
            })
    }

    /// First backend that recognizes `pid`.
    pub fn detect(&self, pid: u32) -> Result<&dyn Backend, Error> {
        for backend in &self.backends {
            tracing::debug!(backend = backend.name(), pid, "probing backend");
            if backend.detect(pid) {
                tracing::info!(backend = backend.name(), pid, "backend matched");
                return Ok(backend.as_ref());
            }
        }
        Err(Error::NoMatchingBackend { pid })
    }
}

#[cfg(test)]
mod tests {
    use super::{Backend, Capabilities, RecordOptions, Registry, Session, Target};
    use crate::error::Error;
    use crate::format::Format;

    struct Scripted {
        name: &'static str,
        matches: u32,
    }

    impl Backend for Scripted {
        fn name(&self) -> &'static str {
            self.name
        }

        fn detect(&self, pid: u32) -> bool {
            pid == self.matches || self.name == "native"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::none()
        }

        fn default_format(&self) -> Format {
            Format::Collapsed
        }

        fn default_sampling_interval(&self) -> std::time::Duration {
            super::PERF_SAMPLING_INTERVAL
        }

        fn start(
            &self,
            _target: &Target,
            _opts: &RecordOptions,
        ) -> Result<Box<dyn Session>, Error> {
            Err(Error::not_implemented(self.name, "test backend"))
        }
    }

    fn registry() -> Registry {
        Registry::new(vec![
            Box::new(Scripted {
                name: "node",
                matches: 2,
            }),
            Box::new(Scripted {
                name: "native",
                matches: 0,
            }),
        ])
    }

    #[test]
    fn specific_runtime_wins_over_native_fallback() {
        let registry = registry();
        assert_eq!(registry.detect(2).unwrap().name(), "node");
        assert_eq!(registry.detect(9).unwrap().name(), "native");
    }

    #[test]
    fn unknown_backend_lists_what_is_registered() {
        let registry = registry();
        let Err(err) = registry.get("python") else {
            panic!("python is not a registered backend");
        };
        match err {
            Error::UnknownBackend { name, available } => {
                assert_eq!(name, "python");
                assert_eq!(available, "node, native");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn missing_pid_has_no_match_when_native_is_absent() {
        let registry = Registry::new(vec![Box::new(Scripted {
            name: "node",
            matches: 2,
        })]);
        assert!(matches!(
            registry.detect(9),
            Err(Error::NoMatchingBackend { pid: 9 })
        ));
    }

    #[test]
    fn capabilities_summary_lists_enabled_flags() {
        let caps = Capabilities {
            on_cpu: true,
            off_cpu: false,
            mixed_stacks: true,
            kernel_frames: false,
            needs_root: true,
        };
        assert_eq!(caps.summary(), "on-cpu, mixed-stacks, needs-root");
        assert_eq!(Capabilities::none().summary(), "no declared capabilities");
    }

    #[test]
    fn default_record_options_are_thirty_seconds_at_the_backend_interval() {
        let opts = RecordOptions::default();
        assert_eq!(opts.duration, Some(std::time::Duration::from_secs(30)));
        assert_eq!(opts.sampling_interval, None);
        assert!(!opts.include_kernel);
        assert!(!opts.stop.stop_requested());
    }

    #[test]
    fn frequencies_become_intervals() {
        use super::{DEVTOOLS_SAMPLING_INTERVAL, PERF_SAMPLING_INTERVAL, interval_for_frequency};
        use std::time::Duration;

        assert_eq!(interval_for_frequency(10_000), DEVTOOLS_SAMPLING_INTERVAL);
        assert_eq!(interval_for_frequency(99), PERF_SAMPLING_INTERVAL);
        assert_eq!(PERF_SAMPLING_INTERVAL.as_micros(), 10_101);
        assert_eq!(interval_for_frequency(1000), Duration::from_millis(1));
        assert_eq!(interval_for_frequency(0), Duration::from_secs(1));
    }
}
