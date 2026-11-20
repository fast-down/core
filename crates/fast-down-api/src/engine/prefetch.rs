use crate::{Config, Event, PlanError, Tx, utils::build_header};
use fast_down::{
    UrlInfo,
    fast_puller::{CookieStore, build_client},
    http::{Prefetch, PrefetchOptions},
};
use reqwest::Response;
use std::time::Duration;
use url::Url;

/// Longest wait a single retry may take when the server sends `Retry-After`.
///
/// `Retry-After` is untrusted input, and nothing can interrupt
/// [`prefetch`] once it is awaiting: [`State::build`](crate::State::build) is
/// only ever stopped by dropping its future, and the queue holds the slot for
/// the whole build. A server answering `503 + Retry-After: 3600` would park the
/// build — and that slot — for an hour. The header is therefore honoured up to
/// this ceiling and no further. There is deliberately no give-up path: the
/// download must eventually finish, and retrying is how it gets there; the cap
/// only makes the next attempt come sooner.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// How long to wait before the next metadata attempt.
///
/// A server-provided `Retry-After` is clamped to [`MAX_RETRY_WAIT`];
/// [`Config::retry_gap`] is the caller's own knob and is used as configured.
#[must_use]
fn retry_wait(retry_after: Option<Duration>, gap: Duration) -> Duration {
    match retry_after {
        Some(server) if server > MAX_RETRY_WAIT => MAX_RETRY_WAIT,
        Some(server) => server,
        None => gap,
    }
}

