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
    Ctx, Event, PartialConfig, PlanError, Record, ResumeOutcome, Rx, StartMode, State, StateError,
    TerminationReason, WriteMethod, create_channel,
};
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, COOKIE, ETAG, LAST_MODIFIED, RANGE, SET_COOKIE,
};
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
    /// When set, the response sends headers but the body never produces a byte.
    stall: bool,
    /// When set, every response carries `Set-Cookie: session=<value>` and any
    /// request that also asks for a `Range` must send it back or gets a 403.
    /// That splits probe traffic from download traffic the way a real session
    /// does: the probe earns the cookie, the range requests must reuse it.
    session: Option<&'static str>,
}

#[derive(Clone)]
struct TestServer {
    data: Arc<RwLock<FileData>>,
    /// How many requests carried a `Range` header, so a test can assert it
    /// actually exercised range traffic instead of the reused probe response.
    ranged_requests: Arc<AtomicU64>,
}

impl TestServer {
    /// How many requests carried a `Range` header so far.
    fn ranged_requests(&self) -> u64 {
        self.ranged_requests.load(Ordering::Relaxed)
    }

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
    let stall = data.stall;
    let session = data.session;
    drop(data);

    let range = req
        .headers()
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if range.is_some() {
        server.ranged_requests.fetch_add(1, Ordering::Relaxed);
    }

    // Range traffic must present the cookie the probe earned -- except the
    // `0-0` capability probe itself, which is issued concurrently with the
    // first request and therefore cannot have it yet.
    let parsed = range
        .as_deref()
        .and_then(|header| parse_range(header, total));
    if let Some(expected) = session
        && parsed.is_some()
        && parsed != Some((0, 1))
        && !has_session_cookie(&req, expected)
    {
        return Ok(Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(http_body_util::Full::new(Bytes::new()).boxed())
            .expect("403"));
    }
    let set_cookie = session.map(|value| format!("session={value}"));

    if supports_range
        && let Some((start, end)) = parsed
        && end <= total
        && start < end
    {
        let chunk = body[start..end].to_vec();
        let end_inclusive = end - 1;
        let mut builder = Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                CONTENT_RANGE,
                format!("bytes {start}-{end_inclusive}/{total}"),
            )
            .header(ACCEPT_RANGES, "bytes")
            .header(CONTENT_LENGTH, chunk.len().to_string())
            .header(ETAG, etag.as_str())
            .header(LAST_MODIFIED, last_modified.as_str());
        if let Some(value) = &set_cookie {
            builder = builder.header(SET_COOKIE, value.as_str());
        }
        return Ok(builder.body(body_of(chunk, throttle, stall)).expect("206"));
    }

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_LENGTH, body.len().to_string())
        .header(ETAG, etag.as_str())
        .header(LAST_MODIFIED, last_modified.as_str());
    if let Some(value) = &set_cookie {
        builder = builder.header(SET_COOKIE, value.as_str());
    }
    Ok(builder.body(body_of(body, throttle, stall)).expect("200"))
}

fn has_session_cookie(req: &Request<Incoming>, expected: &str) -> bool {
    let wanted = format!("session={expected}");
    req.headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').any(|part| part.trim() == wanted))
}

/// A response body: the whole `data` at once, or streamed in chunks with a
/// delay when `throttle` is set.
fn body_of(data: Vec<u8>, throttle: Option<Duration>, stall: bool) -> RespBody {
    if stall {
        let stream = futures::stream::pending::<Result<hyper::body::Frame<Bytes>, Infallible>>();
        return BodyExt::boxed(http_body_util::StreamBody::new(stream));
    }
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
    start_server_with(body, etag, supports_range, throttle, false).await
}

/// A server that answers with headers and then never sends a body byte.
async fn start_server_stalled(
    body: Vec<u8>,
    etag: &str,
    supports_range: bool,
) -> (TestServer, String) {
    start_server_with(body, etag, supports_range, None, true).await
}

async fn start_server_with(
    body: Vec<u8>,
    etag: &str,
    supports_range: bool,
    throttle: Option<Duration>,
    stall: bool,
) -> (TestServer, String) {
    let server = TestServer {
        data: Arc::new(RwLock::new(FileData {
            body,
            etag: etag.to_string(),
            last_modified: "Wed, 21 Oct 2026 07:28:00 GMT".to_string(),
            supports_range,
            throttle,
            stall,
            session: None,
        })),
        ranged_requests: Arc::new(AtomicU64::new(0)),
    };
    let url = server.serve().await;
    (server, url)
}

/// Resolve once to learn the output path and identity, then discard the plan.
async fn probe(url: &str, dir: &Path) -> (PathBuf, fast_down::UrlInfo) {
    let (tx, _rx) = create_channel();
    let ready = State::new(Url::parse(url).unwrap(), make_config(dir))
        .build(&tx)
        .await
        .expect("probe build");
    (ready.final_path, ready.info)
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
    let tmp = fd.with_extension("part");
    let mut record = Record::new(Url::parse(url).unwrap(), make_config(dir));
    record.size = info.size;
    record.last_modified = info.file_id.last_modified.clone();
    record.etag = Some(etag.into());
    record.set_progress(vec![0..progress_end]);
    record.store(&fd).await.expect("seed .fd");
    tokio::fs::write(&tmp, &payload[..progress_end as usize])
        .await
        .expect("seed .part");
    fd
}

