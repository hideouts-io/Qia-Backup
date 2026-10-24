//! Error types used across the backup engine.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error: {0}")]
    PlainIo(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid backup data: {0}")]
    Codec(String),

    #[error("cryptography error: {0}")]
    Crypto(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("S3 request failed: {0}")]
    S3(String),

    #[error("storage error: {0}")]
    Store(String),

    #[error("snapshot error: {0}")]
    Snapshot(String),

    #[error("backup cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl BackupError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn codec(message: impl Into<String>) -> Self {
        Self::Codec(message.into())
    }
}

pub type Result<T> = std::result::Result<T, BackupError>;

pub trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::result::Result<T, std::io::Error> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| BackupError::io(path, source))
    }
}
