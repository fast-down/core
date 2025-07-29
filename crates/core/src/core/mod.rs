use crate::Event;
use async_channel::Receiver;
use std::{fmt::Debug, sync::Arc};
use tokio::{
    sync::Mutex,
    task::{JoinError, JoinHandle},
};

pub mod auto;
pub mod multi;
pub mod prefetch;
pub mod single;

pub type CancelFn = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct DownloadResult {
    pub event_chain: Receiver<Event>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    cancel_fn: Arc<Mutex<Option<CancelFn>>>,
}

impl Debug for DownloadResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadResult")
            .field("event_chain", &self.event_chain)
            .field("handle", &self.handle)
            .field("cancel_fn", &"Box<dyn FnOnce() + Send>")
            .finish()
    }
}

impl DownloadResult {
    pub fn new(event_chain: Receiver<Event>, handle: JoinHandle<()>, cancel_fn: CancelFn) -> Self {
        Self {
            event_chain,
            handle: Arc::new(Mutex::new(Some(handle))),
            cancel_fn: Arc::new(Mutex::new(Some(cancel_fn))),
        }
    }

    pub async fn join(&self) -> Result<(), JoinError> {
        if let Some(handle) = self.handle.lock().await.take() {
            handle.await?
        }
        Ok(())
    }

    pub async fn cancel(&self) {
        if let Some(cancel_fn) = self.cancel_fn.lock().await.take() {
            cancel_fn()
        }
    }
}
