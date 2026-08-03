//! Single-threaded sequential download.

use crate::{
    DownloadResult, Event, ProgressEntry, Puller, PullerError, Pusher, multi::TokioExecutor,
};
use bytes::Bytes;
use core::time::Duration;
use crossfire::{mpmc, spsc};
use futures::TryStreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Options for a single-threaded download.
#[derive(Debug, Clone, Copy)]
pub struct DownloadOptions {
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
}

/// Start a single-threaded sequential download.
///
/// The puller fetches the entire file sequentially, chunk by chunk.
/// Supports retries and progress events via [`DownloadResult`].
/// # Completion
///
/// The download is finished once the push driver has drained `rx_push` and
/// flushed; the `event_chain` sender lives inside that driver, so it is dropped
/// when the driver returns and the receiver disconnects. Draining
/// `event_chain` is therefore the way to await completion. A panic in the
/// blocking push driver also drops the sender and ends the session, but is
/// otherwise swallowed. Normal cancellation (via
/// `abort()`) is observed: the pull task is force-aborted and the push driver
/// returns without flushing, so any buffered bytes are discarded and the file
/// is left incomplete. Completion is still detected by draining `event_chain`,
/// because the senders are dropped when both sides exit.
#[allow(clippy::too_many_lines)]
pub fn download_single<R: Puller, W: Pusher>(
    mut puller: R,
    mut pusher: W,
    options: DownloadOptions,
) -> DownloadResult<TokioExecutor<R, W::Error>, R::Error, W::Error> {
    const ID: usize = 0;
    let token = CancellationToken::new();
    let (tx, event_chain) = mpmc::unbounded_async();
    pusher.set_listener({
        let tx = tx.clone();
        Box::new(move |p| {
            let _ = tx.send(Event::PushProgress(p));
        })
    });

    let (tx_push, rx_push) =
        spsc::bounded_async_blocking::<(ProgressEntry, Bytes)>(options.push_queue_cap);
    let push_thread = Arc::new(std::sync::OnceLock::new());
    let push_handle = tokio::task::spawn_blocking({
        let push_thread = push_thread.clone();
        let token = token.clone();
        let tx = tx.clone();
        move || {
            let _ = push_thread.set(std::thread::current());
            while let Ok((mut spin, mut data)) = rx_push.recv() {
                loop {
                    if token.is_cancelled() {
                        return;
                    }
                    let _ = tx.send(Event::Pushing(ID, spin.clone()));
                    let len_before_push = data.len();
                    match pusher.push(&spin, data) {
                        Ok(()) => break,
                        Err((err, bytes)) => {
                            let _ = tx.send(Event::PushError(ID, spin.clone(), err));
                            let written = len_before_push.saturating_sub(bytes.len());
                            data = bytes;
                            spin.start += written as u64;
                        }
                    }
                    std::thread::park_timeout(options.retry_gap);
                }
            }
            loop {
                if token.is_cancelled() {
                    break;
                }
                let _ = tx.send(Event::Flushing);
                match pusher.flush() {
                    Ok(()) => break,
                    Err(err) => {
                        let _ = tx.send(Event::FlushError(err));
                    }
                }
                std::thread::park_timeout(options.retry_gap);
            }
        }
    });

    let pull_handle = tokio::spawn(async move {
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
                        let _ = tx_push.send((span, chunk)).await;
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

    tokio::spawn({
        let token = token.clone();
        async move {
            tokio::select! {
                _ = push_handle => {},
                () = token.cancelled() => {
                    pull_handle.abort();
                    if let Some(t) = push_thread.get() {
                        t.unpark();
                    }
                }
            }
        }
    });
    DownloadResult::new(event_chain, None, token)
}

#[cfg(test)]
#[cfg(feature = "mem")]
mod tests {
    #![allow(clippy::cast_possible_truncation)]
    use super::*;
    use crate::BufWriterPusher;
    use crate::{
        Merge, ProgressEntry,
        mem::MemPusher,
        mock::{MockPuller, build_mock_data},
    };
    use futures::stream;
    use std::{dbg, vec};
    use tokio::time::{sleep, timeout};
    use vec::Vec;

    #[tokio::test]
    async fn test_sequential_download() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        // Keep only the data handle for the final assertion; the whole `pusher`
        // (whose listener holds a clone of the `event_chain` sender) is moved into
        // the download, so `event_chain` closes once the push thread finishes,
        // terminating the single drain loop below.
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = [0..mock_data.len() as u64];
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
            },
        );

        let mut pull_progress: Vec<ProgressEntry> = Vec::new();
        let mut push_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain().recv().await {
            match e {
                Event::PullProgress(_, p) => pull_progress.merge_progress(p),
                Event::PushProgress(p) => push_progress.merge_progress(p),
                _ => {}
            }
        }
        dbg!(&pull_progress);
        dbg!(&push_progress);
        assert_eq!(pull_progress, download_chunks);
        assert_eq!(push_progress, download_chunks);

        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test]
    async fn test_sequential_download_abort_discards() {
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(mock_data.len());
        // Keep the data handle for the post-abort length assertion; the whole
        // `pusher` (whose listener holds a clone of the `event_chain` sender) is
        // moved into the download.
        let receive = pusher.receive.clone();
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
            },
        );

        // Abort immediately, before the pull/push machinery has a chance to
        // finish. The push driver must observe the shared flag, break out
        // WITHOUT flushing, and let `join()` return promptly.
        result.abort();
        assert!(result.is_aborted());

        // The event loop must end promptly and never hang.
        tokio::time::timeout(Duration::from_secs(10), async {
            while result.event_chain().recv().await.is_ok() {}
        })
        .await
        .expect("event loop hung after abort");

        // The pusher layer is unchanged; for `MemPusher` each `push` is committed
        // immediately, so abort simply stops further writes. The received bytes
        // are a (possibly empty) prefix of the source and never exceed it — i.e.
        // the download was cut short rather than completed.
        let written = receive.lock().len();
        assert!(
            written <= mock_data.len(),
            "abort must not write beyond the source"
        );
    }

    // -------------------------------------------------------------------------
    // Strengthened abort coverage.
    //
    // `BufWriterPusher` only forwards buffered bytes to its inner sink on
    // `flush()` (or overflow). Wrapping `MemPusher` with capacity > source lets
    // us prove that, on abort, the *un-flushed* buffer is discarded and nothing
    // reaches the inner sink — an invariant the bare `MemPusher` test above
    // cannot establish (for `MemPusher` every `push` is already committed).
    // -------------------------------------------------------------------------

    /// A [`Puller`] that stalls for `delay` before yielding any data, so a test
    /// can deterministically abort *mid-flight* — after the push driver has
    /// buffered bytes but before `pusher.flush()` would run.
    #[derive(Debug, Clone)]
    struct SlowMockPuller {
        data: Arc<[u8]>,
        delay: Duration,
    }
    impl Puller for SlowMockPuller {
        type Error = std::convert::Infallible;
        #[allow(clippy::cast_possible_truncation)]
        fn pull(
            &mut self,
            range: Option<&ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            type PullItem = Result<Bytes, (std::convert::Infallible, Option<Duration>)>;
            let owned: Vec<u8> = match range {
                Some(r) => self.data[r.start as usize..r.end as usize].to_vec(),
                None => self.data.to_vec(),
            };
            let delay = self.delay;
            async move {
                sleep(delay).await;
                let items: Vec<PullItem> = owned
                    .chunks(2)
                    .map(|c| Ok(Bytes::from(c.to_vec())))
                    .collect();
                Ok(stream::iter(items))
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sequential_download_abort_discards_buffered() {
        // 64 KiB source. The slow puller stalls before yielding any data so the
        // test can abort mid-flight (buffer filled, flush skipped).
        let mock_data = build_mock_data(64 * 1024);
        let puller = SlowMockPuller {
            data: Arc::from(mock_data.as_slice()),
            delay: Duration::from_millis(50),
        };
        // BufWriterPusher coalesces the contiguous single-stream writes; with
        // capacity > source nothing reaches `MemPusher` until `flush()`.
        let inner = MemPusher::with_capacity(mock_data.len());
        let receive = inner.receive.clone();
        let pusher = BufWriterPusher::new(inner, mock_data.len() + 1);
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
            },
        );

        // Abort as soon as the push driver starts processing (first `Pushing`
        // event). This lands reliably before completion because the puller is
        // slow.
        let mut aborted = false;
        while let Ok(e) = result.event_chain().recv().await {
            if matches!(e, Event::Pushing(_, _)) {
                result.abort();
                assert!(result.is_aborted());
                aborted = true;
                break;
            }
        }
        assert!(aborted, "expected a Pushing event before aborting");

        // The event loop must end promptly and never hang.
        timeout(Duration::from_secs(10), async {
            while result.event_chain().recv().await.is_ok() {}
        })
        .await
        .expect("event loop hung after abort");

        // The buffered (un-flushed) bytes must have been discarded: the inner
        // sink received nothing. This is the stronger invariant the bare
        // `MemPusher` test could not prove.
        let written = receive.lock().len();
        assert_eq!(
            written, 0,
            "abort must discard buffered bytes, not write them to the sink"
        );
    }

    #[cfg(all(feature = "mem", feature = "file"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_sequential_download_abort_discards_file() {
        use std::io::Read;
        // 64 KiB source; real file sink pre-sized by `StdFilePusher::new`, then
        // wrapped in a buffer. Abort must leave the file untouched (all zeros):
        // the buffered bytes are discarded and never written to disk.
        let mock_data = build_mock_data(64 * 1024);
        let puller = SlowMockPuller {
            data: Arc::from(mock_data.as_slice()),
            delay: Duration::from_millis(50),
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let file = tokio::fs::File::from(tmp.reopen().unwrap());
        let inner = crate::file::StdFilePusher::new(file, mock_data.len() as u64, false)
            .await
            .unwrap();
        let pusher = BufWriterPusher::new(inner, mock_data.len() + 1);
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
            },
        );

        let mut aborted = false;
        while let Ok(e) = result.event_chain().recv().await {
            if matches!(e, Event::Pushing(_, _)) {
                result.abort();
                assert!(result.is_aborted());
                aborted = true;
                break;
            }
        }
        assert!(aborted, "expected a Pushing event before aborting");

        timeout(Duration::from_secs(10), async {
            while result.event_chain().recv().await.is_ok() {}
        })
        .await
        .expect("event loop hung after abort");

        // No actual bytes were written: the file is still the zero-filled
        // pre-sized region. This proves the buffered data was discarded rather
        // than flushed to disk.
        let mut f = std::fs::File::open(&path).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len(), mock_data.len(), "file should remain pre-sized");
        assert!(
            buf.iter().all(|&b| b == 0),
            "abort must not write buffered bytes to the file"
        );
    }

    // -------------------------------------------------------------------------
    // Coverage for the push-error retry path (lines 54-59), flush-error retry path
    // (lines 69-73), pull-error path (lines 83-85), stream-error path (lines 99-105)
    // and the `SlowMockPuller` `Some` branch (line 247).
    // -------------------------------------------------------------------------

    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct FatalErr;
    impl std::fmt::Display for FatalErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("fatal")
        }
    }
    impl std::error::Error for FatalErr {}
    impl crate::PullerError for FatalErr {
        fn is_irrecoverable(&self) -> bool {
            true
        }
    }

    #[derive(Debug)]
    struct RecoverableErr;
    impl std::fmt::Display for RecoverableErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("recoverable")
        }
    }
    impl std::error::Error for RecoverableErr {}
    impl crate::PullerError for RecoverableErr {
        fn is_irrecoverable(&self) -> bool {
            false
        }
    }

    /// In-memory sink that can be told to fail the next `push` (lines 54-59) or
    /// `flush` (lines 69-73).
    struct FlakySink {
        fail_push: Arc<AtomicBool>,
        fail_flush: Arc<AtomicBool>,
        receive: Arc<Mutex<Vec<u8>>>,
        listener: Option<crate::ProgressListener>,
    }
    impl FlakySink {
        fn new() -> Self {
            Self {
                fail_push: Arc::new(AtomicBool::new(false)),
                fail_flush: Arc::new(AtomicBool::new(false)),
                receive: Arc::new(Mutex::new(Vec::new())),
                listener: None,
            }
        }
    }
    impl crate::Pusher for FlakySink {
        type Error = std::io::Error;
        fn set_listener(&mut self, cb: crate::ProgressListener) {
            self.listener = Some(cb);
        }
        fn push(
            &mut self,
            range: &crate::ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            if self.fail_push.swap(false, Ordering::SeqCst) {
                return Err((std::io::Error::other("push"), bytes));
            }
            let mut g = self.receive.lock();
            if range.start as usize == g.len() {
                g.extend_from_slice(&bytes);
            } else {
                if g.len() < range.end as usize {
                    g.resize(range.end as usize, 0);
                }
                g[range.start as usize..range.end as usize].copy_from_slice(&bytes);
            }
            drop(g);
            if let Some(l) = &mut self.listener {
                l(range.clone());
            }
            Ok(())
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            if self.fail_flush.swap(false, Ordering::SeqCst) {
                Err(std::io::Error::other("flush"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Debug, Clone)]
    struct PullErrOncePuller {
        data: Arc<[u8]>,
        failed: Arc<AtomicBool>,
    }
    impl crate::Puller for PullErrOncePuller {
        type Error = RecoverableErr;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            if !self.failed.swap(true, Ordering::SeqCst) {
                return std::future::ready(Err((RecoverableErr, Some(Duration::ZERO))));
            }
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let items: Vec<Result<Bytes, (RecoverableErr, Option<Duration>)>> = data
                .chunks(2)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    #[derive(Debug, Clone)]
    struct StreamErrOncePuller {
        data: Arc<[u8]>,
        failed: Arc<AtomicBool>,
    }
    impl crate::Puller for StreamErrOncePuller {
        type Error = FatalErr;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            if !self.failed.swap(true, Ordering::SeqCst) {
                let items: Vec<Result<Bytes, (FatalErr, Option<Duration>)>> =
                    vec![Err((FatalErr, Some(Duration::ZERO)))];
                return std::future::ready(Ok(stream::iter(items)));
            }
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let items: Vec<Result<Bytes, (FatalErr, Option<Duration>)>> = data
                .chunks(2)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    /// Like [`StreamErrOncePuller`] but yields a *recoverable* stream error first,
    /// so the `is_irrecoverable == false` fall-through (line 105) is exercised.
    #[derive(Debug, Clone)]
    struct RecoverableStreamErrOncePuller {
        data: Arc<[u8]>,
        failed: Arc<AtomicBool>,
    }
    impl crate::Puller for RecoverableStreamErrOncePuller {
        type Error = RecoverableErr;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            // The first `pull` yields one recoverable stream error *followed by* the
            // real data in the same stream. In `single` a recoverable stream error
            // falls through (line 105) and then keeps reading the same stream, so the
            // download still completes successfully.
            let first = !self.failed.swap(true, Ordering::SeqCst);
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let mut items: Vec<Result<Bytes, (RecoverableErr, Option<Duration>)>> = data
                .chunks(2)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            if first {
                let mut with_err = vec![Err((RecoverableErr, Some(Duration::ZERO)))];
                with_err.append(&mut items);
                return std::future::ready(Ok(stream::iter(with_err)));
            }
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    #[tokio::test]
    async fn test_single_push_error_retries() {
        // Lines 54-59: a failing inner push is retried after `park_timeout`.
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let sink = FlakySink::new();
        sink.fail_push.store(true, Ordering::SeqCst);
        let receive = sink.receive.clone();
        let result = download_single(
            puller,
            sink,
            DownloadOptions {
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
            },
        );
        while result.event_chain().recv().await.is_ok() {}
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test]
    async fn test_single_flush_error_retries() {
        // Lines 69-73: a failing inner flush is retried after `park_timeout`.
        let mock_data = build_mock_data(3 * 1024);
        let puller = MockPuller::new(&mock_data);
        let sink = FlakySink::new();
        sink.fail_flush.store(true, Ordering::SeqCst);
        let receive = sink.receive.clone();
        let result = download_single(
            puller,
            sink,
            DownloadOptions {
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
            },
        );
        while result.event_chain().recv().await.is_ok() {}
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test]
    async fn test_single_pull_error_retries() {
        // Lines 83-85: a `pull` error (recoverable) is retried.
        let mock_data = build_mock_data(3 * 1024);
        let puller = PullErrOncePuller {
            data: Arc::from(mock_data.as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
            },
        );
        while result.event_chain().recv().await.is_ok() {}
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test]
    async fn test_single_stream_error_irrecoverable_retries() {
        // Lines 99-105: a stream error whose `is_irrecoverable` is true triggers a
        // `continue 'redownload` and a re-pull, which then succeeds.
        let mock_data = build_mock_data(3 * 1024);
        let puller = StreamErrOncePuller {
            data: Arc::from(mock_data.as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
            },
        );
        while result.event_chain().recv().await.is_ok() {}
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test]
    async fn test_single_stream_error_recoverable_retries() {
        // Lines 99-105: a stream error whose `is_irrecoverable` is false does NOT
        // `continue 'redownload`; instead it falls through (line 105) and retries the
        // pull, which then succeeds.
        let mock_data = build_mock_data(3 * 1024);
        let puller = RecoverableStreamErrOncePuller {
            data: Arc::from(mock_data.as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
            },
        );
        while result.event_chain().recv().await.is_ok() {}
        assert_eq!(&**receive.lock(), mock_data);
    }

    #[tokio::test]
    async fn puller_and_error_coverage() {
        // Exercise `Display` for the test error types (lines 386-388, 400-403) and
        // both arms of each test puller's `match range` (lines 483, 511, 541, 550-551).
        assert_eq!(format!("{FatalErr}"), "fatal");
        assert_eq!(format!("{RecoverableErr}"), "recoverable");

        let mut pull_err = PullErrOncePuller {
            data: Arc::from(b"abcdef".as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let _ = pull_err.pull(Some(&(0..2u64))).await; // first call errors, sets `failed`
        let _ = pull_err.pull(Some(&(0..2u64))).await; // success path, `Some` arm
        let _ = pull_err.pull(None).await; // success path, `None` arm

        let mut stream_err = StreamErrOncePuller {
            data: Arc::from(b"abcdef".as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let _ = stream_err.pull(Some(&(0..2u64))).await;
        let _ = stream_err.pull(Some(&(0..2u64))).await; // success path, `Some` arm
        let _ = stream_err.pull(None).await; // success path, `None` arm

        let mut rec_stream_err = RecoverableStreamErrOncePuller {
            data: Arc::from(b"abcdef".as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let _ = rec_stream_err.pull(Some(&(0..2u64))).await; // first: `Some` arm + error branch
        let _ = rec_stream_err.pull(Some(&(0..2u64))).await; // not-first: `Some` arm
        let _ = rec_stream_err.pull(None).await; // `None` arm
    }

    #[test]
    fn flaky_sink_noncontiguous_write_rebuffers() {
        // Lines 446-449: a write whose start is not flush against the current end of
        // the sink takes the `else` branch (resize + scatter copy) instead of `extend`.
        let mut sink = FlakySink::new();
        sink.push(&(5..8u64), Bytes::from_static(b"xyz")).unwrap();
        assert_eq!(&**sink.receive.lock(), b"\0\0\0\0\0xyz");
    }

    #[tokio::test]
    async fn test_slow_mock_puller_some_range() {
        // Line 247: the `Some` branch of `SlowMockPuller::pull` (the sequential
        // download always passes `None`, so this branch is otherwise uncovered).
        let mut p = SlowMockPuller {
            data: Arc::from(b"hello world".as_slice()),
            delay: Duration::ZERO,
        };
        assert!(p.pull(Some(&(0..5))).await.is_ok());
    }

    #[tokio::test]
    async fn test_sequential_download_empty_file() {
        // 0-byte source: the pull stream yields `Ok(None)` immediately, the
        // `downloaded` counter stays 0, and the push worker must still flush and
        // exit without hanging or writing anything.
        let mock_data: Vec<u8> = Vec::new();
        let puller = MockPuller::new(&mock_data);
        let pusher = MemPusher::with_capacity(0);
        let receive = pusher.receive.clone();
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
            },
        );
        // Drain events so `event_chain` does not pin the task open.
        while result.event_chain().recv().await.is_ok() {}
        timeout(Duration::from_secs(10), async {
            while result.event_chain().recv().await.is_ok() {}
        })
        .await
        .expect("event loop hung on empty file");
        assert_eq!(receive.lock().len(), 0);
    }

    /// Like `PullErrOncePuller` but the first `pull` fails with `None` as the retry
    /// gap, exercising the `retry_gap.unwrap_or(options.retry_gap)` fallback.
    #[derive(Debug, Clone)]
    struct PullErrNoGapPuller {
        data: Arc<[u8]>,
        failed: Arc<AtomicBool>,
    }
    impl crate::Puller for PullErrNoGapPuller {
        type Error = RecoverableErr;
        fn pull(
            &mut self,
            range: Option<&crate::ProgressEntry>,
        ) -> impl Future<
            Output = crate::PullResult<impl crate::PullStream<Self::Error>, Self::Error>,
        > + Send {
            if !self.failed.swap(true, Ordering::SeqCst) {
                return std::future::ready(Err((RecoverableErr, None)));
            }
            let data = match range {
                Some(r) => &self.data[r.start as usize..r.end as usize],
                None => &self.data,
            };
            let items: Vec<Result<Bytes, (RecoverableErr, Option<Duration>)>> = data
                .chunks(2)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            std::future::ready(Ok(stream::iter(items)))
        }
    }

    #[tokio::test]
    async fn test_single_pull_error_without_retry_gap_uses_options_default() {
        // Lines 83-85: a `pull` error carrying `None` as the retry gap must fall
        // back to `options.retry_gap` rather than panicking or stalling.
        let mock_data = build_mock_data(3 * 1024);
        let puller = PullErrNoGapPuller {
            data: Arc::from(mock_data.as_slice()),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let pusher = MemPusher::with_capacity(mock_data.len());
        let receive = pusher.receive.clone();
        let result = download_single(
            puller,
            pusher,
            DownloadOptions {
                retry_gap: Duration::ZERO,
                push_queue_cap: 1024,
            },
        );
        while result.event_chain().recv().await.is_ok() {}
        assert_eq!(&**receive.lock(), mock_data);
    }
}
