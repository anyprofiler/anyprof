//! Offline profile viewer: an embedded speedscope build served on 127.0.0.1.
//!
//! [`Viewer`] binds a loopback port and serves speedscope 1.25.0 plus one
//! profile, all under a random `/t/<token>/` prefix. [`open_browser`] opens
//! that URL, or says why it did not so the caller can print it instead.
//! Nothing is fetched from the network.

#![warn(missing_docs)]

mod assets;
mod open;
mod server;
mod token;

pub use open::{Launch, open_browser, ssh_forward_hint};
pub use server::{StopReason, Viewer};

/// Viewer errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The listener could not be created.
    #[error("could not listen on 127.0.0.1:{port}: {message}")]
    Bind {
        /// The requested port (0 means any).
        port: u16,
        /// Why binding failed.
        message: String,
    },
    /// The OS random number generator failed.
    #[error("could not generate the viewer token: {0}")]
    Random(String),
    /// Accepting a request failed.
    #[error("viewer server failed: {0}")]
    Serve(#[source] std::io::Error),
}
