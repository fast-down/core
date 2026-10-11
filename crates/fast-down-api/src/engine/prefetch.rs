use crate::{Config, Event, PlanError, Tx, utils::build_header};
use fast_down::{UrlInfo, fast_puller::build_client, http::Prefetch};
use reqwest::Response;
use url::Url;

/// Resolve the remote file's metadata, retrying up to
/// [`Config::retry_times`](crate::Config::retry_times).
///
/// On success emits [`Event::Prefetch`] and hands back the [`UrlInfo`] together
/// with the open [`Response`], which the download pipeline reuses to seed the
/// first range request instead of paying for another round-trip.
///
/// # Errors
///
/// Retries that still have an attempt left are reported as
/// [`Event::PrefetchError`]; the attempt that exhausts the budget is returned
/// as [`PlanError::Prefetch`] instead, because
/// [`ReqwestResponseError`](fast_down::reqwest::ReqwestResponseError) cannot be
/// cloned into both places. A client that cannot be built at all is returned as
/// [`PlanError::BuildClient`] without emitting an event.
#[allow(clippy::result_large_err)]
pub async fn prefetch(
    url: &Url,
    config: &Config,
    tx: &Tx,
) -> Result<(UrlInfo, Response), PlanError> {
    let client = build_client(
        build_header(&config.headers),
        config.proxy.as_deref(),
        config.accept_invalid_certs,
        config.accept_invalid_hostnames,
        config.cookie_store,
        config.local_address.first().copied(),
        config.max_redirects,
    )
    .map_err(PlanError::BuildClient)?;
    let mut retry_count = 0;
    loop {
        match client.prefetch(url.clone()).await {
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
                tokio::time::sleep(t.unwrap_or(config.retry_gap)).await;
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
    use fast_down::Proxy;
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::header::{
        ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, LAST_MODIFIED, RANGE,
    };
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::sync::Arc;

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
        let result = prefetch(&url, &config, &tx).await;
        match result {
            Err(PlanError::Prefetch(e)) => assert!(
                matches!(e, fast_down::reqwest::ReqwestResponseError::Request(_)),
                "a dropped connection must surface as a transport error, got {e:?}"
            ),
            other => panic!(
                "prefetch must return the last error after exhausting retries, got {other:?}"
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
        // Exercises the Ok branch of `prefetch` (prefetch.rs lines 19-22): a server
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
        let result = prefetch(&url, &config, &tx).await;

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
}
