use std::time::Duration;

use crate::error::Error;
use crate::profile::Profile;

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

/// Options for one recording session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordOptions {
    /// How long to sample.
    pub duration: Duration,
    /// Requested sampling frequency in hertz.
    pub frequency_hz: u32,
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
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(30),
            frequency_hz: 99,
            include_kernel: false,
            inspect: None,
            tab: None,
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

    /// Begin sampling. The returned session is stopped to obtain a [`Profile`].
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
    fn default_record_options_are_thirty_seconds_at_99hz() {
        let opts = RecordOptions::default();
        assert_eq!(opts.duration, std::time::Duration::from_secs(30));
        assert_eq!(opts.frequency_hz, 99);
        assert!(!opts.include_kernel);
    }
}
