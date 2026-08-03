#![cfg(not(target_family = "wasm"))]

//! A `reqwest`-based implementation of the [`crate::http`] HTTP traits with
//! smart redirect handling.
//!
//! This module adapts `reqwest` to the backend-agnostic [`crate::http::HttpClient`]
//! trait family and, more importantly, provides [`SmartRedirectClient`]: a
//! `reqwest::Client` wrapper that follows redirects **manually** so it can honor
//! the `Referrer-Policy` header and strip resource-specific headers
//! (`Origin` / `Authorization` / `Cookie`) on cross-origin hops, per RFC 9110
//! §15.4. The corresponding request builder is [`ManualRedirectRequestBuilder`].
//!
//! Most users do not construct these types directly; instead they build a
//! `FastDownPuller` via `build_client`, which creates a
//! correctly-configured [`SmartRedirectClient`].

use crate::http::{
    HttpClient, HttpHeaders, HttpRequestBuilder, HttpResponse,
    manual_redirect::{ReferrerPolicy, compute_referer},
};
use fast_pull::ProgressEntry;
use httpdate::parse_http_date;
use reqwest::{
    Client, RequestBuilder, Response, StatusCode,
    header::{self, HeaderMap, HeaderValue},
};
use std::{
    borrow::Cow,
    time::{Duration, SystemTime},
};
use url::Url;

impl HttpClient for Client {
    type RequestBuilder = RequestBuilder;
    fn get(&self, url: Url, range: Option<ProgressEntry>) -> Self::RequestBuilder {
        let mut req = self.get(url);
        if let Some(range) = range {
            req = req.header(
                header::RANGE,
                format!("bytes={}-{}", range.start, range.end.saturating_sub(1)),
            );
        }
        req
    }
}

impl HttpRequestBuilder for RequestBuilder {
    type Response = Response;
    type RequestError = ReqwestResponseError;
    async fn send(self) -> Result<Self::Response, (Self::RequestError, Option<Duration>)> {
        let res = self
            .send()
            .await
            .map_err(|e| (ReqwestResponseError::Request(e), None))?;
        let status = res.status();
        if status.is_success() {
            Ok(res)
        } else {
            let retry_after = parse_retry_after(res.headers());
            Err((ReqwestResponseError::StatusCode(res), retry_after))
        }
    }
}

impl HttpResponse for Response {
    type Headers = HeaderMap;
    type ChunkError = reqwest::Error;
    fn headers(&self) -> &Self::Headers {
        self.headers()
    }
    fn url(&self) -> &Url {
        self.url()
    }
    async fn chunk(&mut self) -> Result<Option<bytes::Bytes>, Self::ChunkError> {
        self.chunk().await
    }
}

impl HttpHeaders for HeaderMap {
    type GetHeaderError = ReqwestGetHeaderError;
    fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
        let header_value = self.get(header).ok_or(ReqwestGetHeaderError::NotFound)?;
        Ok(String::from_utf8_lossy(header_value.as_bytes()))
    }
}

/// Errors that can occur when getting a header value from a reqwest response.
#[derive(thiserror::Error, Debug)]
pub enum ReqwestGetHeaderError {
    #[error("Header not found")]
    NotFound,
}

/// Errors that can occur when sending a reqwest request.
#[derive(thiserror::Error, Debug)]
pub enum ReqwestResponseError {
    #[error("Reqwest error {0:?}")]
    Request(reqwest::Error),
    #[error("Url: {}, Status Code: {}, Headers: {:?}", .0.url(), .0.status(), .0.headers())]
    StatusCode(Response),
}

/// Parse the `Retry-After` response header into a [`Duration`].
///
/// Supports both the delta-seconds format (integer) and the HTTP-date format.
/// Returns `None` if the header is missing or unparseable.
#[must_use]
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let retry_after = headers.get(header::RETRY_AFTER)?;
    let retry_str = retry_after.to_str().ok()?;
    if let Ok(secs) = retry_str.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let target_time = parse_http_date(retry_str).ok()?;
    let duration = target_time.duration_since(SystemTime::now()).ok()?;
    Some(duration)
}

