use crate::{Config, Event, Tx, tx_err, utils::build_header};
use fast_down::{UrlInfo, fast_puller::build_client, http::Prefetch};
use reqwest::Response;
use url::Url;

pub async fn prefetch(url: &Url, config: &Config, tx: &Tx) -> Option<(UrlInfo, Response)> {
    let client = build_client(
        build_header(&config.headers),
        config.proxy.as_deref(),
        config.accept_invalid_certs,
        config.accept_invalid_hostnames,
        config.cookie_store,
        config.local_address.first().copied(),
        config.max_redirects,
    );
    let client = tx_err!(client, tx, BuildClientError, None);
    let mut retry_count = 0;
    loop {
        match client.prefetch(url.clone()).await {
            Ok(t) => {
                let _ = tx.send(Event::Prefetch(t.0.clone()));
                break Some(t);
            }
            Err((e, t)) => {
                let _ = tx.send(Event::PrefetchError(e));
                retry_count += 1;
                if retry_count >= config.retry_times {
                    return None;
                }
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
    async fn prefetch_gives_up_after_retries_on_unreachable() {
        // Exercises the Err branch of prefetch (prefetch.rs lines 24-30): a
        // connection that is refused must emit `Event::PrefetchError`, retry up
        // to `retry_times`, then return `None`.
        let url = Url::parse("http://127.0.0.1:1/never").unwrap();
        let config = Config {
            retry_times: 2,
            retry_gap: Duration::ZERO,
            ..Default::default()
        };
        let (tx, rx) = create_channel();
        let result = prefetch(&url, &config, &tx).await;
        assert!(
            result.is_none(),
            "prefetch must give up after exhausting retries"
        );
        drop(tx);
        let mut errors = 0;
        while let Ok(e) = rx.recv().await {
            if matches!(e, Event::PrefetchError(_)) {
                errors += 1;
            }
        }
        assert!(errors >= 1, "expected at least one Event::PrefetchError");
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
        let config = Config::default();
        let (tx, rx) = create_channel();
        let result = prefetch(&url, &config, &tx).await;

        assert!(
            result.is_some(),
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
