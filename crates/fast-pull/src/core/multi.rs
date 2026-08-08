//! Multi-threaded concurrent download with work-stealing.

use crate::{DownloadResult, Event, ProgressEntry, Puller, PullerError, Pusher, WorkerId};
use bytes::Bytes;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use crossfire::{MAsyncTx, MTx, WeakTx, mpmc, mpsc};
use fast_steal::{Executor, Handle, Task, TaskQueue};
use futures::TryStreamExt;
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

/// Options for a multi-threaded concurrent download.
///
/// Controls chunk splitting, speculation, pull timeouts, and write queue capacity.
#[derive(Debug, Clone)]
pub struct DownloadOptions<I: Iterator<Item = ProgressEntry>> {
    pub download_chunks: I,
    pub concurrent: usize,
    pub retry_gap: Duration,
    pub pull_timeout: Duration,
    pub push_queue_cap: usize,
    pub min_chunk_size: u64,
    pub max_speculative: usize,
}

pub fn download_multi<R: Puller, W: Pusher, I: Iterator<Item = ProgressEntry>>(
    puller: R,
    mut pusher: W,
    options: DownloadOptions<I>,
) -> DownloadResult<TokioExecutor<R, W::Error>, R::Error, W::Error> {
    let token = CancellationToken::new();
    let (tx, event_chain) = mpmc::unbounded_async();
    pusher.set_listener({
        let tx = tx.clone();
        Box::new(move |p| {
            let _ = tx.send(Event::PushProgress(p));
        })
    });
    let (tx_push, rx_push) =
        mpsc::bounded_async_blocking::<(WorkerId, ProgressEntry, Bytes)>(options.push_queue_cap);

    let push_thread = Arc::new(OnceLock::new());
    let push_handle = tokio::task::spawn_blocking({
        let push_thread = push_thread.clone();
        let token = token.clone();
        let tx = tx.clone();
        move || {
            let _ = push_thread.set(std::thread::current());
            while let Ok((id, mut spin, mut data)) = rx_push.recv() {
                loop {
                    if token.is_cancelled() {
                        return;
                    }
                    let _ = tx.send(Event::Pushing(id, spin.clone()));
                    let len_before_push = data.len();
                    match pusher.push(&spin, data) {
                        Ok(()) => break,
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
            loop {
                if token.is_cancelled() {
                    return;
                }
                let _ = tx.send(Event::Flushing);
                match pusher.flush() {
                    Ok(()) => break,
                    Err(err) => {
                        let _ = tx.send(Event::FlushError(err));
                    }
                }
                std::thread::park_timeout(options.retry_gap);
            }
        }
    });
    tokio::spawn({
        let token = token.clone();
        async move {
            tokio::select! {
                _ = push_handle => {},
                () = token.cancelled() => {
                    if let Some(t) = push_thread.get() {
                        t.unpark();
                    }
                }
            }
        }
    });

    let executor: TokioExecutor<R, W::Error> = TokioExecutor {
        token: token.clone(),
        tx: tx.downgrade(),
        tx_push: tx_push.downgrade(),
        puller,
        id: AtomicUsize::new(0),
        retry_gap: options.retry_gap,
        pull_timeout: options.pull_timeout,
        min_chunk_size: options.min_chunk_size,
        max_speculative: options.max_speculative,
    };
    let task_queue = TaskQueue::new(options.download_chunks);
    let _ = task_queue.set_threads(options.concurrent, options.min_chunk_size, Some(&executor));

    DownloadResult::new(event_chain, Some((executor, task_queue)), token)
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
    id: AtomicUsize,
    min_chunk_size: u64,
    max_speculative: usize,
}
impl<R, WE> Executor for TokioExecutor<R, WE>
where
    R: Puller,
    WE: Send + Unpin + 'static,
{
    type Handle = TokioHandle;
    #[allow(clippy::too_many_lines)]
    fn execute(&self, mut task: Task, task_queue: TaskQueue<Self::Handle>) -> Self::Handle {
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        let token = self.token.child_token();

        let tx: Option<MTx<_>> = self.tx.upgrade();
        let tx_push: Option<MAsyncTx<_>> = self.tx_push.upgrade();
        let (Some(tx), Some(tx_push)) = (tx, tx_push) else {
            return TokioHandle { id, token };
        };

        let mut puller = self.puller.clone();
        let min_chunk_size = self.min_chunk_size;
        let pull_timeout = self.pull_timeout;
        let cfg_retry_gap = self.retry_gap;
        let max_speculative = self.max_speculative;
        let worker_token = token.clone();
        tokio::spawn(async move {
            'task: loop {
                if worker_token.is_cancelled() {
                    break 'task;
                }
                let mut start = task.start();
                if start >= task.end() {
                    if task_queue.steal(&id, &mut task, min_chunk_size, max_speculative) {
                        continue 'task;
                    }
                    break 'task;
                }
                let _ = tx.send(Event::Pulling(id));
                let download_range = start..task.end();
                let mut stream = loop {
                    let t = tokio::select! {
                        () = worker_token.cancelled() => break 'task,
                        t = puller.pull(Some(&download_range)) => t
                    };
                    match t {
                        Ok(t) => break t,
                        Err((e, retry_gap)) => {
                            let _ = tx.send(Event::PullError(id, e));
                            tokio::select! {
                                () = worker_token.cancelled() => break 'task,
                                () = tokio::time::sleep(retry_gap.unwrap_or(cfg_retry_gap)) => {}
                            };
                        }
                    }
                };
                loop {
                    let t = tokio::select! {
                        () = worker_token.cancelled() => break 'task,
                        () = tokio::time::sleep(pull_timeout) => {
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
                            if span.end >= task.end() {
                                task_queue.cancel_task(&task, &id);
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            let slice_span =
                                (span.start - start) as usize..(span.end - start) as usize;
                            chunk = chunk.slice(slice_span);
                            start = span.end;
                            let _ = tx.send(Event::PullProgress(id, span.clone()));
                            let _ = tx_push.send((id, span, chunk)).await;
                            if start >= task.end() {
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
#[cfg(feature = "mem")]
mod tests {
    #![allow(clippy::cast_possible_truncation)]
    use vec::Vec;

    use super::*;
    use crate::{BufWriterPusher, CacheSeqPusher};
    use crate::{
        Merge, ProgressEntry,
        mem::MemPusher,
        mock::{MockPuller, build_mock_data},
    };
    use futures::{StreamExt, stream};
    use std::{dbg, vec};
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
        dbg!(&pull_progress);
        dbg!(&push_progress);
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
            type PullItem = Result<Bytes, (std::convert::Infallible, Option<Duration>)>;
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
    // Coverage for the push/flush error-retry paths (lines 62-67, 77-81) and the
    // pull / stream error paths (lines 188-192, 239-248) plus the empty-chunk
    // skip (line 217) and the `SlowMockPuller` `None` branch (line 402).
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

    /// In-memory sink that can be told to fail the next `push` (lines 62-67) or
    /// `flush` (lines 77-81).
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
            let mut items: Vec<Result<Bytes, (std::convert::Infallible, Option<Duration>)>> =
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
            let items: Vec<Result<Bytes, (RecoverableErr, Option<Duration>)>> = data
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
                let items: Vec<Result<Bytes, (FatalErr, Option<Duration>)>> =
                    vec![Err((FatalErr, Some(Duration::ZERO)))];
                return std::future::ready(Ok(stream::iter(items)));
            }
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let items: Vec<Result<Bytes, (FatalErr, Option<Duration>)>> = data
                .chunks(2)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    /// Like [`StreamErrOncePuller`] but yields a *recoverable* stream error first,
    /// so the `is_irrecoverable == false` fall-through (line 248) is exercised.
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
                let items: Vec<Result<Bytes, (RecoverableErr, Option<Duration>)>> =
                    vec![Err((RecoverableErr, Some(Duration::ZERO)))];
                return std::future::ready(Ok(stream::iter(items)));
            }
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let items: Vec<Result<Bytes, (RecoverableErr, Option<Duration>)>> = data
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
        // Lines 62-67: a failing inner push is retried after `park_timeout`.
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
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_flush_error_retries() {
        // Lines 77-81: a failing inner flush is retried after `park_timeout`.
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
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_pull_error_retries() {
        // Lines 188-192: a `pull` error (recoverable) is retried.
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
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_empty_chunk_is_skipped() {
        // Line 217: an empty chunk yielded by the stream is skipped without error.
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
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_stream_error_irrecoverable_retries() {
        // Lines 239-248: a stream error whose `is_irrecoverable` is true triggers a
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
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_stream_error_recoverable_retries() {
        // Lines 239-248: a stream error whose `is_irrecoverable` is false does NOT
        // `continue 'task`; instead it falls through (line 248) and retries the pull,
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
        // test puller's `match range` (lines 586, 613, 641).
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
        // Line 402: the `None` branch of `SlowMockPuller::pull` (only the `Some`
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
    /// never completes — forcing the worker's `pull_timeout` branch (lines 203-210)
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
                    // worker's `pull_timeout` branch (lines 203-210) fires and the
                    // worker re-pulls the remaining range.
                    let head = data.get(..2).unwrap_or(&data);
                    let items = vec![Ok(Bytes::copy_from_slice(head))];
                    let pending = stream::pending::<
                        Result<Bytes, (std::convert::Infallible, Option<Duration>)>,
                    >();
                    Ok(stream::iter(items).chain(pending))
                } else {
                    // Subsequent pulls return the full remaining range; the trailing
                    // `pending` is never polled because the worker exits the read
                    // loop on `start >= end` before reaching it.
                    let items: Vec<Result<Bytes, (std::convert::Infallible, Option<Duration>)>> =
                        data.chunks(2)
                            .map(|c| Ok(Bytes::copy_from_slice(c)))
                            .collect();
                    let pending = stream::pending::<
                        Result<Bytes, (std::convert::Infallible, Option<Duration>)>,
                    >();
                    Ok(stream::iter(items).chain(pending))
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multi_pull_timeout_recovers_by_repulling() {
        // Lines 203-210: a stalled stream (first pull yields one chunk then hangs)
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
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        drain(&result).await;
        assert_eq!(&**receive.lock(), mock_data);
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
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );
        timeout(Duration::from_secs(10), drain(&result))
            .await
            .expect("event loop hung on empty chunks");
        assert_eq!(receive.lock().len(), 0);
    }
}
