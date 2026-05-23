use fast_down::http::HttpError;
use reqwest::Client;
use std::sync::Arc;
use tokio::task::JoinError;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(String),
    #[error("Network error: {0:?}")]
    Request(#[from] reqwest::Error),
    #[error("Task error: {0:?}")]
    Task(#[from] Arc<JoinError>),
    #[error("Prefetch timeout: {0:?}")]
    PrefetchTimeout(HttpError<Client>),
    #[error("Task is already running")]
    AlreadyRunning,
}
