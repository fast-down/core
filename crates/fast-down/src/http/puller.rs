//! An HTTP implementation of the [`fast_pull::Puller`] trait.
//!
//! [`HttpPuller`] builds range requests through the generic [`crate::http::HttpClient`]
//! trait and streams the response body back as a [`fast_pull::PullStream`]. It
//! verifies the server's `ETag` / `Last-Modified` headers against the expected
//! [`crate::FileId`] so that a changed file is reported as [`crate::http::HttpError::MismatchedBody`]
//! rather than silently corrupting an incremental download, and it verifies the
//! `Content-Range` of every response: its first byte must be the offset that was
//! requested, a range it declares shorter than the request is reported as a
//! changed resource, and a mid-file request answered without one is refused —
//! so a server that ignores `Range` cannot have its whole-file body written at
//! the requested offset.

use crate::http::{
    ContentRange, FileId, GetResponse, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder,
    HttpResponse, content_encoding,
};
use async_stream::stream;
use fast_pull::{ProgressEntry, PullResult, PullStream, Puller};
use parking_lot::Mutex;
use std::{fmt::Debug, sync::Arc, time::Duration};
use url::Url;

/// A [`Puller`] implementation that fetches data over HTTP.
///
/// Uses the generic [`HttpClient`] trait so it can work with any HTTP backend
/// (e.g. reqwest, custom clients). Supports range requests, file identity
/// checking, and reusing previously opened responses.
///
/// Every response must carry the resource's own bytes. `Accept-Encoding:
/// identity` is requested, but that only negotiates: a server may answer with a
/// coding anyway, and the backend may decode it. A response whose
/// `Content-Encoding` is not `identity` is therefore reported as
/// [`HttpError::ContentEncoding`] instead of being streamed — the engine writes
/// bytes at the offsets it asked for, which a coded body does not line up with.
pub struct HttpPuller<Client: HttpClient> {
    client: Client,
    url: Arc<Url>,
    resp: Option<Arc<Mutex<Option<GetResponse<Client>>>>>,
    file_id: FileId,
}
impl<C: HttpClient> Clone for HttpPuller<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            url: self.url.clone(),
            resp: self.resp.clone(),
            file_id: self.file_id.clone(),
        }
    }
}
impl<Client: HttpClient> HttpPuller<Client> {
    /// Create a new [`HttpPuller`].
    ///
    /// * `url` — the resource to download.
    /// * `client` — the HTTP client used to issue requests.
    /// * `resp` — an optional already-open response to reuse for the first
    ///   (full-file) request, typically the one produced by a prefetch.
    /// * `file_id` — the expected [`crate::FileId`], compared against the
    ///   server's headers to detect a changed resource.
    pub const fn new(
        url: Arc<Url>,
        client: Client,
        resp: Option<Arc<Mutex<Option<GetResponse<Client>>>>>,
        file_id: FileId,
    ) -> Self {
        Self {
            client,
            url,
            resp,
            file_id,
        }
    }
}

impl<Client: HttpClient> Debug for HttpPuller<Client> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpPuller")
            .field("url", &self.url)
            .field("file_id", &self.file_id)
            .field("client", &"...")
            .field("resp", &"...")
            .finish()
    }
}

/// Refuse a response whose body does not start where the request asked.
///
/// A `206` states its offset in `Content-Range`, so any other offset means the
/// body belongs somewhere else and would be written at the wrong place. A `200`
/// without the header is fine only for a request rooted at byte 0: the server
/// ignored `Range` and sent the whole file, which starts there.
fn reject_wrong_start<Client: HttpClient>(
    resp: GetResponse<Client>,
    requested_start: u64,
    answered_start: Option<u64>,
) -> Result<GetResponse<Client>, (HttpError<Client>, Option<Duration>)> {
    if answered_start.is_some_and(|start| start != requested_start)
        || (answered_start.is_none() && requested_start > 0)
    {
        return Err((
            HttpError::RangeIgnored {
                start: requested_start,
                resp,
            },
            None,
        ));
    }
    Ok(resp)
}

impl<Client: HttpClient> Puller for HttpPuller<Client> {
    type Error = HttpError<Client>;
    async fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        // Reuse the prefetched whole-file response whenever the requested range
        // begins at offset 0. That covers both a whole-file request (`range ==
        // None`) and a finite ranged slice (`Some(0..end)`): the prefetched body
        // starts at 0, so it already contains the requested prefix. A slice with
        // `start > 0` cannot be served from a response rooted at 0, so it issues
        // its own ranged request. The download engine caps any overshoot to the
        // task's end, so no per-chunk slicing is needed here.
        let reused = if range.is_none_or(|r| r.start == 0) {
            self.resp.as_ref().and_then(|m| m.lock().take())
        } else {
            None
        };

        // Obtain the response here, not inside the stream. The stream is lazy, so a
        // request issued within it would not start until the first poll — `pull`
        // would report success before a single byte was asked for, and a caller
        // timing `pull` out would never see the wait. Awaiting here makes the request
        // itself observable and cancellable.
        let current: GetResponse<Client> = match reused {
            Some(resp) => resp,
            None => self
                .client
                .get((*self.url).clone(), range.cloned())
                .send()
                .await
                .map_err(|(e, retry)| (HttpError::Request(e), retry))?,
        };

        // `Accept-Encoding: identity` only negotiates: a server may code the body
        // anyway, and `reqwest` decodes a coded body when its decompression
        // features are enabled. Decoded bytes are not the resource's bytes at the
        // offsets the caller is about to write, so the response is refused before
        // any of it is streamed.
        if let Some(encoding) = content_encoding(current.headers()) {
            return Err((
                HttpError::ContentEncoding {
                    encoding,
                    resp: current,
                },
                None,
            ));
        }

