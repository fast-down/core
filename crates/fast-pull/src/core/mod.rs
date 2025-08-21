extern crate alloc;

use alloc::boxed::Box;
use crate::Event;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use kanal::AsyncReceiver;
use spin::Mutex;
use tokio::sync::Barrier;
use tokio::task::JoinError;

mod macros;
#[cfg(test)]
pub mod mock;
pub mod multi;
pub mod single;

type AbortHandle = Box<dyn FnOnce() + Send + 'static>;

#[derive(Clone)]
pub struct DownloadResult<PullError, PushError, PullStreamError, PushStreamError> {
    pub event_chain: AsyncReceiver<Event<PullError, PushError, PullStreamError, PushStreamError>>,
    joined: Arc<AtomicBool>,
    join_handle: Arc<Barrier>,
    abort_handle: Arc<Mutex<Option<AbortHandle>>>,
}

impl<RE, WE, RSE, WSE> DownloadResult<RE, WE, RSE, WSE> {
    pub fn new(
        event_chain: AsyncReceiver<Event<RE, WE, RSE, WSE>>,
        join_handle: Arc<Barrier>,
        abort_handle: AbortHandle,
    ) -> Self {
        Self {
            event_chain,
            join_handle,
            abort_handle: Arc::new(Mutex::new(Some(abort_handle))),
            joined: Default::default(),
        }
    }

    /// 只有第一次调用有效
    pub async fn join(&mut self) -> Result<(), JoinError> {
        match self
            .joined
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {
                self.join_handle.wait().await;
            }
            Err(_) => {}
        };
        Ok(())
    }

    pub fn abort(&self) {
        if let Some(handle) = self.abort_handle.lock().take() {
            handle()
        }
    }
}
