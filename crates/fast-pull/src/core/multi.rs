use crate::{DownloadResult, Event, ProgressEntry, Puller, PullerError, Pusher, WorkerId};
use bytes::Bytes;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use crossfire::{MAsyncTx, MTx, mpmc, mpsc};
use fast_steal::{Executor, Handle, Task, TaskQueue};
use futures::TryStreamExt;
use std::sync::Arc;
use tokio::sync::Notify;

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
/// 当设置线程数，但 executor 意外为空时，panic
pub fn download_multi<R: Puller, W: Pusher, I: Iterator<Item = ProgressEntry>>(
    puller: R,
    mut pusher: W,
    options: DownloadOptions<I>,
) -> DownloadResult<TokioExecutor<R, W::Error>, R::Error, W::Error> {
    let (tx, event_chain) = mpmc::unbounded_async();
    let (tx_push, rx_push) = mpsc::bounded_async(options.push_queue_cap);
    let tx_clone = tx.clone();
    let rx_push = rx_push.into_blocking();
    let push_handle = tokio::task::spawn_blocking(move || {
        while let Ok((id, spin, mut data)) = rx_push.recv() {
            loop {
                match pusher.push(&spin, data) {
                    Ok(()) => break,
                    Err((err, bytes)) => {
                        data = bytes;
                        let _ = tx_clone.send(Event::PushError(id, err));
                    }
                }
                std::thread::sleep(options.retry_gap);
            }
            let _ = tx_clone.send(Event::PushProgress(id, spin));
        }
        loop {
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
    #[allow(clippy::unwrap_used)]
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
    )
}

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
                    if task_queue.steal(&mut task, min_chunk_size, max_speculative) {
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
                loop {
                    let t = tokio::select! {
                        () = notify.notified() => break 'task,
                        t = tokio::time::timeout(pull_timeout, stream.try_next()) => t
                    };
                    match t {
                        Ok(Ok(Some(mut chunk))) => {
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
                        Ok(Ok(None)) => continue 'task,
                        Ok(Err((e, retry_gap))) => {
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
                        Err(_) => {
                            let _ = tx.send(Event::PullTimeout(id));
                            drop(stream);
                            puller = puller.clone();
                            continue 'task;
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
    use crate::{
        Merge, ProgressEntry,
        mem::MemPusher,
        mock::{MockPuller, build_mock_data},
    };
    use std::{dbg, vec};

    #[tokio::test(flavor = "multi_thread")]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher.clone(),
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
        let mut push_ids = [false; 32];
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::PullProgress(id, p) => {
                    pull_ids[id] = true;
                    pull_progress.merge_progress(p);
                }
                Event::PushProgress(id, p) => {
                    push_ids[id] = true;
                    push_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        dbg!(&pull_progress);
        dbg!(&push_progress);
        assert_eq!(pull_progress, download_chunks);
        assert_eq!(push_progress, download_chunks);
        assert!(pull_ids.iter().any(|x| *x));
        assert!(push_ids.iter().any(|x| *x));

        #[allow(clippy::unwrap_used)]
        result.join().await.unwrap();
        assert_eq!(&**pusher.receive.lock(), mock_data);
    }
}
