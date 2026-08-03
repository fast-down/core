//! End-to-end integration tests for the resume (断点续传) feature of
//! `fast_down_api`.
//!
//! These tests stand up a minimal ranged HTTP server (built on `hyper`) and drive
//! the public `download`/`resume` functions through real, resumable
//! scenarios: a successful resume after a mid-download cancel, a remote file
//! change (both the `download` silent-fallback and `resume` error-reporting
//! branches), a missing `.fd` state file, a server that does not support range
//! requests, and the core "cancel keeps `.part`/`.fd` so it can be resumed"
//! contract.
#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::similar_names
)]

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fast_down_api::{
    Event, PartialConfig, Rx, StateError, WriteMethod, create_cancellation_token, create_channel,
    download, resume,
};
use futures::StreamExt;
use futures::stream::unfold;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED, LOCATION, RANGE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Total size of the served file (5 MiB). Large enough that a throttled download
/// cannot finish before our cancel fires, so the partial state is always real.
const FILE_SIZE: usize = 5 * 1024 * 1024;
/// Bytes served per throttled chunk, with a small sleep between chunks, so the
/// download is slow enough for the cancel to land in the middle.
const THROTTLE_CHUNK: usize = 64 * 1024;
const THROTTLE_MS: u64 = 150;

type RespBody = BoxBody<Bytes, Infallible>;

/// In-memory file served by [`TestServer`].
struct FileData {
    body: Vec<u8>,
    etag: String,
    last_modified: String,
    supports_range: bool,
}

#[derive(Clone)]
struct TestServer {
    data: Arc<RwLock<FileData>>,
}

impl TestServer {
    /// Bind to a random loopback port and start serving. Returns the base URL.
    async fn serve(&self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind server socket");
        let addr = listener.local_addr().expect("resolve local addr");
        let server = self.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept connection");
                let conn_server = server.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let s = conn_server.clone();
                        handle(s, req)
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// Swap the served content (and identity headers), e.g. to simulate the
    /// remote file having changed between an interrupted and a resumed download.
    async fn set_content(&self, body: Vec<u8>, etag: &str, last_modified: &str) {
        let mut data = self.data.write().await;
        data.body = body;
        data.etag = etag.to_string();
        data.last_modified = last_modified.to_string();
    }
}

/// The hyper request handler: serves the current [`FileData`], honouring range
/// requests (with throttling) when `supports_range` is set, and always answering
/// with `ETag`/`Last-Modified` so the prefetch can build a `FileId`.
async fn handle(
    server: TestServer,
    req: Request<Incoming>,
) -> Result<Response<RespBody>, Infallible> {
    // A redirect endpoint modelling the signed/CDN pattern: the caller's URL
    // 302-redirects to a transient final URL. Used by the rotated-URL resume test
    // to prove the `.fd` stores the initial URL and the download targets the final
    // one.
    if req.uri().path() == "/redirect" {
        return Ok(Response::builder()
            .status(StatusCode::FOUND)
            .header(LOCATION, "/real")
            .body(BoxBody::new(Empty::<Bytes>::new()))
            .expect("build 302 response"));
    }
    let data = server.data.read().await;
    let total = data.body.len();
    let supports_range = data.supports_range;
    let etag = data.etag.clone();
    let last_modified = data.last_modified.clone();
    let body = data.body.clone();
    drop(data);

    let range = req
        .headers()
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if supports_range
        && let Some(header) = range
        && let Some((start, end)) = parse_range(&header, total)
    {
        let chunk = body[start..end].to_vec();
        let end_inclusive = end - 1;
        let content_range = format!("bytes {start}-{end_inclusive}/{total}");
        return Ok(Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_RANGE, content_range)
            .header(ACCEPT_RANGES, "bytes")
            .header(CONTENT_LENGTH, chunk.len().to_string())
            .header(ETAG, etag.as_str())
            .header(LAST_MODIFIED, last_modified.as_str())
            .body(throttled_stream(chunk))
            .expect("build 206 response"));
    }

    // Fallback: full body, no `Accept-Ranges`, so a `Range` probe (used by the
    // prefetch to detect resumability) will not see a `Content-Range` header.
    // Throttled the same way as the ranged branch so a fresh (non-resumed)
    // download is still slow enough for a mid-flight cancel to land.
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_LENGTH, body.len().to_string())
        .header(ETAG, etag.as_str())
        .header(LAST_MODIFIED, last_modified.as_str())
        .body(throttled_stream(body))
        .expect("build 200 response"))
}

/// Parse a `bytes=START-END` (or `bytes=START-`) header into an exclusive
/// `[start, end)` range against `total` bytes.
fn parse_range(header: &str, total: usize) -> Option<(usize, usize)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    let (start_s, end_s) = spec.split_once('-')?;
    let start = if start_s.is_empty() {
        0
    } else {
        start_s.trim().parse().ok()?
    };
    let end = if end_s.is_empty() {
        total
    } else {
        end_s.trim().parse::<usize>().ok()?.saturating_add(1)
    };
    let end = end.min(total);
    if start >= end {
        return None;
    }
    Some((start, end))
}

/// Build a streaming body that emits `THROTTLE_CHUNK` slices of `data` with a
/// small `sleep` between them, keeping the connection open long enough for a
/// cancel to arrive mid-transfer.
fn throttled_stream(data: Vec<u8>) -> RespBody {
    let stream = unfold((0usize, data), |(pos, data)| async move {
        if pos >= data.len() {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(THROTTLE_MS)).await;
        let end = (pos + THROTTLE_CHUNK).min(data.len());
        Some((
            Ok::<_, Infallible>(Bytes::copy_from_slice(&data[pos..end])),
            (end, data),
        ))
    });
    let framed = stream.map(|result| result.map(Frame::data));
    BodyExt::boxed(StreamBody::new(framed))
}

/// Start a ranged server serving `body` under the given identity headers.
async fn start_server(
    body: Vec<u8>,
    etag: &str,
    last_modified: &str,
    supports_range: bool,
) -> (TestServer, String) {
    let server = TestServer {
        data: Arc::new(RwLock::new(FileData {
            body,
            etag: etag.to_string(),
            last_modified: last_modified.to_string(),
            supports_range,
        })),
    };
    let url = server.serve().await;
    (server, url)
}

