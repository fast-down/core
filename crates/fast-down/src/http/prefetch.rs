//! Prefetch metadata for a downloadable URL over HTTP.
//!
//! [`Prefetch::prefetch`] issues the initial GET (and a range probe) through a
//! [`crate::http::HttpClient`], then assembles a [`crate::UrlInfo`] describing
//! the resource: its size, suggested filename, content type, range support, and
//! the [`crate::FileId`] used for resumable downloads.

use crate::{
    UrlInfo,
    http::{
        ContentDisposition, ContentRange, GetRequestError, GetResponse, HttpClient, HttpHeaders,
        HttpRequestBuilder, HttpResponse,
    },
    url_info::FileId,
};
use fast_pull::PullResult;
use std::{borrow::Borrow, future::Future, time::Duration};
use url::Url;

/// Result of a prefetch operation: the metadata ([`UrlInfo`]) and the initial HTTP response.
pub type PrefetchResult<Client> =
    PullResult<(UrlInfo, GetResponse<Client>), GetRequestError<Client>>;

/// Trait for fetching resource metadata (size, filename, range support) from a URL.
///
/// Implementors perform a GET request to gather [`UrlInfo`] and
/// the initial response for subsequent downloading.
pub trait Prefetch<Client: HttpClient> {
    /// Fetch the resource metadata and the initial response for `url`.
    fn prefetch(&self, url: Url) -> impl Future<Output = PrefetchResult<Client>> + Send;
}

impl<Client, BorrowClient> Prefetch<Client> for BorrowClient
where
    Client: HttpClient,
    BorrowClient: Borrow<Client> + Sync,
{
    async fn prefetch(&self, url: Url) -> PrefetchResult<Client> {
        prefetch(self.borrow(), url).await
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

async fn prefetch<Client: HttpClient>(client: &Client, url: Url) -> PrefetchResult<Client> {
    let (result_no_range, supports_range) = tokio::join!(
        prefetch_no_range(client, url.clone()),
        is_support_range(client, url)
    );
    let mut res = result_no_range?;
    if supports_range {
        res.0.supports_range = true;
        if res.0.size != 0 {
            res.0.fast_download = true;
        }
    }
    Ok(res)
}

async fn prefetch_no_range<Client: HttpClient>(
    client: &Client,
    url: Url,
) -> PrefetchResult<Client> {
    let resp = client.get(url, None).send().await?;
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

/// Number of times the range probe is attempted, the first one included.
///
/// The probe only decides whether the download may use the multi-chunk fast
/// path, so a failed probe must not abort a download whose metadata request
/// succeeded. It is retried a few times (see [`is_support_range`]) because a
/// transient failure — timeout, reset, momentary 5xx — must not be mistaken for
/// the server's answer.
const RANGE_PROBE_ATTEMPTS: usize = 3;

/// Wait between range-probe attempts.
///
/// Every attempt is a single ranged `GET` that runs in parallel with the
/// metadata request, so a short gap is enough to ride out a reset or a blip.
const RANGE_PROBE_GAP: Duration = Duration::from_millis(200);

/// Whether the server honours a `bytes=0-0` range request.
///
/// A server that genuinely ignores ranges answers with a success status and no
/// `Content-Range`, which resolves to `false` without any retry: degrading to a
/// single stream stays a valid outcome. A probe that *fails* is not that
/// answer, so the failure is never folded into the "no ranges" result: the
/// cheap probe is retried up to [`RANGE_PROBE_ATTEMPTS`] times, mirroring the
/// retries the whole prefetch already gets a layer up, and only a probe that
/// keeps failing for every attempt falls back to the conservative
/// single-stream download.
///
/// Retrying is the handling here rather than reporting on
/// `Event::PrefetchError`, because this layer has no event channel: that event
/// carries a `ReqwestResponseError`, which only the `reqwest` backend can
/// build, while `prefetch` is generic over [`HttpClient`]. Fabricating one is
/// exactly what this probe must stop doing.
async fn is_support_range<Client: HttpClient>(client: &Client, url: Url) -> bool {
    let mut attempt = 1;
    loop {
        match client.get(url.clone(), Some(0..1)).send().await {
            Ok(resp) => return advertises_zero_range(resp.headers()),
            Err(_) if attempt < RANGE_PROBE_ATTEMPTS => {
                tokio::time::sleep(RANGE_PROBE_GAP).await;
                attempt += 1;
            }
            // Retries exhausted: the caller keeps the download alive as a
            // single stream instead of failing on an advisory probe.
            Err(_) => return false,
        }
    }
}

/// Whether a probe response advertises the `bytes 0-0/<size>` answer to a
/// `Range: bytes=0-0` request.
///
/// Uses the same `ContentRange` parser as the downloader: a value the
/// downloader reads an offset from must also count as range support here, or a
/// large file silently degrades to a single stream.
pub(super) fn advertises_zero_range(headers: &impl HttpHeaders) -> bool {
    headers
        .get("content-range")
        .ok()
        .and_then(|raw| ContentRange::parse(&raw))
        .is_some_and(ContentRange::is_zero_probe)
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
    }

    impl ProbeFixture {
        fn new(failing_probes: usize, serves_ranges: bool) -> Self {
            Self {
                probe_attempts: Arc::new(AtomicUsize::new(0)),
                failing_probes,
                serves_ranges,
            }
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
            } else {
                headers.insert("content-length".to_string(), "1024".to_string());
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

        let (info, _resp) = prefetch(&client, url).await.unwrap();

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

        let (info, _resp) = prefetch(&client, url).await.unwrap();

        assert_eq!(client.probes(), 1, "an answer must not be retried");
        assert!(!info.supports_range);
        assert!(!info.fast_download);
    }

    #[tokio::test]
    async fn exhausted_probe_retries_fall_back_to_a_single_stream() {
        // A probe that never answers must not fail a download whose metadata
        // request succeeded: after the bounded attempts the prefetch still
        // resolves, conservatively, as a single-stream download.
        let client = ProbeFixture::new(usize::MAX, true);
        let url = Url::parse("http://example.com/file.bin").unwrap();

        let (info, _resp) = prefetch(&client, url).await.unwrap();

        assert_eq!(
            client.probes(),
            RANGE_PROBE_ATTEMPTS,
            "the probe must be attempted exactly RANGE_PROBE_ATTEMPTS times"
        );
        assert!(!info.supports_range);
        assert!(!info.fast_download);
    }
}
