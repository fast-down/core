extern crate alloc;
use super::macros::poll_ok;
use crate::{DownloadResult, Event, ProgressEntry, SeqPuller, SeqPusher};
use bytes::Bytes;
use core::time::Duration;
use futures::TryStreamExt;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
}

pub async fn download_single<R, W>(
    mut puller: R,
    mut pusher: W,
    options: DownloadOptions,
) -> DownloadResult<R::Error, W::Error>
where
    R: SeqPuller + 'static,
    W: SeqPusher + 'static,
{
    let (tx, event_chain) = kanal::unbounded_async();
    let (tx_push, rx_push) = kanal::bounded_async::<(ProgressEntry, Bytes)>(options.push_queue_cap);
    let tx_clone = tx.clone();
    const ID: usize = 0;
    let push_handle = tokio::spawn(async move {
        while let Ok((spin, data)) = rx_push.recv().await {
            poll_ok!(
                pusher.push(&data).await,
                ID @ tx_clone => PushError,
                options.retry_gap
            );
            tx_clone.send(Event::PushProgress(ID, spin)).await.unwrap();
        }
        poll_ok!(
            pusher.flush().await,
            tx_clone => FlushError,
            options.retry_gap
        );
    });
    let handle = tokio::spawn(async move {
        tx.send(Event::Pulling(ID)).await.unwrap();
        let mut downloaded: u64 = 0;
        let mut stream = puller.pull();
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => {
                    let len = chunk.len() as u64;
                    let span = downloaded..(downloaded + len);
                    tx.send(Event::PullProgress(ID, span.clone()))
                        .await
                        .unwrap();
                    tx_push.send((span, chunk)).await.unwrap();
                    downloaded += len;
                }
                Ok(None) => break,
                Err(e) => {
                    tx.send(Event::PullError(ID, e)).await.unwrap();
                    tokio::time::sleep(options.retry_gap).await;
                }
            }
        }
        tx.send(Event::Finished(ID)).await.unwrap();
    });
    DownloadResult::new(event_chain, push_handle, &[handle.abort_handle()])
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{
        MergeProgress,
        mem::MemPusher,
        mock::{MockPuller, build_mock_data},
    };
    use alloc::vec;
    use std::dbg;
    use vec::Vec;

    #[tokio::test]
    async fn test_sequential_download() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_single(
            puller,
            pusher.clone(),
            DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
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
        assert_eq!(&**pusher.receive.lock(), mock_data);
    }
}