/// A fresh, unique, empty temp directory per test (runs in parallel safe).
fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("fast_down_api_resume_{name}_{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Build a config that targets `<dir>/out.bin` and drives the genuinely
/// concurrent path (several `download_multi` workers racing on different
/// offsets). The server is throttled, so a cancel still lands predictably
/// mid-flight; with many workers the partial run leaves fragmented
/// (multi-range) progress, exercising the resume path on real concurrency
/// rather than the single-worker one.
fn make_config(save_dir: &Path) -> PartialConfig {
    make_config_with(save_dir, 32, 1024 * 1024)
}

/// Like [`make_config`] but with an explicit worker count and chunk size, so a
/// test can pin a specific layout (e.g. exactly 8 workers for a controlled
/// fragmented progress) rather than the default 32-worker run.
fn make_config_with(save_dir: &Path, threads: usize, min_chunk_size: u64) -> PartialConfig {
    PartialConfig {
        save_dir: Some(save_dir.to_path_buf()),
        filename: Some("out.bin".to_string()),
        parse_filename: Some(false),
        overwrite: Some(true),
        write_method: Some(WriteMethod::Mmap),
        min_chunk_size: Some(min_chunk_size),
        threads: Some(threads),
        // A server without range support turns off the mmap pusher and falls
        // back to the cached file pusher, whose default watermarks (16 MiB)
        // are larger than `FILE_SIZE`. Nothing would reach the leaf sink until
        // the final flush, and since only the leaf emits `PushProgress`, a test
        // watching that event could not cancel before the whole file was
        // already written. Writing through on every chunk keeps a mid-download
        // cancel observable. The mmap pusher ignores these three fields, so
        // range-capable tests are unaffected.
        cache_high_watermark: Some(1),
        cache_low_watermark: Some(0),
        write_buffer_size: Some(1),
        ..Default::default()
    }
}

/// Drain all events from `rx` until the task ends (channel closed).
async fn drain(rx: Rx) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(e) = rx.recv().await {
        events.push(e);
    }
    events
}

/// Bytes that must actually reach the sink before a partial download is
/// cancelled. Cancelling on `Event::Start` instead would land before any write,
/// leaving `downloaded_chunk` absent from the `.fd` — the subsequent `resume()`
/// would then silently re-download everything and exercise none of the resume
/// path. Two throttle chunks is a small fraction of `FILE_SIZE`, so the download
/// is still far from finishing when the cancel fires.
const CANCEL_AFTER_BYTES: u64 = (THROTTLE_CHUNK * 2) as u64;

/// Start a normal `download()` and cancel it once at least [`CANCEL_AFTER_BYTES`]
/// have been written, leaving a real partial `.part`/`.fd` on disk with non-empty
/// resume progress. Returns the collected events.
async fn partial_download_via_cancel(
    url: &str,
    save_dir: &Path,
    cancel: CancellationToken,
) -> Vec<Event> {
    partial_download_via_cancel_with(url, make_config(save_dir), cancel, CANCEL_AFTER_BYTES).await
}

/// [`partial_download_via_cancel`] with an explicit config and cancel threshold,
/// so a test can interrupt a concurrent run after enough workers have written to
/// leave fragmented (multi-range) progress behind.
async fn partial_download_via_cancel_with(
    url: &str,
    cfg: PartialConfig,
    cancel: CancellationToken,
    cancel_after: u64,
) -> Vec<Event> {
    let (tx, rx) = create_channel();
    download(Url::parse(url).expect("valid url"), cfg, tx, cancel.clone());

    let mut events = Vec::new();
    let mut started = false;
    let mut written = 0u64;
    let mut cancelled_at = None;
    while let Ok(e) = rx.recv().await {
        if matches!(e, Event::Start { .. }) {
            started = true;
        }
        if let Event::PushProgress(ref p) = e {
            written += p.end - p.start;
            if cancelled_at.is_none() && written >= cancel_after {
                cancelled_at = Some(written);
                cancel.cancel();
            }
        }
        events.push(e);
    }
    assert!(started, "expected Event::Start during the partial download");
    let Some(cancelled_at) = cancelled_at else {
        panic!("expected at least {cancel_after} bytes written before cancelling (got {written})")
    };
    // Without this the test degrades silently rather than failing: if the sink
    // buffers the whole file, the first and only `PushProgress` arrives from the
    // final flush, the cancel fires after the download already finished, and
    // every assertion below still passes while testing nothing.
    assert!(
        cancelled_at < FILE_SIZE as u64,
        "cancel must land mid-download, but all {cancelled_at} of {FILE_SIZE} bytes \
         had already reached the sink when it fired"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "a cancelled download must not rename the .part file"
    );
    events
}

fn original_bytes() -> Vec<u8> {
    vec![0xAA; FILE_SIZE]
}

fn new_bytes() -> Vec<u8> {
    vec![0xBB; FILE_SIZE]
}

/// Case 1 (+ Case 5 emphasis): cancel mid-download, then `resume()` finishes it.
#[tokio::test]
async fn test_resume_success() {
    let dir = temp_dir("resume_success");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(part.exists(), ".part must exist after cancel");
    assert!(fd.exists(), ".fd must exist after cancel");
    assert!(
        !final_path.exists(),
        "final file must NOT exist after a cancelled download"
    );

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    let (_, size) = events
        .iter()
        .find_map(|e| match e {
            Event::Resumed { progress, size, .. } => Some((progress, *size)),
            _ => None,
        })
        .expect("expected Event::Resumed");
    assert_eq!(
        size, FILE_SIZE as u64,
        "Event::Resumed.size must equal file size"
    );

    let renamed = events
        .iter()
        .find_map(|e| match e {
            Event::Renamed(p) => Some(p.clone()),
            _ => None,
        })
        .expect("expected Event::Renamed");
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "resume() must not emit a ResumeError on a valid state"
    );

    let got = tokio::fs::read(&renamed).await.expect("read final file");
    assert_eq!(got, original_bytes(), "resumed file content mismatch");
    assert!(
        !fd.exists(),
        ".fd must be removed after a successful resume (contract §1)"
    );
}

/// Case 2 (download branch): a stale `.fd` (remote file changed) makes
/// `download()` silently fall back to a full re-download of the NEW content.
#[tokio::test]
async fn test_file_changed_download_falls_back() {
    let dir = temp_dir("file_changed_download");
    let (server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;
    // The `.fd` now records the ORIGINAL etag/size.

    server.set_content(new_bytes(), "new", "LM-B").await;

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let dl_cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, dl_cancel);
    let events = drain(rx).await;

    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download() must complete (Renamed) after a silent fallback"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "download() must NOT emit a ResumeError"
    );

    let final_path = dir.join("out.bin");
    let got = tokio::fs::read(&final_path)
        .await
        .expect("final file should exist");
    assert_eq!(got, new_bytes(), "final file must be the NEW content");
    let fd = final_path.with_added_extension("fd");
    assert!(
        !fd.exists(),
        ".fd must be removed after a successful download (contract §1)"
    );
}

/// Case 2 (resume branch): a stale `.fd` (remote file changed) makes `resume()`
/// report `StateError::FileChanged` and keep the partial files untouched.
#[tokio::test]
async fn test_file_changed_resume_reports_error() {
    let dir = temp_dir("file_changed_resume");
    let (server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;
    server.set_content(new_bytes(), "new", "LM-B").await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    let err = events
        .iter()
        .find_map(|e| match e {
            Event::ResumeError(r) => Some(r),
            _ => None,
        })
        .expect("expected Event::ResumeError");
    assert!(
        matches!(err, StateError::FileChanged { .. }),
        "expected Event::ResumeError(StateError::FileChanged), got {err:?}"
    );

    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() must NOT rename on FileChanged"
    );

    assert!(part.exists(), ".part must be kept on resume error");
    assert!(fd.exists(), ".fd must be kept on resume error");
    assert!(
        !final_path.exists(),
        "final file must not exist on resume error"
    );
}