/// Resolve the remote file's metadata, retrying up to
/// [`Config::retry_times`](crate::Config::retry_times).
///
/// On success emits [`Event::Prefetch`] and hands back the [`UrlInfo`] together
/// with the open [`Response`], which the download pipeline reuses to seed the
/// first range request instead of paying for another round-trip.
///
/// `cookie_store` is the same store the download builds its clients from, so a
/// session this probe establishes survives into the run instead of dying with
/// the probe client.
///
/// # Errors
///
/// Retries that still have an attempt left are reported as
/// [`Event::PrefetchError`]; the attempt that exhausts the budget is returned
/// as [`PlanError::Prefetch`] instead, because the error cannot be cloned into
/// both places. A client that cannot be built at all is returned as
/// [`PlanError::BuildClient`] without emitting an event.
///
/// A metadata request that outlives its own timeout is reported exactly like a
/// transport failure: [`HttpError::Timeout`](fast_down::http::HttpError::Timeout)
/// counts as one failed attempt, so the retry budget is what bounds a server that
/// accepts the connection and then never answers.
#[allow(clippy::result_large_err)]
pub async fn prefetch(
    url: &Url,
    config: &Config,
    cookie_store: &CookieStore,
    tx: &Tx,
) -> Result<(UrlInfo, Response), PlanError> {
    let (headers, _) = build_header(&config.headers);
    let client = build_client(
        headers,
        config.proxy.as_deref(),
        config.accept_invalid_certs,
        config.accept_invalid_hostnames,
        cookie_store,
        config.local_address.first().copied(),
        config.max_redirects,
        config.connect_timeout,
    )
    .map_err(PlanError::BuildClient)?;
    let mut retry_count = 0;
    let options = PrefetchOptions::new(
        config.probe_timeout,
        config.prefetch_timeout,
        config.probe_attempts,
        config.probe_gap,
    );
    loop {
        match client.prefetch(url.clone(), options).await {
            Ok(t) => {
                let _ = tx.send(Event::Prefetch(t.0.clone()));
                break Ok(t);
            }
            Err((e, t)) => {
                retry_count += 1;
                if retry_count >= config.retry_times {
                    return Err(PlanError::Prefetch(e));
                }
                let _ = tx.send(Event::PrefetchError(e));
                tokio::time::sleep(retry_wait(t, config.retry_gap)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::create_channel;
    use std::time::Duration;

    use bytes::Bytes;
    use fast_down::{Proxy, http::HttpError, reqwest::ReqwestResponseError};
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::header::{
        ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, LAST_MODIFIED, RANGE,
        RETRY_AFTER,
    };
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn prefetch_gives_up_after_retries_on_transport_error() {
        // Exercises the Err branch of prefetch on a *transport* failure: a server
        // that accepts the connection and then drops it mid-response must make
        // `prefetch` emit `Event::PrefetchError` for every attempt that still has
        // a retry left, then return the final failure instead of emitting it. A
        // local accept-then-drop server is used instead of a refused/unreachable
        // address because a refused loopback connect costs ~2s per attempt on
        // some platforms, while this reproduces the same `Request` error in ~1ms.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                // Drop the socket without ever sending a response.
                drop(stream);
            }
        });
        let url = Url::parse(&format!("http://{addr}/never")).unwrap();
        let config = Config {
            // Bind to the local listener directly: the host's system proxy (if
            // any) must not intercept loopback traffic, or this would see a
            // proxy-generated error instead of the dropped connection.
            proxy: Proxy::No,
            retry_times: 2,
            retry_gap: Duration::ZERO,
            ..Default::default()
        };
        let (tx, rx) = create_channel();
        let result = prefetch(&url, &config, &CookieStore::default(), &tx).await;
        match result {
            Err(PlanError::Prefetch(HttpError::Request(ReqwestResponseError::Request(_)))) => {}
            other => panic!(
                "a dropped connection must surface as a transport error after the \
                 retries are exhausted, got {other:?}"
            ),
        }
        drop(tx);
        let mut errors = 0;
        while let Ok(e) = rx.recv().await {
            if matches!(e, Event::PrefetchError(_)) {
                errors += 1;
            }
        }
        assert_eq!(
            errors, 1,
            "2 attempts means 1 retryable failure is reported as an event and 1 is returned"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn prefetch_succeeds_against_local_server() {
        // Exercises the Ok branch of `prefetch`: a server
        // that answers a normal GET with 200 + content-length/etag/last-modified and
        // a Range probe with 206 + content-range must make `prefetch` resolve to
        // `Some((UrlInfo, _))`, populate the UrlInfo correctly, and emit
        // `Event::Prefetch`. The existing test only covers the failure path.
        let body: Arc<Vec<u8>> = Arc::new(vec![0xABu8; 1024]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let stream = stream;
                let body = body.clone();
                let io = TokioIo::new(stream);
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let body = body.clone();
                        async move {
                            let total = body.len();
                            let is_range = req.headers().contains_key(RANGE);
                            let body_bytes: Bytes = if is_range {
                                Bytes::from(vec![0xABu8])
                            } else {
                                Bytes::from(body.to_vec())
                            };
                            let builder = Response::builder()
                                .status(if is_range {
                                    StatusCode::PARTIAL_CONTENT
                                } else {
                                    StatusCode::OK
                                })
                                .header(
                                    CONTENT_LENGTH,
                                    if is_range {
                                        "1".to_string()
                                    } else {
                                        total.to_string()
                                    },
                                )
                                .header(ACCEPT_RANGES, "bytes")
                                .header(ETAG, "etag-test")
                                .header(LAST_MODIFIED, "Wed, 21 Oct 2026 07:28:00 GMT")
                                .header(CONTENT_TYPE, "application/octet-stream");
                            let builder = if is_range {
                                builder.header(CONTENT_RANGE, format!("bytes 0-0/{total}"))
                            } else {
                                builder
                            };
                            let resp = builder.body(Full::new(body_bytes)).unwrap();
                            Ok::<_, Infallible>(resp)
                        }
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });

        let url = Url::parse(&format!("http://{addr}/file.bin")).unwrap();
        let config = Config {
            // Talk to the local listener directly; a host system proxy must not
            // sit between the client and a loopback test server.
            proxy: Proxy::No,
            ..Config::default()
        };
        let (tx, rx) = create_channel();
        let result = prefetch(&url, &config, &CookieStore::default(), &tx).await;

        assert!(
            result.is_ok(),
            "prefetch must succeed against a well-behaved server"
        );
        let (info, _resp) = result.unwrap();
        assert_eq!(info.size, 1024, "size must come from content-length");
        assert_eq!(
            info.file_id.etag.as_deref(),
            Some("etag-test"),
            "etag must be read from the response"
        );
        assert!(
            info.supports_range,
            "a 206 content-range response must advertise range support"
        );
        assert!(
            info.fast_download,
            "size > 0 with range support must enable fast_download"
        );

        drop(tx);
        let mut saw_prefetch = false;
        while let Ok(e) = rx.recv().await {
            if matches!(e, Event::Prefetch(_)) {
                saw_prefetch = true;
            }
        }
        assert!(
            saw_prefetch,
            "prefetch must emit Event::Prefetch on the success path"
        );
    }

    /// A server that rejects every request with `503` and the given
    /// `Retry-After`, counting how many requests it served.
    async fn retry_after_server(
        retry_after: &'static str,
    ) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let served = hits.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let hits = served.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |_req: Request<Incoming>| {
                        let hits = hits.clone();
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::SERVICE_UNAVAILABLE)
                                    .header(RETRY_AFTER, retry_after)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        (addr, hits)
    }

    #[test]
    fn retry_wait_caps_the_server_header() {
        // `Retry-After` is untrusted and the attempt it delays cannot be
        // interrupted, so an hour-long header must not be slept verbatim.
        assert_eq!(
            retry_wait(Some(Duration::from_secs(3600)), Duration::from_millis(500)),
            MAX_RETRY_WAIT,
            "a hostile Retry-After must be clamped to the documented ceiling"
        );
        assert_eq!(
            retry_wait(Some(Duration::from_secs(5)), Duration::from_millis(500)),
            Duration::from_secs(5),
            "a Retry-After below the ceiling is still honoured as sent"
        );
        assert_eq!(
            retry_wait(None, Duration::from_millis(500)),
            Duration::from_millis(500),
            "without the header the configured gap is used as configured"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hostile_retry_after_does_not_park_the_build_for_an_hour() {
        // `Retry-After: 3600` on a 503 is a single header away from parking
        // `State::build` (and its queue slot) for an hour, because nothing can
        // interrupt the future it is awaiting. The retry must happen, and the
        // wait must be the documented ceiling instead of the server's hour.
        let (addr, hits) = retry_after_server("3600").await;
        let url = Url::parse(&format!("http://{addr}/blocked")).unwrap();
        let config = Config {
            proxy: Proxy::No,
            retry_times: 2,
            retry_gap: Duration::ZERO,
            ..Config::default()
        };
        let (tx, rx) = create_channel();
        let started = tokio::time::Instant::now();
        let result = prefetch(&url, &config, &CookieStore::default(), &tx).await;
        let waited = started.elapsed();
        drop(tx);

        let mut errors = 0;
        while let Ok(e) = rx.recv().await {
            if matches!(e, Event::PrefetchError(_)) {
                errors += 1;
            }
        }
        assert_eq!(errors, 1, "the retryable attempt must be reported");
        assert!(
            hits.load(Ordering::SeqCst) >= 2,
            "the retry must reach the server: {} requests",
            hits.load(Ordering::SeqCst)
        );
        match result {
            Err(PlanError::Prefetch(HttpError::Request(ReqwestResponseError::StatusCode(
                resp,
            )))) => assert_eq!(
                resp.status(),
                503,
                "every attempt must have been answered by the server, not cut \
                 short locally"
            ),
            other => panic!("the exhausting attempt is returned, got {other:?}"),
        }
        // The paused clock advances to the *next* pending timer when the
        // runtime goes idle, so `waited` is not the slept duration: it can
        // undershoot (nightly measured 27s for a 60s sleep) and can be pushed
        // past the ceiling by other timers (macOS stable measured 86s). What
        // must not happen is an hour — the raw header would make this ~3600s —
        // and the ceiling itself is pinned by `retry_wait_caps_the_server_header`.
        assert!(
            waited < Duration::from_secs(600),
            "one retry must not wait the server's hour, waited {waited:?}"
        );
    }
}
