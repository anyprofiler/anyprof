use thiserror::Error;

/// Errors returned by backends and the backend registry.
#[derive(Debug, Error)]
pub enum Error {
    /// The backend was selected, but sampling is not built yet.
    #[error("{backend} backend is not implemented yet: {detail}")]
    NotImplemented {
        /// Backend name (`native`, `node`, `go`, ...).
        backend: &'static str,
        /// What is missing, in plain language.
        detail: &'static str,
    },

    /// No registered backend's [`crate::Backend::detect`] returned true.
    #[error("no backend matched pid {pid}")]
    NoMatchingBackend {
        /// Process that was probed.
        pid: u32,
    },

    /// [`crate::Registry::get`] was given a name that is not registered.
    #[error("unknown backend `{name}` (available: {available})")]
    UnknownBackend {
        /// The name the caller asked for.
        name: String,
        /// Comma-separated registered names.
        available: String,
    },

    /// A backend failed while attaching or sampling.
    #[error("{backend}: {message}")]
    Backend {
        /// Backend name (`node`, `browser`, ...).
        backend: &'static str,
        /// What went wrong.
        message: String,
    },
}

impl Error {
    /// Build a [`Error::NotImplemented`] value.
    #[must_use]
    pub fn not_implemented(backend: &'static str, detail: &'static str) -> Self {
        Self::NotImplemented { backend, detail }
    }

    /// Build a [`Error::Backend`] value.
    pub fn backend(backend: &'static str, message: impl Into<String>) -> Self {
        Self::Backend {
            backend,
            message: message.into(),
        }
    }
}