/// A [`reqwest::Client`] wrapper that handles HTTP redirects manually,
/// respecting the `Referrer-Policy` header and RFC 9110 §15.4 redirect rules.
#[derive(Debug, Clone)]
pub struct SmartRedirectClient {
    client: Client,
    initial_referer: Option<HeaderValue>,
    referrer_policy: Option<ReferrerPolicy>,
    /// Per RFC 9110 §15.4 item 2.5, resource-specific headers that are
    /// stripped on redirect and injected only on the first request hop.
    origin: Option<HeaderValue>,
    authorization: Option<HeaderValue>,
    cookie: Option<HeaderValue>,
    max_redirects: usize,
}

impl SmartRedirectClient {
    /// Build a [`SmartRedirectClient`] from an already-constructed `reqwest::Client`.
    ///
    /// * `client` — the underlying client. **Must** be built with
    ///   `redirect(reqwest::redirect::Policy::none())`, otherwise the manual
    ///   redirect logic here conflicts with reqwest's own auto-follow.
    /// * `initial_referer` — the `Referer` sent on the first request.
    /// * `referrer_policy` — the policy applied when no `Referrer-Policy`
    ///   header is present on a response; per-hop headers override it.
    /// * `origin` / `authorization` / `cookie` — resource-specific headers
    ///   injected only on the first hop and stripped on redirect (RFC 9110 §15.4).
    /// * `max_redirects` — the maximum number of redirects to follow before
    ///   failing with a `StatusCode` error.
    #[must_use]
    pub const fn new(
        client: Client,
        initial_referer: Option<HeaderValue>,
        referrer_policy: Option<ReferrerPolicy>,
        origin: Option<HeaderValue>,
        authorization: Option<HeaderValue>,
        cookie: Option<HeaderValue>,
        max_redirects: usize,
    ) -> Self {
        Self {
            client,
            initial_referer,
            referrer_policy,
            origin,
            authorization,
            cookie,
            max_redirects,
        }
    }
}

impl HttpClient for SmartRedirectClient {
    type RequestBuilder = ManualRedirectRequestBuilder;

    fn get(&self, url: Url, range: Option<ProgressEntry>) -> Self::RequestBuilder {
        ManualRedirectRequestBuilder {
            client: self.client.clone(),
            url,
            range,
            next_referer: self.initial_referer.clone(),
            referrer_policy: self.referrer_policy,
            origin: self.origin.clone(),
            authorization: self.authorization.clone(),
            cookie: self.cookie.clone(),
            max_redirects: self.max_redirects,
            redirect_count: 0,
        }
    }
}

/// Request builder that follows redirects manually.
///
/// On each redirect it:
/// - Reads `Referrer-Policy` from the response (overriding the previous policy).
/// - Applies the policy to compute the `Referer` for the next request.
/// - Strips resource-specific headers (Origin, Authorization, Cookie) per
///   RFC 9110 §15.4 item 2.5.
/// - Inherits the fragment from the original URL if the Location header
///   lacks one, per RFC 9110 §10.2.2.
/// - Follows only 301, 302, 303, 307, 308 status codes.
pub struct ManualRedirectRequestBuilder {
    client: Client,
    url: Url,
    range: Option<ProgressEntry>,
    next_referer: Option<HeaderValue>,
    referrer_policy: Option<ReferrerPolicy>,
    /// Resource-specific headers injected only on the first hop.
    origin: Option<HeaderValue>,
    authorization: Option<HeaderValue>,
    cookie: Option<HeaderValue>,
    max_redirects: usize,
    redirect_count: usize,
}

impl HttpRequestBuilder for ManualRedirectRequestBuilder {
    type Response = Response;
    type RequestError = ReqwestResponseError;

