use std::{collections::BTreeMap, fmt, io};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            details: BTreeMap::new(),
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn detail(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("filesystem error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid request: {0}")]
    InvalidInput(String),
    #[error("requested item was not found: {0}")]
    NotFound(String),
    #[error("operation conflicts with current state: {0}")]
    Conflict(String),
    #[error("media validation failed: {0}")]
    Media(String),
    #[error("required media tool is missing: {0}")]
    MediaToolMissing(String),
    #[error("audio capture failed: {0}")]
    Audio(String),
    #[error("worker failed: {0}")]
    Worker(String),
    #[error("security operation failed: {0}")]
    Security(String),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl From<CoreError> for ApiError {
    fn from(value: CoreError) -> Self {
        match value {
            CoreError::Database(error) => ApiError::new("database_error", error.to_string())
                .retryable()
                .detail("source", "sqlite"),
            CoreError::Io(error) => {
                let retryable = matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                );
                let api = ApiError::new("filesystem_error", error.to_string());
                if retryable {
                    api.retryable()
                } else {
                    api
                }
            }
            CoreError::InvalidInput(message) => ApiError::new("invalid_input", message),
            CoreError::NotFound(message) => ApiError::new("not_found", message),
            CoreError::Conflict(message) => ApiError::new("conflict", message),
            CoreError::Media(message) => ApiError::new("media_validation_failed", message),
            CoreError::MediaToolMissing(message) => ApiError::new("ffmpeg_missing", message),
            CoreError::Audio(message) => ApiError::new("audio_capture_failed", message).retryable(),
            CoreError::Worker(message) => ApiError::new("worker_unavailable", message).retryable(),
            CoreError::Security(message) => ApiError::new("security_error", message),
            CoreError::Serialization(error) => {
                ApiError::new("serialization_error", error.to_string())
            }
        }
    }
}

pub type CoreResult<T> = Result<T, CoreError>;
pub type CommandResult<T> = Result<T, ApiError>;