/// Budget for one wait on a run's event channel. Generous — a throttled 1 MiB
/// body still lands in well under a second — but finite, so a run that stops
/// emitting without closing its channel fails the test that started it instead
/// of hanging CI until the job-wide timeout hits with no test name attached.
const RECV_BUDGET: Duration = Duration::from_secs(60);

/// Wait for the next event, `None` once the channel is closed, and fail the test
/// instead of hanging when the run goes quiet but never closes the channel.
///
/// # Panics
///
/// Panics when [`RECV_BUDGET`] elapses with the channel still open, naming the
/// hang: a leaked `Tx` clone, or a task that never terminates.
async fn next_event(rx: &Rx) -> Option<Event> {
    let recv = timeout(RECV_BUDGET, rx.recv()).await.unwrap_or_else(|_| {
        panic!(
            "no event for {}s and the channel is still open, so this test would \
             hang forever (a leaked `Tx` clone, or a task that never terminated)",
            RECV_BUDGET.as_secs()
        )
    });
    recv.ok()
}

/// Drive `start` to completion and return the outcome.
///
/// # Panics
///
/// Panics if the channel closes without a `Terminated`, or if the run stops
/// producing events while leaving the channel open (see [`next_event`]).
async fn run_to_end(rx: Rx) -> TerminationReason {
    while let Some(event) = next_event(&rx).await {
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
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(matches!(&ready.outcome, ResumeOutcome::Fresh));
    let final_path = ready.final_path.clone();

    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);

    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
    assert!(
        !final_path.with_added_extension("fd").exists(),
        "the .fd is removed once the file lands"
    );
}

/// A `.fd` left behind without its `.part` has nothing safe to continue, but it
/// still names the pair. A fresh start must reuse that `.fd` instead of leaving
/// it orphaned under a new `name (1)` stem.
#[tokio::test(flavor = "multi_thread")]
async fn fresh_start_reuses_the_fd_whose_part_is_missing() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 233) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("missing_part");
    let (final_path, info) = probe(&url, &dir).await;
    let fd = final_path.with_added_extension("fd");
    let part = fd.with_extension("part");

    let mut record = Record::new(Url::parse(&url).unwrap(), make_config(&dir));
    record.size = info.size;
    record.last_modified = info.file_id.last_modified.clone();
    record.etag = Some("v1".into());
    record.set_progress(vec![0..PARTIAL]);
    record.store(&fd).await.expect("seed .fd");
    assert!(!part.exists(), "the seed must not leave a `.part`");

    let (tx, rx) = create_channel();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(matches!(&ready.outcome, ResumeOutcome::Fresh));
    assert_eq!(
        ready.fd_path.as_deref(),
        Some(fd.as_path()),
        "the known `.fd` keeps its location even without a `.part`"
    );

    let ctx = Ctx::new();
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
    let mut started_fd = None;
    let mut outcome = None;
    while let Some(event) = next_event(&rx).await {
        match event {
            Event::Start { fd_path, .. } => started_fd = Some(fd_path),
            Event::Terminated(reason) => {
                outcome = Some(reason);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(outcome, Some(TerminationReason::Completed));
    assert_eq!(
        started_fd.as_deref(),
        Some(fd.as_path()),
        "the run must drive the `.fd` it probed, not a new stem"
    );
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

/// A source that sends headers and then never delivers a body must end the run
/// with [`Event::Stalled`] instead of retrying forever. Since it never records a
/// byte, it must not leave a 0% "resumable" pair behind either.
#[tokio::test(flavor = "multi_thread")]
async fn stalled_source_reports_stalled_and_ends() {
    let payload = vec![0x5Au8; FILE_SIZE];
    let (_server, url) = start_server_stalled(payload, "v1", true).await;
    let dir = temp_dir("stalled");

    let mut config = make_config(&dir);
    config.stall_timeout = Some(Duration::from_millis(200));
    config.pull_timeout = Some(Duration::from_millis(50));
    config.threads = Some(2);

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), config)
        .build(&tx)
        .await
        .expect("build");
    let drain = tokio::spawn(async move {
        let mut stalled = false;
        let mut reason = None;
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Stalled(_) => stalled = true,
                Event::Terminated(r) => reason = Some(r),
                _ => {}
            }
        }
        (stalled, reason)
    });
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
    let (stalled, reason) = timeout(Duration::from_secs(10), drain)
        .await
        .expect("a stalled source must end")
        .expect("drain task");
    assert!(stalled, "the run must report Event::Stalled");
    assert_eq!(reason, Some(TerminationReason::Incomplete));
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "a run that recorded no byte must not leave a resumable pair, found {leftovers:?}"
    );
}