/// Case 3: `resume()` with no `.fd` state file reports `StateError::Open`.
#[tokio::test]
async fn test_resume_no_state_file() {
    let dir = temp_dir("resume_no_state_file");
    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    // `tmp_path` (.part) exists but the `.fd` state file is missing → `resume`
    // must report `NoStateFile` (the resume contract), not silently fall back.
    let _ = tokio::fs::remove_file(&fd).await;
    let _ = std::fs::File::create(&part).expect("create .part");

    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;
    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        cancel,
    );
    let events = drain(rx).await;

    let err = events
        .iter()
        .find_map(|e| match e {
            Event::ResumeError(r) => Some(r),
            _ => None,
        })
        .expect("expected Event::ResumeError");
    assert!(
        matches!(err, StateError::Open(_)),
        "expected Event::ResumeError(StateError::Open) for missing .fd, got {err:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() must NOT rename when there is no .fd"
    );
}

/// Case 4: `resume()` against a server that does not support range requests
/// reports `StateError::NotResumable`.
#[tokio::test]
async fn test_resume_not_resumable() {
    let dir = temp_dir("resume_not_resumable");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", false).await;
    // A cancelled download leaves a valid `.fd` + `.part` for the non-range
    // server; `resume()` must then report `NotResumable` (server can't range).
    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        cancel,
    );
    let events = drain(rx).await;

    let err = events
        .iter()
        .find_map(|e| match e {
            Event::ResumeError(r) => Some(r),
            _ => None,
        })
        .expect("expected Event::ResumeError");
    assert!(
        matches!(err, StateError::NotResumable(..)),
        "expected Event::ResumeError(StateError::NotResumable), got {err:?}"
    );
}

/// Case 5 (explicit): the previous round's fix — a cancel must preserve `.part`
/// and `.fd` (not delete them), and a subsequent `resume()` must finish cleanly.
#[tokio::test]
async fn test_cancel_keeps_part_and_fd_then_resume() {
    let dir = temp_dir("cancel_keeps_state");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(
        part.exists() && fd.exists(),
        "cancel must preserve both .part and .fd"
    );
    assert!(
        !final_path.exists(),
        "cancel must NOT create the final file"
    );

    // The cancelled run must have persisted real progress, otherwise the
    // `resume()` below would just re-download the whole file and assert nothing
    // about the resume path.
    let fd_text = std::fs::read_to_string(&fd).expect("read .fd");
    assert!(
        fd_text.contains("downloaded_chunk"),
        "cancelled .fd must carry resume progress, got:\n{fd_text}"
    );

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() must complete with Renamed after a cancel"
    );
    // The resume must continue from the persisted progress rather than restart.
    let resumed_from = events.iter().find_map(|e| match e {
        Event::Resumed { progress, .. } => Some(progress.clone()),
        _ => None,
    });
    let resumed_from = resumed_from.expect("resume() must emit Event::Resumed");
    let resumed_bytes: u64 = resumed_from.iter().map(|r| r.end - r.start).sum();
    assert!(
        resumed_bytes >= CANCEL_AFTER_BYTES,
        "resume must inherit the cancelled run's progress, got {resumed_bytes} bytes \
         from {resumed_from:?}"
    );
    assert!(
        !fd.exists(),
        ".fd must be removed after the resume completes"
    );
    let got = tokio::fs::read(&final_path)
        .await
        .expect("final file should exist after resume");
    assert_eq!(
        got,
        original_bytes(),
        "final content must match after resume"
    );
}

/// Several concurrent workers write ranges out of order, so an interrupted run
/// persists a *fragmented* `downloaded_chunk` holding more than one range — the
/// single-worker path can only ever leave one leading range, so this is the only
/// test that exercises multi-range state on a real socket and a real file.
/// Resuming from it must request exactly the gaps and reassemble the file byte
/// for byte.
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_resume_with_fragmented_progress() {
    let dir = temp_dir("concurrent_fragmented");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    // 8 workers over 64 KiB chunks each start at a different offset, so the
    // persisted progress is non-contiguous. Cancelling only after six chunks'
    // worth of bytes have landed guarantees several distinct ranges.
    let cancel = create_cancellation_token();
    partial_download_via_cancel_with(
        &url,
        make_config_with(&dir, 8, 64 * 1024),
        cancel,
        (THROTTLE_CHUNK * 6) as u64,
    )
    .await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(
        part.exists() && fd.exists(),
        "cancel must preserve both .part and .fd"
    );

    let fd_text = std::fs::read_to_string(&fd).expect("read .fd");
    let chunk_line = fd_text
        .lines()
        .find(|l| l.trim_start().starts_with("downloaded_chunk"))
        .unwrap_or_else(|| panic!("concurrent .fd must carry progress, got:\n{fd_text}"));
    assert!(
        chunk_line.contains(','),
        "a cancelled concurrent run must persist multiple ranges, got: {chunk_line}"
    );

    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        make_config_with(&dir, 8, 64 * 1024),
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() must complete with Renamed"
    );
    let got = tokio::fs::read(&final_path)
        .await
        .expect("final file should exist after resume");
    assert_eq!(
        got,
        original_bytes(),
        "resuming fragmented progress must reassemble the file exactly"
    );
}

