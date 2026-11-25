//! Backend traits and the unified CPU profile data model for anyprof.
//!
//! Node.js is the first-class pid backend. Browser pages are selected explicitly
//! and share the DevTools Protocol client. Native C++ sampling works on macOS
//! (Mach), Windows x64 (`StackWalk64`), and Linux x86_64 (perf events). Rust
//! sampling reuses that native sampler and labels demangled Rust frames.
//! Go sampling is still a stub.
//! Every runtime implements [`Backend`]. The CLI asks each backend whether it
//! recognizes a pid, then [`Backend::start`]s a [`Session`]. Stopping the session
//! yields one [`Profile`], regardless of which runtime produced the samples.
//!
//! # Stack order
//!
//! [`Stack::frames`] is leaf-first, matching pprof: index 0 is the leaf (the frame
//! that was on-CPU) and the last element is the root. Exporters reverse that order
//! when a format wants root-first stacks (collapsed stacks, speedscope).
//!
//! ```
//! use anyprof_core::{Frame, Profile, Runtime, Stack};
//! use std::time::Duration;
//!
//! let profile = Profile {
//!     pid: 1,
//!     comm: None,
//!     duration: Duration::from_secs(1),
//!     period: None,
//!     stacks: vec![Stack {
//!         frames: vec![Frame::new(Runtime::Native, "main")],
//!         weight: 3,
//!     }],
//!     samples: Vec::new(),
//!     raw_cpuprofile: None,
//! };
//! assert_eq!(profile.total_weight(), 3);
//! ```

#![warn(missing_docs)]

mod backend;
mod error;
mod format;
mod process;
mod profile;
mod stop;

pub use backend::{
    Backend, Capabilities, DEVTOOLS_SAMPLING_INTERVAL, LanguageUnwind, PERF_SAMPLING_INTERVAL,
    RecordOptions, Registry, Session, Target, interval_for_frequency,
};
pub use error::Error;
pub use format::Format;
#[cfg(unix)]
pub use process::send_sigusr1;
pub use process::{ProcessSnapshot, executable_file, inspect, process_exists};
pub use profile::{Frame, MAX_TIMED_SAMPLES, Profile, Runtime, Stack, TimedSample};
pub use stop::{SignalAction, StopGate};
