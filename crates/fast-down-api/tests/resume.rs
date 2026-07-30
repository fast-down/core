//! End-to-end integration tests for the resume (断点续传) feature of
//! `fast_down_api`.
//!
//! These tests stand up a minimal ranged HTTP server (built on `hyper`) and drive
//! the public `DownloadHandle::{download, resume}` API through real, resumable
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
    DownloadHandle, Event, PartialConfig, Rx, StateError, WriteMethod, create_cancellation_token,
    create_channel,
};
use futures::StreamExt;
use futures::stream::unfold;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
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

/// Build a deterministic config that targets `<dir>/out.bin` and downloads with
/// a single thread and small chunks so the cancel lands predictably mid-flight.
fn make_config(save_dir: &Path) -> PartialConfig {
    PartialConfig {
        save_dir: Some(save_dir.to_path_buf()),
        filename: Some("out.bin".to_string()),
        parse_filename: Some(false),
        overwrite: Some(true),
        write_method: Some(WriteMethod::Mmap),
        min_chunk_size: Some(1024 * 1024),
        threads: Some(1),
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

/// Start a normal `download()` and cancel it a fixed time after `Event::Start`,
/// leaving a real partial `.part`/`.fd` on disk. Returns the collected events.
/// `DownloadHelper` 的 `partial_download_via_cancel` 版本——开始下载后收到 `Start` 立即取消。
async fn partial_download_via_cancel(
    url: &str,
    save_dir: &Path,
    cancel: CancellationToken,
) -> Vec<Event> {
    let cfg = make_config(save_dir);
    let (tx, rx) = create_channel();
    let _handle =
        DownloadHandle::download(Url::parse(url).expect("valid url"), cfg, tx, cancel.clone());

    let mut events = Vec::new();
    let mut started = false;
    while let Ok(e) = rx.recv().await {
        if matches!(e, Event::Start { .. }) && !started {
            started = true;
            cancel.cancel();
        }
        events.push(e);
    }
    assert!(started, "expected Event::Start during the partial download");
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
    let _handle = DownloadHandle::resume(
        part.clone(),
        Url::parse(&url).expect("valid url"),
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
    let _handle =
        DownloadHandle::download(Url::parse(&url).expect("valid url"), cfg, tx, dl_cancel);
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
    let _handle = DownloadHandle::resume(
        part.clone(),
        Url::parse(&url).expect("valid url"),
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
    let _handle = DownloadHandle::resume(
        part.clone(),
        Url::parse(&url).expect("valid url"),
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
    let _handle = DownloadHandle::resume(
        part.clone(),
        Url::parse(&url).expect("valid url"),
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

    let cfg = make_config(&dir);
    let (tx, rx) = create_channel();
    let resume_cancel = create_cancellation_token();
    let _handle = DownloadHandle::resume(
        part.clone(),
        Url::parse(&url).expect("valid url"),
        cfg,
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    assert!(
        events.iter().any(|e| matches!(e, Event::Renamed(_))),
        "resume() must complete with Renamed after a cancel"
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
    let _handle = DownloadHandle::resume(
        part.clone(),
        Url::parse(&url).expect("valid url"),
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
    let _handle = DownloadHandle::download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
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
    let _handle = DownloadHandle::download(Url::parse(&url).expect("valid url"), cfg, tx, cancel);
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
    let _handle = DownloadHandle::resume(
        part.clone(),
        Url::parse(&url).expect("valid url"),
        cfg,
        tx,
        resume_cancel,
    );
    let events = drain(rx).await;

    let resume_progress: Vec<(Vec<fast_down::ProgressEntry>, u64, Duration, f64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress(s) => {
                Some((s.progress.clone(), s.downloaded, s.elapsed, s.percent, s.total))
            }
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
