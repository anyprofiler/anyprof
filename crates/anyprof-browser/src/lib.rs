//! Browser-page CPU sampling backend.
//!
//! Chrome, Chromium, and Edge expose the same DevTools Protocol when they are
//! started with `--remote-debugging-port`. This backend does not launch the
//! browser and [`BrowserBackend::detect`] never matches a pid. Select it with
//! `--browser`, point `--inspect` at the remote-debugging port (default
//! `127.0.0.1:9222`), and pick a tab with `--tab` (target id, or a substring of
//! the page URL or title).
//!
//! Sampling reuses the Node inspector client: `Profiler.enable`,
//! `Profiler.setSamplingInterval`, `Profiler.start`, and `Profiler.stop`.
//! Frames are stamped [`anyprof_core::Runtime::Browser`].

use std::time::Duration;

use anyprof_cdp::{
    InspectEndpoint, capture_cpu_profile, choose_page, list_targets, parse_inspect,
    sampling_interval_us,
};
use anyprof_core::{
    Backend, Capabilities, DEVTOOLS_SAMPLING_INTERVAL, Error, Format, Profile, RecordOptions,
    Runtime, Session, Target,
};
use anyprof_export::import_cpuprofile;

/// Sampler for one browser tab.
#[derive(Debug, Default)]
pub struct BrowserBackend;

impl Backend for BrowserBackend {
    fn name(&self) -> &'static str {
        "browser"
    }

    fn detect(&self, pid: u32) -> bool {
        let _ = pid;
        false
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            on_cpu: true,
            off_cpu: false,
            mixed_stacks: true,
            kernel_frames: false,
            needs_root: false,
        }
    }

    fn default_format(&self) -> Format {
        // Same DevTools document as Node. Open it in the Performance panel.
        Format::Cpuprofile
    }

    fn default_sampling_interval(&self) -> Duration {
        DEVTOOLS_SAMPLING_INTERVAL
    }

    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error> {
        if opts.duration.is_some_and(|duration| duration.is_zero()) {
            return Err(Error::backend(
                self.name(),
                "profile duration must be greater than zero",
            ));
        }
        let endpoint = match opts.inspect.as_deref() {
            Some(raw) => parse_inspect(raw).map_err(|err| Error::backend(self.name(), err))?,
            None => InspectEndpoint {
                host: "127.0.0.1".to_string(),
                port: 9222,
                websocket_url: None,
            },
        };
        let websocket = if let Some(url) = endpoint.websocket_url.clone() {
            url
        } else {
            let targets = list_targets(&endpoint).map_err(|err| {
                Error::backend(
                    self.name(),
                    format!(
                        "{err}. Start Chrome, Chromium, or Edge with --remote-debugging-port={}",
                        endpoint.port
                    ),
                )
            })?;
            choose_page(&targets, opts.tab.as_deref())
                .map_err(|err| Error::backend(self.name(), err.to_string()))?
                .websocket_url
                .clone()
        };
        let interval_us = sampling_interval_us(
            opts.sampling_interval
                .unwrap_or_else(|| self.default_sampling_interval()),
        );
        tracing::info!(%websocket, ?opts.duration, interval_us, "profiling browser tab");
        let bytes = capture_cpu_profile(&websocket, opts.duration, interval_us, None, &opts.stop)
            .map_err(|err| Error::backend(self.name(), err.to_string()))?;
        let mut imported = import_cpuprofile(&bytes, Runtime::Browser)
            .map_err(|err| Error::backend(self.name(), err.to_string()))?;
        imported.raw_cpuprofile = Some(bytes);
        Ok(Box::new(ReadySession {
            profile: finish_profile(imported, target.pid, opts, interval_us),
        }))
    }
}

fn finish_profile(
    mut profile: Profile,
    pid: u32,
    opts: &RecordOptions,
    interval_us: u32,
) -> Profile {
    profile.pid = pid;
    if profile.duration.is_zero()
        && let Some(duration) = opts.duration
    {
        profile.duration = duration;
    }
    if profile.period.is_none() {
        profile.period = Some(Duration::from_micros(u64::from(interval_us)));
    }
    profile
}

struct ReadySession {
    profile: Profile,
}

impl Session for ReadySession {
    fn stop(self: Box<Self>) -> Result<Profile, Error> {
        Ok(self.profile)
    }
}

#[cfg(test)]
mod tests {
    use super::BrowserBackend;
    use anyprof_core::{Backend, RecordOptions, Target};
    use std::time::Duration;

    #[test]
    fn detection_never_matches_a_pid() {
        assert!(!BrowserBackend.detect(1));
        assert!(!BrowserBackend.detect(std::process::id()));
    }

    #[test]
    fn missing_browser_explains_remote_debugging_port() {
        let Err(err) = BrowserBackend.start(
            &Target { pid: 0 },
            &RecordOptions {
                duration: Some(Duration::from_millis(50)),
                inspect: Some("127.0.0.1:1".to_string()),
                ..RecordOptions::default()
            },
        ) else {
            panic!("port 1 should not be a browser debugger");
        };
        let message = err.to_string();
        assert!(message.contains("remote-debugging-port"), "{message}");
    }
}