/// F1' (rotated/expired URL): the `.fd` must persist the **initial** URL, while
/// the download part targets the freshly-resolved `info.final_url`. We drive the
/// whole flow through a URL that 302-redirects (the initial URL) to the real file
/// (the final URL) — the realistic signed/CDN pattern where the redirect target
/// is a transient link that expires. After a mid-resume cancel we assert the
/// persisted `.fd` still records the *initial* URL, not the final URL; a second
/// resume then completes the file, proving the range requests were issued against
/// `info.final_url` (not the stored initial URL, which would need a redirect per
/// range request).
#[tokio::test]
async fn test_resume_with_rotated_url_succeeds() {
    let dir = temp_dir("resume_rotated_url");
    let (_server, base) = start_server(original_bytes(), "orig", "LM-A", true).await;

    // Initial URL 302-redirects to the real (final) URL.
    let initial_url = format!("{base}/redirect");
    let final_url = format!("{base}/real");

    // Seed a real, interrupted concurrent download against the *initial* URL.
    // `DownloadState::new` records `state.url = initial_url`, so the `.fd` holds
    // the durable initial URL, never the resolved final URL.
    let cancel = create_cancellation_token();
    partial_download_via_cancel_with(
        &initial_url,
        make_config(&dir),
        cancel,
        (THROTTLE_CHUNK * 6) as u64,
    )
    .await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(
        part.exists() && fd.exists(),
        "cancel must leave .part and .fd"
    );

    // Precondition: the seed `.fd` records the INITIAL url, not the final url.
    let fd_text = std::fs::read_to_string(&fd).expect("read .fd");
    let seed_url_line = fd_text
        .lines()
        .find(|l| l.trim_start().starts_with("url = "))
        .expect("precondition: .fd must record a url");
    assert!(
        seed_url_line.contains("/redirect"),
        "seed .fd must store the initial URL, got: {seed_url_line}"
    );
    assert!(
        !seed_url_line.contains(final_url.as_str()),
        "seed .fd must NOT contain the final URL, got: {seed_url_line}"
    );

    // Resume, but cancel right after it starts so the `.fd` remains on disk. The
    // initial `state.store()` inside `overwrite` already persisted `url =
    // initial_url` (refresh_identity does not touch the URL), so the file reflects
    // the design invariant.
    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&initial_url).expect("valid url")),
        cfg,
        tx,
        resume_cancel.clone(),
    );
    let mut started = false;
    while let Ok(e) = rx.recv().await {
        if matches!(e, Event::Start { .. }) {
            started = true;
            resume_cancel.cancel();
        }
        if matches!(e, Event::Renamed(_) | Event::ResumeError(_)) {
            break;
        }
    }
    assert!(started, "resume must emit Event::Start");
    assert!(
        part.exists() && fd.exists(),
        ".part/.fd must survive a mid-resume cancel"
    );

    // The persisted `.fd` must still carry the INITIAL URL — proving the download
    // part uses `info.final_url` and the `.fd` stores the durable initial URL.
    let fd_text = std::fs::read_to_string(&fd).expect("read .fd after cancel");
    let resume_url_line = fd_text
        .lines()
        .find(|l| l.trim_start().starts_with("url = "))
        .expect("post-resume .fd must record a url");
    assert!(
        resume_url_line.contains("/redirect"),
        "after resume the .fd must STILL record the initial URL, got: {resume_url_line}"
    );
    assert!(
        !resume_url_line.contains(final_url.as_str()),
        "after resume the .fd must NOT have been rewritten to the final URL, got: {resume_url_line}"
    );

    // A second resume completes the file using `info.final_url` for the remaining
    // ranges — proving the download part targeted the final URL (the initial URL
    // only redirects and would otherwise cost a redirect round-trip per range).
    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&initial_url).expect("valid url")),
        cfg,
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() must complete with Renamed against a rotated URL"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "a rotated URL must NOT be a resume error (content identity still matches)"
    );
    let renamed = events
        .iter()
        .find_map(|e| match e {
            Event::Renamed(p) => Some(p.clone()),
            _ => None,
        })
        .expect("resume() must complete with Renamed");
    let got = tokio::fs::read(&renamed)
        .await
        .expect("final file should exist after resume");
    assert_eq!(
        got,
        original_bytes(),
        "resuming against a rotated URL must reassemble the file exactly"
    );
}

/// Case 5b (explicit new behavior): when the provided `tmp_path` does **not**
/// exist, `resume()` must fall back to a fresh full download (`force_resume =
/// false`) at that path — it must complete (Renamed), produce the correct
/// content, and never emit a `ResumeError`.
#[tokio::test]
async fn test_resume_missing_tmp_path_falls_back_to_download() {
    let dir = temp_dir("resume_missing_tmp");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    assert!(
        !part.exists(),
        "tmp_path must not exist before resume (the point of this test)"
    );

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        cancel,
    );
    let events = drain(rx).await;

    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "missing tmp_path must fall back to a full download (Renamed)"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "missing tmp_path must NOT emit a ResumeError"
    );

    let got = tokio::fs::read(&final_path)
        .await
        .expect("final file should exist after the fallback download");
    assert_eq!(
        got,
        original_bytes(),
        "fallback download must produce correct content"
    );
    assert!(
        !part.exists(),
        ".part must be renamed away after a successful fallback download"
    );
}

/// Case 6 (fresh download, sanity guard): a brand-new `download()` with no
/// cancel and no resume must write the COMPLETE and CORRECT file. This is the
/// direct regression guard for BUG-1 — an empty `downloaded_chunk` (before the
/// fix) made `run_download` request zero chunks and produce a 0-byte file.
#[tokio::test]
async fn test_fresh_download_writes_full_file() {
    let dir = temp_dir("fresh_download");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = drain(rx).await;

    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "a fresh download must complete with Renamed"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "a fresh download must NOT emit a ResumeError"
    );

    let final_path = dir.join("out.bin");
    assert!(
        final_path.exists(),
        "final file must exist after a fresh download"
    );

    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(
        got.len(),
        FILE_SIZE,
        "fresh download must write the full file (BUG-1 regression guard)"
    );
    assert_eq!(
        got,
        original_bytes(),
        "fresh download content must match source exactly"
    );

    let fd = final_path.with_added_extension("fd");
    assert!(
        !fd.exists(),
        ".fd must be removed after a successful fresh download"
    );
}

/// Verify the `Event::Progress` aggregator: it is emitted on the configured
/// cadence (not merely once), carries the full progress snapshot + a transfer
/// rate, reports the correct total size, and reaches 100% coverage on a complete
/// download. Regression guard for the dedicated, independent progress reporter.
#[tokio::test]
async fn test_progress_event_emitted() {
    let dir = temp_dir("progress_event");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let mut cfg = make_config(&dir);
    // Emit Progress frequently so the cadence is observable.
    cfg.progress_emit_gap = Some(Duration::from_millis(30));

    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = drain(rx).await;

    // At least one Progress event, and it must fire on its own timer (not just
    // the terminal one) — this is the whole point of the dedicated reporter.
    let progresses: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress(s) => Some((
                s.progress.clone(),
                s.bps,
                s.avg_bps,
                s.downloaded,
                s.percent,
                s.total,
                s.elapsed,
                s.eta,
            )),
            _ => None,
        })
        .collect();
    assert!(
        !progresses.is_empty(),
        "expected at least one Event::Progress"
    );
    assert!(
        progresses.len() >= 2,
        "Event::Progress must fire on its own cadence, not only once (got {})",
        progresses.len()
    );

    // Every Progress reports the correct total size.
    assert!(
        progresses
            .iter()
            .all(|(_, _, _, _, _, total, _, _)| *total == FILE_SIZE as u64),
        "Event::Progress.total must equal the file size"
    );

    // Structural invariants for the new aggregate fields.
    for (p, _, avg_bps, downloaded, percent, _, elapsed, _) in &progresses {
        let sum: u64 = p.iter().map(|r| r.end - r.start).sum();
        assert_eq!(
            sum, *downloaded,
            "Event::Progress.downloaded must equal sum(progress ranges)"
        );
        assert!(
            (*percent >= 0.0) && (*percent <= 100.0),
            "percent out of range: {percent}"
        );
        // avg_bps is session-wide bytes/sec; it can never exceed
        // downloaded * 1000 (the rate if the whole file arrived in 1ms).
        assert!(
            *avg_bps <= (*downloaded).saturating_mul(1000).max(1),
            "avg_bps implausibly high: {avg_bps}"
        );
        assert!(
            *elapsed >= Duration::ZERO,
            "elapsed must be non-negative: {elapsed:?}"
        );
    }

    // `elapsed` (cumulative active time) must be non-decreasing across frames.
    let mut prev_elapsed = Duration::ZERO;
    for (_, _, _, _, _, _, el, _) in &progresses {
        assert!(
            *el >= prev_elapsed,
            "Event::Progress.elapsed must be non-decreasing: {el:?} < {prev_elapsed:?}"
        );
        prev_elapsed = *el;
    }

    // `eta` (estimated remaining time) invariants:
    // - At least one intermediate frame must carry a positive `eta`, proving ETA
    //   is actually computed mid-download (the run is not instant).
    // - `eta == (total - downloaded) / rate` with millisecond rounding, so we
    //   check it within a 1ms tolerance against the effective rate.
    assert!(
        progresses
            .iter()
            .any(|(_, _, _, d, _, t, _, eta)| *d < *t && eta.is_some_and(|e| e > Duration::ZERO)),
        "expected at least one intermediate Progress with eta > 0"
    );
    for (_, bps, avg_bps, downloaded, _, total, _, eta) in &progresses {
        let remaining = total.saturating_sub(*downloaded);
        // Mirrors the production formula: prefer the smoothed recent `bps`,
        // fall back to the session-wide `avg_bps` while the EMA warms up.
        let rate = if *bps > 0 { *bps } else { *avg_bps };
        if remaining == 0 {
            // Fully written: eta must be ZERO (absent only if rate is still 0).
            assert!(
                eta.map_or(rate == 0, |e| e == Duration::ZERO),
                "eta must be ZERO once fully downloaded, got {eta:?}"
            );
        } else if rate > 0 {
            let Some(d) = eta else {
                panic!("eta must be Some while downloading with a measurable rate");
            };
            let expected_ms = u128::from(
                remaining
                    .checked_mul(1000)
                    .and_then(|x| x.checked_div(rate))
                    .expect("rate > 0"),
            );
            let got_ms = d.as_millis();
            let diff = got_ms.abs_diff(expected_ms);
            assert!(
                diff <= 1,
                "eta {got_ms}ms disagrees with computed {expected_ms}ms"
            );
        } else {
            assert!(eta.is_none(), "eta must be None with no measurable rate");
        }
    }

    // The download actually progressed: at some point the full size was reported.
    let max_downloaded: u64 = progresses
        .iter()
        .map(|(_, _, _, d, _, _, _, _)| *d)
        .max()
        .unwrap_or(0);
    assert_eq!(
        max_downloaded, FILE_SIZE as u64,
        "downloaded must reach the full file size"
    );

    // At least one intermediate frame reported a positive instantaneous rate.
    assert!(
        progresses.iter().any(|(_, bps, _, _, _, _, _, _)| *bps > 0),
        "expected at least one Event::Progress with bps > 0"
    );

    // The terminal Progress reflects 100% coverage of the whole file.
    let last = progresses.last().expect("last Progress");
    assert_eq!(
        last.0,
        vec![0u64..FILE_SIZE as u64],
        "final Event::Progress must cover the whole file"
    );
    assert_eq!(last.5, FILE_SIZE as u64);
    assert!(
        (last.4 - 100.0).abs() < f64::EPSILON,
        "final Event::Progress.percent must be 100.0, got {}",
        last.4
    );
}

