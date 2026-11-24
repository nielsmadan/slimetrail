use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("index is busy: lock deadline exceeded")]
    IndexBusy,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("I/O failure at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON data")]
    Json(#[from] serde_json::Error),
    #[error("invalid timestamp")]
    Time(#[from] jiff::Error),
    #[error("SQLite operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("append resume state no longer matches the source")]
    ResumeRejected,
}

pub type Result<T> = std::result::Result<T, Error>;