        // File identity is verified for every response — the reused one and every
        // freshly requested one.
        let new_file_id = FileId::new(
            current.headers().get("etag").ok().as_deref(),
            current.headers().get("last-modified").ok().as_deref(),
        );
        if !self.file_id.matches(&new_file_id) {
            // Content changed underneath us. Surface it instead of streaming a body
            // that belongs to a different resource.
            return Err((HttpError::MismatchedBody(new_file_id, current), None));
        }

        // A `Content-Range` is the only proof of where a body starts that reaches
        // this layer. When the response carries one, it must name the offset that
        // was asked for — including 0, both for a whole-file pull and for a slice
        // that starts there: a server answering `bytes 50-…` to a request rooted
        // at 0 is sending a different span than the caller is about to write, and
        // the engine would write it at the requested offset.
        //
        // A response without the header is still fine for a request rooted at 0:
        // a `200` with the whole body legitimately starts there. A mid-file
        // request without it is the case the check exists for — the server
        // ignored `Range` and answered with the whole file.
        // Parse the header once: the offset, the declared span and the complete
        // length all come from the same value. Re-reading it per check would cost
        // a second parse and let a hypothetical disagreement between calls pick
        // different answers for one response.
        let content_range = current
            .headers()
            .get("content-range")
            .ok()
            .and_then(|raw| ContentRange::parse(&raw));
        let requested_start = range.map_or(0, |r| r.start);
        let answered_start = content_range.and_then(ContentRange::first_byte);
        let mut current = reject_wrong_start(current, requested_start, answered_start)?;

        // The absolute offsets this attempt must deliver. A finite range names its
        // end directly. `u64::MAX` is the "to the end of the resource" sentinel
        // callers pass when the size is unknown (`0..u64::MAX`), and a whole-file
        // pull names no end at all — but a `206` reply to either still states the
        // resource's size in its `Content-Range`, which is then the only expected
        // end available. With neither, nothing says how long the body should be
        // and the end cannot be checked.
        let (start, requested_end) = range.map_or((0, None), |r| (r.start, Some(r.end)));
        let requested_end = requested_end.filter(|end| *end != u64::MAX);
        // A finite range also says how long the resource must be. `requested_end`
        // is a half-open byte *position*, and `resource_len` is the resource's
        // total length, so both are measured from the start of the file and are
        // directly comparable: `resource_len == requested_end` is exactly enough
        // (byte `requested_end` itself is not requested). A shorter resource has
        // replaced the file, so this range can never be delivered: report the
        // changed resource instead of letting the body stop short and surface as
        // an endless short-body `Irrecoverable` downstream.
        let resource_len = content_range.and_then(ContentRange::complete_length);
        if let Some(end) = requested_end
            && let Some(len) = resource_len
            && len < end
        {
            return Err((HttpError::MismatchedBody(new_file_id, current), None));
        }
        let expected_end = requested_end.or(resource_len);

