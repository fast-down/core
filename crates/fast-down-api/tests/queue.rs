//! End-to-end tests for the concurrency-limited [`Queue`] against a local
//! ranged HTTP server.
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::single_range_in_vec_init,
    clippy::items_after_statements
)]

use std::collections::HashSet;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use fast_down_api::{
    Event, PartialConfig, Queue, QueueError, QueueEvent, QueueRx, StartMode, State,
    TerminationReason, WriteMethod, create_queue_channel,
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

/// File size: large enough that a throttled download stays in flight while
/// several tasks race for the queue's slots.
const FILE_SIZE: usize = 512 * 1024;
/// Inter-chunk delay, so a download is slow enough to observe concurrency.
const THROTTLE_MS: u64 = 10;
type RespBody = BoxBody<Bytes, Infallible>;

struct FileData {
    body: Vec<u8>,
    etag: String,
    last_modified: String,
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
    let etag = data.etag.clone();
    let last_modified = data.last_modified.clone();
    let body = data.body.clone();
    drop(data);

    let range = req
        .headers()
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if let Some(header) = range
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
            .body(body_of(chunk, Some(Duration::from_millis(THROTTLE_MS))))
            .expect("206"));
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_LENGTH, body.len().to_string())
        .header(ETAG, etag.as_str())
        .header(LAST_MODIFIED, last_modified.as_str())
        .body(body_of(body, Some(Duration::from_millis(THROTTLE_MS))))
        .expect("200"))
}

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
    let dir = std::env::temp_dir().join(format!("fast_down_api_queue_{name}_{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

async fn start_server(payload: Vec<u8>, etag: &str) -> (TestServer, String) {
    let server = TestServer {
        data: Arc::new(RwLock::new(FileData {
            body: payload,
            etag: etag.to_string(),
            last_modified: "Wed, 21 Oct 2026 07:28:00 GMT".to_string(),
        })),
    };
    let url = server.serve().await;
    (server, url)
}

fn config(dir: &Path, name: &str) -> PartialConfig {
    PartialConfig {
        save_dir: Some(dir.to_path_buf()),
        filename: Some(name.to_string()),
        parse_filename: Some(false),
        overwrite: Some(true),
        write_method: Some(WriteMethod::Mmap),
        threads: Some(4),
        ..Default::default()
    }
}

fn payload() -> Vec<u8> {
    (0..FILE_SIZE).map(|i| (i % 251) as u8).collect()
}

/// Budget for one wait on a queue's event channel. Generous — a throttled 512 KiB
/// body still lands in well under a second — but finite, so a run that stops
/// emitting without closing its channel fails the test that started it instead of
/// hanging CI until the job-wide timeout hits with no test name attached.
const RECV_BUDGET: Duration = Duration::from_secs(60);

/// Wait for the next queued event, `None` once the channel is closed, and fail
/// the test instead of hanging when the queue goes quiet but never closes the
/// channel.
///
/// # Panics
///
/// Panics when [`RECV_BUDGET`] elapses with the channel still open, naming the
/// hang: a leaked `QueueTx` clone, or a task that never terminates.
async fn next_event<K: 'static>(rx: &QueueRx<K>) -> Option<QueueEvent<K>> {
    let recv = timeout(RECV_BUDGET, rx.recv()).await.unwrap_or_else(|_| {
        panic!(
            "no queue event for {}s and the channel is still open, so this test \
             would hang forever (a leaked `QueueTx` clone, or a task that never \
             terminated)",
            RECV_BUDGET.as_secs()
        )
    });
    recv.ok()
}

/// Drain the queue channel, returning the peak number of tasks *downloading at
/// once* (counted as started minus terminated, which is exact here because a
/// task holds its slot from `Started` until its terminal event) and each task's
/// terminal reason.
///
/// # Panics
///
/// Panics if the queue stops producing events while leaving the channel open
/// (see [`next_event`]).
async fn drain_events(rx: QueueRx<i32>, tasks: usize) -> (usize, Vec<(i32, TerminationReason)>) {
    let mut live = 0usize;
    let mut max_live = 0usize;
    let mut done: Vec<(i32, TerminationReason)> = Vec::new();
    while let Some(event) = next_event(&rx).await {
        match event {
            QueueEvent::Started(_) => {
                live += 1;
                max_live = max_live.max(live);
            }
            QueueEvent::Task(id, Event::Terminated(reason)) => {
                live = live.saturating_sub(1);
                done.push((id, reason));
                if done.len() == tasks {
                    break;
                }
            }
            _ => {}
        }
    }
    (max_live, done)
}

/// The queue never runs more than `concurrency` tasks at once, yet does run
/// that many in parallel; every task produces a complete file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limits_concurrency_and_completes_every_task() {
    let payload = payload();
    let (_server, url) = start_server(payload.clone(), "v1").await;
    let dir = temp_dir("limit");
    let url = Url::parse(&url).unwrap();

    const TASKS: usize = 4;
    const LIMIT: usize = 2;

    let (tx, rx) = create_queue_channel::<i32>();
    let queue = Queue::<i32>::new(LIMIT, tx);
    for i in 0..TASKS {
        queue
            .push(
                i32::try_from(i).unwrap(),
                State::new(url.clone(), config(&dir, &format!("out-{i}.bin"))),
                StartMode::Auto,
            )
            .expect("push");
    }

    let (max_live, done) = timeout(Duration::from_secs(30), drain_events(rx, TASKS))
        .await
        .expect("queue must finish well within the timeout");

    assert_eq!(done.len(), TASKS, "every task must terminate");
    assert!(
        done.iter().all(|(_, r)| *r == TerminationReason::Completed),
        "every task must complete, got {done:?}"
    );
    assert_eq!(
        max_live, LIMIT,
        "the queue must run exactly {LIMIT} tasks at once (peak {max_live})"
    );

    for i in 0..TASKS {
        let path = dir.join(format!("out-{i}.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), payload, "task {i} content");
    }
}

/// A key that is still waiting or running cannot be pushed again: the second
/// `push` is refused and the first task still completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_a_duplicate_live_key() {
    let payload = payload();
    let (_server, url) = start_server(payload.clone(), "v1").await;
    let dir = temp_dir("duplicate-key");
    let url = Url::parse(&url).unwrap();

    let (tx, rx) = create_queue_channel::<i32>();
    let queue = Queue::<i32>::new(1, tx);
    queue
        .push(
            0,
            State::new(url.clone(), config(&dir, "out.bin")),
            StartMode::Auto,
        )
        .expect("first push");
    let err = queue
        .push(0, State::new(url, config(&dir, "dup.bin")), StartMode::Auto)
        .expect_err("a live key must be rejected");
    assert!(matches!(err, QueueError::AlreadyQueued), "got {err:?}");

    let (_, done) = timeout(Duration::from_secs(30), drain_events(rx, 1))
        .await
        .expect("the first task must still finish");
    assert_eq!(done, vec![(0, TerminationReason::Completed)]);
    assert_eq!(std::fs::read(dir.join("out.bin")).unwrap(), payload);
    assert!(
        !dir.join("dup.bin").exists(),
        "the rejected push must not start a second download"
    );
}

/// Events arrive tagged with the caller's own key, and each task's run is
/// bracketed by exactly one `Started` and one terminal event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_are_tagged_with_the_callers_key() {
    let payload = payload();
    let (_server, url) = start_server(payload.clone(), "v1").await;
    let dir = temp_dir("keys");
    let url = Url::parse(&url).unwrap();

    const TASKS: usize = 3;

    let (tx, rx) = create_queue_channel::<i32>();
    let queue = Queue::<i32>::new(2, tx);
    for i in 0..TASKS {
        queue
            .push(
                i32::try_from(i).unwrap(),
                State::new(url.clone(), config(&dir, &format!("out-{i}.bin"))),
                StartMode::Auto,
            )
            .expect("push");
    }

    let mut started: HashSet<i32> = HashSet::new();
    let mut terminated: HashSet<i32> = HashSet::new();
    while let Some(event) = next_event(&rx).await {
        match event {
            QueueEvent::Started(id) => {
                assert!(started.insert(id), "task {id} started twice");
            }
            QueueEvent::Task(id, Event::Terminated(_)) => {
                assert!(terminated.insert(id), "task {id} terminated twice");
                if terminated.len() == TASKS {
                    break;
                }
            }
            _ => {}
        }
    }

    let expected: HashSet<i32> = (0..TASKS).map(|i| i32::try_from(i).unwrap()).collect();
    assert_eq!(started, expected);
    assert_eq!(
        terminated, started,
        "every started task must terminate once"
    );
}

