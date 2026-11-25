//! Multi-threaded concurrent download with work-stealing.

use crate::core::{PUSH_POLL, PanicGuard, ProgressClock, flush_sink, spawn_stall_watch};
use crate::{DownloadResult, Event, ProgressEntry, Puller, PullerError, Pusher, WorkerId};
use bytes::Bytes;
use core::{
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};
use crossfire::{MAsyncTx, MTx, RecvTimeoutError, WeakTx, mpmc, mpsc};
use fast_steal::{ArmedTask, Executor, Handle, StealOutcome, TaskQueue};
use futures::TryStreamExt;
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

/// Options for a multi-threaded concurrent download.
///
/// Controls chunk splitting, speculation, pull timeouts, and write queue capacity.
#[derive(Debug, Clone)]
pub struct DownloadOptions<I: Iterator<Item = ProgressEntry>> {
    /// Initial byte ranges to download, each an independent unit of work.
    pub download_chunks: I,
    /// Number of concurrent workers.
    pub concurrent: usize,
    /// Delay before retrying a failed pull or push.
    ///
    /// [`Duration::ZERO`] disables the backoff: a failed pull or push is retried
    /// immediately, as fast as the loop can spin. Combined with a zero
    /// [`stall_timeout`](Self::stall_timeout), which turns the watchdog off, a
    /// sink or puller that fails permanently leaves the session with no
    /// terminating condition, and the unbounded event channel grows at memory
    /// speed for as long as the caller does not drain it. The final flush retries
    /// are floored at a millisecond, so a zero gap cannot spin there.
    pub retry_gap: Duration,
    /// Maximum time to wait for bytes before aborting and re-pulling a range.
    ///
    /// Covers both the request itself and each wait for the next chunk.
    /// [`Duration::ZERO`] disables it: a request then waits indefinitely, so a
    /// session that never delivers anything is ended only by
    /// [`stall_timeout`](Self::stall_timeout) or cancellation.
    pub pull_timeout: Duration,
    /// Capacity of the bounded pull→push queue; backpressure when full.
    ///
    /// `0` is treated as `1`. The value is clamped to `1..=u32::MAX`, the
    /// largest bound the underlying channel accepts, and the queue
    /// **immediately preallocates that many slots**: a huge value is a huge
    /// upfront allocation, not a hint.
    pub push_queue_cap: usize,
    /// Give up once the session has made no progress for this long.
    ///
    /// Progress is a pulled chunk or a completed write; errors and new attempts
    /// are not progress by themselves. The window starts when the session starts,
    /// so `stall_timeout` has to exceed the time to first byte (DNS, connect,
    /// TLS), or the first request of every range is killed before it can deliver
    /// anything. The same window bounds retrying a final flush from its first
    /// failure. [`Duration::ZERO`] disables the guard.
    pub stall_timeout: Duration,
    /// Minimum size (bytes) of a range handed to a worker when stealing.
    pub min_chunk_size: u64,
    /// Maximum number of workers allowed to speculatively share one range.
    pub max_speculative: usize,
}

