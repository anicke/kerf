use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not decode {path}: {message}")]
    Decode { path: PathBuf, message: String },
    #[error("could not write audio: {0}")]
    Encode(String),
    #[error("resampling failed: {0}")]
    Resample(String),
    #[error("invalid project file: {0}")]
    ProjectFormat(String),
    #[error("invalid range")]
    InvalidRange,
    #[error("nothing selected")]
    NothingSelected,
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("nothing to normalize: selection is silent")]
    Silent,
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Unsupported(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
