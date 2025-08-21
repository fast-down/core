extern crate alloc;
use super::macros::poll_ok;
use crate::base::SliceOrBytes;
use crate::{DownloadResult, Event, ProgressEntry, Puller, Pusher, ReadStream, WriteStream};
use alloc::boxed::Box;
use alloc::{sync::Arc, vec::Vec};
use alloc::sync::Weak;
use core::ops::ControlFlow;
use core::{
    num::{NonZero, NonZeroU64, NonZeroUsize},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use actix_rt::Arbiter;
use fast_steal::{Executor, Handle, Task, TaskList};
use kanal::AsyncSender;
use tokio::sync::{oneshot, Barrier};
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub download_chunks: Vec<ProgressEntry>,
    pub concurrent: NonZeroUsize,
    pub retry_gap: Duration,
    pub min_chunk_size: NonZeroU64,
}

async fn forward<R, W, E1, E2>(
    mut puller: R,
    mut pusher: W,
    id: usize,
    task: &Task,
    mut range_start: u64,
    retry_gap: Duration,
    tx: AsyncSender<Event<E1, E2, R::Error, W::Error>>
)
where
    R: ReadStream,
    W: WriteStream,
{
    loop {
        if let ControlFlow::Break(()) = poll_ok!(
            puller.read_with(async |chunk: SliceOrBytes| {
                let len = chunk.len() as u64;
                task.fetch_add_start(len);
                if len == 0 {
                    return ControlFlow::Break(())
                }
                let start = task.start();
                let range_end = start.min(task.end());
                if range_start >= range_end {
                    return ControlFlow::Break(());
                }
                let span = range_start..range_end;
                range_start += len;
                tx.send(Event::PullProgress(id, span.clone())).await.unwrap();
                poll_ok!(
                    pusher.write(chunk.clone()).await,
                    id @ tx => PushStreamError,
                    retry_gap
                );
                tx.send(Event::PushProgress(id, span)).await.unwrap();
                ControlFlow::Continue(())
            }).await,
            id @ tx => PullStreamError,
            retry_gap
        ) {
            break;
        }
    }
}

pub async fn download_multi<R, W>(
    puller: R,
    pusher: W,
    options: DownloadOptions,
) -> DownloadResult<R::Error, W::Error, R::StreamError, W::StreamError>
where
    R: Puller + 'static + Sync,
    W: Pusher + 'static + Clone + Sync,
{
    let (tx, event_chain) = kanal::unbounded_async();
    let barrier = Arc::new(Barrier::new(options.concurrent.get() + 1));
    let join_handle = barrier.clone();
    let arbiter = Arbiter::new();
    let handle = arbiter.handle();
    let executor: ArbiterExecutor<R, W> = ArbiterExecutor {
        tx,
        puller,
        pusher,
        barrier,
        retry_gap: options.retry_gap,
        id: Arc::new(AtomicUsize::new(0)),
        min_chunk_size: options.min_chunk_size,
        arbiter,
    };
    let task_list = TaskList::run(
        options.concurrent,
        options.min_chunk_size,
        &options.download_chunks[..],
        executor,
    );
    DownloadResult::new(
        event_chain,
        join_handle,
        Box::new(move || {
            let _ = task_list;
            handle.stop();
        }),
    )
}

#[derive(Clone)]
pub struct OneshotHandle(Arc<spin::Mutex<Option<oneshot::Receiver<JoinHandle<()>>>>>);

impl OneshotHandle {
    fn new(rx: oneshot::Receiver<JoinHandle<()>>) -> Self {
        Self(Arc::new(spin::Mutex::new(Some(rx))))
    }
}

impl Handle for OneshotHandle {
    type Output = ();

    fn abort(&mut self) -> Self::Output {
        let mut guard = self.0.lock();
        if let Some(receiver) = guard.take() {
            receiver.blocking_recv().unwrap().abort();
        }
    }
}
pub struct ArbiterExecutor<R, W>
where
    R: Puller + 'static,
    W: Pusher + 'static + Clone,
{
    tx: AsyncSender<Event<R::Error, W::Error, R::StreamError, W::StreamError>>,
    puller: R,
    pusher: W,
    retry_gap: Duration,
    id: Arc<AtomicUsize>,
    min_chunk_size: NonZeroU64,
    barrier: Arc<Barrier>,
    arbiter: Arbiter
}
impl<R, W> Executor for ArbiterExecutor<R, W>
where
    R: Puller + 'static + Sync,
    W: Pusher + 'static + Clone + Sync,
{
    type Handle = OneshotHandle;
    fn execute(self: Arc<Self>, task: Arc<Task>, task_list: Arc<TaskList<Self>>) -> Self::Handle {
        let id = self.id.fetch_add(1, Ordering::SeqCst);

        let (tx_handle, rx_handle) = oneshot::channel();
        let this = self.clone();
        this.arbiter.spawn(async move {
            let _ = tx_handle.send(actix_rt::spawn(async move {
                let barrier = self.barrier.clone();
                {
                    loop {
                        let start = task.start();
                        if start >= task.end() {
                            if task_list.steal(&task, NonZero::new(2 * self.min_chunk_size.get()).unwrap())
                            {
                                continue;
                            }
                            break;
                        }
                        self.tx.send(Event::Pulling(id)).await.unwrap();
                        let download_range = start..task.end();
                        let stream = poll_ok!(
                            self.puller.init_read(Some(&download_range)).await,
                            id @ self.tx => PullError,
                            self.retry_gap
                        );
                        let writer = poll_ok!(
                            self.pusher.init_write(start).await,
                            id @ self.tx => PushError,
                            self.retry_gap
                        );
                        forward(
                            stream,
                            writer,
                            id,
                            &task,
                            start,
                            self.retry_gap,
                            self.tx.clone()
                        ).await;
                    }
                    self.tx.send(Event::Finished(id)).await.unwrap();
                    task_list.remove(&task);
                    drop(task_list);
                    drop(self);
                }
               barrier.wait().await;
            }));
        });
        drop(this);
        OneshotHandle::new(rx_handle)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{
        core::mock::{build_mock_data, MockRandPuller, MockRandPusher}, MergeProgress,
        ProgressEntry,
    };
    use alloc::vec;
    use std::dbg;

    #[actix_rt::test]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockRandPuller::new(&mock_data);
        let pusher = MockRandPusher::new(mock_data.len());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let mut result = download_multi(
            puller,
            pusher.clone(),
            DownloadOptions {
                concurrent: NonZero::new(32).unwrap(),
                retry_gap: Duration::from_secs(1),
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
        assert_eq!(pusher.into_vec(), mock_data);
    }
}
