use thiserror::Error;

/// Failure while encoding or decoding a profile.
#[derive(Debug, Error)]
pub enum Error {
    /// The bytes are not a valid profile of the requested format.
    #[error("invalid {format} profile: {reason}")]
    Invalid {
        /// Format name (`collapsed`, `pprof`, `speedscope`).
        format: &'static str,
        /// What was wrong with the input.
        reason: String,
    },

    /// Compression or another I/O step failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub(crate) fn invalid(format: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            format,
            reason: reason.into(),
        }
    }
}
