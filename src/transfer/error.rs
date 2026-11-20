// language: Rust, file: src/transfer/error.rs
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("file too large: {size} bytes (limit {limit} bytes)")]
    FileTooLarge { size: u64, limit: u64 },

    #[error("batch too large: {size} bytes (limit {limit} bytes)")]
    BatchTooLarge { size: u64, limit: u64 },

    #[error("too many files: {count} (limit {limit})")]
    TooManyFiles { count: usize, limit: usize },

    #[error("symbolic links are not supported: {0}")]
    Symlink(String),

    #[error("directories are not supported: {0}")]
    Directory(String),

    #[error("invalid file name: {0}")]
    InvalidFileName(String),

    #[error("device does not expose the AFC file-transfer service")]
    AfcUnavailable,

    #[error("device error: {0}")]
    Device(#[from] idevice::IdeviceError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, TransferError>;
