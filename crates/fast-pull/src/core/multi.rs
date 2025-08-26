extern crate alloc;
use super::macros::poll_ok;
use crate::{DownloadResult, Event, ProgressEntry, RandPuller, RandPusher, Total, WorkerId};
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use core::{
    num::{NonZero, NonZeroU64, NonZeroUsize},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use fast_steal::{Executor, Handle, Task, TaskList};
use futures::TryStreamExt;
use tokio::task::AbortHandle;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub download_chunks: Vec<ProgressEntry>,
    pub concurrent: NonZeroUsize,
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
    pub min_chunk_size: NonZeroU64,
}

pub async fn download_multi<R, W>(
    puller: R,
    mut pusher: W,
    options: DownloadOptions,
) -> DownloadResult<TokioExecutor<R, W>, R::Error, W::Error>
where
    R: RandPuller + 'static + Sync,
    W: RandPusher + 'static,
{
    let (tx, event_chain) = kanal::unbounded_async();
    let (tx_push, rx_push) =
        kanal::bounded_async::<(WorkerId, ProgressEntry, Bytes)>(options.push_queue_cap);
    let tx_clone = tx.clone();
    let push_handle = tokio::spawn(async move {
        while let Ok((id, spin, data)) = rx_push.recv().await {
            poll_ok!(
                pusher.push(spin.clone(), &data).await,
                id @ tx_clone => PushError,
                options.retry_gap
            );
            tx_clone.send(Event::PushProgress(id, spin)).await.unwrap();
        }
        poll_ok!(
            pusher.flush().await,
            tx_clone => FlushError,
            options.retry_gap
        );
    });
    let executor: Arc<TokioExecutor<R, W>> = Arc::new(TokioExecutor {
        tx,
        tx_push,
        puller,
        retry_gap: options.retry_gap,
        id: Arc::new(AtomicUsize::new(0)),
        min_chunk_size: options.min_chunk_size,
    });
    let task_list = Arc::new(TaskList::run(&options.download_chunks[..], executor));
    task_list
        .clone()
        .set_threads(options.concurrent, options.min_chunk_size);
    DownloadResult::new(
        event_chain,
        push_handle,
        &task_list.handles(|iter| iter.map(|h| h.0.clone()).collect::<Arc<[_]>>()),
        Some(Arc::downgrade(&task_list)),
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
pub struct TokioExecutor<R, W>
where
    R: RandPuller + 'static,
    W: RandPusher + 'static,
{
    tx: kanal::AsyncSender<Event<R::Error, W::Error>>,
    tx_push: kanal::AsyncSender<(WorkerId, ProgressEntry, Bytes)>,
    puller: R,
    retry_gap: Duration,
    id: Arc<AtomicUsize>,
    min_chunk_size: NonZeroU64,
}
impl<R, W> Executor for TokioExecutor<R, W>
where
    R: RandPuller + 'static + Sync,
    W: RandPusher + 'static,
{
    type Handle = TokioHandle;
    fn execute(self: Arc<Self>, task: Arc<Task>, task_list: Arc<TaskList<Self>>) -> Self::Handle {
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        let mut puller = self.puller.clone();
        let handle = tokio::spawn(async move {
            'steal_task: loop {
                let mut start = task.start();
                if start >= task.end() {
                    if task_list.steal(&task, NonZero::new(2 * self.min_chunk_size.get()).unwrap())
                    {
                        continue;
                    } else {
                        break;
                    }
                }
                self.tx.send(Event::Pulling(id)).await.unwrap();
                let download_range = start..task.end();
                let mut stream = puller.pull(&download_range);
                loop {
                    match stream.try_next().await {
                        Ok(Some(mut chunk)) => {
                            let len = chunk.len() as u64;
                            task.fetch_add_start(len);
                            let range_start = start;
                            start += len;
                            let range_end = start.min(task.end());
                            if range_start >= range_end {
                                continue 'steal_task;
                            }
                            let span = range_start..range_end;
                            chunk.truncate(span.total() as usize);
                            self.tx
                                .send(Event::PullProgress(id, span.clone()))
                                .await
                                .unwrap();
                            self.tx_push.send((id, span, chunk)).await.unwrap();
                        }
                        Ok(None) => break,
                        Err(e) => {
                            self.tx.send(Event::PullError(id, e)).await.unwrap();
                            tokio::time::sleep(self.retry_gap).await;
                        }
                    }
                }
            }
            task_list.remove(&task);
            self.tx.send(Event::Finished(id)).await.unwrap();
        });
        TokioHandle(handle.abort_handle())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{
        MergeProgress, ProgressEntry,
        mem::MemPusher,
        mock::{MockPuller, build_mock_data},
    };
    use alloc::vec;
    use std::dbg;

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
                concurrent: NonZero::new(32).unwrap(),
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.clone(),
                min_chunk_size: NonZero::new(1).unwrap(),
            },
        )
        .await;

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
