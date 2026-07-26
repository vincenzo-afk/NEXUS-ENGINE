//! Centralized error handling for Nexus.
//!
//! Every fallible operation in the engine returns [`NexusError`] wrapped in
//! the [`Result`] alias defined here. Keeping a single error enum makes it
//! possible for the CLI layer to match on failure kinds and present useful,
//! human-readable messages instead of raw panics or opaque error strings.

use std::path::PathBuf;
use thiserror::Error;

/// The single error type returned by all fallible Nexus operations.
///
/// Each variant carries enough context to produce a useful message without
/// requiring the caller to re-derive what went wrong.
#[derive(Debug, Error)]
pub enum NexusError {
    /// Wraps any I/O failure (reading files, walking directories, etc).
    #[error("I/O error at '{path}': {source}")]
    Io {
        /// The path being operated on when the error occurred.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A plain I/O error with no associated path (e.g. stdin/stdout).
    #[error("I/O error: {0}")]
    IoPlain(#[from] std::io::Error),

    /// The on-disk index file could not be deserialized.
    #[error("failed to deserialize index: {0}")]
    Deserialize(#[source] Box<bincode::ErrorKind>),

    /// The in-memory index could not be serialized to disk.
    #[error("failed to serialize index: {0}")]
    Serialize(#[source] Box<bincode::ErrorKind>),

    /// The index file exists but failed its checksum / version check.
    #[error("index file is corrupt or from an incompatible version: {0}")]
    CorruptIndex(String),

    /// The TOML configuration file could not be parsed.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// A search query string could not be parsed into a query AST.
    #[error("query syntax error: {0}")]
    QueryParse(String),

    /// A requested folder is not currently part of the index configuration.
    #[error("folder is not indexed: {0}")]
    FolderNotIndexed(PathBuf),

    /// A requested folder does not exist on disk.
    #[error("path does not exist: {0}")]
    PathNotFound(PathBuf),

    /// The filesystem watcher failed to start or process an event.
    #[error("filesystem watcher error: {0}")]
    Watcher(String),

    /// Catch-all for validation failures with a human-readable message.
    #[error("{0}")]
    Other(String),
}

impl NexusError {
    /// Builds an [`NexusError::Io`] variant, attaching the path that was
    /// being operated on for a more helpful error message.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        NexusError::Io {
            path: path.into(),
            source,
        }
    }
}

/// Convenience alias so modules can write `Result<T>` instead of
/// `Result<T, NexusError>`.
pub type Result<T> = std::result::Result<T, NexusError>;
