extern crate alloc;
use crate::Event;
use alloc::sync::Arc;
use core::fmt::Debug;
use kanal::AsyncReceiver;
use tokio::{
    sync::Mutex,
    task::{AbortHandle, JoinError, JoinHandle},
};

mod macros;
pub mod multi;
pub mod single;
#[cfg(test)]
pub mod utils;

#[derive(Debug)]
pub struct PullResult<PullError, PushError> {
    pub event_chain: AsyncReceiver<Event<PullError, PushError>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    abort_handles: Arc<[AbortHandle]>,
}

impl<RE, WE> Clone for PullResult<RE, WE> {
    fn clone(&self) -> Self {
        Self {
            event_chain: self.event_chain.clone(),
            handle: self.handle.clone(),
            abort_handles: self.abort_handles.clone(),
        }
    }
}

impl<PullError, PushError> PullResult<PullError, PushError> {
    pub fn new(
        event_chain: AsyncReceiver<Event<PullError, PushError>>,
        handle: JoinHandle<()>,
        abort_handles: &[AbortHandle],
    ) -> Self {
        Self {
            event_chain,
            abort_handles: Arc::from(abort_handles),
            handle: Arc::new(Mutex::new(Some(handle))),
        }
    }

    pub async fn join(&self) -> Result<(), JoinError> {
        if let Some(handle) = self.handle.lock().await.take() {
            handle.await?
        }
        Ok(())
    }

    pub fn abort(&self) {
        for abort_handle in self.abort_handles.iter() {
            abort_handle.abort();
        }
    }
}
