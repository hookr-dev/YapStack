use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranscriptionError {
    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("transcription failed: {0}")]
    TranscriptionFailed(String),

    #[error("invalid audio input: {0}")]
    InvalidInput(String),

    #[error("model download failed: {0}")]
    DownloadFailed(String),

    #[error("already downloading: {0}")]
    DownloadInProgress(String),

    #[error("sidecar process error: {0}")]
    SidecarError(String),

    #[error("sidecar not running")]
    SidecarNotRunning,

    #[error("request timed out after {0}s")]
    Timeout(u64),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}
