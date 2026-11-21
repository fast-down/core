//! Top-level download orchestration: session handle plus single- and
//! multi-threaded entry points.
//!
//! [`download_single`](crate::single::download_single) runs a sequential pull,
//! while [`download_multi`](crate::multi::download_multi) splits the work across
//! concurrent workers with work-stealing. Both return a [`DownloadResult`], a
//! cheaply cloneable handle that keeps the session alive until the last clone is
//! dropped (or [`DownloadResult::abort`] is called).

use crate::Event;
use crate::Pusher;
use crossfire::{MAsyncRx, mpmc};
use fast_steal::{Executor, TaskQueue};
use force_send::ForceSend;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

mod force_send;
pub mod mock;
pub mod multi;
pub mod single;

/// How often a blocked write driver wakes up to observe cancellation.
// `core` is glob-re-exported by `lib.rs`, so `pub` here would publish this
// helper; keep it crate-local and silence the private-module lint.
const PUSH_POLL: Duration = Duration::from_millis(500);

/// Shared "last progress" clock, fed whenever a chunk is accepted or a write
/// completes.
///
/// The hot path is one relaxed `fetch_max`: a monotonic millisecond stamp is
/// enough for a watchdog that only needs second-scale resolution, and the
/// maximum keeps an out-of-order beat from moving the clock backwards.
#[derive(Debug)]
struct ProgressClock {
    start: Instant,
    last_ms: AtomicU64,
}

impl ProgressClock {
    /// Create a clock whose idle time starts now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            last_ms: AtomicU64::new(0),
        }
    }

    /// Record that progress was made.
    ///
    /// Concurrent callers may compute their stamps out of order, so the value
    /// only moves forward: a plain store would let a thread that read the clock
    /// earlier overwrite a newer stamp and make the session look idle long
    /// enough to be judged stalled.
    pub fn beat(&self) {
        self.last_ms
            .fetch_max(millis(self.start.elapsed()), Ordering::Relaxed);
    }

    /// How long has passed since the last [`beat`](Self::beat).
    pub fn idle(&self) -> Duration {
        let now = millis(self.start.elapsed());
        Duration::from_millis(now.saturating_sub(self.last_ms.load(Ordering::Relaxed)))
    }
}

fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

impl Default for ProgressClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the session's stall watchdog.
///
/// Once `clock` has not been beaten for `timeout`, it reports
/// [`Event::Stalled`] and cancels the session's root token, so every worker and
/// the write driver wind down on their next poll. `stop` ends the watch without
/// firing (the pull side is done and only the flush remains), and cancelling
/// `abort` ends it too.
///
/// The clock is beaten by an accepted chunk and by a write that completes (so a
/// sink need not forward `set_listener` to stay alive), so the guard watches
/// actual progress rather than error frequency: a burst of retries inside the
/// window does not end a download that is still moving. The window starts with
/// the session, so it has to exceed the time to first byte. A zero timeout
/// disables it.
fn spawn_stall_watch<PullError, PushError>(
    tx: &crossfire::MTx<mpmc::List<Event<PullError, PushError>>>,
    abort: CancellationToken,
    stop: CancellationToken,
    clock: Arc<ProgressClock>,
    timeout: Duration,
) where
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    if timeout.is_zero() {
        return;
    }
    // Hold only a weak sender: a strong one would keep the event channel open
    // and make every consumer's final drain wait forever on a finished session.
    let tx = tx.downgrade();
    // Check several times per window so the session ends close to `timeout`
    // without spinning on very short test timeouts.
    let tick = (timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(1));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = abort.cancelled() => return,
                // The pull side is done; only the final flush is left, which the
                // guard must not cut short.
                () = stop.cancelled() => return,
                () = tokio::time::sleep(tick) => {}
            }
            if clock.idle() >= timeout {
                let upgraded: Option<crossfire::MTx<_>> = tx.upgrade();
                // No strong sender left means the session already ended: there
                // is no one to report to, and nothing left to cancel.
                let Some(tx) = upgraded else { return };
                let _ = tx.send(Event::Stalled(timeout));
                abort.cancel();
                return;
            }
        }
    });
}

/// Lower bound on the interval between flush retries.
///
/// A zero `retry_gap` means "retry as soon as possible" for pulls and pushes; in
/// the flush loop it would spin at full speed for the whole stall window,
/// emitting one `FlushError` per iteration. A millisecond keeps the retry
/// responsive while bounding that.
const FLUSH_RETRY_FLOOR: Duration = Duration::from_millis(1);

