extern crate std;
use crate::{DownloadResult, Event, Puller, PullerError, Pusher, multi::TokioExecutor};
use core::time::Duration;
use crossfire::{mpmc, spsc};
use futures::TryStreamExt;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
}

pub fn download_single<R: Puller, W: Pusher>(
    mut puller: R,
    mut pusher: W,
    options: DownloadOptions,
) -> DownloadResult<TokioExecutor<R, W::Error>, R::Error, W::Error> {
    let (tx, event_chain) = mpmc::unbounded_async();
    let (tx_push, rx_push) = spsc::bounded_async(options.push_queue_cap);
    let tx_clone = tx.clone();
    const ID: usize = 0;
    let rx_push = rx_push.into_blocking();
    let push_handle = tokio::task::spawn_blocking(move || {
        while let Ok((spin, mut data)) = rx_push.recv() {
            loop {
                match pusher.push(&spin, data) {
                    Ok(_) => break,
                    Err((err, bytes)) => {
                        data = bytes;
                        let _ = tx_clone.send(Event::PushError(ID, err));
                    }
                }
                std::thread::sleep(options.retry_gap);
            }
            let _ = tx_clone.send(Event::PushProgress(ID, spin));
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
    let handle = tokio::spawn(async move {
        'redownload: loop {
            let _ = tx.send(Event::Pulling(ID));
            let mut downloaded: u64 = 0;
            let mut stream = loop {
                match puller.pull(None).await {
                    Ok(t) => break t,
                    Err((e, retry_gap)) => {
                        let _ = tx.send(Event::PullError(ID, e));
                        tokio::time::sleep(retry_gap.unwrap_or(options.retry_gap)).await;
                    }
                }
            };
            loop {
                match stream.try_next().await {
                    Ok(Some(chunk)) => {
                        let len = chunk.len() as u64;
                        let span = downloaded..(downloaded + len);
                        let _ = tx.send(Event::PullProgress(ID, span.clone()));
                        tx_push.send((span, chunk)).await.unwrap();
                        downloaded += len;
                    }
                    Ok(None) => break 'redownload,
                    Err((e, retry_gap)) => {
                        let is_irrecoverable = e.is_irrecoverable();
                        let _ = tx.send(Event::PullError(ID, e));
                        tokio::time::sleep(retry_gap.unwrap_or(options.retry_gap)).await;
                        if is_irrecoverable {
                            continue 'redownload;
                        }
                    }
                }
            }
        }
        let _ = tx.send(Event::Finished(ID));
    });
    DownloadResult::new(
        event_chain,
        push_handle,
        Some(&[handle.abort_handle()]),
        None,
    )
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{
        Merge, ProgressEntry,
        mem::MemPusher,
        mock::{MockPuller, build_mock_data},
    };
    use std::{dbg, vec};
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
        );

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