/// Regression guard for persisted `elapsed`: after a mid-download cancel the
/// `.fd` state records the active time spent; a subsequent `resume()` must
/// continue that clock (its first `Progress` reports an `elapsed` at least as
/// large as the cancelled run's last one) instead of resetting to zero, so the
/// session-wide `avg_bps` and `downloaded` span the whole download.
#[tokio::test]
async fn test_progress_elapsed_persisted_across_resume() {
    let dir = temp_dir("progress_elapsed_resume");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    let partial_events = partial_download_via_cancel(&url, &dir, cancel).await;

    let partial_elapsed_max = partial_events
        .iter()
        .filter_map(|e| match e {
            Event::Progress(s) => Some(s.elapsed),
            _ => None,
        })
        .max()
        .expect("partial run must emit at least one Progress with elapsed");

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(
        part.exists() && fd.exists(),
        "cancel must preserve both .part and .fd"
    );

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    let resume_progress: Vec<(Vec<fast_down::ProgressEntry>, u64, Duration, f64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress(s) => Some((
                s.progress.clone(),
                s.downloaded,
                s.elapsed,
                s.percent,
                s.total,
            )),
            _ => None,
        })
        .collect();

    assert!(
        !resume_progress.is_empty(),
        "resume must emit Event::Progress"
    );
    // The resume run continues the elapsed clock: its smallest reported elapsed
    // must be at least the cancelled run's largest (no reset to zero).
    let resume_elapsed_min = resume_progress
        .iter()
        .map(|(_, _, el, _, _)| *el)
        .min()
        .expect("resume Progress must carry elapsed");
    assert!(
        resume_elapsed_min >= partial_elapsed_max,
        "resume elapsed ({resume_elapsed_min:?}) must carry over partial elapsed ({partial_elapsed_max:?})"
    );
    // And it finishes correctly, covering the whole file at 100%.
    let last = resume_progress.last().expect("last resume Progress");
    assert_eq!(
        last.0,
        vec![0u64..FILE_SIZE as u64],
        "resume must cover the whole file"
    );
    assert_eq!(last.4, FILE_SIZE as u64);
    assert!(
        (last.3 - 100.0).abs() < f64::EPSILON,
        "resume percent must be 100.0, got {}",
        last.3
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume must complete with Renamed"
    );
}

/// `download` spawns a detached task that completes once the spawned task
/// finishes: a fresh full download emits `Event::Renamed`, and draining the
/// paired `rx` returns only after the task has fully ended (including the final
/// `overwrite`), proving the detached task did not panic.
#[tokio::test]
async fn test_download_join_resolves() {
    let dir = temp_dir("join");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;
    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = drain(rx).await;
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "fresh download must complete with Renamed"
    );
}

/// Like [`make_config`] but with `overwrite = false`, so downloads fall into the
/// unique-path branch and resume from a `.part`/`.fd` via the non-overwrite arm of
/// `download()` (mod.rs lines 189-213, overwrite.rs 198-202).
fn make_config_no_overwrite(save_dir: &Path) -> PartialConfig {
    PartialConfig {
        save_dir: Some(save_dir.to_path_buf()),
        filename: Some("out.bin".to_string()),
        parse_filename: Some(false),
        overwrite: Some(false),
        write_method: Some(WriteMethod::Mmap),
        min_chunk_size: Some(1024 * 1024),
        threads: Some(32),
        ..Default::default()
    }
}

/// `download` completes cleanly for a successful download: draining the paired
/// `rx` returns once the task has finished (including `overwrite`).
#[tokio::test]
async fn test_handle_join_completes() {
    let dir = temp_dir("handle_join");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);

    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download must complete"
    );
}

/// `download()` silently resumes (emitting `Event::Resumed`) when a valid
/// `.fd`/`.part` already exists and `overwrite = true` (mod.rs lines 157-165).
#[tokio::test]
async fn test_download_silent_resume_overwrite() {
    let dir = temp_dir("silent_resume_overwrite");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(
        part.exists() && fd.exists(),
        "cancel must leave .part and .fd"
    );

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel2 = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel2);
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "download() must silently resume (emit Event::Resumed)"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download() must complete after a silent resume"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "download() must NOT emit a ResumeError"
    );
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(got, original_bytes(), "resumed content mismatch");
}

