use thiserror::Error;

/// Failure while talking to an inspector.
#[derive(Debug, Error)]
pub enum Error {
    /// The endpoint, target list, or profile command was rejected.
    #[error("{0}")]
    Message(String),

    /// A socket read or write failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub(crate) fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}
