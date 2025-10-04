extern crate alloc;
use crate::Event;
use alloc::sync::{Arc, Weak};
use core::num::{NonZeroU64, NonZeroUsize};
use fast_steal::{Executor, Handle, TaskList};
use kanal::AsyncReceiver;
use tokio::{
    sync::Mutex,
    task::{AbortHandle, JoinError, JoinHandle},
};

mod macros;
pub mod mock;
pub mod multi;
pub mod single;

#[derive(Debug)]
pub struct DownloadResult<E: Executor, PullError, PushError> {
    pub event_chain: AsyncReceiver<Event<PullError, PushError>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    abort_handles: Option<Arc<[AbortHandle]>>,
    task_list: Option<Weak<TaskList<E>>>,
}

impl<E: Executor, PullError, PushError> Clone for DownloadResult<E, PullError, PushError> {
    fn clone(&self) -> Self {
        Self {
            event_chain: self.event_chain.clone(),
            handle: self.handle.clone(),
            abort_handles: self.abort_handles.clone(),
            task_list: self.task_list.clone(),
        }
    }
}

impl<E: Executor, PullError, PushError> DownloadResult<E, PullError, PushError> {
    pub fn new(
        event_chain: AsyncReceiver<Event<PullError, PushError>>,
        handle: JoinHandle<()>,
        abort_handles: Option<&[AbortHandle]>,
        task_list: Option<Weak<TaskList<E>>>,
    ) -> Self {
        Self {
            event_chain,
            abort_handles: abort_handles.map(Arc::from),
            handle: Arc::new(Mutex::new(Some(handle))),
            task_list,
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
        if let Some(handles) = &self.abort_handles {
            for handle in handles.iter() {
                handle.abort();
            }
        }
        if let Some(task_list) = &self.task_list
            && let Some(task_list) = task_list.upgrade()
        {
            task_list.handles(|iter| {
                for mut handle in iter {
                    handle.abort();
                }
            });
        }
    }

    pub fn set_threads(&self, threads: NonZeroUsize, min_chunk_size: NonZeroU64) {
        if let Some(task_list) = &self.task_list
            && let Some(task_list) = task_list.upgrade()
        {
            task_list.set_threads(threads, min_chunk_size);
        }
    }
}
