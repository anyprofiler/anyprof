//! Export and import [`anyprof_core::Profile`] values.
//!
//! Collapsed stacks (Brendan Gregg's folded format), speedscope JSON,
//! gzip-compressed pprof protobuf, and V8 `.cpuprofile` JSON can all be written
//! and read back. Collapsed stacks are lossy: file, line, column, module, and
//! address are dropped, and identical folded stacks are merged.
//!
//! Non-native frames are labeled `[runtime] name` (for example `[node] work`)
//! so a mixed stack still records which runtime each frame came from.

#![warn(missing_docs)]

mod collapsed;
mod cpuprofile;
mod error;
mod pprof;
mod proto;
mod speedscope;
mod symbol;

use anyprof_core::{Profile, Runtime};

pub use anyprof_core::Format;
pub use cpuprofile::import_cpuprofile;
pub use error::Error;

/// Encode `profile` as `format`.
pub fn export(profile: &Profile, format: Format) -> Result<Vec<u8>, Error> {
    match format {
        Format::Collapsed => Ok(collapsed::export_string(profile).into_bytes()),
        Format::Pprof => pprof::export(profile),
        Format::Speedscope => speedscope::export(profile),
        Format::Cpuprofile => cpuprofile::export(profile),
    }
}

/// How many samples a profile holds and how many caught the thread idle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SampleCounts {
    /// Samples in the profile.
    pub total: u64,
    /// Samples whose leaf frame is V8's `(idle)` or `(program)`.
    pub idle: u64,
}

impl SampleCounts {
    /// Whether more than `percent` percent of the samples are idle.
    #[must_use]
    pub fn idle_above(self, percent: u64) -> bool {
        self.total > 0 && u128::from(self.idle) * 100 > u128::from(self.total) * u128::from(percent)
    }
}

/// Count the samples in `profile`.
///
/// A captured `.cpuprofile` is counted from the V8 document, one per sample:
/// its imported stack weights are microseconds, not samples. Other profiles
/// count stack weights.
#[must_use]
pub fn sample_counts(profile: &Profile) -> SampleCounts {
    if let Some(counts) = profile
        .raw_cpuprofile
        .as_deref()
        .and_then(|raw| cpuprofile::sample_counts(raw).ok())
    {
        return counts;
    }
    let mut counts = SampleCounts::default();
    for stack in &profile.stacks {
        counts.total = counts.total.saturating_add(stack.weight);
        if stack
            .frames
            .first()
            .is_some_and(|leaf| cpuprofile::is_idle_name(&leaf.name))
        {
            counts.idle = counts.idle.saturating_add(stack.weight);
        }
    }
    counts
}

/// Decode a profile produced by [`export`] (or a compatible file of that format).
pub fn import(bytes: &[u8], format: Format) -> Result<Profile, Error> {
    match format {
        Format::Collapsed => collapsed::import(bytes),
        Format::Pprof => pprof::import(bytes),
        Format::Speedscope => speedscope::import(bytes),
        // Files saved by Node or Chrome have no runtime prefix, so frames are Node.
        // Browser capture calls [`import_cpuprofile`] with [`Runtime::Browser`].
        Format::Cpuprofile => import_cpuprofile(bytes, Runtime::Node),
    }
}

#[cfg(test)]
fn fixture() -> Profile {
    use anyprof_core::{Frame, Runtime, Stack};
    use std::time::Duration;

    let mut root = Frame::new(Runtime::Native, "main");
    root.address = Some(0x1000);

    let mut leaf = Frame::new(Runtime::Node, "work");
    leaf.file = Some("app.js".to_string());
    leaf.line = Some(12);
    leaf.column = Some(3);

    Profile {
        pid: 4242,
        comm: Some("demo".to_string()),
        duration: Duration::from_secs(5),
        period: Some(Duration::from_millis(10)),
        stacks: vec![
            Stack {
                frames: vec![leaf, root],
                weight: 2,
            },
            Stack {
                frames: vec![
                    Frame::new(Runtime::Node, "work"),
                    Frame::new(Runtime::Native, "main"),
                ],
                weight: 1,
            },
            Stack {
                frames: vec![
                    Frame::new(Runtime::Go, "runtime.main"),
                    Frame::new(Runtime::Kernel, "schedule"),
                ],
                weight: 4,
            },
        ],
        raw_cpuprofile: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{SampleCounts, sample_counts};
    use anyprof_core::{Frame, Profile, Runtime, Stack};

    #[test]
    fn stack_weights_count_when_there_is_no_raw_cpuprofile() {
        let profile = Profile {
            stacks: vec![
                Stack {
                    frames: vec![Frame::new(Runtime::Node, "(idle)")],
                    weight: 96,
                },
                Stack {
                    frames: vec![
                        Frame::new(Runtime::Node, "(idle)"),
                        Frame::new(Runtime::Node, "caller"),
                    ],
                    weight: 1,
                },
                Stack {
                    frames: vec![Frame::new(Runtime::Node, "work")],
                    weight: 3,
                },
            ],
            ..Profile::default()
        };
        assert_eq!(
            sample_counts(&profile),
            SampleCounts {
                total: 100,
                idle: 97
            }
        );
    }

    #[test]
    fn idle_share_threshold_is_strict() {
        let at = |idle, total| SampleCounts { total, idle }.idle_above(95);
        assert!(at(96, 100));
        assert!(!at(95, 100));
        assert!(at(848, 850));
        assert!(!at(0, 0));
        assert!(at(u64::MAX, u64::MAX));
    }
}
