//! Prefetch metadata for a downloadable URL over HTTP.
//!
//! [`Prefetch::prefetch`] issues the initial GET (and a range probe) through a
//! [`crate::http::HttpClient`], then assembles a [`crate::UrlInfo`] describing
//! the resource: its size, suggested filename, content type, range support, and
//! the [`crate::FileId`] used for resumable downloads.

use crate::{
    UrlInfo,
    http::{
        ContentDisposition, ContentRange, GetResponse, HttpClient, HttpError, HttpHeaders,
        HttpRequestBuilder, HttpResponse,
    },
    url_info::FileId,
};
use fast_pull::PullResult;
use std::{borrow::Borrow, future::Future, time::Duration};
use url::Url;

/// Result of a prefetch operation: the metadata ([`UrlInfo`]) and the initial HTTP response.
///
/// The error is the layer's own [`HttpError`]: a transport failure surfaces as
/// [`HttpError::Request`], an attempt that outlives its budget as
/// [`HttpError::Timeout`].
pub type PrefetchResult<Client> = PullResult<(UrlInfo, GetResponse<Client>), HttpError<Client>>;

/// The caller's budgets and probe policy for one prefetch attempt.
///
/// The crate supplies no defaults. How long to wait, how many probe attempts to
/// spend and how far apart they sit all depend on the caller's latency budget
/// and on how much it cares about the multi-chunk fast path.
#[derive(Clone, Copy, Debug)]
pub struct PrefetchOptions {
    /// Upper bound on one `bytes=0-0` probe request. [`Duration::ZERO`] disables
    /// the bound.
    pub probe_timeout: Duration,
    /// Upper bound on the metadata request and its response head.
    /// [`Duration::ZERO`] disables the bound.
    pub metadata_timeout: Duration,
    /// Number of probe attempts, the first one included. `0` skips the probe,
    /// which keeps the download on the single-stream path without asking the
    /// server whether it supports ranges.
    pub probe_attempts: usize,
    /// Wait between probe attempts.
    pub probe_gap: Duration,
}

impl PrefetchOptions {
    /// Build the options explicitly; there is no default.
    #[must_use]
    pub const fn new(
        probe_timeout: Duration,
        metadata_timeout: Duration,
        probe_attempts: usize,
        probe_gap: Duration,
    ) -> Self {
        Self {
            probe_timeout,
            metadata_timeout,
            probe_attempts,
            probe_gap,
        }
    }
}

/// Trait for fetching resource metadata (size, filename, range support) from a URL.
///
/// Implementors perform a GET request to gather [`UrlInfo`] and
/// the initial response for subsequent downloading.
pub trait Prefetch<Client: HttpClient> {
    /// Fetch the resource metadata and the initial response for `url`.
    ///
    /// `options` carries both budgets and the probe's retry policy. A request
    /// that outlives its budget fails with [`HttpError::Timeout`], which the
    /// caller treats like any other failed attempt (typically: retry).
    fn prefetch(
        &self,
        url: Url,
        options: PrefetchOptions,
    ) -> impl Future<Output = PrefetchResult<Client>> + Send;
}

impl<Client, BorrowClient> Prefetch<Client> for BorrowClient
where
    Client: HttpClient,
    BorrowClient: Borrow<Client> + Sync,
{
    async fn prefetch(&self, url: Url, options: PrefetchOptions) -> PrefetchResult<Client> {
        prefetch_within(self.borrow(), url, options).await
    }
}

fn get_filename(headers: &impl HttpHeaders, url: &Url) -> String {
    headers
        .get("content-disposition")
        .ok()
        .and_then(|s| ContentDisposition::parse(s.as_ref()).filename)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|s| {
                    let s = urlencoding::decode_binary(s.as_bytes());
                    String::from_utf8_lossy(&s).into_owned()
                })
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| url.host_str().map(|s| s.replace('.', "_")))
        .unwrap_or_else(|| url.to_string().replace('.', "_"))
}

