use std::path::PathBuf;

use tape_core::Timestamp;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("{source_name}, line {line}: {message}")]
    Parse {
        source_name: String,
        line: u64,
        message: String,
    },

    #[error("{source_name}: timestamps go backwards ({previous} then {next})")]
    OutOfOrder {
        source_name: String,
        previous: Timestamp,
        next: Timestamp,
    },

    #[error("checksum mismatch for {file}: expected {expected}, got {actual}")]
    Checksum {
        file: String,
        expected: String,
        actual: String,
    },

    #[error("{path}: {message}")]
    Format { path: PathBuf, message: String },

    #[error("download failed for {url}: {message}")]
    Download { url: String, message: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Csv(#[from] csv::Error),

    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),

    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),

    #[error(transparent)]
    Arrow(#[from] arrow_schema::ArrowError),
}

pub type Result<T, E = DataError> = std::result::Result<T, E>;
