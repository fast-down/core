extern crate alloc;

use crate::Event;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::marker::PhantomData;
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
pub struct DownloadResult<'a, 'b, PullError, PushError, PullStreamError, PushStreamError> {
    _phantom1: PhantomData<&'a ()>,
    _phantom2: PhantomData<&'b ()>,
    pub event_chain: AsyncReceiver<Event<PullError, PushError, PullStreamError, PushStreamError>>,
    joined: Arc<AtomicBool>,
    join_handle: Arc<Barrier>,
    abort_handle: Arc<Mutex<Option<AbortHandle>>>,
}

impl<RE, WE, RSE, WSE> Drop for DownloadResult<'_, '_, RE, WE, RSE, WSE> {
    fn drop(&mut self) {
        self.abort()
    }
}

impl<RE, WE, RSE, WSE> DownloadResult<'_, '_, RE, WE, RSE, WSE> {
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
            _phantom1: PhantomData,
            _phantom2: PhantomData,
        }
    }

    /// 只有第一次调用有效
    pub async fn join(&self) -> Result<(), JoinError> {
        if self
            .joined
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.join_handle.wait().await;
        };
        Ok(())
    }

    pub fn abort(&self) {
        if let Some(handle) = self.abort_handle.lock().take() {
            handle()
        }
    }
}
