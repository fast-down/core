use crate::{DownloadResult, Event, ProgressEntry, Puller, PullerError, Pusher, WorkerId};
use bytes::Bytes;
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use crossfire::{MAsyncTx, MTx, mpmc, mpsc};
use fast_steal::{Executor, Handle, Task, TaskQueue};
use futures::TryStreamExt;
use std::sync::Arc;
use tokio::sync::Notify;

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

/// # Panics
/// Panics if the internal `TaskQueue::set_threads` returns `None`, which only
/// happens when the executor passed to it is unexpectedly `None`.
pub fn download_multi<R: Puller, W: Pusher, I: Iterator<Item = ProgressEntry>>(
    puller: R,
    mut pusher: W,
    options: DownloadOptions<I>,
) -> DownloadResult<TokioExecutor<R, W::Error>, R::Error, W::Error> {
    let (tx, event_chain) = mpmc::unbounded_async();
    let tx_listener = tx.clone();
    pusher.set_listener(Box::new(move |p: ProgressEntry| {
        let _ = tx_listener.send(Event::PushProgress(p));
    }));
    let (tx_push, rx_push) =
        mpsc::bounded_async::<(WorkerId, ProgressEntry, Bytes)>(options.push_queue_cap);
    let tx_clone = tx.clone();
    let rx_push = rx_push.into_blocking();
    let abort_flag = Arc::new(AtomicBool::new(false));
    let abort_flag_clone = abort_flag.clone();
    let push_handle = tokio::task::spawn_blocking(move || {
        'outer: while let Ok((id, spin, mut data)) = rx_push.recv() {
            loop {
                if abort_flag_clone.load(Ordering::Relaxed) {
                    break 'outer;
                }
                let _ = tx_clone.send(Event::Pushing(id, spin.clone()));
                match pusher.push(&spin, data) {
                    Ok(()) => break,
                    Err((err, bytes)) => {
                        data = bytes;
                        let _ = tx_clone.send(Event::PushError(id, spin.clone(), err));
                    }
                }
                std::thread::sleep(options.retry_gap);
            }
        }
        loop {
            if abort_flag_clone.load(Ordering::Relaxed) {
                break;
            }
            let _ = tx_clone.send(Event::Flushing);
            match pusher.flush() {
                Ok(()) => break,
                Err(err) => {
                    let _ = tx_clone.send(Event::FlushError(err));
                }
            }
            std::thread::sleep(options.retry_gap);
        }
    });
    let executor: Arc<TokioExecutor<R, W::Error>> = Arc::new(TokioExecutor {
        tx,
        tx_push,
        puller,
        pull_timeout: options.pull_timeout,
        retry_gap: options.retry_gap,
        id: AtomicUsize::new(0),
        min_chunk_size: options.min_chunk_size,
        max_speculative: options.max_speculative,
    });
    let task_queue = TaskQueue::new(options.download_chunks);
    task_queue
        .set_threads(
            options.concurrent,
            options.min_chunk_size,
            Some(executor.as_ref()),
        )
        .unwrap();
    DownloadResult::new(
        event_chain,
        push_handle,
        None,
        Some((Arc::downgrade(&executor), task_queue)),
        abort_flag,
    )
}

