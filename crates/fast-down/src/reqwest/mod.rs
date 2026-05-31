#![cfg(not(target_family = "wasm"))]

use crate::http::{
    HttpClient, HttpHeaders, HttpRequestBuilder, HttpResponse,
    manual_redirect::{ReferrerPolicy, compute_referer},
};
use fast_pull::ProgressEntry;
use httpdate::parse_http_date;
use reqwest::{
    Client, RequestBuilder, Response, StatusCode,
    header::{self, HeaderMap, HeaderValue, InvalidHeaderName},
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
                format!("bytes={}-{}", range.start, range.end - 1),
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
            .map_err(|e| (ReqwestResponseError::Reqwest(e), None))?;
        let status = res.status();
        if status.is_success() {
            Ok(res)
        } else {
            let retry_after = parse_retry_after(res.headers());
            Err((ReqwestResponseError::StatusCode(status), retry_after))
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

#[derive(thiserror::Error, Debug)]
pub enum ReqwestGetHeaderError {
    #[error("Invalid header name {0:?}")]
    InvalidHeaderName(InvalidHeaderName),
    #[error("Header not found")]
    NotFound,
}

#[derive(thiserror::Error, Debug)]
pub enum ReqwestResponseError {
    #[error("Reqwest error {0:?}")]
    Reqwest(reqwest::Error),
    #[error("Status code {0:?}")]
    StatusCode(reqwest::StatusCode),
}

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
/// respecting the `Referrer-Policy` header per RFC 9110 §7.4.
#[derive(Debug, Clone)]
pub struct SmartRedirectClient {
    client: Client,
    initial_referer: Option<HeaderValue>,
    referrer_policy: Option<ReferrerPolicy>,
    max_redirects: usize,
}

impl SmartRedirectClient {
    #[must_use]
    pub const fn new(
        client: Client,
        initial_referer: Option<HeaderValue>,
        referrer_policy: Option<ReferrerPolicy>,
        max_redirects: usize,
    ) -> Self {
        Self {
            client,
            initial_referer,
            referrer_policy,
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
/// - Follows only 301, 302, 303, 307, 308 status codes.
pub struct ManualRedirectRequestBuilder {
    client: Client,
    url: Url,
    range: Option<ProgressEntry>,
    next_referer: Option<HeaderValue>,
    referrer_policy: Option<ReferrerPolicy>,
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
                    format!("bytes={}-{}", range.start, range.end - 1),
                );
            }
            if let Some(ref referer) = self.next_referer {
                req = req.header(header::REFERER, referer);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| (ReqwestResponseError::Reqwest(e), None))?;

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
                    Err((ReqwestResponseError::StatusCode(status), retry_after))
                };
            }
            if self.redirect_count >= self.max_redirects {
                let retry_after = parse_retry_after(resp.headers());
                return Err((ReqwestResponseError::StatusCode(status), retry_after));
            }
            let location = if let Some(v) = resp.headers().get(header::LOCATION)
                && let Ok(s) = v.to_str()
            {
                s
            } else {
                let retry_after = parse_retry_after(resp.headers());
                return Err((ReqwestResponseError::StatusCode(status), retry_after));
            };
            let Ok(next_url) = self.url.join(location) else {
                let retry_after = parse_retry_after(resp.headers());
                return Err((ReqwestResponseError::StatusCode(status), retry_after));
            };
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
        http::{HttpError, HttpPuller, Prefetch},
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
                HttpError::Request(e) => match e {
                    ReqwestResponseError::Reqwest(error) => unreachable!("{error:?}"),
                    ReqwestResponseError::StatusCode(status_code) => {
                        assert_eq!(status_code, StatusCode::NOT_FOUND);
                    }
                },
                HttpError::Chunk(_, _) | HttpError::Irrecoverable => {
                    unreachable!()
                }
                HttpError::MismatchedBody(file_id, _) => {
                    unreachable!("404 status code should not return mismatched body: {file_id:?}")
                }
            },
        }
    }

    #[tokio::test]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(300 * 1024 * 1024);
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let mock_body_clone = mock_data.clone();
        let _mock = server
            .mock("GET", "/concurrent")
            .with_status(206)
            .with_header("Accept-Ranges", "bytes")
            .with_body_from_request(move |request| {
                if !request.has_header("Range") {
                    return mock_body_clone.clone();
                }
                let range = request.header("Range")[0];
                println!("range: {range:?}");
                range
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
                    .flat_map(|p| mock_body_clone[p].to_vec())
                    .collect()
            })
            .create_async()
            .await;
        let puller = HttpPuller::new(
            format!("{}/concurrent", server.url()).parse().unwrap(),
            client,
            None,
            FileId::default(),
        );
        let pusher = MemPusher::with_capacity(mock_data.len());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher.clone(),
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
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::PullProgress(_, p) => {
                    pull_progress.merge_progress(p);
                }
                Event::PushProgress(_, p) => {
                    push_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        dbg!(&pull_progress);
        dbg!(&push_progress);
        assert_eq!(pull_progress, download_chunks);
        assert_eq!(push_progress, download_chunks);

        result.join().await.unwrap();
        assert_eq!(&**pusher.receive.lock(), mock_data);
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
            format!("{}/sequential", server.url()).parse().unwrap(),
            client,
            None,
            FileId::default(),
        );
        let pusher = MemPusher::with_capacity(mock_data.len());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_single(
            puller,
            pusher.clone(),
            single::DownloadOptions {
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
            },
        );

        let mut pull_progress: Vec<ProgressEntry> = Vec::new();
        let mut push_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::PullProgress(_, p) => {
                    pull_progress.merge_progress(p);
                }
                Event::PushProgress(_, p) => {
                    push_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        dbg!(&pull_progress);
        dbg!(&push_progress);
        assert_eq!(pull_progress, download_chunks);
        assert_eq!(push_progress, download_chunks);

        result.join().await.unwrap();
        assert_eq!(&**pusher.receive.lock(), mock_data);
    }
}
