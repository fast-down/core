extern crate alloc;
extern crate spin;
use super::macros::poll_ok;
use crate::{Event, ProgressEntry, PullResult, RandPuller, RandPusher, Total, WorkerId};
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use core::{num::NonZeroUsize, time::Duration};
use fast_steal::{SplitTask, StealTask, Task, TaskList};
use futures::TryStreamExt;

#[derive(Debug, Clone)]
pub struct PullOptions {
    pub download_chunks: Vec<ProgressEntry>,
    pub concurrent: NonZeroUsize,
    pub retry_gap: Duration,
    pub data_queue_cap: usize,
}

pub async fn pull<R, W>(
    puller: R,
    mut writer: W,
    options: PullOptions,
) -> PullResult<R::Error, W::Error>
where
    R: RandPuller + 'static,
    W: RandPusher + 'static,
{
    let (tx_event, event_chain) = kanal::unbounded_async();
    let (tx_data, rx_data) =
        kanal::bounded_async::<(WorkerId, ProgressEntry, Bytes)>(options.data_queue_cap);
    let tx_clone = tx_event.clone();
    let write_handle = tokio::spawn(async move {
        while let Ok((id, spin, data)) = rx_data.recv().await {
            poll_ok!(
                {},
                writer.push(spin.clone(), data.clone()).await,
                id @ tx_clone => WriteError,
                options.retry_gap
            );
            tx_clone.send(Event::WriteProgress(id, spin)).await.unwrap();
        }
        poll_ok!(
            {},
            writer.end().await,
            tx_clone => SealError,
            options.retry_gap
        );
    });
    let mutex = Arc::new(spin::mutex::SpinMutex::<_>::new(()));
    let task_list = Arc::new(TaskList::from(&options.download_chunks[..]));
    let tasks = Arc::from_iter(
        Task::from(&*task_list)
            .split_task(options.concurrent.get() as u64)
            .map(Arc::new),
    );
    let mut abort_handles = Vec::with_capacity(tasks.len() + 1);
    for (id, task) in tasks.iter().enumerate() {
        let current_task = task.clone();
        let tasks = tasks.clone();
        let task_data = task_list.clone();
        let mutex = mutex.clone();
        let tx_event = tx_event.clone();
        let tx_data = tx_data.clone();
        let mut puller = puller.clone();
        let handle = tokio::spawn(async move {
            'steal_task: loop {
                let mut start = current_task.start();
                if start >= current_task.end() {
                    let guard = mutex.lock();
                    if current_task.steal_from(&tasks, 16 * 1024) {
                        continue;
                    }
                    drop(guard);
                    tx_event.send(Event::Finished(id)).await.unwrap();
                    return;
                }
                let pull_ranges = task_data.get_range(start..current_task.end());
                for range in &pull_ranges {
                    tx_event.send(Event::Reading(id)).await.unwrap();
                    let mut stream = puller.pull(range);
                    let mut counter = 0;
                    loop {
                        match stream.try_next().await {
                            Ok(Some(mut chunk)) => {
                                let len = chunk.len() as u64;
                                current_task.fetch_add_start(len);
                                start += len;
                                counter += len;
                                let range_start = range.start + counter;
                                let range_end = range.start + counter;
                                let span =
                                    range_start..range_end.min(task_data.get(current_task.end()));
                                let len = span.total();
                                tx_event
                                    .send(Event::ReadProgress(id, span.clone()))
                                    .await
                                    .unwrap();
                                tx_data
                                    .send((id, span, chunk.split_to(len as usize)))
                                    .await
                                    .unwrap();
                                if start >= current_task.end() {
                                    continue 'steal_task;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                tx_event.send(Event::ReadError(id, e)).await.unwrap();
                                tokio::time::sleep(options.retry_gap).await;
                            }
                        }
                    }
                }
            }
        });
        abort_handles.push(handle.abort_handle());
    }
    abort_handles.push(write_handle.abort_handle());
    PullResult::new(event_chain, write_handle, &abort_handles)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{
        ProgressEntry, ProgressExt,
        core::utils::{FixedPusher, StaticPuller, build_mock_data},
    };
    use alloc::vec;
    use std::dbg;

    #[tokio::test]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(3 * 1024);
        let reader = StaticPuller::new(&mock_data);
        let writer = FixedPusher::new(mock_data.len());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = pull(
            reader,
            writer.clone(),
            PullOptions {
                concurrent: NonZeroUsize::new(32).unwrap(),
                retry_gap: Duration::from_secs(1),
                data_queue_cap: 1024,
                download_chunks: download_chunks.clone(),
            },
        )
        .await;

        let mut download_progress: Vec<ProgressEntry> = Vec::new();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::ReadProgress(_, p) => {
                    download_progress.merge_progress(p);
                }
                Event::WriteProgress(_, p) => {
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
        writer.assert_eq(&mock_data).await;
    }
}