    async fn send(mut self) -> Result<Response, (Self::RequestError, Option<Duration>)> {
        loop {
            let mut req = self.client.get(self.url.clone());
            if let Some(ref range) = self.range {
                req = req.header(
                    header::RANGE,
                    format!("bytes={}-{}", range.start, range.end.saturating_sub(1)),
                );
            }
            if let Some(ref referer) = self.next_referer {
                req = req.header(header::REFERER, referer);
            }
            // Per RFC 9110 §15.4 item 2.5, resource-specific headers are
            // only sent on the first hop and stripped on redirect.
            if self.redirect_count == 0 {
                if let Some(ref origin) = self.origin {
                    req = req.header(header::ORIGIN, origin);
                }
                if let Some(ref auth) = self.authorization {
                    req = req.header(header::AUTHORIZATION, auth);
                }
                if let Some(ref cookie) = self.cookie {
                    req = req.header(header::COOKIE, cookie);
                }
            }
            let resp = req
                .send()
                .await
                .map_err(|e| (ReqwestResponseError::Request(e), None))?;

            // DEBUG ASSERT: If reqwest auto-followed redirects, resp.url() will differ
            // from the URL we sent the request to. This means the inner Client was NOT
            // built with `redirect::Policy::none()`, which breaks manual redirect logic.
            // This check is removed entirely in release builds (zero cost).
            debug_assert!(
                resp.url() == &self.url,
                "SmartRedirectClient: inner reqwest::Client has auto-redirect ENABLED. \
                 Build it with `.redirect(reqwest::redirect::Policy::none())`. \
                 The Referer-Policy aware redirect logic requires full control over redirects."
            );

            let status = resp.status();
            if !is_redirection(status) {
                return if status.is_success() {
                    Ok(resp)
                } else {
                    let retry_after = parse_retry_after(resp.headers());
                    Err((ReqwestResponseError::StatusCode(resp), retry_after))
                };
            }
            if self.redirect_count >= self.max_redirects {
                let retry_after = parse_retry_after(resp.headers());
                return Err((ReqwestResponseError::StatusCode(resp), retry_after));
            }
            let location = if let Some(v) = resp.headers().get(header::LOCATION)
                && let Ok(s) = v.to_str()
            {
                s
            } else {
                let retry_after = parse_retry_after(resp.headers());
                return Err((ReqwestResponseError::StatusCode(resp), retry_after));
            };
            let Ok(mut next_url) = self.url.join(location) else {
                let retry_after = parse_retry_after(resp.headers());
                return Err((ReqwestResponseError::StatusCode(resp), retry_after));
            };
            // RFC 9110 §10.2.2: If the Location header lacks a fragment,
            // inherit it from the original request URI.
            if next_url.fragment().is_none()
                && let Some(fragment) = self.url.fragment()
            {
                next_url.set_fragment(Some(fragment));
            }
            if let Some(policy_header) = resp.headers().get("referrer-policy")
                && let Ok(s) = policy_header.to_str()
                && let Some(p) = ReferrerPolicy::parse(s)
            {
                self.referrer_policy = Some(p);
            }
            self.next_referer = compute_referer(self.referrer_policy, &self.url, &next_url)
                .and_then(|s| HeaderValue::from_str(&s).ok());
            self.url = next_url;
            self.redirect_count += 1;
        }
    }
}

