//! Backend traits and the unified CPU profile data model for anyprof.
//!
//! Every runtime (native code, Go, Node.js, and later Python, Java, and others)
//! implements [`Backend`]. The CLI asks each backend whether it recognizes a pid,
//! then [`Backend::start`]s a [`Session`]. Stopping the session yields one [`Profile`],
//! regardless of which runtime produced the samples.
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
//! };
//! assert_eq!(profile.total_weight(), 3);
//! ```

#![warn(missing_docs)]

mod backend;
mod error;
mod process;
mod profile;

pub use backend::{Backend, Capabilities, RecordOptions, Registry, Session, Target};
pub use error::Error;
pub use process::{executable_file, inspect, process_exists, ProcessSnapshot};
pub use profile::{Frame, Profile, Runtime, Stack};