/// [`Prefetch::prefetch`] with the caller's budgets and probe policy applied.
async fn prefetch_within<Client: HttpClient>(
    client: &Client,
    url: Url,
    options: PrefetchOptions,
) -> PrefetchResult<Client> {
    let (result_no_range, range) = tokio::join!(
        prefetch_no_range(client, url.clone(), options.metadata_timeout),
        is_support_range(client, url, options)
    );
    let mut res = result_no_range?;
    if let Some(range) = range {
        res.0.supports_range = true;
        // A chunked metadata response declares no length, so the only size on
        // offer is the one the probe's `Content-Range` names. Without it the
        // fast path would stay closed even though the server honours ranges.
        if res.0.size == 0
            && let Some(complete) = range.complete_length()
        {
            res.0.size = complete;
        }
        if res.0.size != 0 {
            res.0.fast_download = true;
        }
    }
    Ok(res)
}

async fn prefetch_no_range<Client: HttpClient>(
    client: &Client,
    url: Url,
    timeout: Duration,
) -> PrefetchResult<Client> {
    // The request and the wait for the response head share one budget: a server
    // that accepts the connection and then stays silent has to end the attempt
    // instead of pinning the whole prefetch to it. `ZERO` means the caller wants
    // no bound at all.
    let request = client.get(url, None).send();
    let attempt = if timeout.is_zero() {
        Ok(request.await)
    } else {
        tokio::time::timeout(timeout, request).await
    };
    let resp = attempt
        .map_err(|_| (HttpError::Timeout(timeout), None))?
        .map_err(|(e, retry)| (HttpError::Request(e), retry))?;
    let headers = resp.headers();
    let size = headers
        .get("content-length")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let final_url = resp.url();
    Ok((
        UrlInfo {
            final_url: final_url.clone(),
            raw_name: get_filename(headers, final_url),
            size,
            supports_range: false,
            fast_download: false,
            file_id: FileId::new(
                headers.get("etag").ok().as_deref(),
                headers.get("last-modified").ok().as_deref(),
            ),
            content_type: headers.get("content-type").ok().map(String::from),
        },
        resp,
    ))
}

/// Whether the server honours a `bytes=0-0` range request, returning the
/// `Content-Range` answer it gave when it does.
///
/// A server that genuinely ignores ranges answers with a success status and no
/// `Content-Range`, which resolves to `None` without any retry: degrading to a
/// single stream stays a valid outcome. A probe that *fails* — an error, or a
/// request that outlives the caller's `probe_timeout` — is not that answer, so the
/// failure is never folded into the "no ranges" result: the cheap probe is
/// retried up to `options.probe_attempts` times, mirroring the retries the whole
/// prefetch already gets a layer up, and only a probe that keeps failing for
/// every attempt falls back to the conservative single-stream download.
///
/// The parsed value is returned rather than a bare `true` because the probe's
/// `Content-Range` is also where a chunked metadata response's size comes from.
///
/// Retrying is the handling here rather than reporting on
/// `Event::PrefetchError`, because this layer has no event channel: that event
/// carries a `ReqwestResponseError`, which only the `reqwest` backend can
/// build, while `prefetch` is generic over [`HttpClient`]. Fabricating one is
/// exactly what this probe must stop doing.
async fn is_support_range<Client: HttpClient>(
    client: &Client,
    url: Url,
    options: PrefetchOptions,
) -> Option<ContentRange> {
    let mut attempt = 0;
    while attempt < options.probe_attempts {
        attempt += 1;
        // The request and the wait for an answer share one budget: a server that
        // accepts the connection and then stays silent has to end the attempt
        // rather than pin the whole `prefetch` to a dead socket. `ZERO` disables
        // the bound.
        let request = client.get(url.clone(), Some(0..1)).send();
        let probe = if options.probe_timeout.is_zero() {
            Ok(request.await)
        } else {
            tokio::time::timeout(options.probe_timeout, request).await
        };
        match probe {
            Ok(Ok(resp)) => return zero_range_answer(resp.headers()),
            // Retries exhausted: the caller keeps the download alive as a
            // single stream instead of failing on an advisory probe.
            Ok(Err(_)) | Err(_) if attempt >= options.probe_attempts => return None,
            Ok(Err(_)) | Err(_) => tokio::time::sleep(options.probe_gap).await,
        }
    }
    // `probe_attempts == 0`: the caller opted out of the probe entirely.
    None
}

