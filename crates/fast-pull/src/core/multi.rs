extern crate alloc;
use super::macros::poll_ok;
use crate::{DownloadResult, Event, ProgressEntry, RandPuller, RandPusher, Total, WorkerId};
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use core::{num::NonZeroUsize, time::Duration};
use fast_steal::{Executor, Handle, Task, TaskList};
use futures::TryStreamExt;
use tokio::task::AbortHandle;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub download_chunks: Vec<ProgressEntry>,
    pub concurrent: NonZeroUsize,
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
}

pub async fn download_multi<R, W>(
    puller: R,
    mut pusher: W,
    options: DownloadOptions,
) -> DownloadResult<R::Error, W::Error>
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
                {},
                pusher.push(spin.clone(), data.clone()).await,
                id @ tx_clone => PushError,
                options.retry_gap
            );
            tx_clone.send(Event::PushProgress(id, spin)).await.unwrap();
        }
        poll_ok!(
            {},
            pusher.flush().await,
            tx_clone => FlushError,
            options.retry_gap
        );
    });
    let executor: TokioExecutor<R, W> = TokioExecutor {
        tx,
        tx_push,
        puller,
        retry_gap: options.retry_gap,
    };
    let task_list = TaskList::run(
        options.concurrent.get(),
        8 * 1024,
        &options.download_chunks[..],
        executor,
    );
    DownloadResult::new(
        event_chain,
        push_handle,
        &task_list
            .handles()
            .iter()
            .map(|h| h.0.clone())
            .collect::<Arc<[_]>>(),
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
}
impl<R, W> Executor for TokioExecutor<R, W>
where
    R: RandPuller + 'static + Sync,
    W: RandPusher + 'static,
{
    type Handle = TokioHandle;
    fn execute(self: Arc<Self>, task: Arc<Task>, task_list: Arc<TaskList<Self>>) -> Self::Handle {
        let id = 1; // TODO: worker id
        let handle = tokio::spawn(async move {
            'steal_task: loop {
                let mut start = task.start();
                if start >= task.end() {
                    if task_list.steal(&task, 16 * 1024) {
                        continue;
                    }
                    break;
                }
                self.tx.send(Event::Pulling(id)).await.unwrap();
                let download_range = start..task.end();
                let mut puller = self.puller.clone();
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
                            let len = span.total() as usize;
                            self.tx
                                .send(Event::PullProgress(id, span.clone()))
                                .await
                                .unwrap();
                            self.tx_push
                                .send((id, span, chunk.split_to(len)))
                                .await
                                .unwrap();
                        }
                        Ok(None) => break,
                        Err(e) => {
                            self.tx.send(Event::PullError(id, e)).await.unwrap();
                            tokio::time::sleep(self.retry_gap).await;
                        }
                    }
                }
            }
            self.tx.send(Event::Finished(id)).await.unwrap();
            task_list.remove(&task);
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
        core::mock::{MockRandPuller, MockRandPusher, build_mock_data},
    };
    use alloc::vec;
    use std::dbg;

    #[tokio::test]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockRandPuller::new(&mock_data);
        let pusher = MockRandPusher::new(&mock_data);
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher.clone(),
            DownloadOptions {
                concurrent: NonZeroUsize::new(32).unwrap(),
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.clone(),
            },
        )
        .await;

        let mut pull_progress: Vec<ProgressEntry> = Vec::new();
        let mut push_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::PullProgress(_, p) => {
                    pull_progress.merge_progress(p);
                }
                Event::PushProgress(_, p) => {
                    push_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        dbg!(&pull_progress);
        dbg!(&push_progress);
        assert_eq!(pull_progress, download_chunks);
        assert_eq!(push_progress, download_chunks);

        result.join().await.unwrap();
        pusher.assert().await;
    }
}
