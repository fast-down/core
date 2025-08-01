use crate::Event;
use crate::base::pusher::Pusher;
use crate::base::source::{Fetcher, Puller};
use async_channel::Receiver;
use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::Mutex,
    task::{JoinError, JoinHandle},
};

mod macros;
#[cfg(test)]
mod mock;
pub mod multi;
pub mod single;

#[derive(Debug, Clone)]
pub struct FetchResult<FetchError, PullError, PushError> {
    pub event_chain: Receiver<Event<FetchError, PullError, PushError>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    is_running: Arc<AtomicBool>,
}

impl<FetchError, PullError, PushError> FetchResult<FetchError, PullError, PushError>
{
    pub fn new(
        event_chain: Receiver<Event<FetchError, PullError, PushError>>,
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