/// `download()` with `overwrite = false` resumes via the non-overwrite arm
/// (mod.rs lines 189-204) and renames into a unique path (overwrite.rs 198-202).
#[tokio::test]
async fn test_download_resume_without_overwrite() {
    let dir = temp_dir("resume_no_overwrite");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    let cfg1 = make_config_no_overwrite(&dir);
    partial_download_via_cancel_with(&url, cfg1, cancel, CANCEL_AFTER_BYTES).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(
        part.exists() && fd.exists(),
        "cancel must leave .part and .fd"
    );

    let cfg2 = make_config_no_overwrite(&dir);
    let (tx, rx) = create_channel();
    let cancel2 = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg2, tx, cancel2);
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "download() must silently resume (emit Event::Resumed)"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download() must complete after resume"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "download() must NOT emit a ResumeError"
    );
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(got, original_bytes(), "resumed content mismatch");
}

/// When `resume()` is pointed at a `.part` that is actually a *directory*, the
/// sink file cannot be opened, so `build_pipeline` fails and `overwrite` returns
/// early (overwrite.rs line 84) emitting `Event::BuildPusherError` — no rename,
/// and crucially not a `ResumeError` (that path is reserved for state/validation
/// failures, not a plain IO open error).
#[tokio::test]
async fn test_resume_with_directory_part_fails_pipeline() {
    let dir = temp_dir("resume_dir_part");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;
    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(part.exists() && fd.exists());

    // Replace the `.part` FILE with a DIRECTORY of the same name.
    std::fs::remove_file(&part).unwrap();
    std::fs::create_dir(&part).unwrap();

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel2 = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        cancel2,
    );
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BuildPusherError(_))),
        "opening a directory as the sink must emit Event::BuildPusherError"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "must NOT rename when the pipeline fails to build"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "a file-open failure is not a StateError resume failure"
    );
    // The directory `.part` is left untouched (we don't delete it; the task only
    // removes real `.part` files on a successful rename).
    assert!(part.is_dir(), "the directory .part must remain");
    let _ = std::fs::remove_dir_all(&part);
    let _ = std::fs::remove_file(&fd);
}

/// `prefetch` retries on failure and gives up (returns `None`, so `download`
/// never proceeds) once `retry_times` attempts are exhausted (prefetch.rs 24-30).
#[tokio::test]
async fn test_prefetch_retries_then_gives_up() {
    // Stand up a server that always answers 500 so every prefetch attempt fails.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind server socket");
    let addr = listener.local_addr().expect("resolve local addr");
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let service = service_fn(|_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(BoxBody::new(http_body_util::Empty::<Bytes>::new()))
                            .expect("build 500 response"),
                    )
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    let url = format!("http://{addr}");

    let dir = temp_dir("prefetch_retry");
    let mut cfg = make_config(&dir);
    cfg.retry_times = Some(2);
    cfg.retry_gap = Some(Duration::from_millis(10));

    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = timeout(Duration::from_secs(20), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        events.iter().any(|e| matches!(e, Event::PrefetchError(_))),
        "expected Event::PrefetchError from the failing server"
    );
    assert!(
        events
            .iter()
            .filter(|e| matches!(e, Event::PrefetchError(_)))
            .count()
            >= 2,
        "prefetch must retry up to retry_times times"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download must not complete against a failing server"
    );
}

/// A zero-byte file reports `total == 0`; `ProgressSample::percent` is then
/// forced to `0.0` (`progress_reporter.rs` line 134) rather than dividing by zero.
#[tokio::test]
async fn test_progress_zero_total_file() {
    let dir = temp_dir("progress_zero");
    // An empty body, range-capable.
    let (_server, url) = start_server(vec![], "etag0", "LM-0", true).await;

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = timeout(Duration::from_secs(20), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "a 0-byte download must still complete"
    );
    let progresses: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let Event::Progress(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect();
    assert!(!progresses.is_empty(), "expected Event::Progress events");
    assert!(
        progresses.iter().all(|s| s.percent == 0.0),
        "percent must be exactly 0.0 for a zero-byte file (line 134)"
    );
    let final_path = dir.join("out.bin");
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert!(got.is_empty(), "0-byte file must produce a 0-byte file");
}

/// A corrupt (non-TOML) `.fd` cannot block `download()`: with `overwrite = true`
/// a failed `DownloadState::load` breaks the silent-resume conjunction, so the
/// task falls back to a fresh download — no `Resumed`, no `ResumeError`, and a
/// correct final file.
#[tokio::test]
async fn test_corrupt_fd_silently_redownloads() {
    let dir = temp_dir("corrupt_fd");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let final_path = dir.join("out.bin");
    let fd = final_path.with_added_extension("fd");
    tokio::fs::write(&fd, "not valid toml {{{")
        .await
        .expect("write corrupt .fd");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        !events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "a corrupt .fd must not be resumed"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "download() must silently fall back, not report a ResumeError"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download() must complete via the silent fallback"
    );
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(got, original_bytes(), "fallback content mismatch");
}

/// A valid `.fd` alone is not enough for silent resume: the `.part` file must
/// exist too (`fs::try_exists` conjunct). Deleting the `.part` makes
/// `download()` silently start over — fresh state, no `Resumed`, full content.
#[tokio::test]
async fn test_missing_part_with_valid_fd_silently_redownloads() {
    let dir = temp_dir("missing_part");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(fd.exists(), "precondition: .fd must exist");
    tokio::fs::remove_file(&part).await.expect("delete .part");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel2 = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel2);
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        !events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "without the .part, silent resume must not kick in"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download() must complete via a fresh download"
    );
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(got, original_bytes(), "fresh download content mismatch");
}

/// `resume = false` disables the silent-resume conjunct (`can_resume`) even when
/// a perfectly valid `.fd`/`.part` pair exists: `download()` starts over from
/// zero without emitting `Resumed`, and never reports a `ResumeError`.
#[tokio::test]
async fn test_resume_disabled_ignores_valid_state() {
    let dir = temp_dir("resume_disabled");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let fd = final_path.with_added_extension("fd");
    assert!(fd.exists(), "precondition: valid .fd must exist");

    let mut cfg = make_config(&dir);
    cfg.resume = Some(false);
    let (tx, rx) = create_channel();
    let cancel2 = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel2);
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        !events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "resume=false must never emit Resumed"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "resume=false must not report a ResumeError"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download() must complete"
    );
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(got, original_bytes(), "content mismatch");
}

/// In the non-overwrite branch, a stray `.part` without a matching `.fd` makes
/// `open_create_new` fail with `AlreadyExists`, so `iter_stem` advances to the
/// next stem variant and the download completes there — the stray file is left
/// untouched and the final content is still correct.
#[tokio::test]
async fn test_non_overwrite_stray_part_advances_iter_stem() {
    let dir = temp_dir("stray_part");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let final_path = dir.join("out.bin");
    let stray = final_path.with_added_extension("part");
    tokio::fs::write(&stray, b"stray")
        .await
        .expect("create stray .part");

    let cfg = make_config_no_overwrite(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::BuildPusherError(_))),
        "AlreadyExists must advance iter_stem, not error out"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "a stray .part without .fd cannot be resumed"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download() must complete on the next stem variant"
    );
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(got, original_bytes(), "content mismatch");
    let stray_bytes = tokio::fs::read(&stray).await.expect("read stray .part");
    assert_eq!(
        stray_bytes, b"stray",
        "the stray .part must be left untouched"
    );
}