fn is_redirection(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

#[cfg(test)]
#[cfg(feature = "mem")]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::significant_drop_tightening
    )]
    use super::*;
    use crate::{
        http::{HttpPuller, Prefetch},
        url_info::FileId,
    };
    use fast_pull::{
        Event, Merge,
        mem::MemPusher,
        mock::build_mock_data,
        multi::{self, download_multi},
        single::{self, download_single},
    };
    use reqwest::{Client, StatusCode};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn test_redirect_and_content_range() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();

        let _mock_redirect = server
            .mock("GET", "/redirect")
            .with_status(301)
            .with_header("Location", "/%e4%bd%a0%e5%a5%bd.txt")
            .create_async()
            .await;

        let _mock_file = server
            .mock("GET", "/%e4%bd%a0%e5%a5%bd.txt")
            .with_status(206)
            .with_header("Content-Length", "1024")
            .with_header("Content-Range", "bytes 0-0/1024")
            .with_body(vec![0; 1024])
            .create_async()
            .await;

        let url = Url::parse(&format!("{}/redirect", server.url())).unwrap();
        let (url_info, _) = client.prefetch(url).await.expect("Request should succeed");

        assert_eq!(
            url_info.final_url.as_str(),
            format!("{}/%e4%bd%a0%e5%a5%bd.txt", server.url())
        );
        assert_eq!(url_info.size, 1024);
        assert_eq!(url_info.raw_name, "你好.txt");
        assert!(url_info.supports_range);
    }

    #[tokio::test]
    async fn test_filename_sources() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();

        // Test with Content-Disposition header
        let _mock1 = server
            .mock("GET", "/test1")
            .with_header("Content-Disposition", r#"attachment; filename="test.txt""#)
            .create_async()
            .await;
        let url = Url::parse(&format!("{}/test1", server.url())).unwrap();
        let (url_info, _) = client.prefetch(url).await.unwrap();
        assert_eq!(url_info.raw_name, "test.txt");

        // Test filename* only (UTF-8 encoding)
        let _mock_star = server
            .mock("GET", "/test_star")
            .with_header(
                "Content-Disposition",
                "attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.txt",
            ) // decodes to "测试.txt"
            .create_async()
            .await;
        let url = Url::parse(&format!("{}/test_star", server.url())).unwrap();
        let (url_info, _) = client.prefetch(url).await.unwrap();
        assert_eq!(url_info.raw_name, "测试.txt");

        let _mock_both = server
            .mock("GET", "/test_both")
            .with_header(
                "Content-Disposition",
                r#"attachment; filename="fallback.txt"; filename*=UTF-8''%E6%B5%8B%E8%AF%95.txt"#,
            )
            .create_async()
            .await;
        let url = Url::parse(&format!("{}/test_both", server.url())).unwrap();
        let (url_info, _) = client.prefetch(url).await.unwrap();
        assert_eq!(url_info.raw_name, "测试.txt");

        // Test URL path source
        let _mock2 = server
            .mock("GET", "/test2/%E5%A5%BD%E5%A5%BD%E5%A5%BD.pdf")
            .create_async()
            .await;
        let url = Url::parse(&format!(
            "{}/test2/%E5%A5%BD%E5%A5%BD%E5%A5%BD.pdf",
            server.url()
        ))
        .unwrap();
        let (url_info, _) = client.prefetch(url).await.unwrap();
        assert_eq!(url_info.raw_name, "好好好.pdf");
    }

    #[tokio::test]
    async fn test_error_handling() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let _mock1 = server
            .mock("GET", "/404")
            .with_status(404)
            .create_async()
            .await;

        let url = Url::parse(&format!("{}/404", server.url())).unwrap();
        match client.prefetch(url).await {
            Ok(info) => unreachable!("404 status code should not success: {info:?}"),
            Err((err, _)) => match err {
                ReqwestResponseError::Request(error) => unreachable!("{error:?}"),
                ReqwestResponseError::StatusCode(resp) => {
                    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
                }
            },
        }
    }

    #[tokio::test]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(300 * 1024 * 1024);
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let _mock = mount_range_endpoint(&mut server, "/concurrent", mock_data.clone()).await;
        let puller = HttpPuller::new(
            Arc::new(format!("{}/concurrent", server.url()).parse().unwrap()),
            client,
            None,
            FileId::default(),
        );
        let pusher = MemPusher::with_capacity(mock_data.len());
        // Keep only the data handle for the final assertion; the whole `pusher`
        // (whose listener holds a clone of the `event_chain` sender) is moved into
        // the download, so `event_chain` closes once the push thread finishes,
        // terminating the single drain loop below.
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher,
            multi::DownloadOptions {
                concurrent: 32,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(5),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );

        let mut pull_progress: Vec<ProgressEntry> = Vec::new();
        let mut push_progress: Vec<ProgressEntry> = Vec::new();
        // `PushProgress` now flows on the same `event_chain` as the engine events
        // (the sink's listener emits it the moment data is actually written), so a
        // single drain collects both pull and push progress.
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

    /// Mount an endpoint that serves `body` and honours `Range` requests, as a
    /// server advertising `Accept-Ranges: bytes` would.
    async fn mount_range_endpoint(
        server: &mut mockito::ServerGuard,
        path: &str,
        body: Vec<u8>,
    ) -> mockito::Mock {
        server
            .mock("GET", path)
            .with_status(206)
            .with_header("Accept-Ranges", "bytes")
            .with_body_from_request(move |request| {
                if !request.has_header("Range") {
                    return body.clone();
                }
                request.header("Range")[0]
                    .to_str()
                    .unwrap()
                    .rsplit('=')
                    .next()
                    .unwrap()
                    .split(',')
                    .map(|p| p.trim().splitn(2, '-'))
                    .map(|mut p| {
                        let start = p.next().unwrap().parse::<usize>().unwrap();
                        let end = p.next().unwrap().parse::<usize>().unwrap();
                        start..=end
                    })
                    .flat_map(|p| body[p].to_vec())
                    .collect()
            })
            .create_async()
            .await
    }

    /// Create an empty temp file of `size` bytes and map it for writing.
    ///
    /// The returned pusher owns the mapping, so the `File` handle is dropped
    /// here: both `mmap` and Windows file mappings keep the underlying object
    /// alive independently of the descriptor used to create them.
    #[cfg(feature = "file")]
    async fn temp_mmap_target(
        tag: &str,
        size: u64,
    ) -> (fast_pull::file::MmapFilePusher, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("fast-down-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.bin");
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await
            .unwrap();
        let pusher = fast_pull::file::MmapFilePusher::new(&file, size, false)
            .await
            .unwrap();
        (pusher, path)
    }

    /// Deterministic xorshift64 so a failing resize sequence can be replayed.
    #[cfg(feature = "file")]
    fn next_rand(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Repeatedly resize the worker pool while a real HTTP download streams
    /// into a real memory-mapped file.
    ///
    /// The in-memory unit tests resize a pool fed by a mock puller and sink, so
    /// they cannot observe how a resize interacts with live sockets, chunked
    /// transfer and page-cache writes. This drives all three at once and checks
    /// the bytes that actually reached the disk.
    #[cfg(feature = "file")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_download_thread_churn_preserves_all_bytes() {
        use std::collections::BTreeSet;
        use std::sync::atomic::{AtomicU64, Ordering};

        let mock_data = build_mock_data(16 * 1024 * 1024);
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let _mock = mount_range_endpoint(&mut server, "/churn", mock_data.clone()).await;

        let size = mock_data.len() as u64;
        let (pusher, path) = temp_mmap_target("churn", size).await;

        let puller = HttpPuller::new(
            Arc::new(format!("{}/churn", server.url()).parse().unwrap()),
            client,
            None,
            FileId::default(),
        );
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..size];
        // Start with a single worker so growth has somewhere to go, and keep
        // `min_chunk_size` at 1 so a resize can split whatever remains.
        let result = download_multi(
            puller,
            pusher,
            multi::DownloadOptions {
                concurrent: 1,
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.iter().cloned(),
                pull_timeout: Duration::from_secs(30),
                min_chunk_size: 1,
                max_speculative: 3,
            },
        );

        let written = Arc::new(AtomicU64::new(0));
        let churner = result.clone();
        let probe = written.clone();
        let churn = tokio::spawn(async move {
            let mut state = 0x2545_F491_4F6C_DD1D_u64;
            let mut seen = BTreeSet::new();
            let mut inflight = 0usize;
            // Track the transfer instead of resizing a fixed number of times: a
            // fixed count would drain long before the download does and only
            // exercise the idle pool.
            for _ in 0..3000 {
                let done = probe.load(Ordering::Relaxed);
                if done >= size {
                    break;
                }
                if done > 0 {
                    inflight += 1;
                }
                let threads = usize::try_from(next_rand(&mut state) % 8 + 1).unwrap();
                seen.insert(threads);
                churner.set_threads(threads, 1);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Leave a healthy pool behind so the remaining ranges drain.
            churner.set_threads(8, 1);
            (seen, inflight)
        });

        let mut pulling_total = 0usize;
        let mut push_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain().recv().await {
            match e {
                Event::Pulling(_) => pulling_total += 1,
                Event::PushProgress(p) => {
                    written.fetch_add(p.end - p.start, Ordering::Relaxed);
                    push_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        let (seen, inflight) = churn.await.unwrap();

        // Without these the test silently degrades into a plain download the
        // moment resizing stops taking effect.
        assert!(
            seen.len() > 2,
            "churn never varied the pool size, so nothing was exercised: {seen:?}"
        );
        assert!(
            inflight > 0,
            "every resize landed outside the transfer, so churn was a no-op"
        );
        // Every effective resize reclaims and re-hands-out the remaining range,
        // so a live pool emits far more `Pulling` events than the session had
        // starting chunks. Equality with the chunk count means resizing was
        // inert and one worker pulled everything.
        assert!(
            pulling_total >= 8,
            "ranges were never redistributed across workers (pulling={pulling_total}, chunks={})",
            download_chunks.len()
        );
        assert_eq!(
            push_progress, download_chunks,
            "repeated resizing lost or duplicated pushed ranges"
        );

        // The mapping is unmapped once the push driver drops the pusher, so the
        // file now holds everything the session claimed to have written.
        let on_disk = tokio::fs::read(&path).await.unwrap();
        assert!(
            on_disk == mock_data,
            "repeated resizing corrupted the bytes written to disk"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_sequential_download() {
        let mock_data = build_mock_data(300 * 1024 * 1024);
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let _mock = server
            .mock("GET", "/sequential")
            .with_status(200)
            .with_body(mock_data.clone())
            .create_async()
            .await;
        let puller = HttpPuller::new(
            Arc::new(format!("{}/sequential", server.url()).parse().unwrap()),
            client,
            None,
            FileId::default(),
        );
        let pusher = MemPusher::with_capacity(mock_data.len());
        // Keep only the data handle for the final assertion; the whole `pusher`
        // (whose listener holds a clone of the `event_chain` sender) is moved into
        // the download, so `event_chain` closes once the push thread finishes,
        // terminating the single drain loop below.
        let receive = pusher.receive.clone();
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_single(
            puller,
            pusher,
            single::DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
            },
        );

        let mut pull_progress: Vec<ProgressEntry> = Vec::new();
        let mut push_progress: Vec<ProgressEntry> = Vec::new();
        // `PushProgress` now flows on the same `event_chain` as the engine events
        // (the sink's listener emits it the moment data is actually written), so a
        // single drain collects both pull and push progress.
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

    /// Drives `ManualRedirectRequestBuilder::send` directly via `SmartRedirectClient`
    /// (the production code path, as opposed to the plain `reqwest::Client` impl that
    /// relies on reqwest's native auto-redirect). Confirms that `self.url = next_url`
    /// correctly advances the request URL across a manual redirect so the final
    /// response lands on the redirected location.
    #[tokio::test]
    async fn test_smart_redirect_follows_redirect() {
        let mut server = mockito::Server::new_async().await;
        // The inner reqwest client MUST use Policy::none() so that the manual
        // redirect logic in `ManualRedirectRequestBuilder` is the one driving
        // following (this is what `build_client` does in production).
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let _mock_redirect = server
            .mock("GET", "/src")
            .with_status(302)
            .with_header("Location", "/dst")
            .create_async()
            .await;
        let _mock_dst = server
            .mock("GET", "/dst")
            .with_status(200)
            .with_header("Content-Length", "11")
            .with_body("hello world")
            .create_async()
            .await;

        let redirect_client = SmartRedirectClient::new(client, None, None, None, None, None, 10);
        let url = Url::parse(&format!("{}/src", server.url())).unwrap();
        let resp = redirect_client
            .get(url, None)
            .send()
            .await
            .expect("manual redirect should succeed");
        assert_eq!(resp.status(), StatusCode::OK);
        // `ManualRedirectRequestBuilder::send` must have advanced `self.url`
        // to the redirected destination.
        assert_eq!(resp.url().path(), "/dst");
    }

    /// Confirms `ManualRedirectRequestBuilder::send` honors `max_redirects`
    /// (an infinite redirect loop must fail once the cap is exceeded, rather
    /// than looping forever or silently succeeding).
    #[tokio::test]
    async fn test_smart_redirect_respects_max_redirects() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock_loop = server
            .mock("GET", "/loop")
            .with_status(301)
            .with_header("Location", "/loop")
            .create_async()
            .await;

        let redirect_client = SmartRedirectClient::new(client, None, None, None, None, None, 2);
        let url = Url::parse(&format!("{}/loop", server.url())).unwrap();
        let err = redirect_client
            .get(url, None)
            .send()
            .await
            .expect_err("exceeding max_redirects should fail");
        assert!(matches!(err.0, ReqwestResponseError::StatusCode(_)));
    }

    #[test]
    fn parse_retry_after_missing_is_none() {
        let headers = HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_delta_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("120"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_mins(2)));
    }

    #[test]
    fn parse_retry_after_http_date() {
        let mut headers = HeaderMap::new();
        // Build a guaranteed-valid future HTTP date via httpdate's own formatter,
        // so `parse_http_date` round-trips it successfully.
        let future = SystemTime::now() + Duration::from_hours(1);
        let date_str = httpdate::fmt_http_date(future);
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&date_str).unwrap(),
        );
        let delay = parse_retry_after(&headers);
        assert!(delay.is_some());
        assert!(delay.unwrap() > Duration::ZERO);
    }

    #[test]
    fn parse_retry_after_unparseable_is_none() {
        // Not a number and not a valid HTTP date -> falls through to `None`.
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("not-a-time"));
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_past_date_is_none() {
        // A valid HTTP date in the past yields a negative duration -> `None`.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("Mon, 01 Jan 2001 00:00:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[tokio::test]
    async fn test_smart_redirect_404_returns_status_code_error() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock = server
            .mock("GET", "/404")
            .with_status(404)
            .create_async()
            .await;
        let redirect_client = SmartRedirectClient::new(client, None, None, None, None, None, 10);
        let url = Url::parse(&format!("{}/404", server.url())).unwrap();
        let err = redirect_client
            .get(url, None)
            .send()
            .await
            .expect_err("404 should produce a StatusCode error");
        assert!(matches!(err.0, ReqwestResponseError::StatusCode(_)));
    }

    #[tokio::test]
    async fn test_smart_redirect_302_without_location_errors() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock = server
            .mock("GET", "/redir")
            .with_status(302)
            .create_async()
            .await;
        let redirect_client = SmartRedirectClient::new(client, None, None, None, None, None, 10);
        let url = Url::parse(&format!("{}/redir", server.url())).unwrap();
        let err = redirect_client
            .get(url, None)
            .send()
            .await
            .expect_err("a 302 without Location should error");
        assert!(matches!(err.0, ReqwestResponseError::StatusCode(_)));
    }

    #[tokio::test]
    async fn test_smart_redirect_302_unjoinable_location_errors() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock = server
            .mock("GET", "/redir")
            .with_status(302)
            .with_header("Location", "http://")
            .create_async()
            .await;
        let redirect_client = SmartRedirectClient::new(client, None, None, None, None, None, 10);
        let url = Url::parse(&format!("{}/redir", server.url())).unwrap();
        let err = redirect_client
            .get(url, None)
            .send()
            .await
            .expect_err("an unjoinable Location should error");
        assert!(matches!(err.0, ReqwestResponseError::StatusCode(_)));
    }

    #[tokio::test]
    async fn test_smart_redirect_reads_referrer_policy() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock_redir = server
            .mock("GET", "/src")
            .with_status(302)
            .with_header("Location", "/dst")
            .with_header("Referrer-Policy", "no-referrer")
            .create_async()
            .await;
        let _mock_dst = server
            .mock("GET", "/dst")
            .with_status(200)
            .with_header("Content-Length", "5")
            .with_body("hello")
            .create_async()
            .await;
        let redirect_client = SmartRedirectClient::new(client, None, None, None, None, None, 10);
        let url = Url::parse(&format!("{}/src", server.url())).unwrap();
        let resp = redirect_client
            .get(url, None)
            .send()
            .await
            .expect("redirect with Referrer-Policy should succeed");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_smart_redirect_injects_resource_headers() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock = server
            .mock("GET", "/dst")
            .with_status(200)
            .with_header("Content-Length", "5")
            .with_body("hello")
            .create_async()
            .await;
        // First-hop resource-specific headers are injected only on redirect_count == 0.
        let redirect_client = SmartRedirectClient::new(
            client,
            None,
            Some(ReferrerPolicy::NoReferrer),
            Some(HeaderValue::from_static("https://origin.example")),
            Some(HeaderValue::from_static("secret-token")),
            Some(HeaderValue::from_static("cookie-value")),
            10,
        );
        let url = Url::parse(&format!("{}/dst", server.url())).unwrap();
        let resp = redirect_client
            .get(url, None)
            .send()
            .await
            .expect("request with resource headers should succeed");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_smart_redirect_with_range_request() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock = server
            .mock("GET", "/range")
            .with_status(200)
            .with_header("Content-Length", "4")
            .with_body("data")
            .create_async()
            .await;
        let redirect_client = SmartRedirectClient::new(client, None, None, None, None, None, 10);
        let url = Url::parse(&format!("{}/range", server.url())).unwrap();
        // A ranged request exercises the `if let Some(ref range)` branch that
        // attaches the `Range` header (lines 219-224).
        let resp = redirect_client
            .get(url, Some(0..3))
            .send()
            .await
            .expect("ranged request must succeed");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Verifies the `Range` header survives a manual redirect: the request is
    /// re-issued to the redirect target with the same byte range, so a ranged
    /// download still works when the URL redirects.
    #[tokio::test]
    async fn test_smart_redirect_preserves_range_across_redirect() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock_src = server
            .mock("GET", "/src")
            .with_status(302)
            .with_header("Location", "/dst")
            .create_async()
            .await;
        // The target only matches if the Range header survives the redirect hop.
        let _mock_dst = server
            .mock("GET", "/dst")
            .match_header("Range", "bytes=0-2")
            .with_status(206)
            .with_header("Content-Length", "3")
            .with_body("dat")
            .create_async()
            .await;
        let redirect_client = SmartRedirectClient::new(client, None, None, None, None, None, 10);
        let url = Url::parse(&format!("{}/src", server.url())).unwrap();
        // Range 0..3 -> "bytes=0-2"; must be re-sent to /dst after the hop.
        let resp = redirect_client
            .get(url, Some(0..3))
            .send()
            .await
            .expect("ranged request across redirect must succeed");
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    }
}