/// The `bytes 0-0/<size>` value a probe response carries, if any.
///
/// Uses the same `ContentRange` parser as the downloader: a value the
/// downloader reads an offset from must also count as range support here, or a
/// large file silently degrades to a single stream.
pub(super) fn zero_range_answer(headers: &impl HttpHeaders) -> Option<ContentRange> {
    headers
        .get("content-range")
        .ok()
        .and_then(|raw| ContentRange::parse(&raw))
        .filter(|range| range.is_zero_probe())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// Budgets and probe policy for the prefetch calls below: these tests
    /// exercise parsing and retry behaviour, not the timeout paths.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
    const METADATA_TIMEOUT: Duration = Duration::from_secs(5);
    const PROBE_ATTEMPTS: usize = 3;
    const PROBE_GAP: Duration = Duration::from_millis(1);

    fn options() -> PrefetchOptions {
        PrefetchOptions::new(PROBE_TIMEOUT, METADATA_TIMEOUT, PROBE_ATTEMPTS, PROBE_GAP)
    }

    #[derive(Debug)]
    struct MapHeaders(HashMap<String, String>);
    impl HttpHeaders for MapHeaders {
        type GetHeaderError = std::convert::Infallible;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            Ok(Cow::Borrowed(self.0.get(header).map_or("", String::as_str)))
        }
    }

    #[test]
    fn get_filename_filename_star_not_double_decoded() {
        // `ContentDisposition::parse` already percent-decodes
        // `filename*` ("%25100" -> "%100"). `get_filename` then calls
        // `urlencoding::decode_binary` on the decoded string, double-decoding
        // it. A server that intends the literal filename "%100" (sent as
        // `filename*=UTF-8''%25100`) must survive unchanged.
        let mut h = HashMap::new();
        h.insert(
            "content-disposition".to_string(),
            "attachment; filename*=UTF-8''%25100".to_string(),
        );
        let headers = MapHeaders(h);
        let url = Url::parse("http://example.com/").unwrap();
        assert_eq!(get_filename(&headers, &url), "%100");
    }

    #[test]
    fn get_filename_filename_star_no_percent_passthrough() {
        // Control: when the decoded `filename*` contains no literal '%' byte,
        // `decode_binary` leaves it untouched, so a normal UTF-8 name is correct.
        let mut h = HashMap::new();
        h.insert(
            "content-disposition".to_string(),
            "attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.txt".to_string(),
        );
        let headers = MapHeaders(h);
        let url = Url::parse("http://example.com/").unwrap();
        assert_eq!(get_filename(&headers, &url), "测试.txt");
    }

    #[test]
    fn get_filename_plain_filename_percent_stays_literal() {
        // Plain `filename` is no longer percent-decoded (RFC 6266: quoted-string
        // is literal). "%E6%B5%8B.txt" stays as-is, unlike the old behavior.
        let mut h = HashMap::new();
        h.insert(
            "content-disposition".to_string(),
            "attachment; filename=\"%E6%B5%8B.txt\"".to_string(),
        );
        let headers = MapHeaders(h);
        let url = Url::parse("http://example.com/").unwrap();
        assert_eq!(get_filename(&headers, &url), "%E6%B5%8B.txt");
    }

    #[test]
    fn get_filename_url_path_fallback_decodes_percent() {
        // No content-disposition → falls back to the last URL path segment,
        // which IS percent-decoded (URL paths are always encoded).
        let headers = MapHeaders(HashMap::new());
        let url = Url::parse("http://example.com/dir/%E6%B5%8B%E8%AF%95.txt").unwrap();
        assert_eq!(get_filename(&headers, &url), "测试.txt");
    }

    #[test]
    fn get_filename_url_path_invalid_utf8_uses_replacement() {
        // %FF is not valid UTF-8; from_utf8_lossy produces U+FFFD.
        let headers = MapHeaders(HashMap::new());
        let url = Url::parse("http://example.com/%FF.txt").unwrap();
        assert_eq!(get_filename(&headers, &url), "\u{FFFD}.txt");
    }

    #[test]
    fn get_filename_whitespace_cd_falls_through_to_url() {
        // Whitespace-only filename from CD is filtered out → URL path is used.
        let mut h = HashMap::new();
        h.insert(
            "content-disposition".to_string(),
            "attachment; filename=\"   \"".to_string(),
        );
        let headers = MapHeaders(h);
        let url = Url::parse("http://example.com/report.pdf").unwrap();
        assert_eq!(get_filename(&headers, &url), "report.pdf");
    }

    #[test]
    fn get_filename_empty_path_falls_back_to_host() {
        // Root URL (empty path segment) → host with dots replaced by underscores.
        let headers = MapHeaders(HashMap::new());
        let url = Url::parse("http://cdn.example.com/").unwrap();
        assert_eq!(get_filename(&headers, &url), "cdn_example_com");
    }

    #[test]
    fn get_filename_path_segment_spaces_fall_back_to_host() {
        // A path segment that is only spaces decodes to whitespace → filtered →
        // host fallback.
        let headers = MapHeaders(HashMap::new());
        let url = Url::parse("http://cdn.example.com/%20%20").unwrap();
        assert_eq!(get_filename(&headers, &url), "cdn_example_com");
    }

    /// A client that answers from memory, so the range probe can be observed
    /// without a server: it counts the ranged requests it receives and can fail
    /// the first few of them the way a reset or a timeout does.
    #[derive(Clone)]
    struct ProbeFixture {
        /// Ranged requests received so far.
        probe_attempts: Arc<AtomicUsize>,
        /// How many of the first ranged requests fail instead of answering.
        failing_probes: usize,
        /// Whether an answered ranged request carries a `Content-Range` header.
        serves_ranges: bool,
        /// The `Content-Length` the metadata response carries; `None` is the
        /// chunked case, where the metadata response declares no length at all.
        metadata_length: Option<&'static str>,
    }

    impl ProbeFixture {
        fn new(failing_probes: usize, serves_ranges: bool) -> Self {
            Self {
                probe_attempts: Arc::new(AtomicUsize::new(0)),
                failing_probes,
                serves_ranges,
                metadata_length: Some("1024"),
            }
        }

        /// A metadata response with no `Content-Length`, as a chunked one has.
        fn chunked_metadata(mut self) -> Self {
            self.metadata_length = None;
            self
        }

        fn probes(&self) -> usize {
            self.probe_attempts.load(Ordering::SeqCst)
        }
    }

    impl HttpClient for ProbeFixture {
        type RequestBuilder = FixtureRequestBuilder;

        fn get(&self, url: Url, range: Option<fast_pull::ProgressEntry>) -> Self::RequestBuilder {
            FixtureRequestBuilder {
                url,
                ranged: range.is_some(),
                fixture: self.clone(),
            }
        }
    }

    struct FixtureRequestBuilder {
        url: Url,
        ranged: bool,
        fixture: ProbeFixture,
    }

    impl FixtureRequestBuilder {
        /// What the fixture answers for this request: a metadata `200`, a ranged
        /// `206`, a transport failure for the configured leading probes, or a
        /// plain `200` without `Content-Range` when ranges are not served.
        fn answer(self) -> PullResult<FixtureResponse, FixtureError> {
            let mut headers = HashMap::new();
            if self.ranged {
                let attempt = self.fixture.probe_attempts.fetch_add(1, Ordering::SeqCst);
                if attempt < self.fixture.failing_probes {
                    return Err((FixtureError, None));
                }
                if self.fixture.serves_ranges {
                    headers.insert("content-range".to_string(), "bytes 0-0/1024".to_string());
                }
            } else if let Some(length) = self.fixture.metadata_length {
                headers.insert("content-length".to_string(), length.to_string());
            }
            Ok(FixtureResponse {
                url: self.url,
                headers: MapHeaders(headers),
            })
        }
    }

    impl HttpRequestBuilder for FixtureRequestBuilder {
        type Response = FixtureResponse;
        type RequestError = FixtureError;

        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            std::future::ready(self.answer())
        }
    }

    #[derive(Debug)]
    struct FixtureResponse {
        url: Url,
        headers: MapHeaders,
    }

    impl HttpResponse for FixtureResponse {
        type Headers = MapHeaders;
        type ChunkError = FixtureError;

        fn headers(&self) -> &Self::Headers {
            &self.headers
        }

        fn url(&self) -> &Url {
            &self.url
        }

        fn chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<bytes::Bytes>, Self::ChunkError>> + Send {
            std::future::ready(Ok(None))
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("fixture transport failure")]
    struct FixtureError;

    #[tokio::test]
    async fn probe_failure_is_retried_instead_of_reading_as_no_ranges() {
        // A probe that fails once (timeout, reset, momentary 5xx) used to be
        // indistinguishable from a server that answers "no ranges": the result
        // silently degraded to a single stream with nothing reported. The probe
        // must be retried, and the retry must recover range support.
        let client = ProbeFixture::new(1, true);
        let url = Url::parse("http://example.com/file.bin").unwrap();

        let (info, _resp) = prefetch_within(&client, url, options()).await.unwrap();

        assert_eq!(
            client.probes(),
            2,
            "a failed probe must be retried once before it counts as an answer"
        );
        assert!(
            info.supports_range,
            "the successful retry must still advertise range support"
        );
        assert!(
            info.fast_download,
            "the recovered probe must keep the multi-chunk fast path"
        );
    }

    #[tokio::test]
    async fn genuine_no_ranges_answer_stays_a_single_stream() {
        // Control: a server that answers the ranged request with a plain
        // success and no `Content-Range` says "no ranges" on purpose. That must
        // stay a valid outcome, reached with a single probe and no retry.
        let client = ProbeFixture::new(0, false);
        let url = Url::parse("http://example.com/file.bin").unwrap();

        let (info, _resp) = prefetch_within(&client, url, options()).await.unwrap();

        assert_eq!(client.probes(), 1, "an answer must not be retried");
        assert!(!info.supports_range);
        assert!(!info.fast_download);
    }

    #[tokio::test]
    async fn chunked_metadata_takes_its_size_from_the_range_probe() {
        // A chunked metadata response carries no `Content-Length`, so the size
        // is 0 after the metadata request alone. The probe's `bytes 0-0/1024`
        // still names the complete length, and without it the download would
        // silently degrade to a single stream even though the server supports
        // ranges — exactly what the fast path exists for.
        let client = ProbeFixture::new(0, true).chunked_metadata();
        let url = Url::parse("http://example.com/file.bin").unwrap();

        let (info, _resp) = prefetch_within(&client, url, options()).await.unwrap();

        assert!(info.supports_range, "the probe answered `bytes 0-0/1024`");
        assert_eq!(
            info.size, 1024,
            "the complete length must come from the probe's Content-Range"
        );
        assert!(
            info.fast_download,
            "a known size plus range support must keep the multi-chunk fast path"
        );
    }

    #[tokio::test]
    async fn chunked_metadata_without_ranges_stays_sizeless_and_single_stream() {
        // Control: neither response declares a length, and the ranged request
        // is answered with a plain `200` and no `Content-Range`. Nothing may be
        // invented: the size stays unknown and the download stays single-stream.
        let client = ProbeFixture::new(0, false).chunked_metadata();
        let url = Url::parse("http://example.com/file.bin").unwrap();

        let (info, _resp) = prefetch_within(&client, url, options()).await.unwrap();

        assert_eq!(client.probes(), 1, "an answer must not be retried");
        assert!(!info.supports_range);
        assert!(!info.fast_download);
        assert_eq!(
            info.size, 0,
            "a server that says no ranges declares no size"
        );
    }

    #[tokio::test]
    async fn exhausted_probe_retries_fall_back_to_a_single_stream() {
        // A probe that never answers must not fail a download whose metadata
        // request succeeded: after the bounded attempts the prefetch still
        // resolves, conservatively, as a single-stream download.
        let client = ProbeFixture::new(usize::MAX, true);
        let url = Url::parse("http://example.com/file.bin").unwrap();

        let (info, _resp) = prefetch_within(&client, url, options()).await.unwrap();

        assert_eq!(
            client.probes(),
            PROBE_ATTEMPTS,
            "the probe must be attempted exactly probe_attempts times"
        );
        assert!(!info.supports_range);
        assert!(!info.fast_download);
    }

    /// Answers the metadata request from memory and lets the ranged probe reach a
    /// real server that accepts the connection and then never replies: the case
    /// where the request is delivered but no answer ever arrives.
    #[cfg(feature = "reqwest")]
    #[derive(Clone, Debug)]
    struct HangingProbeClient {
        /// Address of the server that accepts and stays silent.
        addr: std::net::SocketAddr,
        /// Ranged requests sent so far, including the ones that timed out.
        attempts: Arc<AtomicUsize>,
    }

    #[cfg(feature = "reqwest")]
    impl HangingProbeClient {
        fn new(addr: std::net::SocketAddr) -> Self {
            Self {
                addr,
                attempts: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn probes(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }
    }

    #[cfg(feature = "reqwest")]
    impl HttpClient for HangingProbeClient {
        type RequestBuilder = HangingProbeRequestBuilder;

        fn get(&self, url: Url, range: Option<fast_pull::ProgressEntry>) -> Self::RequestBuilder {
            HangingProbeRequestBuilder {
                url,
                ranged: range.is_some(),
                client: self.clone(),
            }
        }
    }

    #[cfg(feature = "reqwest")]
    struct HangingProbeRequestBuilder {
        url: Url,
        ranged: bool,
        client: HangingProbeClient,
    }

    #[cfg(feature = "reqwest")]
    impl HttpRequestBuilder for HangingProbeRequestBuilder {
        type Response = FixtureResponse;
        type RequestError = FixtureError;

        async fn send(self) -> PullResult<Self::Response, Self::RequestError> {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            if !self.ranged {
                let mut headers = HashMap::new();
                headers.insert("content-length".to_string(), "1024".to_string());
                return Ok(FixtureResponse {
                    url: self.url,
                    headers: MapHeaders(headers),
                });
            }

            self.client.attempts.fetch_add(1, Ordering::SeqCst);
            let mut stream = tokio::net::TcpStream::connect(self.client.addr)
                .await
                .map_err(|_| (FixtureError, None))?;
            stream
                .write_all(b"GET /probe HTTP/1.1\r\nRange: bytes=0-0\r\n\r\n")
                .await
                .map_err(|_| (FixtureError, None))?;
            // The server holds the connection open without answering, so this
            // read stays pending and only the probe's timeout can end the
            // attempt.
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf).await;
            Err((FixtureError, None))
        }
    }

    /// A server that accepts a connection and never answers must not keep the
    /// prefetch waiting forever: the probe is bounded by its timeout, retried the
    /// usual number of times, and then reported as no range support.
    #[cfg(feature = "reqwest")]
    #[tokio::test]
    async fn a_probe_that_never_answers_times_out_into_a_single_stream() {
        use tokio::net::TcpListener;

        // Hold every accepted connection open without writing a byte.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                // Keep the connection open without answering: the probe waits for
                // a response that never comes.
                tokio::spawn(async move {
                    let _socket = socket;
                    std::future::pending::<()>().await;
                });
            }
        });

        let client = HangingProbeClient::new(addr);
        let url = Url::parse(&format!("http://{addr}/file.bin")).unwrap();
        let started = std::time::Instant::now();
        let (info, _resp) = prefetch_within(
            &client,
            url,
            PrefetchOptions::new(
                Duration::from_millis(100),
                METADATA_TIMEOUT,
                PROBE_ATTEMPTS,
                PROBE_GAP,
            ),
        )
        .await
        .expect("a hanging probe must not fail a prefetch whose metadata arrived");
        let elapsed = started.elapsed();

        assert!(
            !info.supports_range,
            "a probe that never answered cannot claim range support"
        );
        assert!(!info.fast_download, "the fast path needs a range answer");
        assert_eq!(
            info.size, 1024,
            "the metadata request still supplies the size"
        );
        assert_eq!(
            client.probes(),
            PROBE_ATTEMPTS,
            "a timed-out attempt must be retried like a reset"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the probe must end once its timeout budget is spent, took {elapsed:?}"
        );
        server.abort();
    }

    /// A client whose metadata request reaches a real server that accepts the
    /// connection and never answers, while the ranged probe answers at once with
    /// no range support. The metadata budget is the only thing under test.
    #[cfg(feature = "reqwest")]
    #[derive(Clone, Debug)]
    struct SilentMetadataClient {
        /// Address of the server that accepts and stays silent.
        addr: std::net::SocketAddr,
    }

    #[cfg(feature = "reqwest")]
    impl HttpClient for SilentMetadataClient {
        type RequestBuilder = SilentMetadataRequestBuilder;

        fn get(&self, url: Url, range: Option<fast_pull::ProgressEntry>) -> Self::RequestBuilder {
            SilentMetadataRequestBuilder {
                url,
                ranged: range.is_some(),
                addr: self.addr,
            }
        }
    }

    #[cfg(feature = "reqwest")]
    struct SilentMetadataRequestBuilder {
        url: Url,
        ranged: bool,
        addr: std::net::SocketAddr,
    }

    #[cfg(feature = "reqwest")]
    impl HttpRequestBuilder for SilentMetadataRequestBuilder {
        type Response = FixtureResponse;
        type RequestError = FixtureError;

        async fn send(self) -> PullResult<Self::Response, Self::RequestError> {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            if self.ranged {
                // A plain `200` without `Content-Range`: no ranges, so the probe
                // resolves without waiting on the silent server.
                return Ok(FixtureResponse {
                    url: self.url,
                    headers: MapHeaders(HashMap::new()),
                });
            }

            let mut stream = tokio::net::TcpStream::connect(self.addr)
                .await
                .map_err(|_| (FixtureError, None))?;
            stream
                .write_all(b"GET /file.bin HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .map_err(|_| (FixtureError, None))?;
            // The server holds the connection open without answering, so this read
            // stays pending and only the metadata timeout can end the attempt.
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf).await;
            Err((FixtureError, None))
        }
    }

    /// A metadata request that is accepted but never answered must fail once its
    /// budget is spent, so the caller's retries can run, instead of parking the
    /// resolve step on a dead socket forever.
    #[cfg(feature = "reqwest")]
    #[tokio::test]
    async fn a_metadata_request_that_never_answers_times_out() {
        use tokio::net::TcpListener;

        // Hold every accepted connection open without writing a byte.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _socket = socket;
                    std::future::pending::<()>().await;
                });
            }
        });

        let client = SilentMetadataClient { addr };
        let url = Url::parse(&format!("http://{addr}/file.bin")).unwrap();
        let budget = Duration::from_millis(100);
        let started = std::time::Instant::now();
        let result = prefetch_within(
            &client,
            url,
            PrefetchOptions::new(Duration::from_millis(50), budget, PROBE_ATTEMPTS, PROBE_GAP),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err((HttpError::Timeout(timeout), None)) if timeout == budget),
            "a metadata request that never answered must time out, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the metadata budget must end the request, took {elapsed:?}"
        );
        server.abort();
    }
}