/// The pipeline reports the actually-chosen writer and, with `pre_alloc` on, the
/// completion of the disk reservation. Guards the two events added on top of
/// `Allocating`.
#[tokio::test(flavor = "multi_thread")]
async fn reports_write_method_and_allocation() {
    let payload: Vec<u8> = vec![0x5A; FILE_SIZE];
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("write_method");

    let mut cfg = make_config(&dir);
    cfg.pre_alloc = Some(true);

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), cfg)
        .build(&tx)
        .await
        .expect("build");

    // Collect events while the run proceeds.
    let collect = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(ev) = next_event(&rx).await {
            let done = matches!(ev, Event::Terminated(_));
            seen.push(ev);
            if done {
                break;
            }
        }
        seen
    });
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
    let events = collect.await.unwrap();

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::WriteMethodChosen(WriteMethod::Mmap))),
        "a resumable 64-bit Mmap download must report the Mmap writer"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Allocating(n) if *n == FILE_SIZE as u64)),
        "pre_alloc must announce the reservation"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Allocated(n) if *n == FILE_SIZE as u64)),
        "a successful reservation must report completion"
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
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(
        matches!(&ready.outcome, ResumeOutcome::Resumable),
        "a valid .fd + long enough .part must resolve as resumable"
    );

    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
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
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(
        matches!(&ready.outcome, ResumeOutcome::Mismatch(_)),
        "a changed remote must be reported as a mismatch, not silently resumable"
    );

    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Resume, tx, &ctx);
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
    let tmp = fd.with_extension("part");
    tokio::fs::write(&tmp, vec![0u8; 16]).await.unwrap();

    let (tx, _rx) = create_channel();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(
        matches!(&ready.outcome, ResumeOutcome::Mismatch(_)),
        "a truncated .part must not be resumed"
    );
}

/// A forced resume accepts the remote's identity change only because the size
/// still matches, so the recorded ranges still describe the bytes it continues.
/// The `.fd` it leaves behind keeps the identity it was seeded with: adopting the
/// remote's would launder a spliced pair into something a later `Auto` run
/// resumes, hiding the fact that the first half came from another version. Such a
/// run must see the change and download the whole file instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_resume_keeps_the_recorded_identity() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 241) as u8).collect();
    // Slow enough that the cancel below lands before the transfer completes.
    let (_server, url) =
        start_server_throttled(payload.clone(), "v2", true, Some(Duration::from_millis(50))).await;
    let dir = temp_dir("forced_identity");
    let (final_path, info) = probe(&url, &dir).await;
    let fd = seed_partial(&url, &dir, &final_path, &info, "v1", &payload, PARTIAL).await;

    // Both identity headers differ from the remote, so keeping them is visible.
    let stale_last_modified = "Tue, 01 Jan 2019 00:00:00 GMT";
    let mut seeded = Record::load(&fd).await.expect("read seeded .fd");
    seeded.last_modified = Some(stale_last_modified.into());
    seeded.store(&fd).await.expect("rewrite seeded .fd");

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(
        matches!(
            &ready.outcome,
            ResumeOutcome::Mismatch(StateError::FileChanged { .. })
        ),
        "precondition: same size, different identity, got {:?}",
        ready.outcome
    );

    let handle = ready.start(StartMode::Forced, tx, &ctx);
    // Stop right after the run announces itself, before its bytes arrive.
    loop {
        match rx.recv().await {
            Ok(Event::Start { .. }) => {
                ctx.cancel();
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("the run ended before it started: {e:?}"),
        }
    }
    while rx.recv().await.is_ok() {}
    assert_eq!(handle.await, TerminationReason::Cancelled);

    let record = Record::load(&fd)
        .await
        .expect("an interrupted forced run keeps its .fd");
    assert_eq!(
        record.etag.as_deref(),
        Some("v1"),
        "the forced resume must not adopt the remote ETag: the early bytes belong to the old one"
    );
    assert_eq!(
        record.last_modified.as_deref(),
        Some(stale_last_modified),
        "the forced resume must not adopt the remote Last-Modified either"
    );

    // The identity is still the old one, so the next run cannot mistake the pair
    // for the current file: `Auto` re-downloads from zero instead of continuing
    // the splice.
    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(
        !matches!(&ready.outcome, ResumeOutcome::Resumable),
        "a pair left by a forced resume must not validate as resumable, got {:?}",
        ready.outcome
    );
    let final_path = ready.final_path.clone();
    let handle = ready.start(StartMode::Auto, tx, &ctx);
    while rx.recv().await.is_ok() {}
    assert_eq!(handle.await, TerminationReason::Completed);
    assert_eq!(
        tokio::fs::read(&final_path).await.expect("read final file"),
        payload,
        "the whole file must come from the remote, not from the stale prefix"
    );
}