        Ok(Box::pin(stream! {
            // One response per call. Retrying is the engine's job: a chunk error ends
            // this attempt, the engine rebuilds the puller (new client, next local
            // source IP) and calls `pull` again with its own advanced cursor.
            // Re-requesting here would pin the retry to the same client and could
            // never rotate away from a path the server keeps cutting.
            //
            // Stream contract: `Ok(None)` (clean termination) is reached ONLY on a
            // genuine EOF. `current.chunk()` returning `Ok(None)` is not proof of
            // one: under chunked coding a body that stops early is a clean EOF, so
            // the end is accepted only when the expected length is unknown or this
            // attempt has already delivered all of it. A short body is reported as
            // `Irrecoverable`, which makes the engine drop this stream and re-pull
            // from the cursor it has already accepted. A failed read reports the
            // failure and then keeps yielding `Irrecoverable`, so the engine can
            // never mistake a failed pull for a finished one.
            let declared_len = content_range.and_then(ContentRange::span_len);
            let mut yielded: u64 = 0;
            loop {
                match current.chunk().await {
                    Ok(Some(chunk)) => {
                        let len = chunk.len() as u64;
                        let delivered = yielded.saturating_add(len);
                        if let Some(expected) = declared_len
                            && delivered > expected
                        {
                            // The body is longer than the span it announced, so the
                            // excess belongs to no offset the caller asked for. Report
                            // it and keep reporting the failure instead of handing a
                            // body that does not match the requested range out.
                            yield Err((
                                HttpError::OverDelivery {
                                    expected,
                                    resp: current,
                                },
                                None,
                            ));
                            loop {
                                yield Err((HttpError::Irrecoverable, None));
                            }
                        }
                        yielded = delivered;
                        yield Ok(chunk);
                    }
                    Ok(None) => {
                        if let Some(end) = expected_end
                            && start.saturating_add(yielded) < end
                        {
                            // A short body must never look like a clean end: report
                            // the failure and keep reporting it, so a consumer that
                            // polls again cannot read `None` as EOF.
                            loop {
                                yield Err((HttpError::Irrecoverable, None));
                            }
                        }
                        return;
                    }
                    Err(e) => {
                        // Report the underlying read failure (carries `current` for
                        // diagnostics). The response is spent, so there is nothing to
                        // resume from: hand the range back to the engine, which
                        // re-pulls from the bytes it has already accepted.
                        yield Err((HttpError::Chunk(e, current), None));
                        loop {
                            // `Irrecoverable` is the engine's signal to drop this
                            // stream and rebuild the puller. Any other error keeps it
                            // polling — on a spent stream that never resolves again.
                            yield Err((HttpError::Irrecoverable, None));
                        }
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::borrow::Cow;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use bytes::Bytes;

    use super::*;
    use futures::TryStreamExt;

    /// A mock client whose single response carries a `Content-Range` starting at
    /// the given offset (`None` = no such header, like a `200` reply).
    #[derive(Clone, Debug)]
    struct MockClient(Option<u64>);
    impl HttpClient for MockClient {
        type RequestBuilder = MockRequestBuilder;
        fn get(&self, _url: Url, _range: Option<ProgressEntry>) -> Self::RequestBuilder {
            MockRequestBuilder(self.0)
        }
    }
    struct MockRequestBuilder(Option<u64>);
    impl HttpRequestBuilder for MockRequestBuilder {
        type Response = MockResponse;
        type RequestError = MockError;
        fn send(self) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> {
            std::future::ready(Ok(MockResponse::new(self.0)))
        }
    }
    #[derive(Debug)]
    struct MockResponse {
        headers: MockHeaders,
        url: Url,
    }
    impl MockResponse {
        fn new(content_range: Option<u64>) -> Self {
            Self {
                headers: MockHeaders(content_range),
                url: Url::parse("http://mock-url").unwrap(),
            }
        }
    }
    impl HttpResponse for MockResponse {
        type Headers = MockHeaders;
        type ChunkError = MockError;
        fn headers(&self) -> &Self::Headers {
            &self.headers
        }
        fn url(&self) -> &Url {
            &self.url
        }
        async fn chunk(&mut self) -> Result<Option<Bytes>, Self::ChunkError> {
            DelayChunk::new().await
        }
    }
    /// Only `content-range` is ever present; every other header reads as absent.
    /// The value describes a 1024-byte body starting at the given offset, in the
    /// `Content-Range` form the parser reads.
    #[derive(Debug)]
    struct MockHeaders(Option<u64>);
    impl HttpHeaders for MockHeaders {
        type GetHeaderError = MockError;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            match (header, self.0) {
                ("content-range", Some(start)) => {
                    // A span wide enough for the handful of bytes these mocks
                    // deliver, so only a test that means to over-deliver trips
                    // the check.
                    let last = start.saturating_add(1023);
                    let complete = start.saturating_add(1024);
                    Ok(Cow::Owned(format!("bytes {start}-{last}/{complete}")))
                }
                _ => Err(MockError),
            }
        }
    }
    #[derive(Debug, thiserror::Error)]
    #[error("MockError")]
    struct MockError;

    struct DelayChunk {
        polled_once: bool,
    }
    impl DelayChunk {
        fn new() -> Self {
            Self { polled_once: false }
        }
    }
    impl Future for DelayChunk {
        type Output = Result<Option<Bytes>, MockError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if !self.polled_once {
                self.polled_once = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Poll::Ready(Ok(Some(Bytes::from_static(b"success"))))
        }
    }

    #[tokio::test]
    async fn test_http_puller_infinite_loop_fix() {
        let url = Url::parse("http://localhost").unwrap();
        let client = MockClient(None);
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let range = 0..7;
        let mut stream = Puller::pull(&mut puller, Some(&range))
            .await
            .expect("Failed to create stream");
        let result =
            tokio::time::timeout(Duration::from_secs(1), async { stream.try_next().await }).await;
        match result {
            Ok(Ok(Some(bytes))) => {
                assert_eq!(bytes, Bytes::from_static(b"success"));
            }
            e => {
                panic!(
                    "Test failed: timeout! This indicates HttpPuller may have lost the Future state after receiving Pending and entered an infinite loop. {e:?}"
                );
            }
        }
    }

    #[derive(Debug)]
    struct MismatchHeaders;
    impl HttpHeaders for MismatchHeaders {
        type GetHeaderError = MockError;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            if header == "etag" {
                Ok(Cow::Borrowed("etag-x"))
            } else {
                Err(MockError)
            }
        }
    }

    #[derive(Clone, Debug)]
    struct MismatchClient;
    impl HttpClient for MismatchClient {
        type RequestBuilder = MismatchRequestBuilder;
        fn get(&self, _url: Url, _range: Option<ProgressEntry>) -> Self::RequestBuilder {
            MismatchRequestBuilder
        }
    }
    struct MismatchRequestBuilder;
    impl HttpRequestBuilder for MismatchRequestBuilder {
        type Response = MismatchResponse;
        type RequestError = MockError;
        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            std::future::ready(Ok(MismatchResponse::new()))
        }
    }
    #[derive(Debug)]
    struct MismatchResponse {
        url: Url,
    }
    impl MismatchResponse {
        fn new() -> Self {
            Self {
                url: Url::parse("http://mock-url").unwrap(),
            }
        }
    }
    impl HttpResponse for MismatchResponse {
        type Headers = MismatchHeaders;
        type ChunkError = MockError;
        fn headers(&self) -> &Self::Headers {
            &MismatchHeaders
        }
        fn url(&self) -> &Url {
            &self.url
        }
        fn chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send {
            std::future::ready(Ok(None))
        }
    }

    #[derive(Clone, Debug)]
    struct ReqErrClient;
    impl HttpClient for ReqErrClient {
        type RequestBuilder = ReqErrRequestBuilder;
        fn get(&self, _url: Url, _range: Option<ProgressEntry>) -> Self::RequestBuilder {
            ReqErrRequestBuilder
        }
    }
    struct ReqErrRequestBuilder;
    impl HttpRequestBuilder for ReqErrRequestBuilder {
        type Response = MockResponse;
        type RequestError = MockError;
        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            std::future::ready(Err((MockError, None)))
        }
    }

    #[derive(Clone, Debug)]
    struct ChunkErrClient;
    impl HttpClient for ChunkErrClient {
        type RequestBuilder = ChunkErrRequestBuilder;
        fn get(&self, _url: Url, _range: Option<ProgressEntry>) -> Self::RequestBuilder {
            ChunkErrRequestBuilder
        }
    }
    struct ChunkErrRequestBuilder;
    impl HttpRequestBuilder for ChunkErrRequestBuilder {
        type Response = ChunkErrResponse;
        type RequestError = MockError;
        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            std::future::ready(Ok(ChunkErrResponse::new()))
        }
    }
    #[derive(Debug)]
    struct ChunkErrResponse {
        url: Url,
    }
    impl ChunkErrResponse {
        fn new() -> Self {
            Self {
                url: Url::parse("http://mock-url").unwrap(),
            }
        }
    }
    impl HttpResponse for ChunkErrResponse {
        type Headers = MockHeaders;
        type ChunkError = MockError;
        fn headers(&self) -> &Self::Headers {
            &MockHeaders(None)
        }
        fn url(&self) -> &Url {
            &self.url
        }
        fn chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send {
            std::future::ready(Err(MockError))
        }
    }

    #[test]
    fn http_puller_debug_formats() {
        let url = Url::parse("http://localhost").unwrap();
        let puller = HttpPuller::new(
            Arc::new(url),
            MockClient(None),
            None,
            FileId::new(None, None),
        );
        let s = format!("{puller:?}");
        assert!(s.contains("HttpPuller"));
        assert!(s.contains("url"));
    }

    #[test]
    fn mock_response_url_is_accessible() {
        let r = MockResponse::new(None);
        assert_eq!(r.url().as_str(), "http://mock-url/");
    }

    #[tokio::test]
    async fn test_http_puller_mismatched_body() {
        // The identity check runs on the response head, so a changed resource is
        // reported by `pull` itself rather than by the stream. Both sides name an
        // `ETag` and the values differ, which is the case the check exists for.
        let url = Url::parse("http://localhost").unwrap();
        let client = MismatchClient;
        let file_id = FileId::new(Some("etag-expected"), None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let result = Puller::pull(&mut puller, None).await;
        assert!(matches!(
            result,
            Err((HttpError::MismatchedBody(_, _), None))
        ));
    }

    #[tokio::test]
    async fn test_http_puller_request_error() {
        // A failed request is a failed `pull`, not a stream that reports the error
        // on its first poll.
        let url = Url::parse("http://localhost").unwrap();
        let client = ReqErrClient;
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let result = Puller::pull(&mut puller, None).await;
        assert!(matches!(result, Err((HttpError::Request(_), None))));
    }

    #[tokio::test]
    async fn test_http_puller_chunk_error_then_irrecoverable() {
        let url = Url::parse("http://localhost").unwrap();
        let client = ChunkErrClient;
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        // The first poll reports the read failure, carrying the spent response.
        let first = stream.try_next().await;
        assert!(matches!(first, Err((HttpError::Chunk(_, _), None))));
        // The response is spent, so the stream reports `Irrecoverable` from then on.
        // That is what makes the engine drop the stream and rebuild the puller; a
        // recoverable error would leave it polling a stream that never yields again.
        let second = stream.try_next().await;
        assert!(matches!(second, Err((HttpError::Irrecoverable, None))));
    }

    #[tokio::test]
    async fn test_http_puller_error_stream_never_terminates() {
        // After a read failure the stream must keep reporting `Irrecoverable` and
        // never produce `Ok(None)`: `Ok(None)` is reserved for a genuine EOF, so the
        // engine can never treat a failed pull as finished.
        let url = Url::parse("http://localhost").unwrap();
        let client = ChunkErrClient;
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        assert!(matches!(
            stream.try_next().await,
            Err((HttpError::Chunk(_, _), None))
        ));
        for _ in 0..5 {
            let polled = tokio::time::timeout(Duration::from_millis(50), stream.try_next()).await;
            assert!(
                matches!(polled, Ok(Err((HttpError::Irrecoverable, None)))),
                "a spent stream must keep reporting Irrecoverable"
            );
        }
    }

    #[tokio::test]
    async fn test_http_puller_partial_range_request() {
        let url = Url::parse("http://localhost").unwrap();
        let client = MockClient(Some(10));
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let range = 10..100;
        let mut stream = Puller::pull(&mut puller, Some(&range)).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), stream.try_next()).await;
        match result {
            Ok(Ok(Some(bytes))) => assert_eq!(bytes, Bytes::from_static(b"success")),
            e => panic!("expected a successful chunk, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn test_http_puller_reuses_response() {
        let url = Url::parse("http://localhost").unwrap();
        // Hand a pre-opened response to the puller; a full-file pull should reuse
        // it directly instead of issuing a new request.
        let resp = Some(Arc::new(parking_lot::Mutex::new(Some(MockResponse::new(
            None,
        )))));
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), MockClient(None), resp, file_id);
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), stream.try_next()).await;
        match result {
            Ok(Ok(Some(bytes))) => assert_eq!(bytes, Bytes::from_static(b"success")),
            e => panic!("expected the reused response to yield data, got {e:?}"),
        }
    }

    // Resume-range test: verifies that after a mid-stream chunk error the retry
    // request starts from the advanced offset, not the original range.start.
    static RESUME_RANGE_START: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Debug)]
    struct ResumeClient;
    impl HttpClient for ResumeClient {
        type RequestBuilder = ResumeRequestBuilder;
        fn get(&self, _url: Url, range: Option<ProgressEntry>) -> Self::RequestBuilder {
            if let Some(r) = &range {
                RESUME_RANGE_START.store(r.start, Ordering::SeqCst);
            }
            ResumeRequestBuilder(range.map_or(0, |r| r.start))
        }
    }
    struct ResumeRequestBuilder(u64);
    impl HttpRequestBuilder for ResumeRequestBuilder {
        type Response = ResumeResponse;
        type RequestError = MockError;
        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            std::future::ready(Ok(ResumeResponse::new(self.0)))
        }
    }
    #[derive(Debug)]
    struct ResumeResponse {
        url: Url,
        calls: u32,
        headers: MockHeaders,
    }
    impl ResumeResponse {
        fn new(start: u64) -> Self {
            Self {
                url: Url::parse("http://mock-url").unwrap(),
                calls: 0,
                headers: MockHeaders(Some(start)),
            }
        }
    }
    impl HttpResponse for ResumeResponse {
        type Headers = MockHeaders;
        type ChunkError = MockError;
        fn headers(&self) -> &Self::Headers {
            &self.headers
        }
        fn url(&self) -> &Url {
            &self.url
        }
        fn chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send {
            self.calls += 1;
            let first = self.calls == 1;
            let fut = async move {
                if first {
                    Ok(Some(Bytes::from_static(b"success")))
                } else {
                    Err(MockError)
                }
            };
            Box::pin(fut) as Pin<Box<dyn Future<Output = Result<Option<Bytes>, MockError>> + Send>>
        }
    }

    #[tokio::test]
    async fn test_chunk_error_hands_the_range_back_to_the_engine() {
        // A chunk error ends the attempt: the stream must not re-request on its own
        // (that would pin the retry to the same client). It reports the failure, then
        // keeps yielding `Irrecoverable` so the engine rebuilds the puller and
        // re-pulls from its own advanced cursor.
        let url = Url::parse("http://localhost").unwrap();
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), ResumeClient, None, file_id);
        let range = 0..100;
        let mut stream = Puller::pull(&mut puller, Some(&range)).await.unwrap();
        // First chunk succeeds ("success" = 7 bytes).
        assert!(matches!(stream.try_next().await, Ok(Some(_))));
        // Next poll reports the read failure, carrying the response for diagnostics.
        assert!(matches!(
            stream.try_next().await,
            Err((HttpError::Chunk(_, _), None))
        ));
        // From there on the stream is spent: it issues no second request, and every
        // later poll says `Irrecoverable` — the error that makes the engine drop it.
        for _ in 0..3 {
            let polled = tokio::time::timeout(Duration::from_millis(50), stream.try_next()).await;
            assert!(
                matches!(polled, Ok(Err((HttpError::Irrecoverable, None)))),
                "a spent stream must report Irrecoverable, not go silent"
            );
        }
        assert_eq!(
            RESUME_RANGE_START.load(Ordering::SeqCst),
            0,
            "the stream must not issue a follow-up request of its own"
        );
        drop(stream);

        // The engine's retry, expressed as a fresh pull from the advanced offset,
        // does resume at 7.
        let mut stream = Puller::pull(&mut puller, Some(&(7..100))).await.unwrap();
        assert!(matches!(stream.try_next().await, Ok(Some(_))));
        assert_eq!(RESUME_RANGE_START.load(Ordering::SeqCst), 7);
    }

    /// The corruption case this guards: a server that ignores `Range` answers
    /// `200` with the whole file. Streaming it at the requested offset would
    /// overwrite the wrong bytes, so the pull must fail instead.
    #[tokio::test]
    async fn a_whole_file_answer_to_a_mid_file_range_is_rejected() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            MockClient(None),
            None,
            FileId::new(None, None),
        );
        let range = 5..10;
        let err = Puller::pull(&mut puller, Some(&range))
            .await
            .err()
            .expect("a body without Content-Range must not be streamed as a chunk")
            .0;
        assert!(
            matches!(err, HttpError::RangeIgnored { start: 5, .. }),
            "expected RangeIgnored at the requested offset, got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_body_starting_at_the_requested_offset_is_accepted() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            MockClient(Some(5)),
            None,
            FileId::new(None, None),
        );
        let range = 5..10;
        assert!(
            Puller::pull(&mut puller, Some(&range)).await.is_ok(),
            "a response whose Content-Range starts at the requested byte is a valid chunk"
        );
    }

    #[tokio::test]
    async fn a_zero_offset_range_needs_no_content_range() {
        // A whole-file pull legitimately starts at 0, and a `200` reply to it
        // carries the right bytes, so the header is not required.
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            MockClient(None),
            None,
            FileId::new(None, None),
        );
        let range = 0..10;
        assert!(
            Puller::pull(&mut puller, Some(&range)).await.is_ok(),
            "a range starting at 0 must not require Content-Range"
        );
    }

    /// A response that delivers `body_len` bytes and then ends, so a body that
    /// stops before the end of the requested range can be told apart from a
    /// complete one. `content_range` is the header the response carries, verbatim.
    #[derive(Clone, Debug)]
    struct EndingClient {
        body_len: usize,
        content_range: Option<&'static str>,
    }
    impl EndingClient {
        fn new(body_len: usize, content_range: Option<&'static str>) -> Self {
            Self {
                body_len,
                content_range,
            }
        }
    }
    impl HttpClient for EndingClient {
        type RequestBuilder = EndingRequestBuilder;
        fn get(&self, _url: Url, _range: Option<ProgressEntry>) -> Self::RequestBuilder {
            EndingRequestBuilder(self.clone())
        }
    }
    struct EndingRequestBuilder(EndingClient);
    impl HttpRequestBuilder for EndingRequestBuilder {
        type Response = EndingResponse;
        type RequestError = MockError;
        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            let body_len = self.0.body_len;
            let content_range = self.0.content_range;
            std::future::ready(Ok(EndingResponse::new(body_len, content_range)))
        }
    }
    #[derive(Debug)]
    struct EndingResponse {
        url: Url,
        remaining: usize,
        headers: EndingHeaders,
    }
    impl EndingResponse {
        fn new(body_len: usize, content_range: Option<&'static str>) -> Self {
            Self {
                url: Url::parse("http://mock-url").unwrap(),
                remaining: body_len,
                headers: EndingHeaders { content_range },
            }
        }
    }
    /// Only `content-range` is ever present; its value is the fixture's verbatim.
    #[derive(Debug)]
    struct EndingHeaders {
        content_range: Option<&'static str>,
    }
    impl HttpHeaders for EndingHeaders {
        type GetHeaderError = MockError;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            match (header, self.content_range) {
                ("content-range", Some(value)) => Ok(Cow::Borrowed(value)),
                _ => Err(MockError),
            }
        }
    }
    impl HttpResponse for EndingResponse {
        type Headers = EndingHeaders;
        type ChunkError = MockError;
        fn headers(&self) -> &Self::Headers {
            &self.headers
        }
        fn url(&self) -> &Url {
            &self.url
        }
        fn chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send {
            let next = match self.remaining {
                0 => None,
                remaining => {
                    let len = remaining.min(64);
                    self.remaining -= len;
                    Some(Bytes::from(vec![b'x'; len]))
                }
            };
            std::future::ready(Ok(next))
        }
    }

    /// A body that ends before the requested range is short, not finished: under
    /// chunked coding that is a clean EOF, so `Ok(None)` alone would let a
    /// truncated download pass as a completed one.
    #[tokio::test]
    async fn a_body_that_stops_before_the_range_end_is_irrecoverable() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(50, None),
            None,
            FileId::new(None, None),
        );
        let range = 0..100;
        let mut stream = Puller::pull(&mut puller, Some(&range)).await.unwrap();
        assert_eq!(stream.try_next().await.unwrap().unwrap().len(), 50);
        assert!(
            matches!(
                stream.try_next().await,
                Err((HttpError::Irrecoverable, None))
            ),
            "a body shorter than the range must be reported as Irrecoverable, not as EOF"
        );
        assert!(
            matches!(
                stream.try_next().await,
                Err((HttpError::Irrecoverable, None))
            ),
            "a short body must keep reporting the failure, never a clean EOF"
        );
    }

    /// Control: a body that fills the requested range still ends cleanly.
    #[tokio::test]
    async fn a_body_that_fills_the_range_ends_cleanly() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(100, None),
            None,
            FileId::new(None, None),
        );
        let range = 0..100;
        let mut stream = Puller::pull(&mut puller, Some(&range)).await.unwrap();
        let mut total = 0;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => total += chunk.len(),
                Ok(None) => break,
                Err(e) => panic!("a complete body must end cleanly, got {e:?}"),
            }
        }
        assert_eq!(total, 100);
    }

    /// Without a range the expected length is unknown, so `Ok(None)` is the only
    /// signal the body has ended and no short-body check may reject it.
    #[tokio::test]
    async fn an_unknown_expected_length_ends_cleanly() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(50, None),
            None,
            FileId::new(None, None),
        );
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        let mut total = 0;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => total += chunk.len(),
                Ok(None) => break,
                Err(e) => panic!("an unknown-length body may end at any size, got {e:?}"),
            }
        }
        assert_eq!(total, 50);
    }

    /// `0..u64::MAX` is how a caller asks for "the rest of the resource" when the
    /// size is unknown; it names no length, so it must not turn a normal EOF into
    /// a short-body error.
    #[tokio::test]
    async fn the_to_the_end_sentinel_ends_cleanly() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(50, None),
            None,
            FileId::new(None, None),
        );
        let range = 0..u64::MAX;
        let mut stream = Puller::pull(&mut puller, Some(&range)).await.unwrap();
        let mut total = 0;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => total += chunk.len(),
                Ok(None) => break,
                Err(e) => panic!("a to-the-end range has no length to enforce, got {e:?}"),
            }
        }
        assert_eq!(total, 50);
    }

    /// A whole-file pull has no range to measure against, but a `206` reply still
    /// names the resource's size in `Content-Range`: a body that stops short of it
    /// is truncated, and reporting that as a completed download is what let the
    /// single-stream path keep a partial file.
    #[tokio::test]
    async fn a_declared_complete_length_is_enforced_for_a_whole_file_pull() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(50, Some("bytes 0-99/100")),
            None,
            FileId::new(None, None),
        );
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        assert_eq!(stream.try_next().await.unwrap().unwrap().len(), 50);
        assert!(
            matches!(
                stream.try_next().await,
                Err((HttpError::Irrecoverable, None))
            ),
            "a body short of the declared complete length must be Irrecoverable"
        );
    }

    /// Control for the case above: when the `Content-Range` names no complete
    /// length, nothing says how long the body should be and the end is accepted.
    #[tokio::test]
    async fn a_whole_file_pull_without_a_declared_length_ends_cleanly() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(50, None),
            None,
            FileId::new(None, None),
        );
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        let mut total = 0;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => total += chunk.len(),
                Ok(None) => break,
                Err(e) => panic!("an undeclared length cannot be enforced, got {e:?}"),
            }
        }
        assert_eq!(total, 50);
    }

    /// A slice rooted at 0 used to skip the `Content-Range` check because its
    /// start was 0. A `bytes 50-149/200` answer to a `0..100` request is a
    /// different span, and streaming it at 0 would write the wrong bytes.
    #[tokio::test]
    async fn a_zero_offset_range_rejects_a_body_that_starts_later() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(100, Some("bytes 50-149/200")),
            None,
            FileId::new(None, None),
        );
        let range = 0..100;
        let err = Puller::pull(&mut puller, Some(&range))
            .await
            .err()
            .expect("a body starting at 50 cannot serve a range rooted at 0")
            .0;
        assert!(
            matches!(err, HttpError::RangeIgnored { start: 0, .. }),
            "expected RangeIgnored at 0, got {err:?}"
        );
    }

    /// A whole-file pull names no range, but its body still has to start at 0:
    /// `bytes 5-…` is a different span and cannot be written at offset 0.
    #[tokio::test]
    async fn a_whole_file_pull_rejects_a_body_that_starts_later() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(100, Some("bytes 5-104/200")),
            None,
            FileId::new(None, None),
        );
        let err = Puller::pull(&mut puller, None)
            .await
            .err()
            .expect("a whole-file pull must start at 0")
            .0;
        assert!(
            matches!(err, HttpError::RangeIgnored { start: 0, .. }),
            "expected RangeIgnored at 0, got {err:?}"
        );
    }

    /// The `Content-Range` can also say the resource is now shorter than the
    /// range being requested. Retrying that range can never fill it, so it is
    /// reported as a changed resource instead of reaching the short-body loop.
    #[tokio::test]
    async fn a_declared_resource_shorter_than_the_range_is_a_mismatch() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(200, Some("bytes 0-49/50")),
            None,
            FileId::new(None, None),
        );
        let range = 0..100;
        let err = Puller::pull(&mut puller, Some(&range))
            .await
            .err()
            .expect("a 50-byte resource cannot serve a 100-byte range")
            .0;
        assert!(
            matches!(err, HttpError::MismatchedBody(_, _)),
            "a resource that shrank must read as a changed file, got {err:?}"
        );
    }

    /// A body longer than the span its own `Content-Range` announced is not the
    /// range that was asked for; the excess belongs to no requested offset.
    #[tokio::test]
    async fn a_body_longer_than_the_announced_span_is_rejected() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(60, Some("bytes 0-49/200")),
            None,
            FileId::new(None, None),
        );
        let range = 0..50;
        let mut stream = Puller::pull(&mut puller, Some(&range)).await.unwrap();
        assert!(
            matches!(
                stream.try_next().await,
                Err((HttpError::OverDelivery { expected: 50, .. }, None))
            ),
            "a 60-byte body over-runs the 50-byte span it announced"
        );
        assert!(
            matches!(
                stream.try_next().await,
                Err((HttpError::Irrecoverable, None))
            ),
            "an over-delivering body must keep reporting the failure, never EOF"
        );
    }

    /// Control: a body that fills exactly the announced span ends cleanly.
    #[tokio::test]
    async fn a_body_that_matches_the_announced_span_ends_cleanly() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            EndingClient::new(50, Some("bytes 0-49/200")),
            None,
            FileId::new(None, None),
        );
        let range = 0..50;
        let mut stream = Puller::pull(&mut puller, Some(&range)).await.unwrap();
        let mut total = 0;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => total += chunk.len(),
                Ok(None) => break,
                Err(e) => panic!("a body that matches its span must end cleanly, got {e:?}"),
            }
        }
        assert_eq!(total, 50);
    }

    #[cfg(feature = "reqwest")]
    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn a_chunked_206_that_stops_short_is_irrecoverable() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/short")
            .with_status(206)
            .with_header("content-range", "bytes 0-99/100")
            .with_chunked_body(|w| w.write_all(&[b'x'; 50]))
            .create_async()
            .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(Url::parse(&format!("{}/short", server.url())).unwrap()),
            client,
            None,
            FileId::new(None, None),
        );
        let range = 0..100;
        let mut stream = Puller::pull(&mut puller, Some(&range))
            .await
            .expect("the 206 must be accepted");
        let mut total = 0;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => total += chunk.len(),
                Ok(None) => panic!("a 50-byte body for a 100-byte range must not end cleanly"),
                Err((e, _)) => {
                    assert!(
                        matches!(e, HttpError::Irrecoverable),
                        "expected Irrecoverable, got {e:?}"
                    );
                    break;
                }
            }
        }
        assert_eq!(total, 50);
    }

    /// Regression: a chunked `206` that delivers the whole range still ends with
    /// a clean `Ok(None)`.
    #[cfg(feature = "reqwest")]
    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn a_chunked_206_that_fills_the_range_ends_cleanly() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/full")
            .with_status(206)
            .with_header("content-range", "bytes 0-99/100")
            .with_chunked_body(|w| w.write_all(&[b'x'; 100]))
            .create_async()
            .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(Url::parse(&format!("{}/full", server.url())).unwrap()),
            client,
            None,
            FileId::new(None, None),
        );
        let range = 0..100;
        let mut stream = Puller::pull(&mut puller, Some(&range))
            .await
            .expect("the 206 must be accepted");
        let mut total = 0;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => total += chunk.len(),
                Ok(None) => break,
                Err((e, _)) => panic!("a complete body must end cleanly, got {e:?}"),
            }
        }
        assert_eq!(total, 100);
    }

    /// A response carrying the given validators, so the identity check can be
    /// exercised field by field.
    #[derive(Clone, Debug)]
    struct IdentityClient {
        etag: Option<&'static str>,
        last_modified: Option<&'static str>,
    }
    impl HttpClient for IdentityClient {
        type RequestBuilder = IdentityRequestBuilder;
        fn get(&self, _url: Url, _range: Option<ProgressEntry>) -> Self::RequestBuilder {
            IdentityRequestBuilder(self.clone())
        }
    }
    struct IdentityRequestBuilder(IdentityClient);
    impl HttpRequestBuilder for IdentityRequestBuilder {
        type Response = IdentityResponse;
        type RequestError = MockError;
        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            std::future::ready(Ok(IdentityResponse::new(self.0.etag, self.0.last_modified)))
        }
    }
    #[derive(Debug)]
    struct IdentityResponse {
        url: Url,
        headers: IdentityHeaders,
    }
    impl IdentityResponse {
        fn new(etag: Option<&'static str>, last_modified: Option<&'static str>) -> Self {
            Self {
                url: Url::parse("http://mock-url").unwrap(),
                headers: IdentityHeaders {
                    etag,
                    last_modified,
                },
            }
        }
    }
    impl HttpResponse for IdentityResponse {
        type Headers = IdentityHeaders;
        type ChunkError = MockError;
        fn headers(&self) -> &Self::Headers {
            &self.headers
        }
        fn url(&self) -> &Url {
            &self.url
        }
        fn chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send {
            std::future::ready(Ok(None))
        }
    }
    /// Only the two validators are ever present.
    #[derive(Debug)]
    struct IdentityHeaders {
        etag: Option<&'static str>,
        last_modified: Option<&'static str>,
    }
    impl HttpHeaders for IdentityHeaders {
        type GetHeaderError = MockError;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            match header {
                "etag" => self.etag.map(Cow::Borrowed).ok_or(MockError),
                "last-modified" => self.last_modified.map(Cow::Borrowed).ok_or(MockError),
                _ => Err(MockError),
            }
        }
    }

    /// The metadata `200` carries `Last-Modified`; the ranged `206` the puller
    /// sees for a slice does not. A whole-identity comparison reported that
    /// omission as a changed file, and since `MismatchedBody` is retryable the
    /// download never finished.
    #[tokio::test]
    async fn a_validator_missing_from_the_ranged_reply_is_not_a_mismatch() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            IdentityClient {
                etag: None,
                last_modified: None,
            },
            None,
            FileId::new(None, Some("Mon, 01 Jan 2024 00:00:00 GMT")),
        );
        let range = 0..20;
        assert!(
            Puller::pull(&mut puller, Some(&range)).await.is_ok(),
            "a validator the 206 omits must not read as a changed file"
        );
    }

    /// A validator both sides carry and that differs is the case the check
    /// exists for: the body belongs to another resource.
    #[tokio::test]
    async fn a_different_etag_is_a_mismatch() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            IdentityClient {
                etag: Some("v2"),
                last_modified: None,
            },
            None,
            FileId::new(Some("v1"), None),
        );
        let result = Puller::pull(&mut puller, None).await;
        assert!(matches!(
            result,
            Err((HttpError::MismatchedBody(_, _), None))
        ));
    }

    /// Neither side names a validator: nothing changed as far as the headers can
    /// tell, so the body is streamed rather than refused.
    #[tokio::test]
    async fn identities_without_validators_match() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller = HttpPuller::new(
            Arc::new(url),
            IdentityClient {
                etag: None,
                last_modified: None,
            },
            None,
            FileId::new(None, None),
        );
        assert!(Puller::pull(&mut puller, None).await.is_ok());
    }

    /// A response that declares `Content-Encoding: gzip`, as a server that
    /// ignored `Accept-Encoding: identity` does.
    #[derive(Clone, Debug)]
    struct EncodedClient;
    impl HttpClient for EncodedClient {
        type RequestBuilder = EncodedRequestBuilder;
        fn get(&self, _url: Url, _range: Option<ProgressEntry>) -> Self::RequestBuilder {
            EncodedRequestBuilder
        }
    }
    struct EncodedRequestBuilder;
    impl HttpRequestBuilder for EncodedRequestBuilder {
        type Response = EncodedResponse;
        type RequestError = MockError;
        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            std::future::ready(Ok(EncodedResponse::new()))
        }
    }
    #[derive(Debug)]
    struct EncodedResponse {
        url: Url,
    }
    impl EncodedResponse {
        fn new() -> Self {
            Self {
                url: Url::parse("http://mock-url").unwrap(),
            }
        }
    }
    impl HttpResponse for EncodedResponse {
        type Headers = EncodedHeaders;
        type ChunkError = MockError;
        fn headers(&self) -> &Self::Headers {
            &EncodedHeaders
        }
        fn url(&self) -> &Url {
            &self.url
        }
        fn chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send {
            // The body a backend with decompression enabled would hand over. It
            // must never reach the caller: decoded bytes do not line up with the
            // offsets that were requested.
            std::future::ready(Ok(Some(Bytes::from_static(b"decoded"))))
        }
    }
    /// Only `content-encoding` is present.
    #[derive(Debug)]
    struct EncodedHeaders;
    impl HttpHeaders for EncodedHeaders {
        type GetHeaderError = MockError;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            match header {
                "content-encoding" => Ok(Cow::Borrowed("gzip")),
                _ => Err(MockError),
            }
        }
    }

    /// `Accept-Encoding: identity` only negotiates; it cannot stop a server from
    /// coding the body, nor the backend from decoding it. A coded response must
    /// fail the pull instead of being streamed at the requested offsets.
    #[tokio::test]
    async fn a_coded_response_is_refused_before_any_byte_is_streamed() {
        let url = Url::parse("http://localhost").unwrap();
        let mut puller =
            HttpPuller::new(Arc::new(url), EncodedClient, None, FileId::new(None, None));
        let err = Puller::pull(&mut puller, None)
            .await
            .err()
            .expect("a gzip-coded body must not be streamed")
            .0;
        match err {
            HttpError::ContentEncoding { encoding, .. } => assert_eq!(encoding, "gzip"),
            other => panic!("expected the response's coding to be reported, got {other:?}"),
        }
    }

    /// Control: `identity` (in any case), an empty value, and a missing header
    /// all mean the body is the resource's own bytes.
    #[test]
    fn identity_is_not_a_coding() {
        struct EncodingHeader(&'static str);
        impl HttpHeaders for EncodingHeader {
            type GetHeaderError = MockError;
            fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
                match header {
                    "content-encoding" => Ok(Cow::Borrowed(self.0)),
                    _ => Err(MockError),
                }
            }
        }
        assert_eq!(content_encoding(&EncodingHeader("identity")), None);
        assert_eq!(content_encoding(&EncodingHeader("IDENTITY")), None);
        assert_eq!(content_encoding(&EncodingHeader("  ")), None);
        assert_eq!(content_encoding(&MockHeaders(None)), None);
        assert_eq!(
            content_encoding(&EncodingHeader("gzip")).as_deref(),
            Some("gzip")
        );
    }
}
