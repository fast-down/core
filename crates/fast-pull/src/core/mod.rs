use crate::{Event, handle::SharedHandle};
use core::sync::atomic::{AtomicBool, Ordering};
use crossfire::{MAsyncRx, mpmc};
use fast_steal::{Executor, Handle, TaskQueue};
use std::sync::{Arc, Weak};
use tokio::task::{AbortHandle, JoinError, JoinHandle};

pub mod handle;
pub mod mock;
pub mod multi;
pub mod single;

#[derive(Debug)]
pub struct DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    pub event_chain: MAsyncRx<mpmc::List<Event<PullError, PushError>>>,
    handle: Arc<SharedHandle<()>>,
    abort_handles: Option<Arc<[AbortHandle]>>,
    task_queue: Option<(Weak<E>, TaskQueue<E::Handle>)>,
    is_aborted: Arc<AtomicBool>,
}

impl<E, PullError, PushError> Clone for DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    fn clone(&self) -> Self {
        Self {
            event_chain: self.event_chain.clone(),
            handle: self.handle.clone(),
            abort_handles: self.abort_handles.clone(),
            task_queue: self.task_queue.clone(),
            is_aborted: self.is_aborted.clone(),
        }
    }
}

impl<E, PullError, PushError> DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    pub fn new(
        event_chain: MAsyncRx<mpmc::List<Event<PullError, PushError>>>,
        handle: JoinHandle<()>,
        abort_handles: Option<&[AbortHandle]>,
        task_queue: Option<(Weak<E>, TaskQueue<E::Handle>)>,
    ) -> Self {
        Self {
            event_chain,
            handle: Arc::new(SharedHandle::new(handle)),
            abort_handles: abort_handles.map(Arc::from),
            task_queue,
            is_aborted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// # Errors
    /// 当写入线程意外退出时返回 `Arc<JoinError>`
    pub async fn join(&self) -> Result<(), Arc<JoinError>> {
        self.handle.join().await
    }

    pub fn abort(&self) {
        if let Some(handles) = &self.abort_handles {
            for handle in handles.iter() {
                handle.abort();
            }
        }
        if let Some((_, task_queue)) = &self.task_queue {
            task_queue.handles(|iter| {
                for handle in iter {
                    handle.abort();
                }
            });
        }
        self.is_aborted.store(true, Ordering::Release);
    }

    pub fn set_threads(&self, threads: usize, min_chunk_size: u64) {
        if let Some((executor, task_queue)) = &self.task_queue {
            let executor = executor.upgrade();
            let res = task_queue.set_threads(
                threads,
                min_chunk_size,
                executor.as_ref().map(AsRef::as_ref),
            );
            if res.is_some() && threads > 0 {
                self.is_aborted.store(false, Ordering::Release);
            }
        }
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.is_aborted.load(Ordering::Acquire)
    }
}

impl<E, PullError, PushError> Drop for DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    fn drop(&mut self) {
        self.abort();
    }
}