/// A `.part` shorter than the recorded progress cannot be continued, even when
/// the recorded identity also changed: `Forced` accepts the latter, and must not
/// turn the former into a resume that pads the missing span with zeros and then
/// reports completion.
#[tokio::test(flavor = "multi_thread")]
async fn forced_resume_refuses_a_truncated_part_when_the_identity_changed() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 241) as u8).collect();
    // Same size, different identity: that alone is exactly the case a forced
    // resume is meant to accept.
    let (_server, url) = start_server(payload.clone(), "v2", true).await;
    let dir = temp_dir("forced_truncated");
    let (final_path, info) = probe(&url, &dir).await;
    let fd = seed_partial(&url, &dir, &final_path, &info, "v1", &payload, PARTIAL).await;
    let part = fd.with_extension("part");
    let short: Vec<u8> = (0..16u8).map(|b| b.wrapping_mul(17)).collect();
    tokio::fs::write(&part, &short).await.unwrap();

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(
        matches!(
            &ready.outcome,
            ResumeOutcome::Mismatch(StateError::Truncated { .. })
        ),
        "a short .part must be reported as truncated even when the identity \
         changed too, got {:?}",
        ready.outcome
    );

    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Forced, tx, &ctx);
    assert_eq!(
        drain.await.unwrap(),
        TerminationReason::Failed,
        "a forced resume must refuse a truncated .part"
    );
    assert_eq!(
        tokio::fs::read(&part).await.unwrap(),
        short,
        "a refused forced resume must not touch the .part"
    );
    assert!(
        !final_path.exists(),
        "a refused forced resume must not rename anything into place"
    );

    // The same state is still a plain truncation for `Auto`, which re-downloads
    // the whole file instead of failing.
    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(matches!(
        &ready.outcome,
        ResumeOutcome::Mismatch(StateError::Truncated { .. })
    ));
    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);
    assert_eq!(
        tokio::fs::read(&final_path).await.unwrap(),
        payload,
        "Auto must still redownload the whole file"
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
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(matches!(&ready.outcome, ResumeOutcome::Resumable));

    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Fresh, tx, &ctx);
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_keeps_state_and_resume_completes() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 251) as u8).collect();
    // Throttled so the run cannot finish before the cancel lands mid-transfer,
    // and single-stream: with the pool split across workers the whole file can
    // land in a few tens of milliseconds, which is the same order as the cancel
    // round trip and made this assertion flaky.
    let (_server, url) = start_server_throttled(
        payload.clone(),
        "v1",
        true,
        Some(Duration::from_millis(100)),
    )
    .await;
    let dir = temp_dir("cancel_resume");
    let (final_path, info) = probe(&url, &dir).await;
    // Seed a *complete* run's worth would finish too fast, so seed nothing and
    // let the throttled full download be interrupted instead.
    let _ = info;
    let mut first_run = make_config(&dir);
    first_run.threads = Some(1);

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), first_run)
        .build(&tx)
        .await
        .expect("build");
    assert!(matches!(&ready.outcome, ResumeOutcome::Fresh));
    // Cancel once a chunk has actually reached the sink instead of after a fixed
    // delay: a timer races the pipeline build, and a cancel that lands before a
    // byte is recorded drops the empty pair this run created rather than keeping
    // it resumable. The throttle leaves the remaining chunks well over a cancel
    // round trip away.
    let drain = tokio::spawn({
        let cancel = ctx.clone();
        async move {
            let mut pushed = 0u64;
            while let Ok(event) = rx.recv().await {
                if let Event::PushProgress(range) = &event
                    && range.start < range.end
                {
                    pushed = pushed.saturating_add(range.end - range.start);
                    cancel.cancel();
                }
                if let Event::Terminated(reason) = event {
                    return (reason, pushed);
                }
            }
            panic!("the run must report Terminated");
        }
    });
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
    let (reason, pushed) = timeout(Duration::from_secs(10), drain)
        .await
        .expect("a chunk must reach the sink")
        .expect("drain task");
    assert_eq!(
        reason,
        TerminationReason::Cancelled,
        "a mid-transfer cancel must report Cancelled"
    );
    // Without this the test degrades silently: a cancel that only lands after the
    // whole file reached the sink still reports `Cancelled`, and the resume below
    // would then re-check a complete pair instead of a partial one.
    assert!(
        pushed < FILE_SIZE as u64,
        "the cancel must land mid-transfer, but {pushed} of {FILE_SIZE} bytes \
         had already reached the sink when it fired"
    );

    // The `.fd` survives so a later run continues and completes the file.
    let fd = final_path.with_added_extension("fd");
    assert!(fd.exists(), "cancel must leave the .fd behind");
    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(matches!(&ready.outcome, ResumeOutcome::Resumable));
    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

/// A cancel that lands before provisioning must report
/// [`Cancelled`](TerminationReason::Cancelled) and leave no `.fd`/`.part`
/// behind: `drive` returns before it claims a pair.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_before_provisioning_reports_cancelled_without_files() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 211) as u8).collect();
    let (_server, url) = start_server(payload, "v1", true).await;
    let dir = temp_dir("cancel_build");

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    // Cancel before the run is spawned: `drive` checks the token before it
    // resolves the `.fd`/`.part` pair, so nothing is created.
    ctx.cancel();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Auto, tx, &ctx);
    assert_eq!(drain.await.unwrap(), TerminationReason::Cancelled);

    let leftovers: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read save dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("fd" | "part")
            )
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "a cancelled-before-start run must not leave a state pair: {leftovers:?}"
    );
}

/// A second run pinned to the same `.fd` while the first is still running must be
/// refused instead of interleaving writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_run_on_the_same_fd_is_refused() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 197) as u8).collect();
    // The body never produces a byte, so the first run cannot finish (and thus
    // cannot release the claim) before the second one tries to take it. A
    // throttled body would make this a race against the transfer's remaining
    // time, which a loaded machine can lose.
    let (_server, url) = start_server_stalled(payload, "v1", true).await;
    let dir = temp_dir("claim");
    let fd = dir.join("out.bin").with_added_extension("fd");
    let url = Url::parse(&url).unwrap();

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::at_path(url.clone(), fd.clone(), make_config(&dir))
        .build(&tx)
        .await
        .expect("first build");
    let first = ready.start(StartMode::Auto, tx, &ctx);

    // Wait until the first run owns the pair.
    let mut started = false;
    while let Some(event) = next_event(&rx).await {
        if matches!(event, Event::Start { .. }) {
            started = true;
            break;
        }
    }
    assert!(started, "the first run must reach Start");

    let (tx2, rx2) = create_channel();
    let ctx2 = Ctx::new();
    let ready2 = State::at_path(url, fd, make_config(&dir))
        .build(&tx2)
        .await
        .expect("second build");
    let second = ready2.start(StartMode::Auto, tx2, &ctx2);
    let mut events = Vec::new();
    while let Some(event) = next_event(&rx2).await {
        events.push(event);
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AlreadyRunning(_))),
        "the second run must report the claim conflict, got {events:?}"
    );
    assert_eq!(second.await, TerminationReason::Failed);

    ctx.cancel();
    drop(first);
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_guard() {
    let _ = timeout(
        Duration::from_millis(10),
        tokio::time::sleep(Duration::from_secs(60)),
    )
    .await;
}

