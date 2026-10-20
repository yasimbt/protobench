//! Crate-wide error type.

use std::path::PathBuf;

/// Errors returned by `pb-core`.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// I/O failure, with the path when one is known.
    #[error("{path}: {source}")]
    Io {
        /// File or device involved.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// Plain I/O failure without a path (sockets, ports).
    #[error(transparent)]
    IoBare(#[from] std::io::Error),
    /// Configuration problem.
    #[error("config: {0}")]
    Config(String),
    /// Schema problem.
    #[error("schema: {0}")]
    Schema(String),
    /// A message could not be encoded or decoded.
    #[error("codec: {0}")]
    Codec(String),
    /// A payload could not be built.
    #[error("payload: {0}")]
    Payload(String),
    /// Transport failure.
    #[error("transport: {0}")]
    Transport(String),
    /// Recording or export failure.
    #[error("recorder: {0}")]
    Recorder(String),
    /// Sequence failure (bad file or runtime error, not an assertion failure).
    #[error("sequence: {0}")]
    Sequence(String),
    /// Expression failure.
    #[error("expression: {0}")]
    Expr(String),
}

impl CoreError {
    /// Wraps an I/O error with the path it relates to.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        CoreError::Io {
            path: path.into(),
            source,
        }
    }
}

/// Result alias for `pb-core`.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;