/// Flush the sink, retrying until it succeeds, the session is cancelled, or
/// `stall_timeout` has passed since the first failure.
///
/// A flush error is normally permanent (full disk, EIO), so a session that keeps
/// failing ends instead of looping forever. `watch_stop` ends the stall watchdog
/// first: every pulled chunk has been handled and only the flush is left, which
/// the watchdog must not cut short. Retries are spaced by `retry_gap` but never
/// closer than [`FLUSH_RETRY_FLOOR`].
fn flush_sink<W, PullError, PushError>(
    pusher: &mut W,
    tx: &crossfire::MTx<mpmc::List<Event<PullError, PushError>>>,
    token: &CancellationToken,
    watch_stop: &CancellationToken,
    retry_gap: Duration,
    stall_timeout: Duration,
) where
    W: Pusher<Error = PushError>,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    watch_stop.cancel();
    let retry_gap = retry_gap.max(FLUSH_RETRY_FLOOR);
    let mut first_failure: Option<Instant> = None;
    loop {
        if token.is_cancelled() {
            return;
        }
        let _ = tx.send(Event::Flushing);
        match pusher.flush() {
            Ok(()) => return,
            Err(err) => {
                let _ = tx.send(Event::FlushError(err));
                let since = *first_failure.get_or_insert_with(Instant::now);
                if !stall_timeout.is_zero() && since.elapsed() >= stall_timeout {
                    token.cancel();
                    return;
                }
            }
        }
        std::thread::park_timeout(retry_gap);
    }
}

/// Cancels the session when the task holding it is dropped by a panic.
///
/// The task owns its queue sender — the pull task of a single-threaded session,
/// or one worker of a multi-threaded one — so a panic would otherwise look like
/// a finished stream to the push driver: it flushes, returns, and the session
/// can end as a clean download with a truncated file. Running during the unwind
/// marks the session aborted before the sender goes away, so the failure is
/// observable as [`is_aborted`](DownloadResult::is_aborted) instead of a
/// successful result.
///
/// `std::thread::panicking()` tells a panic apart from a normal exit, including
/// the one caused by `abort`.
///
/// It does not hand a worker's unclaimed range back: `TaskQueue::set_threads`
/// sweeps dead workers and takes their remaining interval into `waiting`, which
/// covers both a panic and an exit that dropped work.
struct PanicGuard(CancellationToken);

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.cancel();
        }
    }
}

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
    /// Live lower bound on how finely a worker may split a range when it steals,
    /// shared with the session's executor. `None` for a single-threaded session,
    /// which has no work-stealing and so no granularity to change.
    min_chunk_size: Option<Arc<AtomicU64>>,
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

    pub fn set_threads(&self, threads: usize) -> Option<()> {
        let (executor, task_queue) = self.task_queue.as_ref()?;
        let min = self
            .min_chunk_size
            .as_ref()
            .map_or(1, |live| live.load(Ordering::Relaxed));
        task_queue.set_threads(threads, min, Some(executor))
    }

    /// Set the live minimum chunk size used when a worker next steals work.
    ///
    /// A no-op for single-threaded sessions, which have no worker pool.
    pub fn set_min_chunk_size(&self, min_chunk_size: u64) {
        if let Some(live) = &self.min_chunk_size {
            live.store(min_chunk_size.max(1), Ordering::Relaxed);
        }
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
/// methods (`abort`, `set_threads`, `set_min_chunk_size`, `is_aborted`) and
/// [`event_chain`](Self::event_chain) directly; each delegates to the inner
/// value. There is intentionally **no** `Deref` impl — `DownloadResultInner`
/// is private, so callers reach session state only through these methods.
///
/// The inner value is held in a `ForceSend`, a deliberately isolated
/// workaround so the handle is `Send` even when it is built inside an `async`
/// block (see that type's module docs).
///
/// Completion is observed by draining the event stream: once the last sender is
/// dropped (the download finished or was aborted) the receiver disconnects, so
/// `while result.recv().await.is_ok() {}` awaits the session end. [`recv`](Self::recv)
/// is the `Send`-safe form of that call; prefer it over
/// `result.event_chain().recv()` in a `tokio::spawn`ed driver.
#[must_use = "dropping the last handle cancels the download"]
pub struct DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    inner: ForceSend<Arc<DownloadResultInner<E, PullError, PushError>>>,
}