#[cfg(test)]
mod range_underflow_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::http::HttpClient;

    #[test]
    fn client_get_empty_range_does_not_underflow() {
        // Regression for hypothesis D: `range.end.saturating_sub(1)` must not
        // underflow u64 when `range.end == 0`. Building the Range header for an
        // empty range (`0..0`) must construct a request without panicking.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let _ = HttpClient::get(
            &client,
            Url::parse("http://example.com/x").unwrap(),
            Some(0..0u64),
        );
    }

    #[tokio::test]
    async fn smart_redirect_get_empty_range_does_not_underflow() {
        // Same fix in the production path (`ManualRedirectRequestBuilder::send`).
        // A local mock server avoids a real network call while still exercising
        // the Range-header construction inside `send`.
        let mut server = mockito::Server::new_async().await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let _mock = server
            .mock("GET", "/x")
            .with_status(200)
            .with_header("Content-Length", "1")
            .with_body("a")
            .create_async()
            .await;
        let rc = SmartRedirectClient::new(client, None, None, None, None, None, 10);
        let resp = rc
            .get(
                Url::parse(&format!("{}/x", server.url())).unwrap(),
                Some(0..0u64),
            )
            .send()
            .await;
        // The mock server is no longer needed once the response is received.
        drop(server);
        assert!(resp.is_ok(), "empty-range request must not panic");
    }
}