/// `Ready::start` hands back a `RunHandle` that is `Send + Sync`, so it can be
/// moved into another task and shared for control.
#[test]
fn run_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<fast_down_api::RunHandle>();
    fn check(
        ready: fast_down_api::Ready,
        mode: StartMode,
        tx: fast_down_api::Tx,
        ctx: &fast_down_api::Ctx,
    ) -> fast_down_api::RunHandle {
        ready.start(mode, tx, ctx)
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
    let tmp = fd.with_extension("part");
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .await
        .unwrap();
    file.set_len(16).await.unwrap();
    drop(file);

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::at_path(Url::parse(&url).unwrap(), fd, make_config(&dir))
        .build(&tx)
        .await
        .unwrap();
    assert!(
        matches!(&ready.outcome, ResumeOutcome::Mismatch(_)),
        "precondition: the shrunk .part must be a mismatch"
    );

    // Default `Auto` would silently re-download; this policy refuses instead.
    let policy = StartPolicy {
        truncated: Recovery::Fail,
        ..StartPolicy::auto()
    };
    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(policy, tx, &ctx);
    assert_eq!(
        drain.await.unwrap(),
        TerminationReason::Failed,
        "truncated: Fail must stop the run"
    );
}

/// A `set_threads` / `set_min_chunk_size` issued *before* the pipeline exists is
/// remembered and applied once it is built, and the download still completes.
#[tokio::test(flavor = "multi_thread")]
async fn ctx_resizes_pool_before_start() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 233) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("ctx_resize");

    let (tx, _rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    let final_path = ready.final_path.clone();

    // Issued before the session exists: both must be remembered and applied.
    ctx.set_min_chunk_size(4096);
    ctx.set_threads(8);
    let handle = ready.start(StartMode::Auto, tx, &ctx);
    // Also exercisable through the handle, which delegates to the same `Ctx`.
    handle.set_threads(4);

    assert_eq!(handle.await, TerminationReason::Completed);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

/// Changing the minimum chunk size and pool size *while workers are running*
/// must be accepted, reach the live pool, and leave the download intact.
#[tokio::test(flavor = "multi_thread")]
async fn ctx_sets_min_chunk_size_midflight() {
    let payload: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 233) as u8).collect();
    // Throttle the body so the transfer is still in flight when we mutate the
    // live granularity and pool size.
    let (_server, url) =
        start_server_throttled(payload.clone(), "v1", true, Some(Duration::from_millis(20))).await;
    let dir = temp_dir("ctx_min_midflight");

    // Small chunks so several workers are actually running when the setters fire.
    let mut cfg = make_config(&dir);
    cfg.min_chunk_size = Some(64 * 1024);
    cfg.threads = Some(4);

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), cfg)
        .build(&tx)
        .await
        .expect("build");
    let final_path = ready.final_path.clone();

    let handle = ready.start(StartMode::Auto, tx, &ctx);

    // Wait until the transfer has started, then resize the live pool.
    let mut saw_pull = false;
    while let Some(event) = next_event(&rx).await {
        if matches!(event, Event::PullProgress(..)) {
            saw_pull = true;
            break;
        }
    }
    assert!(
        saw_pull,
        "expected a pull before mutating the live granularity"
    );
    ctx.set_min_chunk_size(4096);
    ctx.set_threads(4);

    assert_eq!(handle.await, TerminationReason::Completed);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