impl<E, PullError, PushError> fmt::Debug for DownloadResult<E, PullError, PushError>
where
    E: Executor + Send + Sync,
    PullError: Send + Unpin + 'static,
    PushError: Send + Unpin + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadResult")
            .field("inner", &*self.inner)
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
    ///
    /// `min_chunk_size` is the session's live steal-granularity cell, or `None`
    /// for a single-threaded session (see [`set_min_chunk_size`](Self::set_min_chunk_size)).
    ///
    /// Not part of the stable surface: kept reachable only so the allocation
    /// tests can build a session without going through an entry point that
    /// spawns tasks.
    #[doc(hidden)]
    pub fn new(
        event_chain: MAsyncRx<mpmc::List<Event<PullError, PushError>>>,
        task_queue: Option<(E, TaskQueue<E::Handle>)>,
        abort_token: CancellationToken,
        min_chunk_size: Option<Arc<AtomicU64>>,
    ) -> Self {
        Self {
            inner: ForceSend(Arc::new(DownloadResultInner {
                event_chain,
                task_queue,
                min_chunk_size,
                abort_token,
            })),
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

    /// Await the next event, or `Err` once the session has ended.
    ///
    /// Equivalent to `self.event_chain().recv()`, but returns
    /// `impl Future + Send`, so an enclosing `async` block that awaits it stays
    /// `Send`. Prefer this over `event_chain().recv()` in a `tokio::spawn`ed
    /// driver: the underlying `RecvFuture` is only `Send` through a hand-written
    /// impl that the `async fn` auto-trait leak check does not see through, so
    /// awaiting it inline reintroduces a spurious `higher-ranked lifetime error`.
    /// No allocation.
    pub fn recv(
        &self,
    ) -> impl std::future::Future<Output = Result<Event<PullError, PushError>, crossfire::RecvError>>
    + Send
    + '_ {
        self.inner.event_chain.recv()
    }

    /// Cancel all workers immediately.
    ///
    /// Safe to call multiple times and safe to call while other clones of the
    /// owning [`DownloadResult`] are still alive. The implicit drop-based
    /// cancellation (on the last clone) becomes a no-op once this has run.
    pub fn abort(&self) {
        self.inner.abort();
    }

    /// Adjust the worker thread count of a running multi-threaded session.
    ///
    /// The minimum chunk size used while growing is the session's live value,
    /// set independently with [`set_min_chunk_size`](Self::set_min_chunk_size)
    /// (or the configured value until then).
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
    pub fn set_threads(&self, threads: usize) {
        self.inner.set_threads(threads);
    }

    /// Set the minimum chunk size used when a worker next steals work.
    ///
    /// Bounds how finely a running worker may split a range to take a share of a
    /// peer's work: a range is only halved when at least `min_chunk_size * 2`
    /// bytes remain. The new value takes effect on the next steal — there is no
    /// need to also call [`set_threads`](Self::set_threads) — and it is also the
    /// granularity used to split ranges the next time the pool grows.
    ///
    /// A no-op for single-threaded sessions, which have no worker pool.
    pub fn set_min_chunk_size(&self, min_chunk_size: u64) {
        self.inner.set_min_chunk_size(min_chunk_size);
    }

    /// Whether the session has been (or is being) cancelled.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.inner.is_aborted()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::cast_possible_truncation)]
    use crate::MemPusher;
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
            type PullItem = PullResult<Bytes, std::convert::Infallible>;
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
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        // `Debug` of `DownloadResultInner` is reached through `DownloadResult`'s
        // own `Debug` impl, which forwards to the inner value.
        let _ = format!("{result:?}");
        // Exercises the forwarding and the inner task-queue adjustment.
        result.set_threads(4);
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
        // Single-threaded sessions have no task queue, so `set_threads` is a no-op.
        let result = download_single(
            puller,
            pusher,
            crate::single::DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                stall_timeout: Duration::from_secs(60),
            },
        );
        // `DownloadResult` is `Clone`.
        let _clone = result.clone();
        result.set_threads(4);
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
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        // Live session: flag starts false and must stay false after a resize.
        assert!(!result.is_aborted());
        result.set_threads(4);
        assert!(!result.is_aborted());
        // Now abort: flag is true and must *stay* true across a later resize.
        result.abort();
        assert!(result.is_aborted());
        result.set_threads(4);
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
            delay: Duration::from_millis(80),
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
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );

        // Resize while the lone worker is still stalled on its first range.
        let grower = result.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(30)).await;
            grower.set_threads(8);
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
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );

        result.abort();
        result.set_threads(8);
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
                stall_timeout: Duration::from_secs(60),
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
                churner.set_threads(threads);
                sleep(Duration::from_millis(3)).await;
            }
            // Leave a healthy pool behind so the remaining ranges drain.
            churner.set_threads(8);
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

    /// A high-frequency churn of *both* live setters at once (pool size and
    /// granularity) must leave the download intact. Unlike the threads-only
    /// churn, this stresses the granularity cell being written concurrently with
    /// every worker's steal. The schedule is seeded and the granularity is
    /// varied across the `split_two` boundary (`>= 2 * piece`) and beyond, and
    /// reset small at the end so the remaining ranges can drain.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_threads_and_min_chunk_size_churn_preserves_all_bytes() {
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
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );

        let churner = result.clone();
        let probe = receive.clone();
        let total = mock_data.len();
        let churn = tokio::spawn(async move {
            let mut state = 0x9E37_79B9_7F4A_7C15_u64;
            let mut threads_seen = BTreeSet::new();
            let mut mins_seen = BTreeSet::new();
            let mut inflight = 0usize;
            for _ in 0..40 {
                let threads = (next_rand(&mut state) % 8 + 1) as usize;
                // Straddle the split boundary (`2 * piece == 1024`) and go well
                // past it, so both "can split" and "cannot split" are exercised.
                let min = match next_rand(&mut state) % 4 {
                    0 => 1,
                    1 => 512,
                    2 => 1024,
                    _ => 4096,
                };
                threads_seen.insert(threads);
                mins_seen.insert(min);
                if probe.lock().len() < total {
                    inflight += 1;
                }
                churner.set_threads(threads);
                churner.set_min_chunk_size(min);
                sleep(Duration::from_millis(3)).await;
            }
            // Leave a healthy pool and a permissive granularity behind so the
            // remaining ranges can still be split and drained.
            churner.set_threads(8);
            churner.set_min_chunk_size(1);
            (threads_seen, mins_seen, inflight)
        });

        let mut pulling_total = 0usize;
        while let Ok(e) = result.event_chain().recv().await {
            if matches!(e, Event::Pulling(_)) {
                pulling_total += 1;
            }
        }
        let (threads_seen, mins_seen, inflight) = churn.await.unwrap();
        assert!(
            threads_seen.len() > 2,
            "churn never varied the pool size: {threads_seen:?}"
        );
        assert!(
            mins_seen.len() > 2,
            "churn never varied the granularity: {mins_seen:?}"
        );
        assert!(
            inflight > 0,
            "every churn step landed after the download finished"
        );
        assert!(
            pulling_total >= download_chunks.len(),
            "ranges were never redistributed ({pulling_total} pulls)"
        );
        assert_eq!(
            &**receive.lock(),
            mock_data,
            "churning threads and granularity together corrupted the bytes"
        );
    }

    #[test]
    fn progress_clock_beat_never_moves_the_stamp_backwards() {
        use super::ProgressClock;
        use std::sync::atomic::Ordering;

        // A beat from a thread that sampled the clock earlier must not overwrite
        // a newer stamp: with a plain `store`, `idle()` would jump forward and
        // the stall watchdog could cancel a session that is still moving.
        let clock = ProgressClock::new();
        clock.last_ms.store(1_000_000, Ordering::Relaxed);
        clock.beat();
        assert_eq!(
            clock.last_ms.load(Ordering::Relaxed),
            1_000_000,
            "a beat must not move the stamp backwards"
        );
        assert_eq!(clock.idle(), Duration::ZERO);
    }

    #[test]
    fn progress_clock_concurrent_beats_are_monotonic() {
        use super::ProgressClock;
        use std::sync::atomic::{AtomicBool, Ordering};

        let clock = Arc::new(ProgressClock::new());
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let clock = clock.clone();
            workers.push(std::thread::spawn(move || {
                for _ in 0..2_000 {
                    clock.beat();
                }
            }));
        }
        // The shared stamp must never be observed going backwards while beats
        // race each other.
        let reader = clock;
        let reader_stop = stop.clone();
        let sampler = std::thread::spawn(move || {
            let mut last = 0;
            while !reader_stop.load(Ordering::Relaxed) {
                let now = reader.last_ms.load(Ordering::Relaxed);
                assert!(now >= last, "beat moved the stamp from {last} to {now}");
                last = now;
            }
        });
        for worker in workers {
            worker.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        sampler.join().unwrap();
    }
}