/// A task whose resolve fails still publishes its failure event and exactly one
/// `Terminated`, and it releases its slot so the queue drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resolve_failure_still_terminates_once() {
    let dir = temp_dir("resolve-fail");
    // A listener that accepts and immediately drops every connection: the
    // request fails with a transport error within a millisecond, and the port
    // stays ours for the whole test. Releasing the port instead (bind, then drop
    // the listener) made the address reassignable, and another task in this
    // binary could bind it and answer `/never.bin` with a payload: the prefetch
    // then succeeded, the download completed, and no failure event was emitted.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            // Drop without a response: the client sees the connection close.
            drop(stream);
        }
    });
    let url = Url::parse(&format!("http://{addr}/never.bin")).unwrap();

    // Fail fast: one attempt, a 1ms retry gap, a short connect timeout. The
    // default config retries 10 times with a 500ms gap, which would outlast the
    // test's timeout.
    let mut cfg = config(&dir, "never.bin");
    cfg.retry_times = Some(1);
    cfg.retry_gap = Some(Duration::from_millis(1));
    cfg.connect_timeout = Some(Duration::from_millis(200));

    let (tx, rx) = create_queue_channel::<i32>();
    let queue = Queue::<i32>::new(1, tx);
    queue
        .push(0, State::new(url, cfg), StartMode::Auto)
        .expect("push");

    let mut errors = 0usize;
    let mut terminated = 0usize;
    let mut reason = None;
    // Every event seen, so a failure says which path the run took instead of
    // only that a count was off.
    let mut trace = Vec::new();
    let watch = async {
        while let Ok(event) = rx.recv().await {
            match event {
                QueueEvent::Task(_, Event::PrefetchError(_)) => {
                    errors += 1;
                    trace.push("PrefetchError".to_string());
                }
                QueueEvent::Task(_, Event::Terminated(r)) => {
                    terminated += 1;
                    reason = Some(r);
                    trace.push(format!("Terminated({r:?})"));
                    break;
                }
                QueueEvent::Task(_, other) => trace.push(format!("{other:?}")),
                other => trace.push(format!("{other:?}")),
            }
        }
    };
    timeout(Duration::from_secs(20), watch)
        .await
        .expect("a failed resolve must still terminate the task");

    assert_eq!(
        errors, 1,
        "the resolve failure must be reported; events: {trace:?}"
    );
    assert_eq!(
        terminated, 1,
        "a failed task must terminate exactly once; events: {trace:?}"
    );
    assert_eq!(reason, Some(TerminationReason::Failed), "events: {trace:?}");
}

