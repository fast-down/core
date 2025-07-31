use super::FetchResult;
use super::macros::{check_running, poll_ok};
use crate::base::pusher::RandomPusher;
use crate::base::source::Fetcher;
use crate::base::source::Puller;
use crate::{Event, ProgressEntry, Total, WorkerId};
use bytes::Bytes;
use fast_steal::{SplitTask, StealTask, Task, TaskList};
use std::num::NonZeroUsize;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub pull_chunks: Vec<ProgressEntry>,
    pub concurrent: NonZeroUsize,
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
}

pub async fn download_multi<F, P>(
    fetcher: F,
    mut pusher: P,
    options: DownloadOptions,
) -> FetchResult<F, F::Puller, P>
where
    F: Fetcher + Send + 'static,
    P: RandomPusher + Send + 'static,
{
    let (tx, event_chain) = async_channel::unbounded();
    let (tx_write, rx_write) =
        async_channel::bounded::<(WorkerId, ProgressEntry, Bytes)>(options.push_queue_cap);
    let tx_clone = tx.clone();
    let handle = tokio::spawn(async move {
        while let Ok((id, spin, data)) = rx_write.recv().await {
            poll_ok!(
                {},
                pusher.push_range(spin.clone(), data.clone()).await,
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
    let mutex = Arc::new(Mutex::new(()));
    let task_list = Arc::new(TaskList::from(options.pull_chunks));
    let tasks = Arc::new(
        Task::from(&*task_list)
            .split_task(options.concurrent.get() as u64)
            .map(Arc::new)
            .collect::<Vec<_>>(),
    );
    let running = Arc::new(AtomicBool::new(true));
    for (id, task) in tasks.iter().enumerate() {
        let task = task.clone();
        let tasks = tasks.clone();
        let task_list = task_list.clone();
        let mutex = mutex.clone();
        let tx = tx.clone();
        let running = running.clone();
        let fetcher = fetcher.clone();
        let tx_write = tx_write.clone();
        tokio::spawn(async move {
            'steal_task: loop {
                check_running!(id, running, tx);
                let mut start = task.start();
                if start >= task.end() {
                    let guard = mutex.lock().await;
                    if task.steal(&tasks, 2) {
                        continue;
                    }
                    drop(guard);
                    tx.send(Event::Finished(id)).await.unwrap();
                    return;
                }
                let download_range = &task_list.get_range(start..task.end());
                for range in download_range {
                    'retry: loop {
                        let mut puller = poll_ok!(
                            {
                                check_running!(id, running, tx);
                                tx.send(Event::Fetching(id)).await.unwrap();
                            },
                            fetcher.fetch(id, Some(range)).await,
                            id @ tx => FetchError,
                            options.retry_gap
                        );
                        tx.send(Event::Pulling(id)).await.unwrap();
                        let mut downloaded = 0;
                        loop {
                            let mut count = 0;
                            let mut chunk = match poll_ok!(
                                {
                                    check_running!(id, running, tx);
                                    count += 1;
                                    if count > 3 {
                                        continue 'retry;
                                    }
                                }, puller.pull().await,
                                id @ tx => PullError,
                                options.retry_gap
                            ) {
                                None => break,
                                Some(chunk) => chunk,
                            };
                            let len = chunk.len() as u64;
                            task.fetch_add_start(len);
                            start += len;
                            let range_start = range.start + downloaded;
                            downloaded += len;
                            let range_end = range.start + downloaded;
                            let span = range_start..range_end.min(task_list.get(task.end()));
                            let len = span.total();
                            tx.send(Event::PullProgress(id, span.clone()))
                                .await
                                .unwrap();
                            tx_write
                                .send((id, span, chunk.split_to(len as usize)))
                                .await
                                .unwrap();
                            if start >= task.end() {
                                continue 'steal_task;
                            }
                        }
                        break;
                    }
                }
            }
        });
    }
    FetchResult::new(event_chain, handle, running)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mock::{MockFetcher, MockPusher, build_mock_data};
    use crate::{MergeProgress, ProgressEntry};

    #[tokio::test]
    async fn test_concurrent_pulling() {
        let mock_data = build_mock_data(3 * 1024);

        let result_buf = Arc::new(std::sync::Mutex::new(vec![0u8; 3 * 1024]));
        let fetcher = MockFetcher(mock_data.clone());
        let pusher = MockPusher(result_buf.clone());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_multi(
            fetcher,
            pusher,
            DownloadOptions {
                concurrent: NonZeroUsize::new(32).unwrap(),
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                pull_chunks: download_chunks.clone(),
            },
        )
        .await;

        let mut download_progress: Vec<ProgressEntry> = Vec::new();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::PullProgress(_, p) => {
                    download_progress.merge_progress(p);
                }
                Event::PushProgress(_, p) => {
                    write_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        dbg!(&download_progress);
        dbg!(&write_progress);
        assert_eq!(download_progress, download_chunks);
        assert_eq!(write_progress, download_chunks);

        result.join().await.unwrap();

        assert_eq!(&*result_buf.lock().unwrap(), &mock_data);
    }
}
