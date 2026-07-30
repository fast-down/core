//! Top-level download orchestration: session handle plus single- and
//! multi-threaded entry points.
//!
//! [`download_single`](crate::single::download_single) runs a sequential pull,
//! while [`download_multi`](crate::multi::download_multi) splits the work across
//! concurrent workers with work-stealing. Both return a [`DownloadResult`], a
//! cheaply cloneable handle that keeps the session alive until the last clone is
//! dropped (or [`DownloadResult::abort`] is called).

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
/// dropped. An explicit [`abort`](Self::abort) cancels immediately.
///
/// `DownloadResult` derefs to `DownloadResultInner`, so all session methods
/// (`join`, `abort`, `set_threads`, `is_aborted`) and the [`event_chain`](Self::event_chain)
/// method are reachable directly on the handle.
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
    /// Construct a [`DownloadResult`] from the raw session pieces.
    ///
    /// This is an internal constructor used by
    /// [`download_single`](crate::single::download_single) and
    /// [`download_multi`](crate::multi::download_multi); prefer those entry
    /// points instead of calling this directly.
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

    /// Access the stream of [`Event`]s emitted during the session.
    ///
    /// The receiver closes once the last clone of this handle is dropped or the
    /// session is aborted, so draining it is a natural way to observe progress.
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

    /// Adjust the worker thread count and minimum chunk size of a running
    /// multi-threaded session.
    ///
    /// No-op for single-threaded sessions, which have no task queue.
    pub fn set_threads(&self, threads: usize, min_chunk_size: u64) {
        self.inner.set_threads(threads, min_chunk_size);
    }

    /// Whether the session has been (or is being) cancelled.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.inner.is_aborted()
    }
}

#[cfg(test)]
#[cfg(feature = "mem")]
mod tests {
    #![allow(clippy::unwrap_used)]
    use crate::mem::MemPusher;
    use crate::mock::{MockPuller, build_mock_data};
    use crate::multi::{DownloadOptions, download_multi};
    use tokio::time::Duration;

    #[tokio::test(flavor = "multi_thread")]
    async fn download_result_debug_and_set_threads() {
        let mock_data = build_mock_data(1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 4,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        // Lines 63-68: `Debug` of `DownloadResultInner` (via the derived `Debug`).
        let _ = format!("{result:?}");
        // Lines 234-236 (forwarding) and 111-123 (inner task-queue adjustment).
        result.set_threads(4, 1);
        result.join().await.unwrap();
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_result_clone_and_set_threads_no_queue() {
        use crate::single::download_single;
        let mock_data = build_mock_data(1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        // Single-threaded sessions have no task queue, so `set_threads` is a no-op
        // (covers the `if let` else path, line 122 of `DownloadResultInner`).
        let result = download_single(
            puller,
            pusher,
            crate::single::DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
            },
        );
        // Lines 167-171: `DownloadResult` is `Clone`.
        let _clone = result.clone();
        result.set_threads(4, 1);
        while result.event_chain().recv().await.is_ok() {}
        result.join().await.unwrap();
        assert_eq!(&**receive.lock(), mock_data);
    }
}
