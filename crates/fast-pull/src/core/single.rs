extern crate alloc;
use super::macros::poll_ok;
use crate::{Event, ProgressEntry, PullResult, Puller, Pusher};
use bytes::Bytes;
use core::time::Duration;
use futures::TryStreamExt;

#[derive(Debug, Clone)]
pub struct PullOptions {
    pub retry_gap: Duration,
    pub data_queue_cap: usize,
}

pub async fn pull<R, W>(
    mut puller: R,
    mut pusher: W,
    options: PullOptions,
) -> PullResult<R::Error, W::Error>
where
    R: Puller + 'static,
    W: Pusher + 'static,
{
    let (tx_event, event_chain) = kanal::unbounded_async();
    let (tx_data, rx_data) = kanal::bounded_async::<(ProgressEntry, Bytes)>(options.data_queue_cap);
    let tx_clone = tx_event.clone();
    const ID: usize = 0;
    let write_handle = tokio::spawn(async move {
        while let Ok((spin, data)) = rx_data.recv().await {
            poll_ok!(
                {},
                pusher.push(data.clone()).await,
                ID @ tx_clone => WriteError,
                options.retry_gap
            );
            tx_clone.send(Event::WriteProgress(ID, spin)).await.unwrap();
        }
        poll_ok!(
            {},
            pusher.end().await,
            tx_clone => SealError,
            options.retry_gap
        );
    });
    let handle = tokio::spawn(async move {
        tx_event.send(Event::Reading(ID)).await.unwrap();
        let mut pos: u64 = 0;
        let mut stream = puller.pull();
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => {
                    let len = chunk.len() as u64;
                    let span = pos..(pos + len);
                    tx_event
                        .send(Event::ReadProgress(ID, span.clone()))
                        .await
                        .unwrap();
                    tx_data.send((span, chunk)).await.unwrap();
                    pos += len;
                }
                Ok(None) => break,
                Err(e) => {
                    tx_event.send(Event::ReadError(ID, e)).await.unwrap();
                    tokio::time::sleep(options.retry_gap).await;
                }
            }
        }
        tx_event.send(Event::Finished(ID)).await.unwrap();
    });
    let write_abort_handle = write_handle.abort_handle();
    PullResult::new(
        event_chain,
        write_handle,
        &[handle.abort_handle(), write_abort_handle],
    )
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{
        ProgressExt,
        core::utils::{FixedSeqPusher, StaticPuller, build_mock_data},
    };
    use alloc::vec;
    use std::dbg;
    use vec::Vec;

    #[tokio::test]
    async fn test_sequential_download() {
        let mock_data = build_mock_data(3 * 1024);
        let reader = StaticPuller::new(&mock_data);
        let writer = FixedSeqPusher::new();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = pull(
            reader,
            writer.clone(),
            PullOptions {
                retry_gap: Duration::from_secs(1),
                data_queue_cap: 1024,
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