/// Start a multi-threaded concurrent download with work-stealing.
///
/// # Completion
///
/// The session ends when every worker has exited: a worker is done with its
/// range once the cursor reaches the range end, it steals fresh work when the
/// queue still has some, and otherwise leaves. Each worker drops its `tx_push`
/// sender on the way out, so the push driver sees the channel disconnect, breaks
/// out of its receive loop and flushes; the `event_chain` sender lives inside
/// that driver, so draining the receiver is how completion is awaited.
///
/// An ended pull stream (`Ok(None)`) does **not** finish the download — it only
/// ends that pull attempt. The worker continues its task loop with the cursor
/// where it stopped (re-reading it, since a speculative twin may have advanced
/// it) and requests the same range again. If the server keeps answering with an
/// empty body, that loop has no attempt limit: no chunk is accepted, so the
/// progress clock never beats and only
/// [`stall_timeout`](DownloadOptions::stall_timeout) can end the session. With
/// [`Duration::ZERO`] there the watchdog is not even spawned, so the worker
/// re-requests the range forever. This is the opposite of
/// [`download_single`](crate::single::download_single), where `Ok(None)` ends the
/// whole download.
///
/// A panic in a worker or in the blocking push driver cancels the session
/// instead of letting a truncated download look like a clean one: a dead worker
/// would otherwise just stop advancing its cursor, and the push driver would
/// flush and finish. The panicking worker's unclaimed interval is put back into
/// the queue first as a best effort, so a survivor can pick it up if one is
/// still running. Normal cancellation via `abort()` makes every worker leave its
/// range and the push driver return without flushing, so buffered bytes are
/// discarded and the file is left incomplete. Either ending is observed by
/// draining `event_chain`, because the senders are dropped when the last side
/// exits.
///
/// # Panics
///
/// Must be called from within a tokio runtime: the worker pool, the supervisor
/// and (unless [`stall_timeout`](DownloadOptions::stall_timeout) is
/// [`Duration::ZERO`]) the stall watchdog are `tokio::spawn`ed, and the push
/// driver runs on `spawn_blocking`. All of those panic outside a runtime, as do
/// the workers a later [`DownloadResult::set_threads`](crate::DownloadResult::set_threads)
/// spawns.
#[allow(clippy::too_many_lines)]
pub fn download_multi<R: Puller, W: Pusher, I: Iterator<Item = ProgressEntry>>(
    puller: R,
    mut pusher: W,
    options: DownloadOptions<I>,
) -> DownloadResult<TokioExecutor<R, W::Error>, R::Error, W::Error> {
    let token = CancellationToken::new();
    let (tx, event_chain) = mpmc::unbounded_async();
    let clock = Arc::new(ProgressClock::new());
    let watch_stop = CancellationToken::new();
    pusher.set_listener({
        let tx = tx.clone();
        let clock = clock.clone();
        Box::new(move |p| {
            let _ = tx.send(Event::PushProgress(p));
            // A slow-but-moving sink must not trip the stall guard.
            clock.beat();
        })
    });
    // The channel asserts `1 <= capacity <= u32::MAX` (and normalises 0 to 1),
    // so clamp to that bound here instead of panicking inside `crossfire`. The
    // queue preallocates the capacity immediately, so this is a real allocation
    // bound rather than a throughput hint.
    let push_queue_cap = options
        .push_queue_cap
        .clamp(1, usize::try_from(u32::MAX).unwrap_or(usize::MAX));
    let (tx_push, rx_push) =
        mpsc::bounded_async_blocking::<(WorkerId, ProgressEntry, Bytes)>(push_queue_cap);

    let push_thread = Arc::new(OnceLock::new());
    let push_handle = tokio::task::spawn_blocking({
        let push_thread = push_thread.clone();
        let token = token.clone();
        let watch_stop = watch_stop.clone();
        let tx = tx.clone();
        let clock = clock.clone();
        move || {
            let _ = push_thread.set(std::thread::current());
            loop {
                // Wake periodically instead of blocking in `recv()` forever:
                // cancellation must not depend on the pull side dropping its
                // sender before `recv` can observe the token.
                if token.is_cancelled() {
                    return;
                }
                let (id, mut spin, mut data) = match rx_push.recv_timeout(PUSH_POLL) {
                    Ok(item) => item,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                loop {
                    if token.is_cancelled() {
                        return;
                    }
                    let _ = tx.send(Event::Pushing(id, spin.clone()));
                    let len_before_push = data.len();
                    match pusher.push(&spin, data) {
                        Ok(()) => {
                            // A completed write is direct evidence that the
                            // session is moving, and unlike the listener it is
                            // available from every sink: `set_listener` may be
                            // the default no-op.
                            clock.beat();
                            break;
                        }
                        Err((err, bytes)) => {
                            let _ = tx.send(Event::PushError(id, spin.clone(), err));
                            let written = len_before_push.saturating_sub(bytes.len());
                            data = bytes;
                            spin.start += written as u64;
                        }
                    }
                    std::thread::park_timeout(options.retry_gap);
                }
            }
            // Every pulled chunk has been handled; only the flush is left.
            flush_sink(
                &mut pusher,
                &tx,
                &token,
                &watch_stop,
                options.retry_gap,
                options.stall_timeout,
            );
        }
    });
    tokio::spawn({
        let token = token.clone();
        async move {
            tokio::select! {
                res = push_handle => {
                    // The push driver is the only writer. If it panicked, nothing
                    // can consume another chunk, so the run is aborted instead of
                    // transferring the rest of the file into a closed channel and
                    // dropping every byte.
                    //
                    // Only the failure branch cancels: cancelling is observable
                    // (`is_aborted()` *is* `token.is_cancelled()`), so doing it
                    // after a clean flush would mark every success as aborted.
                    // The clean path needs no cleanup either: the driver only
                    // returns after `rx_push` disconnects, which requires every
                    // worker to have dropped its sender, and they hold the only
                    // strong ones. `single` reaches the same state by cancelling
                    // when its push driver dies.
                    if res.is_err() {
                        token.cancel();
                    }
                }
                () = token.cancelled() => {}
            }
            if let Some(t) = push_thread.get() {
                t.unpark();
            }
        }
    });

    let min_chunk_size = Arc::new(AtomicU64::new(options.min_chunk_size.max(1)));
    let executor: TokioExecutor<R, W::Error> = TokioExecutor {
        token: token.clone(),
        tx: tx.downgrade(),
        tx_push: tx_push.downgrade(),
        puller,
        id: AtomicUsize::new(0),
        retry_gap: options.retry_gap,
        pull_timeout: options.pull_timeout,
        clock: clock.clone(),
        min_chunk_size: min_chunk_size.clone(),
        max_speculative: options.max_speculative,
    };
    let task_queue = TaskQueue::new(options.download_chunks);
    let _ = task_queue.set_threads(options.concurrent, options.min_chunk_size, Some(&executor));
    spawn_stall_watch(&tx, token.clone(), watch_stop, clock, options.stall_timeout);

    DownloadResult::new(
        event_chain,
        Some((executor, task_queue)),
        token,
        Some(min_chunk_size),
    )
}

/// A [`Handle`] implementation whose cancellation is a worker-local
/// [`CancellationToken`], itself a child of the session's root token.
#[derive(Debug, Clone)]
pub struct TokioHandle {
    id: usize,
    token: CancellationToken,
}
impl Handle for TokioHandle {
    type Id = usize;
    fn abort(&mut self) {
        self.token.cancel();
    }
    fn is_self(&self, id: &Self::Id) -> bool {
        self.id == *id
    }
}

/// A built-in [`Executor`] implementation based on tokio tasks.
///
/// Each worker is a `tokio::spawn`-ed task that pulls chunks from the puller,
/// sends them to the write queue, and steals new work via [`TaskQueue`].
///
/// The executor outlives the workers — [`DownloadResult::set_threads`] uses it
/// to grow the pool mid-session — so it must not own anything that keeps a
/// finished session alive. It therefore reaches the session's channels through
/// a [`WeakTx`](crossfire::WeakTx): once every worker is gone the upgrade fails
/// and no further worker can be spawned.
pub struct TokioExecutor<R, WE>
where
    R: Puller,
    WE: Send + Unpin + 'static,
{
    tx: WeakTx<mpmc::List<Event<R::Error, WE>>>,
    tx_push: WeakTx<mpsc::Array<(WorkerId, ProgressEntry, Bytes)>>,
    /// Session-wide cancellation token, shared with the push driver and with
    /// [`DownloadResult::abort`]. Cancelling it broadcasts to every linked
    /// worker token (including ones spawned after the cancel call), so a worker
    /// only has to observe the cancel once to know the session is over.
    token: CancellationToken,
    puller: R,
    retry_gap: Duration,
    pull_timeout: Duration,
    /// Fed by every worker when a chunk is accepted; read by the session's
    /// stall watchdog (see [`spawn_stall_watch`]).
    clock: Arc<ProgressClock>,
    id: AtomicUsize,
    /// Live lower bound on how finely a worker may split a range when it steals.
    /// Shared with the owning [`DownloadResult`], so
    /// [`set_min_chunk_size`](crate::DownloadResult::set_min_chunk_size) takes
    /// effect on the very next steal instead of only on the next pool growth.
    min_chunk_size: Arc<AtomicU64>,
    max_speculative: usize,
}
impl<R, WE> Executor for TokioExecutor<R, WE>
where
    R: Puller,
    WE: Send + Unpin + 'static,
{
    type Handle = TokioHandle;
    #[allow(clippy::too_many_lines)]
    fn execute(&self, mut task: ArmedTask, task_queue: TaskQueue<Self::Handle>) -> Self::Handle {
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        let token = self.token.child_token();

        let tx: Option<MTx<_>> = self.tx.upgrade();
        let tx_push: Option<MAsyncTx<_>> = self.tx_push.upgrade();
        let (Some(tx), Some(tx_push)) = (tx, tx_push) else {
            return TokioHandle { id, token };
        };

        let mut puller = self.puller.clone();
        let min_chunk_size = self.min_chunk_size.clone();
        let pull_timeout = self.pull_timeout;
        let cfg_retry_gap = self.retry_gap;
        let max_speculative = self.max_speculative;
        let clock = self.clock.clone();
        let worker_token = token.clone();
        // A panicking worker would otherwise look like one that finished: its
        // cursor stops moving and it drops its queue sender, after which the push
        // driver flushes and can end the session as if the missing range had been
        // downloaded. The guard is dropped during the unwind, before the senders
        // go away, so the cancel lands while the failure is still observable.
        let panic_token = self.token.clone();
        tokio::spawn(async move {
            let _panic_guard = PanicGuard(panic_token);
            'task: loop {
                if worker_token.is_cancelled() {
                    break 'task;
                }
                // One load for both bounds: `start` and `end` move independently
                // under a steal, so reading them separately can mix two snapshots.
                let current = task.get();
                let mut start = current.start;
                if start >= current.end {
                    let current_min = min_chunk_size.load(Ordering::Relaxed);
                    if task_queue.steal(&id, &mut task, current_min, max_speculative)
                        == StealOutcome::Refilled
                    {
                        continue 'task;
                    }
                    break 'task;
                }
                let _ = tx.send(Event::Pulling(id));
                let download_range = start..task.get().end;
                let mut stream = 'attempt: {
                    // The `select!` result may borrow `puller` through the stream it
                    // returns, so it lives in its own scope: that borrow ends here and
                    // the retry below is free to rebind `puller`.
                    let gap = {
                        let t = tokio::select! {
                            () = worker_token.cancelled() => break 'task,
                            // A zero timeout disables the guard instead of arming
                            // an always-ready `sleep(ZERO)` that would win every
                            // poll and flood the session with `PullTimeout`.
                            () = tokio::time::sleep(pull_timeout), if !pull_timeout.is_zero() => None,
                            t = puller.pull(Some(&download_range)) => Some(t)
                        };
                        match t {
                            Some(Ok(stream)) => break 'attempt stream,
                            Some(Err((e, retry_gap))) => {
                                let _ = tx.send(Event::PullError(id, e));
                                retry_gap.unwrap_or(cfg_retry_gap)
                            }
                            None => {
                                let _ = tx.send(Event::PullTimeout(id));
                                // The stall already consumed `pull_timeout`; waiting
                                // again would only double the gap.
                                Duration::ZERO
                            }
                        }
                    };
                    // Wait out the backoff first: cancelling during it must not
                    // pay for building a client that will never be used. Rebuilding
                    // after the wait also hands the next attempt a client that is
                    // fresh at the moment it is needed, rather than one that has
                    // been idle for the whole gap.
                    tokio::select! {
                        () = worker_token.cancelled() => break 'task,
                        () = tokio::time::sleep(gap) => {}
                    }
                    // A failed pull is retried the same way whether it errored or
                    // stalled: rebuild the puller, which yields a fresh client and
                    // the next local source IP, so a rejected or wedged path is left
                    // behind instead of being retried on the same one.
                    puller = puller.clone();
                    // Retry from the top of the task loop instead of re-pulling this
                    // range: a speculative twin shares the cursor and may have
                    // advanced or drained it while this attempt was being rebuilt. The
                    // top re-reads the cursor, steals fresh work or finishes when
                    // nothing is left, and announces the request it is about to issue.
                    continue 'task;
                };
                loop {
                    let t = tokio::select! {
                        () = worker_token.cancelled() => break 'task,
                        // Same guard as the request above: a zero timeout means
                        // "wait for the chunk indefinitely".
                        () = tokio::time::sleep(pull_timeout), if !pull_timeout.is_zero() => {
                            let _ = tx.send(Event::PullTimeout(id));
                            drop(stream);
                            puller = puller.clone();
                            continue 'task;
                        },
                        t = stream.try_next() => t,
                    };
                    match t {
                        Ok(Some(mut chunk)) => {
                            if chunk.is_empty() {
                                continue;
                            }
                            let len = chunk.len() as u64;
                            let Ok(span) = task.safe_add_start(start, len) else {
                                start += len;
                                continue;
                            };
                            if span.end >= task.get().end {
                                task_queue.cancel_task(&task, &id);
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            let slice_span =
                                (span.start - start) as usize..(span.end - start) as usize;
                            chunk = chunk.slice(slice_span);
                            start = span.end;
                            let _ = tx.send(Event::PullProgress(id, span.clone()));
                            clock.beat();
                            let _ = tx_push.send((id, span, chunk)).await;
                            if start >= task.get().end {
                                continue 'task;
                            }
                        }
                        Ok(None) => continue 'task,
                        Err((e, retry_gap)) => {
                            let is_irrecoverable = e.is_irrecoverable();
                            let _ = tx.send(Event::PullError(id, e));
                            tokio::select! {
                                () = worker_token.cancelled() => break 'task,
                                () = tokio::time::sleep(retry_gap.unwrap_or(cfg_retry_gap)) => {}
                            };
                            if is_irrecoverable {
                                // Nothing left in this stream: the attempt is
                                // abandoned. Rebuild the puller so the retry leaves
                                // the current client and source IP behind. As with a
                                // recoverable error this only re-requests the range;
                                // it does not end the session.
                                drop(stream);
                                puller = puller.clone();
                                continue 'task;
                            }
                        }
                    }
                }
            }
            let _ = tx.send(Event::Finished(id));
        });
        TokioHandle { id, token }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation)]
    use vec::Vec;

    use super::*;
    use crate::{BufWriterPusher, CacheSeqPusher, PullResult};
    use crate::{
        MemPusher, Merge, ProgressEntry,
        mock::{MockPuller, build_mock_data},
    };
    use futures::{StreamExt, stream};
    use std::vec;
    use tokio::sync::watch;
    use tokio::time::{sleep, timeout};

    #[tokio::test(flavor = "multi_thread")]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        // Keep only the data handle for the final assertion; the whole `pusher`
        // (whose listener holds a clone of the `event_chain` sender) is moved into
        // the download, so `event_chain` closes once the push thread finishes,
        // terminating the single drain loop below.
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );

        let mut pull_progress: Vec<ProgressEntry> = Vec::new();
        let mut push_progress: Vec<ProgressEntry> = Vec::new();
        let mut pull_ids = [false; 32];
        while let Ok(e) = result.event_chain().recv().await {
            match e {
                Event::PullProgress(id, p) => {
                    pull_ids[id] = true;
                    pull_progress.merge_progress(p);
                }
                Event::PushProgress(p) => push_progress.merge_progress(p),
                _ => {}
            }
        }
        assert_eq!(pull_progress, download_chunks);
        assert_eq!(push_progress, download_chunks);
        assert!(pull_ids.iter().any(|x| *x));

        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_concurrent_download_abort_discards() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );

        // Abort immediately. The push driver must observe the shared flag and break
        // out WITHOUT flushing, letting the event loop end promptly.
        result.abort();
        assert!(result.is_aborted());

        tokio::time::timeout(Duration::from_secs(10), drain(&result))
            .await
            .expect("event loop hung after abort");

        let written = receive.lock().len();
        assert!(
            written <= mock_data.len(),
            "abort must not write beyond the source"
        );
    }

    // -------------------------------------------------------------------------
    // Strengthened abort coverage.
    //
    // `BufWriterPusher` only forwards buffered bytes to its inner sink on
    // `flush()` (or overflow). This concurrent test wraps `MemPusher` with
    // `CacheSeqPusher<BufWriterPusher<_>>` — the same layering `CacheFilePusher`
    // uses in production — so out-of-order chunks are reordered into a
    // contiguous stream *before* `BufWriterPusher` coalesces them. On abort the
    // un-flushed buffer (CacheSeqPusher's BTreeMap + BufWriterPusher's BytesMut)
    // is discarded, so the inner sink sees strictly less than the full source
    // (the bare `MemPusher` test's `written <= source` would also pass if abort
    // missed). Layering also removes the old flakiness: without `CacheSeqPusher`
    // the interleaved concurrent writes were all non-contiguous and every piece
    // was flushed straight through, so a slow abort could drain the whole source.
    // -------------------------------------------------------------------------

    /// A [`Puller`] that stalls for `delay` before yielding any data, so a test
    /// can deterministically abort *mid-flight*.
    #[derive(Debug, Clone)]
    struct SlowMockPuller {
        data: Arc<[u8]>,
        delay: Duration,
    }
    impl Puller for SlowMockPuller {
        type Error = std::convert::Infallible;
        #[allow(clippy::cast_possible_truncation)]
        fn pull(
            &mut self,
            range: Option<&ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            type PullItem = PullResult<Bytes, std::convert::Infallible>;
            let owned: Vec<u8> = match range {
                Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                None => self.data.to_vec(),
            };
            let delay = self.delay;
            async move {
                sleep(delay).await;
                let items: Vec<PullItem> = owned
                    .chunks(2)
                    .map(|c| Ok(Bytes::from(c.to_vec())))
                    .collect();
                Ok(stream::iter(items))
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_concurrent_download_abort_discards_buffered() {
        // 64 KiB source; the slow puller stalls so the test can abort mid-flight.
        let mock_data = build_mock_data(64 * 1024);
        let puller = SlowMockPuller {
            data: Arc::from(mock_data.as_slice()),
            delay: Duration::from_millis(50),
        };
        // Restore the production reordering layer (CacheFilePusher = CacheSeqPusher<BufWriterPusher<...>>):
        // concurrent out-of-order chunks are first reordered into contiguous runs inside CacheSeqPusher's BTreeMap,
        // then fed contiguously to BufWriterPusher (which only coalesces contiguous writes). On abort, the
        // un-flushed buffer (CacheSeqPusher's BTreeMap + BufWriterPusher's BytesMut) is discarded as a whole,
        // so the inner MemPusher receives zero bytes -> written is deterministically == 0.
        // high_watermark is set to source+1 so CacheSeqPusher never proactively evicts and holds everything.
        let inner = MemPusher::with_capacity(mock_data.len());
        let receive = inner.receive.clone();
        let buf = BufWriterPusher::new(inner, mock_data.len() + 1);
        let pusher = CacheSeqPusher::new(buf, mock_data.len() + 1, 0);
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );

        // Abort as soon as the push driver starts processing (first `Pushing`).
        let mut aborted = false;
        while let Ok(e) = result.event_chain().recv().await {
            if matches!(e, Event::Pushing(_, _)) {
                result.abort();
                assert!(result.is_aborted());
                aborted = true;
                break;
            }
        }
        assert!(aborted, "expected a Pushing event before aborting");

        timeout(Duration::from_secs(10), drain(&result))
            .await
            .expect("event loop hung after abort");

        // Abort stops the download well before completion, so the inner sink
        // must hold strictly less than the full source (some out-of-order runs
        // may have been flushed, but completion is impossible after this abort).
        let written = receive.lock().len();
        assert!(
            written < mock_data.len(),
            "abort must stop before the full source is written (got {written} of {})",
            mock_data.len()
        );
    }

    // -------------------------------------------------------------------------
    // Coverage for the push/flush error-retry paths, the pull and stream error
    // paths, the empty-chunk skip, and the `SlowMockPuller` `None` branch.
    // -------------------------------------------------------------------------

    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct FatalErr;
    impl std::fmt::Display for FatalErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("fatal")
        }
    }
    impl std::error::Error for FatalErr {}
    impl crate::PullerError for FatalErr {
        fn is_irrecoverable(&self) -> bool {
            true
        }
    }

    #[derive(Debug)]
    struct RecoverableErr;
    impl std::fmt::Display for RecoverableErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("recoverable")
        }
    }
    impl std::error::Error for RecoverableErr {}
    impl crate::PullerError for RecoverableErr {
        fn is_irrecoverable(&self) -> bool {
            false
        }
    }

    /// In-memory sink that can be told to fail the next `push` or `flush`.
    struct FlakySink {
        fail_push: Arc<AtomicBool>,
        fail_flush: Arc<AtomicBool>,
        receive: Arc<Mutex<Vec<u8>>>,
        listener: Option<crate::ProgressListener>,
    }
    impl FlakySink {
        fn new() -> Self {
            Self {
                fail_push: Arc::new(AtomicBool::new(false)),
                fail_flush: Arc::new(AtomicBool::new(false)),
                receive: Arc::new(Mutex::new(Vec::new())),
                listener: None,
            }
        }
    }
    impl crate::Pusher for FlakySink {
        type Error = std::io::Error;
        fn set_listener(&mut self, cb: crate::ProgressListener) {
            self.listener = Some(cb);
        }
        fn push(
            &mut self,
            range: &crate::ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            if self.fail_push.swap(false, Ordering::SeqCst) {
                return Err((std::io::Error::other("push"), bytes));
            }
            let mut g = self.receive.lock();
            if range.start as usize == g.len() {
                g.extend_from_slice(&bytes);
            } else {
                if g.len() < range.end as usize {
                    g.resize(range.end as usize, 0);
                }
                g[range.start as usize..range.end as usize].copy_from_slice(&bytes);
            }
            drop(g);
            if let Some(l) = &mut self.listener {
                l(range.clone());
            }
            Ok(())
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            if self.fail_flush.swap(false, Ordering::SeqCst) {
                Err(std::io::Error::other("flush"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Debug, Clone)]
    struct EmptyChunkPuller {
        data: Arc<[u8]>,
    }
    impl crate::Puller for EmptyChunkPuller {
        type Error = std::convert::Infallible;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let mut items: Vec<crate::PullResult<Bytes, std::convert::Infallible>> =
                vec![Ok(Bytes::new())];
            items.extend(data.chunks(2).map(|c| Ok(Bytes::copy_from_slice(c))));
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    #[derive(Debug, Clone)]
    struct PullErrOncePuller {
        data: Arc<[u8]>,
        failed: Arc<AtomicBool>,
    }
    impl crate::Puller for PullErrOncePuller {
        type Error = RecoverableErr;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            if !self.failed.swap(true, Ordering::SeqCst) {
                return std::future::ready(Err((RecoverableErr, Some(Duration::ZERO))));
            }
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let items: Vec<crate::PullResult<Bytes, RecoverableErr>> = data
                .chunks(2)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    #[derive(Debug, Clone)]
    struct StreamErrOncePuller {
        data: Arc<[u8]>,
        failed: Arc<AtomicBool>,
    }
    impl crate::Puller for StreamErrOncePuller {
        type Error = FatalErr;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            if !self.failed.swap(true, Ordering::SeqCst) {
                let items: Vec<crate::PullResult<Bytes, FatalErr>> =
                    vec![Err((FatalErr, Some(Duration::ZERO)))];
                return std::future::ready(Ok(stream::iter(items)));
            }
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let items: Vec<crate::PullResult<Bytes, FatalErr>> = data
                .chunks(2)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    /// Like [`StreamErrOncePuller`] but yields a *recoverable* stream error first,
    /// so the `is_irrecoverable == false` fall-through is exercised.
    #[derive(Debug, Clone)]
    struct RecoverableStreamErrOncePuller {
        data: Arc<[u8]>,
        failed: Arc<AtomicBool>,
    }
    impl crate::Puller for RecoverableStreamErrOncePuller {
        type Error = RecoverableErr;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            if !self.failed.swap(true, Ordering::SeqCst) {
                let items: Vec<crate::PullResult<Bytes, RecoverableErr>> =
                    vec![Err((RecoverableErr, Some(Duration::ZERO)))];
                return std::future::ready(Ok(stream::iter(items)));
            }
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let items: Vec<crate::PullResult<Bytes, RecoverableErr>> = data
                .chunks(2)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    async fn drain<R, WE>(result: &DownloadResult<TokioExecutor<R, WE>, R::Error, WE>)
    where
        R: crate::Puller,
        WE: Send + Unpin + 'static,
    {
        while result.event_chain().recv().await.is_ok() {}
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_push_error_retries() {
        // a failing inner push is retried after `park_timeout`.
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let sink = FlakySink::new();
        sink.fail_push.store(true, Ordering::SeqCst);
        let receive = sink.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            sink,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_flush_error_retries() {
        // a failing inner flush is retried after `park_timeout`.
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let sink = FlakySink::new();
        sink.fail_flush.store(true, Ordering::SeqCst);
        let receive = sink.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            sink,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_pull_error_retries() {
        // a `pull` error (recoverable) is retried.
        let mock_data = build_mock_data(3 * 1024);
        let puller = PullErrOncePuller {
            data: Arc::from(mock_data.as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_empty_chunk_is_skipped() {
        // an empty chunk yielded by the stream is skipped without error.
        let mock_data = build_mock_data(3 * 1024);
        let puller = EmptyChunkPuller {
            data: Arc::from(mock_data.as_slice()),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_stream_error_irrecoverable_retries() {
        // a stream error whose `is_irrecoverable` is true triggers a
        // `continue 'task` and a re-pull, which then succeeds.
        let mock_data = build_mock_data(3 * 1024);
        let puller = StreamErrOncePuller {
            data: Arc::from(mock_data.as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_stream_error_recoverable_retries() {
        // a stream error whose `is_irrecoverable` is false does NOT
        // `continue 'task`; instead it falls through and retries the pull,
        // which then succeeds.
        let mock_data = build_mock_data(3 * 1024);
        let puller = RecoverableStreamErrOncePuller {
            data: Arc::from(mock_data.as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test]
    async fn puller_and_error_coverage() {
        // Exercise `Display` for the test error types and the `None` arm of each
        // test puller's `match range`.
        assert_eq!(format!("{FatalErr}"), "fatal");
        assert_eq!(format!("{RecoverableErr}"), "recoverable");

        let mut empty = EmptyChunkPuller {
            data: Arc::from(b"abcdef".as_slice()),
        };
        let _ = empty.pull(Some(&(0..2u64))).await;
        let _ = empty.pull(None).await;

        let mut pull_err = PullErrOncePuller {
            data: Arc::from(b"abcdef".as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let _ = pull_err.pull(Some(&(0..2u64))).await; // first call errors, sets `failed`
        let _ = pull_err.pull(None).await; // success path, `None` arm

        let mut stream_err = StreamErrOncePuller {
            data: Arc::from(b"abcdef".as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let _ = stream_err.pull(Some(&(0..2u64))).await;
        let _ = stream_err.pull(None).await;

        let mut rec_stream_err = RecoverableStreamErrOncePuller {
            data: Arc::from(b"abcdef".as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let _ = rec_stream_err.pull(Some(&(0..2u64))).await; // first: `Some` arm + error
        let _ = rec_stream_err.pull(Some(&(0..2u64))).await; // not-first: `Some` arm
        let _ = rec_stream_err.pull(None).await; // `None` arm
    }

    #[tokio::test]
    async fn test_slow_mock_puller_none_range() {
        // the `None` branch of `SlowMockPuller::pull` (only the `Some`
        // branch is exercised by the concurrent download path).
        let mut p = SlowMockPuller {
            data: Arc::from(b"hello world".as_slice()),
            delay: Duration::ZERO,
        };
        assert!(p.pull(None).await.is_ok());
        let mut p2 = SlowMockPuller {
            data: Arc::from(b"hello world".as_slice()),
            delay: Duration::ZERO,
        };
        assert!(p2.pull(Some(&(0..5))).await.is_ok());
    }

    /// A [`Puller`] that yields one chunk on its *first* pull, then a stream that
    /// never completes — forcing the worker's `pull_timeout` branch
    /// to fire. The *second* pull returns the remaining range, so the download
    /// recovers by re-pulling instead of hanging.
    #[derive(Debug, Clone)]
    struct TimeoutOncePuller {
        data: Arc<[u8]>,
        first: Arc<AtomicBool>,
    }
    impl crate::Puller for TimeoutOncePuller {
        type Error = std::convert::Infallible;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            let is_first = !self.first.swap(true, Ordering::SeqCst);
            let data: Vec<u8> = match range {
                Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                None => self.data.to_vec(),
            };
            async move {
                if is_first {
                    // Yield one chunk, then a stream that never completes, so the
                    // worker's `pull_timeout` branch fires and the
                    // worker re-pulls the remaining range.
                    let head = data.get(..2).unwrap_or(&data);
                    let items = vec![Ok(Bytes::copy_from_slice(head))];
                    let pending =
                        stream::pending::<crate::PullResult<Bytes, std::convert::Infallible>>();
                    Ok(stream::iter(items).chain(pending))
                } else {
                    // Subsequent pulls return the full remaining range; the trailing
                    // `pending` is never polled because the worker exits the read
                    // loop on `start >= end` before reaching it.
                    let items: Vec<crate::PullResult<Bytes, std::convert::Infallible>> = data
                        .chunks(2)
                        .map(|c| Ok(Bytes::copy_from_slice(c)))
                        .collect();
                    let pending =
                        stream::pending::<crate::PullResult<Bytes, std::convert::Infallible>>();
                    Ok(stream::iter(items).chain(pending))
                }
            }
        }
    }

    /// A [`Puller`] whose first `pull` never resolves, modelling a server that
    /// accepts the connection but never sends a response head. Later pulls serve
    /// the data, so the worker recovers instead of parking forever.
    #[derive(Debug, Clone)]
    struct HeadStallOncePuller {
        data: Arc<[u8]>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl crate::Puller for HeadStallOncePuller {
        type Error = std::convert::Infallible;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let data: Vec<u8> = match range {
                Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                None => self.data.to_vec(),
            };
            async move {
                type Item = crate::PullResult<Bytes, std::convert::Infallible>;
                if call == 0 {
                    std::future::pending::<crate::PullResult<_, std::convert::Infallible>>().await
                } else {
                    let items: Vec<Item> = data
                        .chunks(2)
                        .map(|c| Ok(Bytes::copy_from_slice(c)))
                        .collect();
                    Ok(Box::pin(stream::iter(items))
                        as std::pin::Pin<
                            Box<dyn futures::Stream<Item = Item> + Send>,
                        >)
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_pull_timeout_covers_the_request() {
        // A `pull` that never resolves (the response head never arrives) must be
        // abandoned after `pull_timeout` and retried; a single worker leaves nothing
        // else able to make progress.
        let mock_data = build_mock_data(2 * 1024);
        let puller = HeadStallOncePuller {
            data: Arc::from(mock_data.as_slice()),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 1,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_millis(50),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );
        timeout(Duration::from_secs(10), drain(&result))
            .await
            .expect("download must not hang when the response head never arrives");
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_pull_error_also_rebuilds_the_puller() {
        // A failed pull is retried like a stalled one: the puller is rebuilt, not
        // reused. The failure flag is shared, so only the first attempt errors and
        // every clone serves the data. Reaching the full payload therefore proves the
        // retry ran on a rebuilt puller.
        #[derive(Debug)]
        struct CloneCountingPuller {
            data: Arc<[u8]>,
            failed: Arc<AtomicBool>,
            clones: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Clone for CloneCountingPuller {
            fn clone(&self) -> Self {
                self.clones.fetch_add(1, Ordering::SeqCst);
                Self {
                    data: self.data.clone(),
                    failed: self.failed.clone(),
                    clones: self.clones.clone(),
                }
            }
        }
        impl crate::Puller for CloneCountingPuller {
            type Error = RecoverableErr;
            fn pull(
                &mut self,
                range: Option<&crate::ProgressEntry>,
            ) -> impl Future<
                Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
            > + Send {
                if !self.failed.swap(true, Ordering::SeqCst) {
                    return std::future::ready(Err((RecoverableErr, Some(Duration::ZERO))));
                }
                let data: Vec<u8> = match range {
                    Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                    None => self.data.to_vec(),
                };
                let items: Vec<crate::PullResult<Bytes, RecoverableErr>> = data
                    .chunks(2)
                    .map(|c| Ok(Bytes::copy_from_slice(c)))
                    .collect();
                std::future::ready(Ok(stream::iter(items)))
            }
        }

        let mock_data = build_mock_data(2 * 1024);
        let clones = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let puller = CloneCountingPuller {
            data: Arc::from(mock_data.as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
            clones: clones.clone(),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 1,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
        // `execute` clones the puller once for its own worker, so a single retry
        // raises the count to 2. Asserting on that offset keeps the test from
        // passing on the initial clone alone.
        assert!(
            clones.load(Ordering::SeqCst) >= 2,
            "retrying a failed pull must rebuild the puller instead of reusing it \
             (one clone belongs to `execute`, the second to the retry)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_pull_timeout_recovers_by_repulling() {
        // a stalled stream (first pull yields one chunk then hangs)
        // must trigger `PullTimeout`, drop the stream, and re-pull the remaining
        // range — the download still completes with the full payload.
        let mock_data = build_mock_data(3 * 1024);
        let puller = TimeoutOncePuller {
            data: Arc::from(mock_data.as_slice()),
            first: Arc::new(AtomicBool::new(false)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_millis(50),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_zero_pull_timeout_disables_the_timeout() {
        // `pull_timeout = ZERO` must disable the guard rather than arming an
        // always-ready `sleep(ZERO)`. The stream here is not ready on the first
        // poll, so an armed zero timeout wins the `select!` and re-requests the
        // same range instead of waiting for it.
        let mock_data = build_mock_data(3 * 1024);
        let puller = SlowStreamPuller {
            data: Arc::from(mock_data.as_slice()),
            delay: Duration::from_millis(2),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 4,
                retry_gap: Duration::from_millis(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::ZERO,
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1024,
                max_speculative: 1,
            },
        );
        let mut timeouts = 0;
        timeout(Duration::from_secs(5), async {
            while let Ok(event) = result.event_chain().recv().await {
                if matches!(event, Event::PullTimeout(_)) {
                    timeouts += 1;
                }
            }
        })
        .await
        .expect("a disabled pull timeout must not stall the session");
        assert_eq!(timeouts, 0, "a disabled timeout must not fire");
        assert_eq!(&**receive.lock(), mock_data);
    }

    /// A [`Puller`] whose chunks arrive only after `delay`, so an armed zero
    /// `pull_timeout` would always win the worker's `select!`.
    #[derive(Debug, Clone)]
    struct SlowStreamPuller {
        data: Arc<[u8]>,
        delay: Duration,
    }
    impl crate::Puller for SlowStreamPuller {
        type Error = std::convert::Infallible;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            type Item = crate::PullResult<Bytes, std::convert::Infallible>;
            #[allow(clippy::cast_possible_truncation)]
            let data: Vec<u8> = match range {
                Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                None => self.data.to_vec(),
            };
            let delay = self.delay;
            async move {
                let chunks: Vec<Bytes> = data.chunks(1024).map(Bytes::copy_from_slice).collect();
                let stream = stream::iter(chunks).then(move |chunk| async move {
                    sleep(delay).await;
                    Ok::<_, (std::convert::Infallible, Option<Duration>)>(chunk)
                });
                Ok(Box::pin(stream)
                    as std::pin::Pin<
                        Box<dyn futures::Stream<Item = Item> + Send>,
                    >)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_concurrent_download_empty_chunks() {
        // Degenerate input: an empty `download_chunks` list must not hang or panic.
        // With no work queued, `set_threads` spawns zero workers and the event loop
        // ends once the push worker sees the closed channel.
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: std::iter::empty(),
                pull_timeout: Duration::from_secs(5),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        timeout(Duration::from_secs(10), drain(&result))
            .await
            .expect("event loop hung on empty chunks");
        assert_eq!(receive.lock().len(), 0);
    }

    /// A [`Puller`] that holds every `pull` until the test opens the gate, then
    /// parks the one originally-second chunk (`64..128`) until released. With a
    /// two-chunk pool this keeps that chunk's worker in flight while the first
    /// chunk's worker finishes and steals, and any extra worker a buggy steal
    /// spawns for a *sub-range* runs to completion instead of dead-locking.
    #[derive(Clone)]
    struct GatedPuller {
        data: Arc<[u8]>,
        /// Closed until the test has set the live granularity.
        gate: watch::Receiver<bool>,
        /// Closed until the first worker's steal outcome is fixed.
        hold: watch::Receiver<bool>,
    }

    impl crate::Puller for GatedPuller {
        type Error = std::convert::Infallible;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            let bytes: Vec<u8> = match range {
                Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                None => self.data.to_vec(),
            };
            // Only the originally-second chunk is held; a stolen sub-range of it
            // proceeds, so a buggy split is observable rather than dead-locking.
            let slow = range.is_some_and(|r| *r == (64..128u64));
            let mut gate = self.gate.clone();
            let mut hold = self.hold.clone();
            async move {
                loop {
                    if *gate.borrow_and_update() {
                        break;
                    }
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
                if slow {
                    loop {
                        if *hold.borrow_and_update() {
                            break;
                        }
                        if hold.changed().await.is_err() {
                            break;
                        }
                    }
                }
                let items: Vec<crate::PullResult<Bytes, std::convert::Infallible>> =
                    vec![Ok(Bytes::from(bytes))];
                Ok(stream::iter(items))
            }
        }
    }

    /// Drive the two-chunk steal probe with the live granularity set to `v`
    /// *before* the workers are released, and return the pull spans observed,
    /// sorted by start. Asserts the reassembled bytes are intact.
    ///
    /// Worker 0 owns `0..64` and finishes first; worker 1 owns `64..128` and is
    /// held until worker 0 exits, so the only question is whether worker 0's
    /// steal halves worker 1's 64-byte remainder (needs `remain >= 2 * v`).
    /// `v = 0` is clamped to `1`.
    async fn steal_granularity_probe(v: u64) -> Vec<ProgressEntry> {
        let mock_data = build_mock_data(128);
        let data: Arc<[u8]> = Arc::from(mock_data.as_slice());
        let (gate_tx, gate_rx) = watch::channel(false);
        let (hold_tx, hold_rx) = watch::channel(false);
        let puller = GatedPuller {
            data,
            gate: gate_rx,
            hold: hold_rx,
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..64u64, 64..128u64];
        let result = download_multi(
            puller,
            pusher,
            DownloadOptions {
                concurrent: 2,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(10),
                stall_timeout: Duration::from_secs(60),
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );

        // Both workers are parked on the gate; the live granularity must be set
        // before they run, so the steal sees `v` rather than the build value.
        result.set_min_chunk_size(v);
        gate_tx.send(true).expect("open gate");

        let mut ranges: Vec<ProgressEntry> = Vec::new();
        let mut released = false;
        let drained = timeout(Duration::from_secs(10), async {
            while let Ok(event) = result.event_chain().recv().await {
                if let Event::PullProgress(_, range) = &event {
                    ranges.push(range.clone());
                }
                // The steal outcome is fixed once worker 0 exits.
                if matches!(event, Event::Finished(0)) && !released {
                    released = true;
                    let _ = hold_tx.send(true);
                }
            }
        })
        .await;
        assert!(drained.is_ok(), "the download hung for v={v}");
        assert_eq!(&**receive.lock(), mock_data, "bytes corrupted for v={v}");
        ranges.sort_by_key(|r| r.start);
        ranges
    }

    /// `2 * 32 == 64`, so the finishing worker may halve the peer's 64-byte
    /// remainder exactly once; the peer's remainder is then below the threshold,
    /// so it cannot be halved again. Pins the boundary deterministically.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_min_chunk_size_at_half_allows_one_split() {
        assert_eq!(
            steal_granularity_probe(32).await,
            vec![0..64u64, 64..96, 96..128]
        );
    }

    /// One byte above half blocks the split (`2 * 33 > 64`), so the remainder is
    /// pulled whole. Distinguishes the live value from the build value (1).
    #[tokio::test(flavor = "multi_thread")]
    async fn set_min_chunk_size_just_above_half_blocks_split() {
        assert_eq!(steal_granularity_probe(33).await, vec![0..64u64, 64..128]);
    }

    /// A granularity larger than the remainder blocks any split.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_min_chunk_size_above_remainder_blocks_split() {
        assert_eq!(steal_granularity_probe(65).await, vec![0..64u64, 64..128]);
    }

    /// `set_min_chunk_size(0)` is clamped internally: the download must complete
    /// with intact bytes (the probe asserts that) and every pull span must be
    /// non-empty, rather than the clamp being skipped and a zero-width split
    /// being handed out.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_min_chunk_size_zero_is_clamped() {
        let spans = steal_granularity_probe(0).await;
        assert!(
            spans.iter().all(|r| r.start < r.end),
            "a zero minimum must not yield empty ranges: {spans:?}"
        );
        assert_eq!(spans.first().map(|r| r.start), Some(0));
        assert_eq!(spans.last().map(|r| r.end), Some(128));
    }

    /// A puller that answers every request with a body that never yields, so no
    /// `PullProgress` can ever arrive.
    #[derive(Debug, Clone)]
    struct AlwaysStallPuller;
    impl crate::Puller for AlwaysStallPuller {
        type Error = std::convert::Infallible;
        fn pull(
            &mut self,
            _range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            std::future::ready(Ok(
                stream::pending::<crate::PullResult<Bytes, Self::Error>>(),
            ))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stall_timeout_ends_a_session_that_never_produces_bytes() {
        // Retries alone never end a dead source: every attempt times out and is
        // retried. The stall watchdog must cancel the session and report it.
        let result = download_multi(
            AlwaysStallPuller,
            MemPusher::with_capacity(0),
            DownloadOptions {
                concurrent: 2,
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
                download_chunks: std::iter::once(0..1024u64),
                pull_timeout: Duration::from_millis(50),
                stall_timeout: Duration::from_millis(100),
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );
        let mut stalled = false;
        timeout(Duration::from_secs(5), async {
            while let Ok(event) = result.event_chain().recv().await {
                if matches!(event, Event::Stalled(_)) {
                    stalled = true;
                }
            }
        })
        .await
        .expect("a source that never yields must end after the stall timeout");
        assert!(stalled, "the session must report Event::Stalled");
    }

    /// A sink that panics on its first write, killing the push thread.
    struct PanickingSink;

    impl Pusher for PanickingSink {
        type Error = std::convert::Infallible;

        fn push(
            &mut self,
            _range: &crate::ProgressEntry,
            _bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            panic!("sink panic");
        }
    }

    /// Yields one chunk, then a stream that never resolves: the workers stay
    /// blocked in `try_next` after the push driver dies.
    #[derive(Clone)]
    struct OneChunkThenStallPuller;

    impl crate::Puller for OneChunkThenStallPuller {
        type Error = std::convert::Infallible;

        fn pull(
            &mut self,
            _range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            type Item = crate::PullResult<Bytes, std::convert::Infallible>;
            let items: Vec<Item> = vec![Ok(Bytes::from_static(b"x"))];
            std::future::ready(Ok(stream::iter(items).chain(stream::pending::<Item>())))
        }
    }

    /// The push driver is the only writer, so its death must end the session.
    /// Without cancelling, the workers keep pulling into a closed channel and
    /// drop every chunk: the whole file is transferred for nothing and the
    /// session keeps "running" until the last byte arrives.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_driver_panic_stops_the_workers() {
        let result = download_multi(
            OneChunkThenStallPuller,
            PanickingSink,
            DownloadOptions {
                concurrent: 2,
                retry_gap: Duration::ZERO,
                push_queue_cap: 16,
                download_chunks: std::iter::once(0..1024u64),
                pull_timeout: Duration::from_secs(60),
                stall_timeout: Duration::ZERO,
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );
        timeout(Duration::from_secs(5), async {
            while result.event_chain().recv().await.is_ok() {}
        })
        .await
        .expect("a panicking push driver must not keep the workers alive");
        assert!(
            result.is_aborted(),
            "a dead writer must report the session as aborted, not merely unfinished"
        );
    }

    /// A puller whose request panics instead of returning a stream.
    #[derive(Clone)]
    struct PanickingPuller;

    impl crate::Puller for PanickingPuller {
        type Error = std::convert::Infallible;

        async fn pull(
            &mut self,
            _range: Option<&crate::ProgressEntry>,
        ) -> crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error> {
            // A real `.await` makes this a genuine `async fn`; the pull itself
            // panics on its first poll, which is what the test is about.
            std::future::ready(()).await;
            panic!("pull panic");
            // Unreachable: it only pins the opaque stream type.
            #[allow(unreachable_code)]
            {
                Ok::<_, (Self::Error, Option<Duration>)>(stream::empty())
            }
        }
    }

    /// A panicking worker must abort the session: otherwise its cursor simply
    /// stops advancing, the worker drops its queue sender, and the run can end as
    /// if the missing range had been downloaded.
    #[tokio::test(flavor = "multi_thread")]
    async fn worker_panic_aborts_the_session() {
        let result = download_multi(
            PanickingPuller,
            MemPusher::with_capacity(1024),
            DownloadOptions {
                concurrent: 2,
                retry_gap: Duration::ZERO,
                push_queue_cap: 16,
                download_chunks: std::iter::once(0..1024u64),
                // Neither a pull timeout nor the stall watchdog may be what ends
                // the session; only the panic detection can.
                pull_timeout: Duration::ZERO,
                stall_timeout: Duration::ZERO,
                min_chunk_size: 1,
                max_speculative: 1,
            },
        );
        timeout(Duration::from_secs(5), async {
            while result.event_chain().recv().await.is_ok() {}
        })
        .await
        .expect("a panicking worker must not keep the session alive");
        assert!(
            result.is_aborted(),
            "a panicking pull must report the session as aborted, not completed"
        );
    }
}