/// A corrupt (non-TOML) `.fd` makes `resume()` report
/// `StateError::Decode` (the `.part` exists, so there is no fallback) and keep
/// both partial files untouched — no rename, no silent re-download.
#[tokio::test]
async fn test_resume_corrupt_fd_reports_decode_error() {
    let dir = temp_dir("resume_corrupt_fd");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    tokio::fs::write(&part, b"partial")
        .await
        .expect("create .part");
    tokio::fs::write(&fd, "not valid toml {{{")
        .await
        .expect("write corrupt .fd");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        cancel,
    );
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    let err = events
        .iter()
        .find_map(|e| match e {
            Event::ResumeError(r) => Some(r),
            _ => None,
        })
        .expect("expected Event::ResumeError");
    assert!(
        matches!(err, StateError::Decode(_)),
        "expected Event::ResumeError(StateError::Decode) for a corrupt .fd, got {err:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() must NOT rename on a decode error"
    );
    assert!(part.exists() && fd.exists(), "partial files must be kept");
}

/// `resume()` forces `overwrite = false`, so an already-existing final file is
/// never clobbered: the finished download is renamed into a *unique* variant
/// while the pre-existing file keeps its exact content.
#[tokio::test]
async fn test_resume_never_overwrites_existing_final_file() {
    let dir = temp_dir("resume_no_clobber");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    assert!(part.exists(), "cancel must leave the .part");
    // A pre-existing final file the resume must not touch.
    tokio::fs::write(&final_path, b"sentinel")
        .await
        .expect("create pre-existing final file");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        resume_cancel,
    );
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    let renamed = events
        .iter()
        .find_map(|e| match e {
            Event::Renamed(p) => Some(p.clone()),
            _ => None,
        })
        .expect("resume must complete with Renamed");
    assert_ne!(
        renamed, final_path,
        "resume must NOT overwrite the existing final file"
    );
    let sentinel = tokio::fs::read(&final_path)
        .await
        .expect("pre-existing file must remain readable");
    assert_eq!(
        sentinel, b"sentinel",
        "the pre-existing final file content must be untouched"
    );
    let got = tokio::fs::read(&renamed).await.expect("read renamed file");
    assert_eq!(got, original_bytes(), "renamed content mismatch");
}

/// `resume()` forces `partial_config.resume = true`, so a user-supplied
/// `resume = false` is overridden: with a valid `.fd`/`.part` pair the task
/// still resumes (emits `Resumed`) instead of refusing.
#[tokio::test]
async fn test_resume_overrides_user_resume_false() {
    let dir = temp_dir("resume_forces_true");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    assert!(part.exists(), "cancel must leave the .part");

    let mut cfg = make_config(&dir);
    cfg.resume = Some(false);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        resume_cancel,
    );
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "resume() must override resume=false and resume anyway"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "no ResumeError when the state is valid"
    );
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(got, original_bytes(), "resumed content mismatch");
}

/// When the final destination is a DIRECTORY and `overwrite = true`, the engine
/// still completes but `fs::rename` fails: `overwrite()` must emit
/// `Event::RenameFailed` (not panic, not `Renamed`) and leave both the `.part`
/// and the `.fd` in place so the download is not lost.
#[tokio::test]
async fn test_rename_failed_when_final_path_is_directory() {
    let dir = temp_dir("rename_dir_dest");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let final_path = dir.join("out.bin");
    std::fs::create_dir(&final_path).expect("final path must be a directory");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = timeout(Duration::from_secs(60), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        events.iter().any(|e| matches!(e, Event::RenameFailed(_))),
        "renaming onto a directory must emit Event::RenameFailed, got: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "must NOT emit Renamed when the rename failed"
    );
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(
        part.exists() && fd.exists(),
        ".part/.fd must be kept after a failed rename"
    );
    let _ = std::fs::remove_dir_all(&final_path);
}

/// With `overwrite = true` a re-download must replace an already-existing final
/// file (rename-replace semantics): the pre-existing content is overwritten by
/// the fresh download and no unique variant is created.
#[tokio::test]
async fn test_overwrite_true_replaces_existing_final_file() {
    let dir = temp_dir("overwrite_replace");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let final_path = dir.join("out.bin");
    tokio::fs::write(&final_path, b"stale content")
        .await
        .expect("create pre-existing final file");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = timeout(Duration::from_secs(60), drain(rx))
        .await
        .expect("drain timed out");

    let renamed = events
        .iter()
        .find_map(|e| match e {
            Event::Renamed(p) => Some(p.clone()),
            _ => None,
        })
        .expect("expected Event::Renamed");
    // Compare by file name only: on Windows the rename can return a
    // `\\?\`-prefixed verbatim path, so an exact PathBuf match is fragile.
    assert_eq!(
        renamed.file_name(),
        final_path.file_name(),
        "overwrite=true must rename onto the original file name, not a unique variant"
    );
    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(
        got,
        original_bytes(),
        "the pre-existing file must be replaced by the fresh download"
    );
}

/// An invalid custom proxy URL makes `FastDownPuller::new` fail inside
/// `build_pipeline`, so the task must end with `Event::BuildClientError` —
/// never `Renamed`, and no hang.
#[tokio::test]
async fn test_invalid_proxy_reports_build_client_error() {
    let dir = temp_dir("bad_proxy");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let mut cfg = make_config(&dir);
    cfg.proxy = Some(fast_down_api::fast_down::Proxy::Custom(
        "not a valid url".to_string(),
    ));
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
    let events = timeout(Duration::from_secs(30), drain(rx))
        .await
        .expect("drain timed out");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BuildClientError(_))),
        "an invalid proxy must emit Event::BuildClientError, got: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download must not complete when the client cannot be built"
    );
}

/// Regression test for F-A01: in the **non-overwrite** path, a fatal I/O error
/// while creating the `.part` file must surface as `Event::BuildPusherError` and
/// end the task — NOT spin forever on the unbounded `iter_stem` iterator. The
/// pre-fix code did `else { continue }` unconditionally, so a full disk / read
/// only target (any `create_new` failure that is not `AlreadyExists`) made the
/// download task hang indefinitely.
///
/// The fatal case is simulated on unix by making `save_dir` read-only
/// (`0o555`): `gen_path` still succeeds (`create_dir_all` is a no-op on an
/// existing dir) but `open_create_new` on any candidate `.part` fails with
/// `PermissionDenied`. The Windows readonly attribute does not block file
/// creation, so this is intentionally unix-only; the fix itself is cross
/// platform (the `io::ErrorKind` branch runs everywhere).
#[cfg(unix)]
#[tokio::test]
async fn test_non_overwrite_create_new_fatal_error_reports_not_hangs() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("fa01_readonly_create_new");
    // Make the save dir read-only so `open_create_new` on any candidate `.part`
    // fails with `PermissionDenied` (a non-`AlreadyExists` error).
    let mut perms = std::fs::metadata(&dir)
        .expect("stat save dir")
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&dir, perms).expect("chmod save dir read-only");

    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cfg = PartialConfig {
        save_dir: Some(dir.clone()),
        filename: Some("out.bin".to_string()),
        parse_filename: Some(false),
        overwrite: Some(false),
        resume: Some(false),
        write_method: Some(WriteMethod::Mmap),
        min_chunk_size: Some(1024 * 1024),
        threads: Some(32),
        ..Default::default()
    };
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);

    // Guard against the pre-fix infinite loop: a hung task never closes the
    // channel, so `drain` would block past this timeout and the test would fail
    // with a clear message instead of hanging the whole suite.
    let events = timeout(Duration::from_secs(15), drain(rx))
        .await
        .expect("download task hung on a fatal create_new error (F-A01 regression)");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BuildPusherError(_))),
        "a fatal .part creation error must emit Event::BuildPusherError, got: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "download must not complete when the .part cannot be created"
    );

    // Best-effort restore so a later run's cleanup of this temp dir is not
    // blocked by the read-only bit.
    let mut restore = std::fs::metadata(&dir)
        .expect("stat save dir")
        .permissions();
    restore.set_mode(0o755);
    let _ = std::fs::set_permissions(&dir, restore);
}

