use crate::Event;
use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};
use futures::lock::Mutex;
use kanal::AsyncReceiver;
use std::sync::Arc;
use tokio::task::{JoinError, JoinHandle};

mod macros;
#[cfg(test)]
mod mock;
pub mod multi;
pub mod single;

#[derive(Debug, Clone)]
pub struct DownloadResult<ReadError, WriteError> {
    pub event_chain: AsyncReceiver<Event<ReadError, WriteError>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    is_running: Arc<AtomicBool>,
}

impl<ReadError, WriteError> DownloadResult<ReadError, WriteError> {
    pub fn new(
        event_chain: AsyncReceiver<Event<ReadError, WriteError>>,
        handle: JoinHandle<()>,
        is_running: Arc<AtomicBool>,
    ) -> Self {
        Self {
            event_chain,
            handle: Arc::new(Mutex::new(Some(handle))),
            is_running,
        }
    }

    pub async fn join(&self) -> Result<(), JoinError> {
        if let Some(handle) = self.handle.lock().await.take() {
            handle.await?
        }
        Ok(())
    }

    /// 取消后记得调用 `self.join().await` 等待真正的退出
    pub fn cancel(&self) {
        self.is_running.store(false, Ordering::Relaxed);
    }
}
