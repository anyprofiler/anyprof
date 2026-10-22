//! Blocking Chrome DevTools Protocol client.
//!
//! Node.js and browser pages share this crate. It discovers inspector targets
//! over HTTP (`/json/list`) and drives `Profiler.*` on the target's WebSocket.
//! The V8 `.cpuprofile` JSON that `Profiler.stop` returns is decoded by
//! `anyprof-export`.

#![warn(missing_docs)]

mod client;
mod endpoint;
mod error;
mod http;
mod targets;

pub use client::capture_cpu_profile;
pub use endpoint::{InspectEndpoint, parse_inspect};
pub use error::Error;
pub use targets::{DebugTarget, choose_node, choose_page, list_targets};

/// Convert a sampling interval to the whole microseconds
/// `Profiler.setSamplingInterval` takes.
///
/// Sub-microsecond intervals round up to one microsecond; very long ones
/// saturate at `u32::MAX`.
#[must_use]
pub fn sampling_interval_us(interval: std::time::Duration) -> u32 {
    u32::try_from(interval.as_micros())
        .unwrap_or(u32::MAX)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::sampling_interval_us;

    #[test]
    fn sampling_runs_until_a_limit_or_a_stop_request() {
        use std::time::Duration;

        use crate::client::keep_sampling;

        assert!(keep_sampling(Duration::from_secs(1), None, false));
        assert!(!keep_sampling(Duration::from_secs(1), None, true));
        assert!(keep_sampling(
            Duration::from_millis(500),
            Some(Duration::from_secs(2)),
            false
        ));
        assert!(!keep_sampling(
            Duration::from_secs(2),
            Some(Duration::from_secs(2)),
            false
        ));
        assert!(!keep_sampling(
            Duration::from_millis(10),
            Some(Duration::from_secs(2)),
            true
        ));
    }

    #[test]
    fn intervals_map_to_whole_microseconds() {
        use std::time::Duration;

        assert_eq!(sampling_interval_us(Duration::from_micros(100)), 100);
        assert_eq!(sampling_interval_us(Duration::from_millis(1)), 1000);
        assert_eq!(
            sampling_interval_us(Duration::from_nanos(10_101_010)),
            10_101
        );
        assert_eq!(sampling_interval_us(Duration::from_nanos(200)), 1);
        assert_eq!(sampling_interval_us(Duration::ZERO), 1);
        assert_eq!(sampling_interval_us(Duration::from_secs(1 << 40)), u32::MAX);
    }
}
