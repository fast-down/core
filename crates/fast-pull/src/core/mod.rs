//! Top-level download orchestration: session handle plus single- and
//! multi-threaded entry points.
//!
//! [`download_single`](crate::single::download_single) runs a sequential pull,
//! while [`download_multi`](crate::multi::download_multi) splits the work across
//! concurrent workers with work-stealing. Both return a [`DownloadResult`], a
//! cheaply cloneable handle that keeps the session alive until the last clone is
//! dropped (or [`DownloadResult::abort`] is called).

use crate::Event;
use crossfire::{MAsyncRx, mpmc};
use fast_steal::{Executor, TaskQueue};
use std::fmt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
    /// The work-stealing queue of a multi-threaded session together with the
    /// executor that spawns its workers, or `None` for a single-threaded one.
    ///
    /// The executor is kept for the entire lifetime of the handle so
    /// [`set_threads`](Self::set_threads) can spawn additional workers while the
    /// download is running. It reaches the session's channels through a weak
    /// reference, so retaining it here never keeps a finished session open.
    task_queue: Option<(E, TaskQueue<E::Handle>)>,
    /// Session-wide cancellation token, shared (as a clone) with every worker
    /// and with the `spawn_blocking` push driver. [`abort`](Self::abort) cancels
    /// this root token, which broadcasts to all linked child tokens — including
    /// workers spawned *after* the cancel call — so a late worker observes
    /// cancellation on its next poll instead of needing a separate one-shot
    /// notify per handle. Cancellation is terminal: once cancelled it never
    /// clears, so `is_aborted` stays `true` for the rest of the session.
    abort_token: CancellationToken,
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
            .field("is_aborted", &self.abort_token.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl<E, PullError, PushError> DownloadResultInner<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    /// Cancel all workers immediately.
    ///
    /// Safe to call multiple times and safe to call while other clones of the
    /// owning [`DownloadResult`] are still alive. The implicit drop-based
    /// cancellation (on the last clone) becomes a no-op once this has run.
    pub fn abort(&self) {
        self.abort_token.cancel();
    }

    pub fn set_threads(&self, threads: usize, min_chunk_size: u64) -> Option<()> {
        let (executor, task_queue) = self.task_queue.as_ref()?;
        task_queue.set_threads(threads, min_chunk_size, Some(executor))
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.abort_token.is_cancelled()
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
/// `DownloadResult` wraps `Arc<DownloadResultInner>` and exposes the session
/// methods (`abort`, `set_threads`, `is_aborted`) and
/// [`event_chain`](Self::event_chain) directly; each delegates to the inner
/// value. There is intentionally **no** `Deref` impl — `DownloadResultInner`
/// is private, so callers reach session state only through these methods.
///
/// Completion is observed by draining [`event_chain`](Self::event_chain): once
/// the last sender is dropped (the download finished or was aborted) the
/// receiver disconnects, so `while result.event_chain().recv().await.is_ok() {}`
/// awaits the session end.
pub struct DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    inner: Arc<DownloadResultInner<E, PullError, PushError>>,
}

impl<E, PullError, PushError> fmt::Debug for DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadResult")
            .field("inner", &self.inner)
            .finish()
    }
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
        task_queue: Option<(E, TaskQueue<E::Handle>)>,
        abort_token: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(DownloadResultInner {
                event_chain,
                task_queue,
                abort_token,
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
    /// Growing spawns workers for ranges still waiting in the queue, or splits a
    /// range off the busiest running worker when nothing is waiting. Shrinking
    /// aborts the surplus workers and returns their ranges to the queue for the
    /// survivors to steal. No-op for single-threaded sessions, which have no
    /// task queue.
    ///
    /// A session ends when its last worker exits, and it cannot be restarted:
    /// growing afterwards spawns nothing, because the workers' shared channels
    /// are closed at that point. Shrinking is clamped to a minimum of one worker
    /// by the scheduler, so `set_threads(0)` keeps a single worker alive rather
    /// than ending the session. The session terminates only when that last
    /// (clamped) worker exits on its own.
    ///
    /// This never touches the [`abort`](Self::abort) token: abort is terminal and
    /// cannot be undone by resizing, so [`is_aborted`](Self::is_aborted) stays
    /// `true` across any later `set_threads` call, and any worker spawned by such
    /// a call observes the cancelled token and exits without pulling.
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
    #![allow(clippy::cast_possible_truncation)]
    use crate::mem::MemPusher;
    use crate::mock::{MockPuller, build_mock_data};
    use crate::multi::{DownloadOptions, download_multi};
    use crate::{Event, ProgressEntry, PullResult, PullStream, Puller};
    use bytes::Bytes;
    use futures::{StreamExt, stream};
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use tokio::time::{Duration, sleep, timeout};

    /// A [`Puller`] that stalls for `delay` before yielding a range in one piece,
    /// so a test can resize the worker pool while the download is still running.
    #[derive(Debug, Clone)]
    struct SlowPuller {
        data: Arc<[u8]>,
        delay: Duration,
    }
    impl Puller for SlowPuller {
        type Error = std::convert::Infallible;
        fn pull(
            &mut self,
            range: Option<&ProgressEntry>,
        ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send
        {
            type PullItem = Result<Bytes, (std::convert::Infallible, Option<Duration>)>;
            let owned: Vec<u8> = match range {
                Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                None => self.data.to_vec(),
            };
            let delay = self.delay;
            async move {
                sleep(delay).await;
                let items: Vec<PullItem> = vec![Ok(Bytes::from(owned))];
                Ok(stream::iter(items))
            }
        }
    }

    /// A [`Puller`] that yields its range in small pieces with a pause between
    /// each, so a shrink that aborts a worker lands *mid-range* instead of on a
    /// clean boundary. That is the shape which exercises cursor hand-off: the
    /// reclaimed range must resume from the advanced cursor, without dropping or
    /// re-delivering the bytes the aborted worker had already pushed.
    #[derive(Debug, Clone)]
    struct ChunkedPuller {
        data: Arc<[u8]>,
        piece: usize,
        delay: Duration,
    }
    impl Puller for ChunkedPuller {
        type Error = std::convert::Infallible;
        fn pull(
            &mut self,
            range: Option<&ProgressEntry>,
        ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send
        {
            let owned: Vec<u8> = match range {
                Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                None => self.data.to_vec(),
            };
            let piece = self.piece;
            let delay = self.delay;
            async move {
                Ok(
                    stream::unfold(Bytes::from(owned), move |mut buf| async move {
                        if buf.is_empty() {
                            return None;
                        }
                        let next = buf.split_to(piece.min(buf.len()));
                        sleep(delay).await;
                        Some((Ok(next), buf))
                    })
                    .boxed(),
                )
            }
        }
    }

    /// Deterministic xorshift64. A churn schedule driven by a fixed seed keeps a
    /// failure reproducible instead of turning the test into a lottery.
    fn next_rand(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    /// Eight equally sized ranges over `size` bytes, so a one-worker session
    /// leaves seven of them waiting in the queue.
    fn eight_chunks(size: u64) -> Vec<ProgressEntry> {
        let step = size / 8;
        (0..8).map(|i| i * step..(i + 1) * step).collect()
    }

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
        // `Debug` of `DownloadResultInner` is reached through `DownloadResult`'s
        // own `Debug` impl, which forwards to the inner value.
        let _ = format!("{result:?}");
        // Lines 234-236 (forwarding) and 111-123 (inner task-queue adjustment).
        result.set_threads(4, 1);
        // Await completion by draining `event_chain` (it disconnects once the
        // push driver drops its sender) — this replaces `join()`.
        while result.event_chain().recv().await.is_ok() {}
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
        assert_eq!(&**receive.lock(), mock_data);
    }

    // Pins the `is_aborted` interaction with `set_threads`: a *live*
    // (never-aborted) session keeps `is_aborted() == false` after `set_threads`,
    // while an *already-aborted* session stays aborted — the flag is a one-way
    // latch, so `is_aborted()` cannot report a false "live" state for a cancelled
    // download.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_threads_does_not_unabort_an_aborted_session() {
        let mock_data = build_mock_data(1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 4,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: std::iter::once(0..mock_data.len() as u64),
                pull_timeout: Duration::from_secs(5),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        // Live session: flag starts false and must stay false after a resize.
        assert!(!result.is_aborted());
        result.set_threads(4, 1);
        assert!(!result.is_aborted());
        // Now abort: flag is true and must *stay* true across a later resize.
        result.abort();
        assert!(result.is_aborted());
        result.set_threads(4, 1);
        assert!(result.is_aborted());
        while result.event_chain().recv().await.is_ok() {}
    }

    // Growing the pool of a *running* session must actually put more workers to
    // work. The session starts with a single worker and seven ranges waiting;
    // if the growth were a no-op, every `Pulling` event would carry worker id 0.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_threads_growth_spawns_additional_workers() {
        let mock_data = build_mock_data(8 * 1024);
        let download_chunks = eight_chunks(mock_data.len() as u64);
        let puller = SlowPuller {
            data: Arc::from(mock_data.as_slice()),
            delay: Duration::from_millis(150),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 1,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );

        // Resize while the lone worker is still stalled on its first range.
        let grower = result.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            grower.set_threads(8, 1);
        });

        let mut pulling_ids = BTreeSet::new();
        while let Ok(e) = result.event_chain().recv().await {
            if let Event::Pulling(id) = e {
                pulling_ids.insert(id);
            }
        }
        assert!(
            pulling_ids.len() > 1,
            "growth spawned no additional worker (pulling ids: {pulling_ids:?})"
        );
        assert_eq!(&**receive.lock(), mock_data);
    }

    // The mirror image of the test above: a worker spawned by a growth that
    // races an `abort` must observe the abort latch and exit without pulling, so
    // the session still finalizes promptly instead of resuming.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_threads_growth_after_abort_does_not_resume() {
        let mock_data = build_mock_data(8 * 1024);
        let download_chunks = eight_chunks(mock_data.len() as u64);
        let puller = SlowPuller {
            data: Arc::from(mock_data.as_slice()),
            delay: Duration::from_millis(150),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 1,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );

        result.abort();
        result.set_threads(8, 1);
        // Await termination by draining `event_chain` (replaces `join()`); the
        // timeout guards against a hang.
        timeout(Duration::from_secs(10), async {
            while result.event_chain().recv().await.is_ok() {}
        })
        .await
        .expect("join() hung after abort followed by growth");
        assert!(
            receive.lock().len() < mock_data.len(),
            "an aborted session must not be resumed by a later resize"
        );
    }

    // Repeatedly resizing a *running* pool in both directions must never corrupt
    // the download. Each shrink aborts live workers and reclaims their ranges
    // mid-flight; each growth hands those ranges to fresh workers. A cursor
    // hand-off that is off by even one piece shows up here as missing or
    // duplicated bytes, which a single grow-once test cannot catch. The pieces
    // are small and paced so the churn lands inside a range rather than on a
    // boundary, and the schedule is seeded so any failure reproduces exactly.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_threads_random_churn_preserves_all_bytes() {
        let mock_data = build_mock_data(64 * 1024);
        let download_chunks = eight_chunks(mock_data.len() as u64);
        let puller = ChunkedPuller {
            data: Arc::from(mock_data.as_slice()),
            piece: 512,
            delay: Duration::from_millis(2),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 4,
                retry_gap: Duration::from_millis(10),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );

        // Never resize to zero: that collapses the pool and finalizes the
        // session, which is a separate contract from mid-flight churn.
        let churner = result.clone();
        let probe = receive.clone();
        let total = mock_data.len();
        let churn = tokio::spawn(async move {
            let mut state = 0x2545_F491_4F6C_DD1D_u64;
            let mut seen = BTreeSet::new();
            // Resizes landing after the last byte is written prove nothing, so
            // count the ones that actually hit a still-running download.
            let mut inflight = 0usize;
            for _ in 0..40 {
                let threads = (next_rand(&mut state) % 8 + 1) as usize;
                seen.insert(threads);
                if probe.lock().len() < total {
                    inflight += 1;
                }
                churner.set_threads(threads, 1);
                sleep(Duration::from_millis(3)).await;
            }
            // Leave a healthy pool behind so the remaining ranges drain.
            churner.set_threads(8, 1);
            (seen, inflight)
        });

        let chunk_count = download_chunks.len();
        let mut pulling_total = 0usize;
        while let Ok(e) = result.event_chain().recv().await {
            if matches!(e, Event::Pulling(_)) {
                pulling_total += 1;
            }
        }
        let (seen, inflight) = churn.await.unwrap();
        assert!(
            seen.len() > 2,
            "churn never varied the pool size, so nothing was exercised: {seen:?}"
        );
        // Without this the test could silently degrade into a no-op: a download
        // that outran the churn loop would take every resize on a dead session.
        assert!(
            inflight > 0,
            "every resize landed after the download finished, so no running pool was churned"
        );
        // Reclaimed ranges are handed out again, so a pool that really churned
        // pulls far more often than once per chunk.
        assert!(
            pulling_total > chunk_count,
            "ranges were never redistributed ({pulling_total} pulls for {chunk_count} chunks)"
        );
        assert_eq!(
            &**receive.lock(),
            mock_data,
            "repeated resizing corrupted the downloaded bytes"
        );
    }
}
