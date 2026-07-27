use crate::{Event, handle::SharedHandle};
use core::sync::atomic::{AtomicBool, Ordering};
use crossfire::{MAsyncRx, mpmc};
use fast_steal::{Executor, Handle, TaskQueue};
use std::fmt;
use std::sync::{Arc, OnceLock, Weak};
use std::thread::Thread;
use tokio::task::{AbortHandle, JoinError, JoinHandle};

pub mod handle;
pub mod mock;
pub mod multi;
pub mod single;

/// Shared state of an active download session.
///
/// Owned inside an `Arc` by [`DownloadResult`]. Because all clones of a
/// `DownloadResult` share the **same** `Arc<DownloadResultInner>`, the `Drop`
/// impl below runs exactly once — when the last clone is dropped. That is where
/// cancellation happens, giving `DownloadResult` `Arc`-style "last owner gone →
/// release" semantics: the download keeps running as long as any handle is
/// alive, and is cancelled only when the final one is dropped.
struct DownloadResultInner<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    event_chain: MAsyncRx<mpmc::List<Event<PullError, PushError>>>,
    handle: SharedHandle<()>,
    abort_handles: Option<Arc<[AbortHandle]>>,
    task_queue: Option<(Weak<E>, TaskQueue<E::Handle>)>,
    /// Shared abort signal. Wrapped in `Arc` so the very same `AtomicBool`
    /// instance can be handed to the `spawn_blocking` push driver (which reads
    /// it) while `abort()` (which writes it) keeps a clone. `Arc` derefs to
    /// `AtomicBool`, so every `.swap`/`.load`/`.store` call below is unchanged.
    is_aborted: Arc<AtomicBool>,
    /// Handle of the blocking push/flush worker thread, set by the worker
    /// itself as its first action. `abort()` uses it to `unpark()` the worker
    /// so a retry backoff (`std::thread::park_timeout`) is cut short instead
    /// of sleeping out the full `retry_gap`. `unpark()` on an already-exited
    /// thread is safe, and a stored "unpark token" (from an unpark racing
    /// ahead of the park) only causes one spurious wakeup, after which the
    /// worker re-checks `is_aborted` at the top of its loop.
    push_worker: Arc<OnceLock<Thread>>,
}

impl<E, PullError, PushError> fmt::Debug for DownloadResultInner<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadResultInner")
            .field("event_chain", &self.event_chain)
            .field("is_aborted", &self.is_aborted.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl<E, PullError, PushError> DownloadResultInner<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    /// # Errors
    /// Returns `Arc<JoinError>` if the writer thread exits unexpectedly
    pub async fn join(&self) -> Result<(), Arc<JoinError>> {
        self.handle.join().await
    }

    /// Cancel all workers immediately.
    ///
    /// Safe to call multiple times and safe to call while other clones of the
    /// owning [`DownloadResult`] are still alive. The implicit drop-based
    /// cancellation (on the last clone) becomes a no-op once this has run.
    pub fn abort(&self) {
        if !self.is_aborted.swap(true, Ordering::Release) {
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
            // Wake the push worker if it is sleeping in `park_timeout`
            // between retries, so `join()` returns promptly instead of
            // waiting out the remaining `retry_gap`.
            if let Some(worker) = self.push_worker.get() {
                worker.unpark();
            }
        }
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

impl<E, PullError, PushError> Drop for DownloadResultInner<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    fn drop(&mut self) {
        self.abort();
    }
}

/// Handle to an active download session.
///
/// Cheaply cloneable shared handle. The underlying download keeps running as
/// long as **any** clone is alive, and is cancelled only once the last clone is
/// dropped. An explicit [`DownloadResultInner::abort`] cancels immediately.
///
/// `DownloadResult` derefs to [`DownloadResultInner`], so all session methods
/// (`join`, `abort`, `set_threads`, `is_aborted`) and the `event_chain` field
/// are reachable directly on the handle.
#[derive(Debug)]
pub struct DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    inner: Arc<DownloadResultInner<E, PullError, PushError>>,
}

impl<E, PullError, PushError> Clone for DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
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
        abort_flag: Arc<AtomicBool>,
        push_worker: Arc<OnceLock<Thread>>,
    ) -> Self {
        Self {
            inner: Arc::new(DownloadResultInner {
                event_chain,
                handle: SharedHandle::new(handle),
                abort_handles: abort_handles.map(Arc::from),
                task_queue,
                is_aborted: abort_flag,
                push_worker,
            }),
        }
    }

    #[must_use]
    pub fn event_chain(&self) -> &MAsyncRx<mpmc::List<Event<PullError, PushError>>> {
        &self.inner.event_chain
    }

    /// # Errors
    /// Returns `Arc<JoinError>` if the writer thread exits unexpectedly
    pub async fn join(&self) -> Result<(), Arc<JoinError>> {
        self.inner.join().await
    }

    /// Cancel all workers immediately.
    ///
    /// Safe to call multiple times and safe to call while other clones of the
    /// owning [`DownloadResult`] are still alive. The implicit drop-based
    /// cancellation (on the last clone) becomes a no-op once this has run.
    pub fn abort(&self) {
        self.inner.abort();
    }

    pub fn set_threads(&self, threads: usize, min_chunk_size: u64) {
        self.inner.set_threads(threads, min_chunk_size);
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.inner.is_aborted()
    }
}
