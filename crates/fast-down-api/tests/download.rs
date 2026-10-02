//! End-to-end tests driving `State::build` + `Ready::start` against a local
//! ranged HTTP server.
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::single_range_in_vec_init,
    clippy::items_after_statements
)]

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use fast_down_api::{
    Event, PartialConfig, Record, ResumeOutcome, Rx, StartMode, State, TerminationReason,
    WriteMethod, create_cancellation_token, create_channel, tmp_path_for,
};
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::time::timeout;
use url::Url;

const FILE_SIZE: usize = 1024 * 1024;
/// How much of the file the seeded partial states have "already downloaded".
const PARTIAL: u64 = FILE_SIZE as u64 / 2;
type RespBody = BoxBody<Bytes, Infallible>;

struct FileData {
    body: Vec<u8>,
    etag: String,
    last_modified: String,
    supports_range: bool,
    /// When set, the body is streamed in chunks with this delay between them, so
    /// a slow client-side action (like cancelling) can land mid-transfer.
    throttle: Option<Duration>,
}

#[derive(Clone)]
struct TestServer {
    data: Arc<RwLock<FileData>>,
}

impl TestServer {
    async fn serve(&self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = self.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let conn = server.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| handle(conn.clone(), req));
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });
        format!("http://{addr}")
    }
}

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
    let throttle = data.throttle;
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
        return Ok(Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                CONTENT_RANGE,
                format!("bytes {start}-{end_inclusive}/{total}"),
            )
            .header(ACCEPT_RANGES, "bytes")
            .header(CONTENT_LENGTH, chunk.len().to_string())
            .header(ETAG, etag.as_str())
            .header(LAST_MODIFIED, last_modified.as_str())
            .body(body_of(chunk, throttle))
            .expect("206"));
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_LENGTH, body.len().to_string())
        .header(ETAG, etag.as_str())
        .header(LAST_MODIFIED, last_modified.as_str())
        .body(body_of(body, throttle))
        .expect("200"))
}

/// A response body: the whole `data` at once, or streamed in chunks with a
/// delay when `throttle` is set.
fn body_of(data: Vec<u8>, throttle: Option<Duration>) -> RespBody {
    let Some(delay) = throttle else {
        return http_body_util::Full::new(Bytes::from(data)).boxed();
    };
    const CHUNK: usize = 64 * 1024;
    let stream = futures::stream::unfold((0usize, data), move |(pos, data)| async move {
        if pos >= data.len() {
            return None;
        }
        tokio::time::sleep(delay).await;
        let end = (pos + CHUNK).min(data.len());
        Some((
            Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::copy_from_slice(
                &data[pos..end],
            ))),
            (end, data),
        ))
    });
    BodyExt::boxed(http_body_util::StreamBody::new(stream))
}

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

fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("fast_down_api_dl_{name}_{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn make_config(save_dir: &Path) -> PartialConfig {
    PartialConfig {
        save_dir: Some(save_dir.to_path_buf()),
        filename: Some("out.bin".to_string()),
        parse_filename: Some(false),
        overwrite: Some(true),
        write_method: Some(WriteMethod::Mmap),
        threads: Some(8),
        ..Default::default()
    }
}

async fn start_server(body: Vec<u8>, etag: &str, supports_range: bool) -> (TestServer, String) {
    start_server_throttled(body, etag, supports_range, None).await
}

async fn start_server_throttled(
    body: Vec<u8>,
    etag: &str,
    supports_range: bool,
    throttle: Option<Duration>,
) -> (TestServer, String) {
    let server = TestServer {
        data: Arc::new(RwLock::new(FileData {
            body,
            etag: etag.to_string(),
            last_modified: "Wed, 21 Oct 2026 07:28:00 GMT".to_string(),
            supports_range,
            throttle,
        })),
    };
    let url = server.serve().await;
    (server, url)
}

/// Resolve once to learn the output path and identity, then discard the plan.
async fn probe(url: &str, dir: &Path) -> (PathBuf, fast_down::UrlInfo) {
    let (tx, _rx) = create_channel();
    let ready = State::new(Url::parse(url).unwrap(), make_config(dir))
        .build(tx)
        .await
        .expect("probe build");
    (ready.final_path().to_path_buf(), ready.info().clone())
}

