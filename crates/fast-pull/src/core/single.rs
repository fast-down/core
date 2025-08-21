extern crate alloc;

use super::macros::poll_ok;
use crate::{DownloadResult, Event, ReadStream, SliceOrBytes, WriteStream};
use actix_rt::Arbiter;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::ops::ControlFlow;
use core::time::Duration;
use kanal::AsyncSender;
use tokio::sync::Barrier;

#[derive(Debug, Clone)]
pub struct TransferOptions {
    pub retry_gap: Duration,
}

pub(crate) async fn forward<R, W>(
    mut puller: R,
    mut pusher: W,
    id: usize,
    options: TransferOptions,
    tx: AsyncSender<
        Event<core::convert::Infallible, core::convert::Infallible, R::Error, W::Error>,
    >,
) where
    R: ReadStream + Send,
    W: WriteStream + Send,
{
    let mut downloaded: u64 = 0;
    loop {
        if let ControlFlow::Break(()) = poll_ok!(puller.read_with(async |chunk: SliceOrBytes| {
            let len = chunk.len() as u64;
            if len == 0 {
                return ControlFlow::Break(());
            }
            let span = downloaded..(downloaded + len);
            tx.send(Event::PullProgress(id, span.clone()))
                .await
                .unwrap();
            poll_ok!(
              pusher.write(chunk.clone()).await,
              id @ tx => PushStreamError,
              options.retry_gap
            );
            tx.send(Event::PushProgress(id, span.clone()))
                .await
                .unwrap();
            downloaded += len;
            ControlFlow::Continue(())
        }).await, id @ tx => PullStreamError, options.retry_gap)
        {
            break;
        }
    }
}

pub(crate) mod internal {
    use futures::future::{BoxFuture, LocalBoxFuture};
    use futures::FutureExt;

    pub unsafe fn boxed_expanded<T>(fut: impl Future<Output = T> + Send) -> BoxFuture<'static, T> {
        unsafe { core::mem::transmute(fut.boxed()) }
    }

    pub unsafe fn boxed_local_expanded<T>(fut: impl Future<Output = T>) -> LocalBoxFuture<'static, T> {
        unsafe { core::mem::transmute(fut.boxed_local()) }
    }
}

pub async fn transfer<'a, 'b, R: 'a, W: 'b + 'a>(
    puller: R,
    pusher: W,
    options: TransferOptions,
) -> DownloadResult<'a, 'b, core::convert::Infallible, core::convert::Infallible, R::Error, W::Error>
where
    R: ReadStream + Send,
    R::Error: 'static,
    W: WriteStream + Send,
    W::Error: 'static,
{
    let (tx, event_chain) = kanal::unbounded_async();
    let barrier = Arc::new(Barrier::new(1 + 1));
    let join_handle = barrier.clone();
    const ID: usize = 0;
    let arbiter = Arbiter::new();
    arbiter.spawn(unsafe { internal::boxed_expanded(async move {
        actix_rt::spawn(internal::boxed_local_expanded(async move {
            forward(puller, pusher, ID, options, tx.clone()).await;
            tx.send(Event::Finished(ID)).await.unwrap();
            drop(tx);
            barrier.wait().await;
        }))
        .await
        .unwrap();
    }) });
    DownloadResult::new(
        event_chain,
        join_handle,
        Box::new(move || {
            arbiter.stop();
        }),
    )
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{
        MergeProgress, ProgressEntry,
        core::mock::{MockSeqPuller, MockSeqPusher, build_mock_data},
    };
    use alloc::vec;
    use std::dbg;
    use vec::Vec;

    #[actix_rt::test]
    async fn test_sequential_download() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockSeqPuller::new(mock_data.clone());
        let pusher = MockSeqPusher::new();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let mut result = transfer(
            puller,
            pusher.clone(),
            TransferOptions {
                retry_gap: Duration::from_secs(1),
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
        assert_eq!(pusher.into_vec(), mock_data);
    }
}