/// `resume()` with `url = None` must re-use the durable initial URL persisted in
/// the `.fd` state file (recorded by the original `download`) to re-resolve and
/// resume — so a caller can resume purely from the `.part` path. The resumed
/// download still completes correctly.
#[tokio::test]
async fn test_resume_without_url_uses_fd_url() {
    let dir = temp_dir("resume_no_url_fd");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    assert!(
        part.exists() && fd.exists(),
        "precondition: partial .part/.fd must exist"
    );

    // Resume with NO url: must auto-use the `.fd`'s stored initial URL.
    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(part.clone(), None, cfg, tx, resume_cancel);
    let events = drain(rx).await;

    assert!(
        events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "resume() with no url must still resume (using the .fd's initial URL)"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() with no url must complete (Renamed)"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::ResumeError(_))),
        "resume() with no url must NOT report a ResumeError when the .fd has a URL"
    );
    let got = tokio::fs::read(&final_path)
        .await
        .expect("final file should exist after resume");
    assert_eq!(got, original_bytes(), "resumed content mismatch");
}

/// `resume()` with `url = None` and no `.part` to fall back from must report
/// `StateError::NoUrl`: there is no URL to fetch and no `.fd` to read one from,
/// so a silent fallback to a fresh download is impossible.
#[tokio::test]
async fn test_resume_without_url_missing_tmp_path_errors() {
    let dir = temp_dir("resume_no_url_no_tmp");
    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    assert!(!part.exists(), "precondition: tmp_path must not exist");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    resume(part.clone(), None, cfg, tx, cancel);
    let events = drain(rx).await;

    let err = events
        .iter()
        .find_map(|e| match e {
            Event::ResumeError(r) => Some(r),
            _ => None,
        })
        .expect("expected Event::ResumeError when no url and no .part");
    assert!(
        matches!(err, StateError::NoUrl(_)),
        "expected StateError::NoUrl, got {err:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() must NOT rename when it cannot resolve a URL"
    );
}

/// `resume()` with `url = None` against a `.fd` that carries no resolvable URL
/// (written by an older build, or hand-trimmed) must report `StateError::NoUrl`
/// rather than silently failing at prefetch with a default/blank URL.
#[tokio::test]
async fn test_resume_without_url_and_fd_has_no_url_errors() {
    let dir = temp_dir("resume_no_url_fd_blank");
    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    // A `.fd` with content identity but NO url field, plus a present `.part`.
    tokio::fs::write(&fd, "size = 1024\n")
        .await
        .expect("write .fd without a url");
    tokio::fs::write(&part, b"partial")
        .await
        .expect("create .part");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();
    resume(part.clone(), None, cfg, tx, cancel);
    let events = drain(rx).await;

    let err = events
        .iter()
        .find_map(|e| match e {
            Event::ResumeError(r) => Some(r),
            _ => None,
        })
        .expect("expected Event::ResumeError when .fd has no url and none supplied");
    assert!(
        matches!(err, StateError::NoUrl(_)),
        "expected StateError::NoUrl for a url-less .fd with no url arg, got {err:?}"
    );
    assert!(
        part.exists() && fd.exists(),
        "partial files must be kept on NoUrl"
    );
}

/// P1 regression test: `resume()` must check that the `.part` file size is consistent with
/// the recorded progress. If the `.part` file is smaller than the recorded progress,
/// it should fall back to a fresh download to avoid data corruption.
#[tokio::test]
async fn test_resume_truncated_part_falls_back_to_fresh_download() {
    let dir = temp_dir("resume_truncated_part");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    // Start a partial download
    let cancel = create_cancellation_token();
    partial_download_via_cancel(&url, &dir, cancel).await;

    let final_path = dir.join("out.bin");
    let part = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");

    assert!(
        part.exists() && fd.exists(),
        "precondition: .part and .fd must exist"
    );

    // Verify that some progress was recorded
    let fd_content = tokio::fs::read_to_string(&fd).await.expect("read .fd");
    assert!(
        fd_content.contains("downloaded_chunk"),
        "precondition: .fd must contain progress"
    );

    // Truncate the .part file to simulate corruption (smaller than recorded progress)
    {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&part)
            .await
            .expect("open .part for truncation");
        file.set_len(100).await.expect("truncate .part file"); // Much smaller than recorded progress
    }

    // Now try to resume - this should fall back to a fresh download since the .part file
    // is smaller than the recorded progress
    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    resume(
        part.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    // Should complete successfully with a fresh download (not a corrupted resume)
    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume with truncated .part should complete with fresh download"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Resumed { .. })),
        "should not emit Resumed event when .part is truncated"
    );

    let got = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(
        got,
        original_bytes(),
        "content should match original after fresh download"
    );
}

/// P2 regression test: `resume()` should reject `tmp_path` that doesn't end with `.part` extension.
#[tokio::test]
async fn test_resume_rejects_non_part_extension() {
    let dir = temp_dir("resume_wrong_ext");
    let (_server, url) = start_server(original_bytes(), "orig", "LM-A", true).await;

    let final_path = dir.join("out.bin");
    let wrong_ext_path = final_path.with_extension("tmp"); // Wrong extension

    // Create a dummy file with wrong extension
    tokio::fs::write(&wrong_ext_path, b"dummy")
        .await
        .expect("create dummy file");

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let cancel = create_cancellation_token();

    // This should return an error because tmp_path doesn't end with .part
    resume(
        wrong_ext_path.clone(),
        Some(Url::parse(&url).expect("valid url")),
        cfg,
        tx,
        cancel,
    );
    let events = drain(rx).await;

    let err = events
        .iter()
        .find_map(|e| match e {
            Event::ResumeError(r) => Some(r),
            _ => None,
        })
        .expect("expected Event::ResumeError for non-.part extension");

    // Check that it's specifically an Open error with the right message
    if let StateError::Open(e) = err {
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            e.to_string()
                .contains("tmp_path must end with .part extension")
        );
    } else {
        panic!("expected StateError::Open, got {err:?}");
    }

    assert!(
        !events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "should not rename when tmp_path has wrong extension"
    );
}