/// Seed a `.fd` + `.part` pair as if a previous run had already fetched the
/// first `progress_end` bytes of `payload`, using `etag` as the recorded
/// identity. The `.part` is written with the *real* payload prefix, so a resume
/// that only fetches the remainder still reconstructs the full file.
async fn seed_partial(
    url: &str,
    dir: &Path,
    final_path: &Path,
    info: &fast_down::UrlInfo,
    etag: &str,
    payload: &[u8],
    progress_end: u64,
) -> PathBuf {
    let fd = final_path.with_added_extension("fd");
    let tmp = tmp_path_for(&fd);
    let mut record = Record::from_url(Url::parse(url).unwrap(), make_config(dir));
    record.refresh_identity(info);
    record.etag = Some(etag.into());
    record.set_progress(vec![0..progress_end]);
    record.store(&fd).await.expect("seed .fd");
    tokio::fs::write(&tmp, &payload[..progress_end as usize])
        .await
        .expect("seed .part");
    fd
}

/// Drive `start` to completion and return the outcome.
async fn run_to_end(rx: Rx) -> TerminationReason {
    while let Ok(event) = rx.recv().await {
        if let Event::Terminated(reason) = event {
            return reason;
        }
    }
    panic!("channel closed without Terminated");
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_download_completes_and_renames() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 251) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("fresh");

    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(tx.clone())
        .await
        .expect("build");
    assert!(matches!(ready.resume_outcome(), ResumeOutcome::Fresh));
    let final_path = ready.final_path().to_path_buf();

    let drain = tokio::spawn(run_to_end(rx));
    ready.start(StartMode::Auto, tx, token).await;
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);

    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
    assert!(
        !final_path.with_added_extension("fd").exists(),
        "the .fd is removed once the file lands"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_from_seeded_state() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i * 7 % 251) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("resume");
    let (final_path, info) = probe(&url, &dir).await;
    seed_partial(&url, &dir, &final_path, &info, "v1", &payload, PARTIAL).await;

    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(tx.clone())
        .await
        .expect("build");
    assert!(
        matches!(ready.resume_outcome(), ResumeOutcome::Resumable),
        "a valid .fd + long enough .part must resolve as resumable"
    );

    let drain = tokio::spawn(run_to_end(rx));
    ready.start(StartMode::Auto, tx, token).await;
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_remote_is_mismatch_and_resume_mode_refuses() {
    let payload: Vec<u8> = vec![0xAB; FILE_SIZE];
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("mismatch");
    let (final_path, info) = probe(&url, &dir).await;
    // Recorded identity differs from the server's: a stale state.
    seed_partial(&url, &dir, &final_path, &info, "old", &payload, PARTIAL).await;

    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(tx.clone())
        .await
        .expect("build");
    assert!(
        matches!(ready.resume_outcome(), ResumeOutcome::Mismatch(_)),
        "a changed remote must be reported as a mismatch, not silently resumable"
    );

    let drain = tokio::spawn(run_to_end(rx));
    ready.start(StartMode::Resume, tx, token).await;
    assert_eq!(drain.await.unwrap(), TerminationReason::Failed);
    assert!(!final_path.exists(), "a refused resume must not rename");
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_part_is_mismatch() {
    let payload: Vec<u8> = vec![0xCD; FILE_SIZE];
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("truncated");
    let (final_path, info) = probe(&url, &dir).await;
    let fd = seed_partial(&url, &dir, &final_path, &info, "v1", &payload, PARTIAL).await;
    // Shrink the `.part` below what the state claims.
    let tmp = tmp_path_for(&fd);
    tokio::fs::write(&tmp, vec![0u8; 16]).await.unwrap();

    let (tx, _rx) = create_channel();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(tx)
        .await
        .expect("build");
    assert!(
        matches!(ready.resume_outcome(), ResumeOutcome::Mismatch(_)),
        "a truncated .part must not be resumed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_mode_redownloads_and_completes() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 97) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("fresh_mode");
    let (final_path, info) = probe(&url, &dir).await;
    seed_partial(&url, &dir, &final_path, &info, "v1", &payload, PARTIAL).await;

    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(tx.clone())
        .await
        .expect("build");
    assert!(matches!(ready.resume_outcome(), ResumeOutcome::Resumable));

    let drain = tokio::spawn(run_to_end(rx));
    ready.start(StartMode::Fresh, tx, token).await;
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_keeps_state_and_resume_completes() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 251) as u8).collect();
    // Throttled so the run cannot finish before the cancel lands mid-transfer.
    let (_server, url) =
        start_server_throttled(payload.clone(), "v1", true, Some(Duration::from_millis(80))).await;
    let dir = temp_dir("cancel_resume");
    let (final_path, info) = probe(&url, &dir).await;
    // Seed a *complete* run's worth would finish too fast, so seed nothing and
    // let the throttled full download be interrupted instead.
    let _ = info;

    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let cancel = token.clone();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(tx.clone())
        .await
        .expect("build");
    assert!(matches!(ready.resume_outcome(), ResumeOutcome::Fresh));
    let drain = tokio::spawn(run_to_end(rx));
    let watcher = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    ready.start(StartMode::Auto, tx, token).await;
    watcher.await.unwrap();
    let reason = drain.await.unwrap();
    assert_eq!(
        reason,
        TerminationReason::Cancelled,
        "a mid-transfer cancel must report Cancelled"
    );

    // The `.fd` survives so a later run continues and completes the file.
    let fd = final_path.with_added_extension("fd");
    assert!(fd.exists(), "cancel must leave the .fd behind");
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(tx.clone())
        .await
        .expect("build");
    assert!(matches!(ready.resume_outcome(), ResumeOutcome::Resumable));
    let drain = tokio::spawn(run_to_end(rx));
    ready.start(StartMode::Auto, tx, token).await;
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_guard() {
    let _ = timeout(
        Duration::from_millis(10),
        tokio::time::sleep(Duration::from_secs(60)),
    )
    .await;
}

/// Locks in that `Ready::start`'s future is `Send`: this compiles only if it is,
/// because `tokio::spawn` requires `Send + 'static`. Guards against a future
/// refactor quietly reintroducing the `async fn` auto-trait inference gap.
#[test]
fn start_future_is_send() {
    fn assert_send_future<F: std::future::Future + Send + 'static>(_: F) {}
    fn check(
        ready: fast_down_api::Ready,
        mode: StartMode,
        tx: fast_down_api::Tx,
        token: tokio_util::sync::CancellationToken,
    ) {
        assert_send_future(async move {
            ready.start(mode, tx, token).await;
        });
    }
    let _ = check;
}

/// A hand-built [`StartPolicy`] is accepted by `start` exactly like a
/// [`StartMode`] preset, and the per-dimension override takes effect. Here a
/// truncated `.part` is made to `Fail` (instead of the preset's silent
/// re-download), proving the field is honored.
#[tokio::test(flavor = "multi_thread")]
async fn start_policy_per_dimension_override() {
    use fast_down_api::{Recovery, StartPolicy};

    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 251) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("policy_override");

    // Seed a valid `.fd`/`.part`, then shrink the `.part` below the recorded
    // progress so the state is a `Mismatch(Truncated)`.
    let (final_path, info) = probe(&url, &dir).await;
    let fd = seed_partial(
        &url,
        &dir,
        &final_path,
        &info,
        "v1",
        &payload,
        FILE_SIZE as u64 / 2,
    )
    .await;
    let tmp = tmp_path_for(&fd);
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .await
        .unwrap();
    file.set_len(16).await.unwrap();
    drop(file);

    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let ready = State::load(&fd)
        .await
        .unwrap()
        .build(tx.clone())
        .await
        .unwrap();
    assert!(
        matches!(ready.resume_outcome(), ResumeOutcome::Mismatch(_)),
        "precondition: the shrunk .part must be a mismatch"
    );

    // Default `Auto` would silently re-download; this policy refuses instead.
    let policy = StartPolicy {
        truncated: Recovery::Fail,
        ..StartPolicy::auto()
    };
    let drain = tokio::spawn(run_to_end(rx));
    ready.start(policy, tx, token).await;
    assert_eq!(
        drain.await.unwrap(),
        TerminationReason::Failed,
        "truncated: Fail must stop the run"
    );
}