/// Drive both live setters at high frequency through [`RunHandle`] while a
/// throttled download is in flight, re-pulling whenever a shrink aborts a
/// worker. The download must still finish and land the exact payload.
///
/// The churn loop runs for as long as the transfer is *partial* (it stops once
/// the sink has the whole payload), so every resize lands on a live pool. A
/// fixed step count would silently degrade into resizing an already-finished
/// session once the payload is small enough to outrun the loop.
#[tokio::test(flavor = "multi_thread")]
async fn handle_churns_both_setters_midflight() {
    const SIZE: usize = 2 * 1024 * 1024;
    let payload: Vec<u8> = (0..SIZE).map(|i| (i % 233) as u8).collect();
    let (_server, url) =
        start_server_throttled(payload.clone(), "v1", true, Some(Duration::from_millis(2))).await;
    let dir = temp_dir("ctx_churn");

    let mut cfg = make_config(&dir);
    cfg.min_chunk_size = Some(1024);
    cfg.threads = Some(4);

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), cfg)
        .build(&tx)
        .await
        .expect("build");
    let final_path = ready.final_path.clone();

    let handle = Arc::new(ready.start(StartMode::Auto, tx, &ctx));

    // Drain while tracking how many bytes have reached the sink, so the churn
    // loops below can tell whether the pool is still live.
    let written = Arc::new(AtomicU64::new(0));
    let w = written.clone();
    let drain = tokio::spawn(async move {
        while let Some(event) = next_event(&rx).await {
            match event {
                Event::PushProgress(p) => {
                    w.fetch_add(p.end - p.start, Ordering::Relaxed);
                }
                Event::Terminated(reason) => return reason,
                _ => {}
            }
        }
        panic!("channel closed without Terminated");
    });

    // Two detached tasks, one per setter, so the two cell writes race the
    // workers independently instead of being serialized by a single churn loop.
    // The values vary so the granularity crosses the split boundary and re-pulls
    // actually happen. Each task counts the resizes that landed while the
    // transfer was still in flight; the assertions below require at least one.
    let size = SIZE as u64;
    let t_handle = handle.clone();
    let t_written = written.clone();
    let t_churn = tokio::spawn(async move {
        let mut inflight = 0u64;
        let mut i = 0usize;
        while t_written.load(Ordering::Relaxed) < size {
            t_handle.set_threads(i % 8 + 1);
            if t_written.load(Ordering::Relaxed) < size {
                inflight += 1;
            }
            i += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        inflight
    });
    let m_handle = handle.clone();
    let m_written = written.clone();
    let m_churn = tokio::spawn(async move {
        let mut inflight = 0u64;
        let mut i = 0usize;
        while m_written.load(Ordering::Relaxed) < size {
            let min = match i % 4 {
                0 => 1,
                1 => 512,
                2 => 4096,
                _ => 64 * 1024,
            };
            m_handle.set_min_chunk_size(min);
            if m_written.load(Ordering::Relaxed) < size {
                inflight += 1;
            }
            i += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        inflight
    });

    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);
    let threads_inflight = t_churn.await.unwrap();
    let min_inflight = m_churn.await.unwrap();
    assert!(
        threads_inflight > 0 && min_inflight > 0,
        "both setters must land at least one resize while the transfer is live \
         (threads={threads_inflight}, min_chunk_size={min_inflight})"
    );

    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

/// A `.fd` pinned by path is self-contained: resuming with the crate defaults
/// needs no `save_dir`, and the finished file lands next to the state file.
#[tokio::test(flavor = "multi_thread")]
async fn resume_with_the_crate_defaults_lands_next_to_the_fd() {
    let payload: Vec<u8> = (0..4096usize).map(|i| (i % 251) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("resume_defaults");
    let fd = dir.join("out.bin").with_added_extension("fd");
    let recorded = PartialConfig {
        save_dir: Some(dir.clone()),
        filename: Some("out.bin".to_string()),
        parse_filename: Some(false),
        ..PartialConfig::default()
    };
    Record::new(Url::parse(&url).unwrap(), recorded)
        .store(&fd)
        .await
        .expect("write the .fd");

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::at_path(
        Url::parse(&url).unwrap(),
        fd.clone(),
        PartialConfig::default(),
    )
    .build(&tx)
    .await
    .expect("resuming with the crate defaults must resolve a path");
    let canon = |p: &Path| std::fs::canonicalize(p).expect("canonicalize");
    assert_eq!(
        canon(ready.final_path.parent().unwrap()),
        canon(fd.parent().unwrap()),
        "the destination is resolved under the .fd's directory"
    );

    let handle = ready.start(StartMode::Auto, tx, &ctx);
    while rx.recv().await.is_ok() {}
    assert_eq!(handle.await, TerminationReason::Completed);

    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(
        leftovers.len(),
        1,
        "only the finished file remains: {leftovers:?}"
    );
    assert_eq!(
        std::fs::read(dir.join(leftovers[0].clone())).unwrap(),
        payload
    );
}

/// A `.fd` remembers where its download was heading: resuming under the crate
/// defaults must land the file under the recorded `filename`, not under the name
/// the freshly probed URL would suggest.
#[tokio::test(flavor = "multi_thread")]
async fn resume_takes_the_filename_from_the_fd() {
    let payload: Vec<u8> = (0..4096usize).map(|i| (i % 251) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("resume_recorded_name");
    let (_, info) = probe(&url, &dir).await;

    // The pair keeps its own `<stem>.fd`/`<stem>.part` names; only the finished
    // file is called `custom.mkv`.
    let fd = dir.join("out.bin").with_added_extension("fd");
    let tmp = fd.with_extension("part");
    let recorded = PartialConfig {
        save_dir: Some(dir.clone()),
        filename: Some("custom.mkv".to_string()),
        parse_filename: Some(false),
        ..PartialConfig::default()
    };
    let mut record = Record::new(Url::parse(&url).unwrap(), recorded);
    record.size = info.size;
    record.etag = info.file_id.etag.clone();
    record.last_modified = info.file_id.last_modified.clone();
    let progress = payload.len() as u64 / 2;
    record.set_progress(vec![0..progress]);
    record.store(&fd).await.expect("seed .fd");
    std::fs::write(&tmp, &payload[..progress as usize]).expect("seed .part");

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::at_path(
        Url::parse(&url).unwrap(),
        fd.clone(),
        PartialConfig::default(),
    )
    .build(&tx)
    .await
    .expect("resuming with the crate defaults must resolve a path");
    assert!(
        matches!(&ready.outcome, ResumeOutcome::Resumable),
        "the seeded identity and progress must be resumable, got {:?}",
        ready.outcome
    );
    assert_eq!(
        ready.final_path.file_name().and_then(|n| n.to_str()),
        Some("custom.mkv"),
        "the recorded filename decides the output name: {:?}",
        ready.final_path
    );

    let handle = ready.start(StartMode::Auto, tx, &ctx);
    while rx.recv().await.is_ok() {}
    assert_eq!(handle.await, TerminationReason::Completed);

    assert_eq!(std::fs::read(dir.join("custom.mkv")).unwrap(), payload);
    assert!(
        !fd.exists(),
        "the .fd must be removed after a completed resume"
    );
}

/// `save_dir` is a new-download setting: a pinned `.fd` keeps its pair where it
/// is instead of relocating the output to whatever the caller configured.
#[tokio::test(flavor = "multi_thread")]
async fn resume_ignores_a_conflicting_save_dir() {
    let payload = vec![0x2Bu8; 2048];
    let (_server, url) = start_server(payload, "v1", true).await;
    let pair_dir = temp_dir("resume_pair");
    let other_dir = temp_dir("resume_other");
    let fd = pair_dir.join("out.bin").with_added_extension("fd");
    let recorded = PartialConfig {
        save_dir: Some(pair_dir.clone()),
        filename: Some("out.bin".to_string()),
        parse_filename: Some(false),
        ..PartialConfig::default()
    };
    Record::new(Url::parse(&url).unwrap(), recorded)
        .store(&fd)
        .await
        .expect("write the .fd");

    let (tx, _rx) = create_channel();
    let ready = State::at_path(
        Url::parse(&url).unwrap(),
        fd.clone(),
        make_config(&other_dir),
    )
    .build(&tx)
    .await
    .expect("build");

    let canon = |p: &Path| std::fs::canonicalize(p).expect("canonicalize");
    assert_eq!(
        canon(ready.final_path.parent().unwrap()),
        canon(fd.parent().unwrap()),
        "the pair decides the directory, not save_dir: {}",
        ready.final_path.display()
    );
    assert!(
        std::fs::read_dir(&other_dir)
            .expect("read dir")
            .next()
            .is_none(),
        "nothing is planned inside the caller's unrelated save_dir"
    );
}

/// A new download has no pair to inherit a directory from, so an empty
/// `save_dir` is reported as such instead of failing to canonicalize "".
#[tokio::test(flavor = "multi_thread")]
async fn a_new_download_without_save_dir_says_so() {
    let (_server, url) = start_server(vec![1u8; 64], "v1", true).await;
    let (tx, _rx) = create_channel();
    let Err(err) = State::new(Url::parse(&url).unwrap(), PartialConfig::default())
        .build(&tx)
        .await
    else {
        panic!("a new download without save_dir must not resolve a path");
    };
    assert!(
        matches!(err, fast_down_api::PlanError::MissingSaveDir),
        "the failure names the missing setting: {err}"
    );
    assert!(
        err.to_string().contains("save_dir"),
        "the message names the missing setting: {err}"
    );
}

/// A policy that refuses a missing state file must say *which* `.fd` it looked
/// for before ending `Failed`: a `Fresh` outcome used to emit no `ResumeError`
/// at all, leaving only `Terminated(Failed)`.
///
/// The `Resume` preset deliberately falls back for `no_state` (see
/// [`StartPolicy::resume`]: "nothing safe to continue" is not a mismatch), so
/// the reachable way to demand a refusal is the per-dimension policy below.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_state_failure_names_the_probed_fd() {
    use fast_down_api::{Recovery, StartPolicy};

    let (_server, url) = start_server(vec![0x42u8; 128], "v1", true).await;
    let dir = temp_dir("resume_no_state");

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(matches!(&ready.outcome, ResumeOutcome::Fresh));
    // The `.fd` the probe would have continued from, though it is absent.
    let expected = ready.final_path.with_added_extension("fd");
    assert!(!expected.exists(), "precondition: no state file exists");

    let policy = StartPolicy {
        no_state: Recovery::Fail,
        ..StartMode::Resume.into()
    };
    let handle = ready.start(policy, tx, &ctx);

    let mut events = Vec::new();
    while let Some(event) = next_event(&rx).await {
        let done = matches!(event, Event::Terminated(_));
        events.push(event);
        if done {
            break;
        }
    }
    assert_eq!(handle.await, TerminationReason::Failed);

    let error_at = events
        .iter()
        .position(|event| {
            matches!(event, Event::ResumeError(StateError::NotFound(path)) if path == &expected)
        })
        .unwrap_or_else(|| {
            panic!("expected ResumeError(NotFound({expected:?})) before Terminated, got {events:?}")
        });
    let terminated_at = events
        .iter()
        .position(|event| matches!(event, Event::Terminated(TerminationReason::Failed)))
        .expect("the run must end Failed");
    assert!(
        error_at < terminated_at,
        "the refusal must be reported before the terminal event, got {events:?}"
    );
}

/// The `Resume` preset keeps its documented fallback for a missing `.fd`:
/// "nothing safe to continue" is not a mismatch, so it downloads the whole
/// file. Pinned here next to the refusal above so the two behaviors cannot
/// drift into each other.
#[tokio::test(flavor = "multi_thread")]
async fn resume_mode_without_state_falls_back_to_a_full_download() {
    let payload: Vec<u8> = (0..256usize).map(|i| (i % 251) as u8).collect();
    let (_server, url) = start_server(payload.clone(), "v1", true).await;
    let dir = temp_dir("resume_no_state_fallback");

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::new(Url::parse(&url).unwrap(), make_config(&dir))
        .build(&tx)
        .await
        .expect("build");
    assert!(matches!(&ready.outcome, ResumeOutcome::Fresh));
    let final_path = ready.final_path.clone();

    let drain = tokio::spawn(run_to_end(rx));
    let _handle = ready.start(StartMode::Resume, tx, &ctx);
    assert_eq!(drain.await.unwrap(), TerminationReason::Completed);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

/// A pinned state file whose name already ends in `.part` derives itself as its
/// own `.part`, so a run would write the download over its own state: `build`
/// must refuse the plan instead of resolving it.
#[tokio::test(flavor = "multi_thread")]
async fn a_fd_path_that_is_its_own_part_is_refused() {
    let (_server, url) = start_server(vec![1u8; 64], "v1", true).await;
    let dir = temp_dir("bad_fd_part");
    let fd = dir.join("out.part");

    let (tx, _rx) = create_channel();
    let err = State::at_path(Url::parse(&url).unwrap(), fd.clone(), make_config(&dir))
        .build(&tx)
        .await
        .expect_err("a .fd that is also its own .part must not resolve");
    assert!(
        matches!(&err, PlanError::BadStatePath(path) if path == &fd),
        "expected PlanError::BadStatePath({fd:?}), got {err:?}"
    );
    assert!(
        err.to_string().contains("out.part"),
        "the failure must name the colliding path, got: {err}"
    );
}

/// A `filename` equal to the `.fd`'s file name makes the output *be* the state
/// file, so `finalize`'s rename would replace the `.fd` with the finished file:
/// `build` must refuse the plan instead of resolving it.
#[tokio::test(flavor = "multi_thread")]
async fn a_filename_equal_to_the_fd_name_is_refused() {
    let (_server, url) = start_server(vec![1u8; 64], "v1", true).await;
    let dir = temp_dir("bad_fd_output");
    // Canonicalize before deriving anything: `gen_path` canonicalizes the
    // `save_dir` itself, and comparing a plain spelling with its verbatim
    // (`\\?\`) twin is exactly the case the check must see through.
    let canonical = std::fs::canonicalize(&dir).expect("canonicalize temp dir");
    let fd = canonical.join("state.fd");
    let mut cfg = make_config(&dir);
    cfg.filename = Some("state.fd".to_string());
    cfg.parse_filename = Some(false);

    let (tx, _rx) = create_channel();
    let err = State::at_path(Url::parse(&url).unwrap(), fd.clone(), cfg)
        .build(&tx)
        .await
        .expect_err("an output named like the .fd must not resolve");
    assert!(
        matches!(&err, PlanError::BadStatePath(path) if path == &fd),
        "expected PlanError::BadStatePath({fd:?}), got {err:?}"
    );
}

/// A server whose responses set a session cookie and whose *ranged* responses
/// require it back, so probe traffic and download traffic are split the way a
/// real session splits them.
async fn start_server_with_session(
    body: Vec<u8>,
    etag: &str,
    session: &'static str,
) -> (TestServer, String) {
    let server = TestServer {
        data: Arc::new(RwLock::new(FileData {
            body,
            etag: etag.to_string(),
            last_modified: "Wed, 21 Oct 2026 07:28:00 GMT".to_string(),
            supports_range: true,
            throttle: None,
            stall: false,
            session: Some(session),
        })),
        ranged_requests: Arc::new(AtomicU64::new(0)),
    };
    let url = server.serve().await;
    (server, url)
}

/// The cookie the probe earns has to reach the range requests the run issues
/// afterwards. They are served by clients built later, in the pipeline, so this
/// only holds when the probe and the run share one store.
#[tokio::test(flavor = "multi_thread")]
async fn a_cookie_set_by_the_probe_reaches_the_range_requests() {
    let size = FILE_SIZE;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let (server, url) = start_server_with_session(payload.clone(), "v1", "s3ss10n").await;
    let dir = temp_dir("cookie_session");
    let half = size / 2;

    // Seed half the file so the run asks for a sub-range. A fresh download would
    // be served entirely from the prefetch response and never issue a range
    // request, which would hide the cookie path altogether.
    let fd = dir.join("out.bin").with_added_extension("fd");
    let mut part = vec![0u8; size];
    part[..half].copy_from_slice(&payload[..half]);
    std::fs::write(dir.join("out.bin").with_added_extension("part"), &part).expect("seed .part");
    let mut record = Record::new(Url::parse(&url).unwrap(), make_config(&dir));
    record.size = size as u64;
    record.etag = Some(std::sync::Arc::from("v1"));
    record.last_modified = Some(std::sync::Arc::from("Wed, 21 Oct 2026 07:28:00 GMT"));
    record.set_progress(vec![0..half as u64]);
    record.store(&fd).await.expect("write the .fd");

    let mut config = make_config(&dir);
    config.cookie_store = Some(true);
    config.min_chunk_size = Some(64 * 1024);
    // Keep the negative case short: without a shared store every range request
    // is a 403 and the run would otherwise sit out the default stall window.
    config.stall_timeout = Some(Duration::from_secs(5));

    let (tx, rx) = create_channel();
    let ctx = Ctx::new();
    let ready = State::at_path(Url::parse(&url).unwrap(), fd, config)
        .build(&tx)
        .await
        .expect("build");
    let handle = ready.start(StartMode::Auto, tx, &ctx);
    while rx.recv().await.is_ok() {}

    assert!(
        server.ranged_requests() > 1,
        "the run issued {} ranged requests, so it never exercised the cookie",
        server.ranged_requests()
    );
    assert_eq!(handle.await, TerminationReason::Completed);
    assert_eq!(std::fs::read(dir.join("out.bin")).unwrap(), payload);
}
