use std::path::PathBuf;
use std::time::Duration;

/// Errors returned by ttry's public API.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid terminal dimensions {cols}x{rows}; both values must be greater than zero")]
    InvalidDimensions { cols: u16, rows: u16 },
    #[error("invalid {field}; timeout must be greater than zero")]
    InvalidTimeout { field: &'static str },
    #[error("invalid key expression `{0}`")]
    InvalidKey(String),
    #[error("unsupported key combination `{0}`: {1}")]
    UnsupportedKey(String, String),
    #[error("terminal coordinate ({col}, {row}) is outside the {cols}x{rows} screen")]
    OutOfBounds {
        col: u16,
        row: u16,
        cols: u16,
        rows: u16,
    },
    #[error("strict locator matched {count} locations: {locations}")]
    StrictLocator { count: usize, locations: String },
    #[error("operation timed out after {timeout:?}: {context}")]
    Timeout { timeout: Duration, context: String },
    #[error("process exited while waiting: {0}")]
    ProcessExited(String),
    #[error("process is already closed")]
    ProcessClosed,
    #[error("failed to launch `{command}`: {source}")]
    Launch {
        command: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid configuration in {path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("snapshot `{name}` is missing at {path}; rerun with update mode enabled")]
    SnapshotMissing { name: String, path: PathBuf },
    #[error("snapshot `{name}` does not match\n{diff}")]
    SnapshotMismatch { name: String, diff: String },
    #[error("runner error: {0}")]
    Runner(String),
}

pub type Result<T> = std::result::Result<T, Error>;
