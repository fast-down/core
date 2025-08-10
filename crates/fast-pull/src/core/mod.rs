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
#[cfg(test)]
pub mod mock;
pub mod multi;
pub mod single;

#[derive(Debug)]
pub struct DownloadResult<ReadError, WriteError> {
    pub event_chain: AsyncReceiver<Event<ReadError, WriteError>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    abort_handles: Arc<[AbortHandle]>,
}

impl<RE, WE> Clone for DownloadResult<RE, WE> {
    fn clone(&self) -> Self {
        Self {
            event_chain: self.event_chain.clone(),
            handle: self.handle.clone(),
            abort_handles: self.abort_handles.clone(),
        }
    }
}

impl<ReadError, WriteError> DownloadResult<ReadError, WriteError> {
    pub fn new(
        event_chain: AsyncReceiver<Event<ReadError, WriteError>>,
        handle: JoinHandle<()>,
        abort_handles: &[AbortHandle],
    ) -> Self {
        Self {
            event_chain,
            abort_handles: Arc::from(abort_handles),
            handle: Arc::new(Mutex::new(Some(handle))),
        }
    }

    /// 只有第一次调用有效
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
