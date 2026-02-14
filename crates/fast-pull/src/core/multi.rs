extern crate std;
use crate::{DownloadResult, Event, ProgressEntry, Puller, PullerError, Pusher, Total, WorkerId};
use bytes::Bytes;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use crossfire::{MAsyncTx, MTx, mpmc, mpsc};
use fast_steal::{Executor, Handle, Task, TaskQueue};
use futures::TryStreamExt;
use std::sync::Arc;
use tokio::task::AbortHandle;

#[derive(Debug, Clone)]
pub struct DownloadOptions<'a, I: Iterator<Item = &'a ProgressEntry>> {
    pub download_chunks: I,
    pub concurrent: usize,
    pub retry_gap: Duration,
    pub pull_timeout: Duration,
    pub push_queue_cap: usize,
    pub min_chunk_size: u64,
}

pub fn download_multi<'a, R: Puller, W: Pusher, I: Iterator<Item = &'a ProgressEntry>>(
    puller: R,
    mut pusher: W,
    options: DownloadOptions<'a, I>,
) -> DownloadResult<TokioExecutor<R, W::Error>, R::Error, W::Error> {
    let (tx, event_chain) = mpmc::unbounded_async();
    let (tx_push, rx_push) = mpsc::bounded_async(options.push_queue_cap);
    let tx_clone = tx.clone();
    let rx_push = rx_push.into_blocking();
    let push_handle = tokio::task::spawn_blocking(move || {
        while let Ok((id, spin, mut data)) = rx_push.recv() {
            loop {
                match pusher.push(&spin, data) {
                    Ok(_) => break,
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
                Ok(_) => break,
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
    });
    let task_queue = TaskQueue::new(options.download_chunks);
    task_queue.set_threads(
        options.concurrent,
        options.min_chunk_size,
        Some(executor.as_ref()),
    );
    DownloadResult::new(
        event_chain,
        push_handle,
        None,
        Some((Arc::downgrade(&executor), task_queue)),
    )
}

#[derive(Clone)]
pub struct TokioHandle(AbortHandle);
impl Handle for TokioHandle {
    type Output = ();
    fn abort(&mut self) -> Self::Output {
        self.0.abort();
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
}
impl<R, WE> Executor for TokioExecutor<R, WE>
where
    R: Puller,
    WE: Send + Unpin + 'static,
{
    type Handle = TokioHandle;
    fn execute(&self, task: Task, task_queue: TaskQueue<Self::Handle>) -> Self::Handle {
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        let mut puller = self.puller.clone();
        let min_chunk_size = self.min_chunk_size;
        let pull_timeout = self.pull_timeout;
        let cfg_retry_gap = self.retry_gap;
        let tx = self.tx.clone();
        let tx_push = self.tx_push.clone();
        let handle = tokio::spawn(async move {
            let _guard = WorkerGuard::new(id, &task_queue, &task, &tx);
            loop {
                let mut start = task.start();
                if start >= task.end() {
                    if task_queue.steal(&task, min_chunk_size) {
                        continue;
                    } else {
                        break;
                    }
                }
                let _ = tx.send(Event::Pulling(id));
                let download_range = start..task.end();
                let mut stream = loop {
                    match puller.pull(Some(&download_range)).await {
                        Ok(t) => break t,
                        Err((e, retry_gap)) => {
                            let _ = tx.send(Event::PullError(id, e));
                            tokio::time::sleep(retry_gap.unwrap_or(cfg_retry_gap)).await;
                        }
                    }
                };
                loop {
                    match tokio::time::timeout(pull_timeout, stream.try_next()).await {
                        Ok(Ok(Some(mut chunk))) => {
                            let len = chunk.len() as u64;
                            if task.fetch_add_start(len).is_err() {
                                break;
                            }
                            let range_start = start;
                            start += len;
                            let range_end = start.min(task.end());
                            if range_start >= range_end {
                                break;
                            }
                            let span = range_start..range_end;
                            chunk.truncate(span.total() as usize);
                            let _ = tx.send(Event::PullProgress(id, span.clone()));
                            let tx_push = tx_push.clone();
                            let _ = tokio::spawn(async move {
                                tx_push.send((id, span, chunk)).await.unwrap();
                            })
                            .await;
                        }
                        Ok(Ok(None)) => break,
                        Ok(Err((e, retry_gap))) => {
                            let is_irrecoverable = e.is_irrecoverable();
                            let _ = tx.send(Event::PullError(id, e));
                            tokio::time::sleep(retry_gap.unwrap_or(cfg_retry_gap)).await;
                            if is_irrecoverable {
                                break;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(Event::PullTimeout(id));
                            drop(stream);
                            puller = puller.clone();
                            break;
                        }
                    }
                }
            }
        });
        TokioHandle(handle.abort_handle())
    }
}

pub struct WorkerGuard<RE, WE>
where
    RE: PullerError,
    WE: Send + Unpin + 'static,
{
    pub task_queue: TaskQueue<TokioHandle>,
    pub task: Task,
    pub id: WorkerId,
    pub tx: crossfire::MTx<crossfire::mpmc::List<Event<RE, WE>>>,
}

impl<RE, WE> WorkerGuard<RE, WE>
where
    RE: PullerError,
    WE: Send + Unpin + 'static,
{
    pub fn new(
        id: WorkerId,
        task_queue: &TaskQueue<TokioHandle>,
        task: &Task,
        tx: &crossfire::MTx<crossfire::mpmc::List<Event<RE, WE>>>,
    ) -> Self {
        Self {
            task_queue: task_queue.clone(),
            task: task.clone(),
            id,
            tx: tx.clone(),
        }
    }
}

impl<RE, WE> Drop for WorkerGuard<RE, WE>
where
    RE: PullerError,
    WE: Send + Unpin + 'static,
{
    fn drop(&mut self) {
        self.task_queue.finish_work(&self.task);
        let _ = self.tx.send(Event::Finished(self.id));
    }
}

#[cfg(test)]
mod tests {
    use vec::Vec;

    use super::*;
    use crate::{
        Merge, ProgressEntry,
        mem::MemPusher,
        mock::{MockPuller, build_mock_data},
    };
    use std::{dbg, vec};

    #[tokio::test]
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
                download_chunks: download_chunks.iter(),
                pull_timeout: Duration::from_secs(5),
                min_chunk_size: 1,
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
        assert_eq!(pull_ids, [true; 32]);
        assert_eq!(push_ids, [true; 32]);

        result.join().await.unwrap();
        assert_eq!(&**pusher.receive.lock(), mock_data);
    }
}