/// A task still waiting for a slot that is cancelled reports `Cancelled` and
/// never starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_removes_a_waiting_task() {
    let payload = payload();
    let (_server, url) = start_server(payload, "v1").await;
    let dir = temp_dir("cancel_pending");
    let url = Url::parse(&url).unwrap();
    let (tx, rx) = create_queue_channel::<i32>();
    let queue = Queue::<i32>::new(1, tx);
    queue
        .push(
            1,
            State::new(url.clone(), config(&dir, "a.bin")),
            StartMode::Auto,
        )
        .unwrap();

    // Wait until the first task holds the only slot.
    while let Some(event) = next_event(&rx).await {
        if matches!(event, QueueEvent::Started(1)) {
            break;
        }
    }

    queue
        .push(2, State::new(url, config(&dir, "b.bin")), StartMode::Auto)
        .unwrap();
    assert!(queue.cancel(&2));

    let mut cancelled = false;
    let mut second_started = false;
    timeout(Duration::from_secs(20), async {
        while let Ok(event) = rx.recv().await {
            match event {
                QueueEvent::Cancelled(2) => cancelled = true,
                QueueEvent::Started(2) => second_started = true,
                QueueEvent::Task(1, Event::Terminated(_)) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("the first task must finish");

    assert!(cancelled, "a cancelled waiting task must report Cancelled");
    assert!(!second_started, "the cancelled task must never start");
}

/// A running task stops when it is cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_stops_a_running_task() {
    let payload = payload();
    let (_server, url) = start_server(payload, "v1").await;
    let dir = temp_dir("cancel_running");
    let (tx, rx) = create_queue_channel::<i32>();
    let queue = Queue::<i32>::new(1, tx);
    queue
        .push(
            1,
            State::new(Url::parse(&url).unwrap(), config(&dir, "a.bin")),
            StartMode::Auto,
        )
        .unwrap();

    let mut requested = false;
    let mut reason = None;
    timeout(Duration::from_secs(20), async {
        while let Ok(event) = rx.recv().await {
            match event {
                QueueEvent::Task(1, Event::Start { .. }) if !requested => {
                    requested = true;
                    assert!(queue.cancel(&1));
                }
                QueueEvent::Task(1, Event::Terminated(r)) => {
                    reason = Some(r);
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("a cancelled run must terminate");

    assert!(requested, "the test must cancel after the run starts");
    assert_eq!(reason, Some(TerminationReason::Cancelled));
}

/// A key freed by its terminal event can be pushed again immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_can_be_requeued_after_terminated() {
    let payload = payload();
    let (_server, url) = start_server(payload, "v1").await;
    let dir = temp_dir("requeue");
    let url = Url::parse(&url).unwrap();
    let (tx, rx) = create_queue_channel::<i32>();
    let queue = Queue::<i32>::new(1, tx);
    queue
        .push(
            1,
            State::new(url.clone(), config(&dir, "a.bin")),
            StartMode::Auto,
        )
        .unwrap();

    let mut requeued = false;
    let mut terminals = 0usize;
    timeout(Duration::from_secs(20), async {
        while let Ok(event) = rx.recv().await {
            if let QueueEvent::Task(1, Event::Terminated(_)) = event {
                terminals += 1;
                if terminals == 1 {
                    queue
                        .push(
                            1,
                            State::new(url.clone(), config(&dir, "b.bin")),
                            StartMode::Auto,
                        )
                        .expect("the key is free once its terminal event is delivered");
                    requeued = true;
                } else {
                    break;
                }
            }
        }
    })
    .await
    .expect("both runs must terminate");

    assert!(requeued, "a terminal event must free the key");
    assert_eq!(terminals, 2);
}