/// A [`Handle`] implementation using tokio's [`Notify`] for cancellation.
#[derive(Debug, Clone)]
pub struct TokioHandle {
    id: usize,
    notify: Arc<Notify>,
}
impl Handle for TokioHandle {
    type Output = ();
    type Id = usize;
    fn abort(&mut self) -> Self::Output {
        self.notify.notify_one();
    }
    fn is_self(&mut self, id: &Self::Id) -> bool {
        self.id == *id
    }
}
/// A built-in [`Executor`] implementation based on tokio tasks.
///
/// Each worker is a `tokio::spawn`-ed task that pulls chunks from the puller,
/// sends them to the write queue, and steals new work via [`TaskQueue`].
#[derive(Debug)]
pub struct TokioExecutor<R, WE>
where
    R: Puller,
    WE: Send + Unpin + 'static,
{
    tx: MTx<mpmc::List<Event<R::Error, WE>>>,
    tx_push: MAsyncTx<mpsc::Array<(WorkerId, ProgressEntry, Bytes)>>,
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
        let mut puller = self.puller.clone();
        let min_chunk_size = self.min_chunk_size;
        let pull_timeout = self.pull_timeout;
        let cfg_retry_gap = self.retry_gap;
        let max_speculative = self.max_speculative;
        let tx = self.tx.clone();
        let tx_push = self.tx_push.clone();
        let notify = Arc::new(Notify::new());
        let notify_clone = notify.clone();
        tokio::spawn(async move {
            'task: loop {
                let mut start = task.start();
                if start >= task.end() {
                    if task_queue.steal(&id, &mut task, min_chunk_size, max_speculative) {
                        tokio::select! {
                            biased;
                            () = notify.notified() => {}
                            () = async {} => {}
                        }
                        continue 'task;
                    }
                    break;
                }
                let _ = tx.send(Event::Pulling(id));
                let download_range = start..task.end();
                let mut stream = loop {
                    let t = tokio::select! {
                        () = notify.notified() => break 'task,
                        t = puller.pull(Some(&download_range)) => t
                    };
                    match t {
                        Ok(t) => break t,
                        Err((e, retry_gap)) => {
                            let _ = tx.send(Event::PullError(id, e));
                            tokio::select! {
                                () = notify.notified() => break 'task,
                                () = tokio::time::sleep(retry_gap.unwrap_or(cfg_retry_gap)) => {}
                            };
                        }
                    }
                };
                tokio::pin! {
                    let sleep = tokio::time::sleep(pull_timeout);
                }
                loop {
                    sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + pull_timeout);
                    let t = tokio::select! {
                        () = notify.notified() => break 'task,
                        () = &mut sleep => {
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
                                () = notify.notified() => break 'task,
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
        TokioHandle {
            id,
            notify: notify_clone,
        }
    }
}

#[cfg(test)]
#[cfg(feature = "mem")]
mod tests {
    use vec::Vec;

    use super::*;
    use crate::BufWriterPusher;
    use crate::{
        Merge, ProgressEntry,
        mem::MemPusher,
        mock::{MockPuller, build_mock_data},
    };
    use futures::stream;
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
        while let Ok(e) = result.event_chain.recv().await {
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

        #[allow(clippy::unwrap_used)]
        result.join().await.unwrap();
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

        // Abort immediately. The push driver must observe the shared flag, break
        // out WITHOUT flushing, and let `join()` return promptly.
        result.abort();
        assert!(result.is_aborted());

        let joined = tokio::time::timeout(Duration::from_secs(10), result.join())
            .await
            .expect("join() hung after abort");
        assert!(joined.is_ok());

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
    // `flush()` (or overflow). Wrapping `MemPusher` with capacity > source lets
    // us prove that, on abort, the *un-flushed* buffer is discarded — i.e. the
    // download stops strictly before the full source is written (the bare
    // `MemPusher` test's `written <= source` would also pass if abort missed).
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
        // Capacity > source: nothing reaches `MemPusher` until `flush()`.
        let inner = MemPusher::with_capacity(mock_data.len());
        let receive = inner.receive.clone();
        let pusher = BufWriterPusher::new(inner, mock_data.len() + 1);
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
        while let Ok(e) = result.event_chain.recv().await {
            if matches!(e, Event::Pushing(_, _)) {
                result.abort();
                assert!(result.is_aborted());
                aborted = true;
                break;
            }
        }
        assert!(aborted, "expected a Pushing event before aborting");

        let joined = timeout(Duration::from_secs(10), result.join())
            .await
            .expect("join() hung after abort");
        assert!(joined.is_ok());

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
}
