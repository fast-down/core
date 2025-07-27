use crate::Event;
use std::sync::Arc;
use tokio::{
    sync::{mpsc::UnboundedReceiver, Mutex},
    task::{JoinError, JoinHandle},
};

pub mod auto;
pub mod multi;
pub mod prefetch;
pub mod single;

pub type CancelFn = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct DownloadResult {
    pub event_chain: Arc<Mutex<UnboundedReceiver<Event>>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    cancel_fn: Arc<Mutex<Option<CancelFn>>>,
}

impl DownloadResult {
    pub fn new(
        event_chain: UnboundedReceiver<Event>,
        handle: JoinHandle<()>,
        cancel_fn: CancelFn,
    ) -> Self {
        Self {
            event_chain: Arc::new(Mutex::new(event_chain)),
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
