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
