use super::macros::{check_running, poll_ok};
use crate::{DownloadResult, Event, ProgressEntry, SeqReader, SeqWriter};
use bytes::Bytes;
use core::{sync::atomic::AtomicBool, time::Duration};
use futures::TryStreamExt;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub retry_gap: Duration,
    pub write_queue_cap: usize,
}

pub async fn download_single<R, W>(
    mut reader: R,
    mut writer: W,
    options: DownloadOptions,
) -> DownloadResult<R::Error, W::Error>
where
    R: SeqReader + 'static,
    W: SeqWriter + 'static,
{
    let (tx, event_chain) = kanal::unbounded_async();
    let (tx_write, rx_write) =
        kanal::bounded_async::<(ProgressEntry, Bytes)>(options.write_queue_cap);
    let tx_clone = tx.clone();
    const ID: usize = 0;
    let handle = tokio::spawn(async move {
        while let Ok((spin, data)) = rx_write.recv().await {
            poll_ok!(
                {},
                writer.write(data.clone()).await,
                ID @ tx_clone => WriteError,
                options.retry_gap
            );
            tx_clone.send(Event::WriteProgress(ID, spin)).await.unwrap();
        }
        poll_ok!(
            {},
            writer.flush().await,
            tx_clone => FlushError,
            options.retry_gap
        );
    });
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    tokio::spawn(async move {
        check_running!(ID, running, tx);
        tx.send(Event::Reading(ID)).await.unwrap();
        let mut downloaded: u64 = 0;
        let mut stream = reader.read();
        loop {
            check_running!(ID, running, tx);
            match stream.try_next().await {
                Ok(chunk) => match chunk {
                    Some(chunk) => {
                        let len = chunk.len() as u64;
                        let span = downloaded..(downloaded + len);
                        tx.send(Event::ReadProgress(ID, span.clone()))
                            .await
                            .unwrap();
                        tx_write.send((span, chunk)).await.unwrap();
                        downloaded += len;
                    }
                    None => break,
                },
                Err(e) => tx.send(Event::ReadError(ID, e)).await.unwrap(),
            }
        }
        tx.send(Event::Finished(ID)).await.unwrap();
    });
    DownloadResult::new(event_chain, handle, running_clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MergeProgress,
        core::mock::{MockSeqReader, MockSeqWriter, build_mock_data},
    };

    #[tokio::test]
    async fn test_sequential_pulling() {
        let mock_data = build_mock_data(3 * 1024);
        let reader = MockSeqReader::new(mock_data.clone());
        let writer = MockSeqWriter::new(mock_data.clone());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_single(
            reader,
            writer.clone(),
            DownloadOptions {
                retry_gap: Duration::from_secs(1),
                write_queue_cap: 1024,
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
        writer.assert().await;
    }
}
