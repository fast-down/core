use super::FetchResult;
use crate::Event;
use crate::ProgressEntry;
use crate::base::pusher::Pusher;
use crate::base::source::{Fetcher, Puller};
use crate::core::macros::{check_running, poll_ok};
use bytes::Bytes;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
}

pub async fn download_single<F, P>(
    fetcher: F,
    mut pusher: P,
    options: DownloadOptions,
) -> FetchResult<F::Error, <F::Puller as Puller>::Error, P::Error>
where
    F: Fetcher + Send + 'static,
    P: Pusher + Send + 'static,
{
    let (tx, event_chain) = async_channel::unbounded();
    let (tx_write, rx_write) =
        async_channel::bounded::<(ProgressEntry, Bytes)>(options.push_queue_cap);
    let tx_clone = tx.clone();
    const ID: usize = 0;
    let handle = tokio::spawn(async move {
        while let Ok((spin, data)) = rx_write.recv().await {
            poll_ok!(
                {},
                pusher.push(data.clone()).await,
                ID @ tx_clone => PushError,
                options.retry_gap
            );
            tx_clone.send(Event::PushProgress(0, spin)).await.unwrap();
        }
        poll_ok!(
            {},
            pusher.flush().await,
            tx_clone => FlushError,
            options.retry_gap
        );
    });
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let fetcher = fetcher.clone();
    tokio::spawn(async move {
        let mut downloaded: u64 = 0;
        let mut puller = poll_ok!(
            {
                check_running!(ID, running, tx);
                tx.send(Event::Fetching(ID)).await.unwrap();
            },
            fetcher.fetch(ID, None).await,
            ID @ tx => FetchError,
            options.retry_gap
        );
        tx.send(Event::Pulling(0)).await.unwrap();
        loop {
            let chunk = match poll_ok!(
                {
                    check_running!(ID, running, tx);
                }, puller.pull().await,
                ID @ tx => PullError,
                options.retry_gap
            ) {
                None => break,
                Some(chunk) => chunk,
            };
            let len = chunk.len() as u64;
            let span = downloaded..(downloaded + len);
            tx.send(Event::PullProgress(ID, span.clone()))
                .await
                .unwrap();
            tx_write.send((span, chunk)).await.unwrap();
            downloaded += len;
        }
        tx.send(Event::Finished(ID)).await.unwrap();
    });
    FetchResult::new(event_chain, handle, running_clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MergeProgress;
    use crate::core::mock::{MockFetcher, MockPusher, build_mock_data};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_sequential_pulling() {
        let mock_data = build_mock_data(3 * 1024);

        let result_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fetcher = MockFetcher(mock_data.clone());
        let pusher = MockPusher(result_buf.clone());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_single(
            fetcher,
            pusher,
            DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
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
