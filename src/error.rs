use crate::vcs::VcsError;
use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum LensError {
    #[error("{0}")]
    VcsError(#[from] VcsError),

    #[allow(dead_code)]
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),

    #[error(transparent)]
    IoError(#[from] io::Error),

    #[error(transparent)]
    Utf8Error(#[from] std::string::FromUtf8Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
}
