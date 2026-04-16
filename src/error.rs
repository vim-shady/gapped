use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum GappedError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("I/O error at path {path}: {source}")]
    IoPath {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Serialization error: {0}")]
    Serialize(#[from] rmp_serde::encode::Error),

    #[error("Deserialization error: {0}")]
    Deserialize(#[from] rmp_serde::decode::Error),

    #[error("Invalid format: {0}")]
    InvalidFormat(String),

    #[error("Checksum mismatch: expected {expected}, got {got}")]
    ChecksumMismatch { expected: String, got: String },

    #[error("Path is not relative or contains invalid components: {0}")]
    InvalidPath(PathBuf),

    #[error("Root directory does not exist: {0}")]
    RootNotFound(PathBuf),

    #[error("Verification failed: {0} discrepancies found ")]
    VerificationFailed(usize),

    #[error("Walk error: {0}")]
    Walk(#[from] walkdir::Error),

    #[error("Worker pool failed: {0}")]
    WorkerPoolFailure(&'static str),
}

pub type Result<T> = std::result::Result<T, GappedError>;
