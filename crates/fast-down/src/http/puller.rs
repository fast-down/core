//! An HTTP implementation of the [`fast_pull::Puller`] trait.
//!
//! [`HttpPuller`] builds range requests through the generic [`crate::http::HttpClient`]
//! trait and streams the response body back as a [`fast_pull::PullStream`]. It
//! verifies the server's `ETag` / `Last-Modified` headers against the expected
//! [`crate::FileId`] so that a changed file is reported as [`crate::http::HttpError::MismatchedBody`]
//! rather than silently corrupting an incremental download, and it verifies the
//! `Content-Range` start of every mid-file request so a server that ignores
//! `Range` cannot have its whole-file body written at the requested offset.

use crate::http::{
    ContentRange, FileId, GetResponse, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder,
    HttpResponse,
};
use async_stream::stream;
use fast_pull::{ProgressEntry, PullResult, PullStream, Puller};
use parking_lot::Mutex;
use std::{fmt::Debug, sync::Arc};
use url::Url;

/// A [`Puller`] implementation that fetches data over HTTP.
///
/// Uses the generic [`HttpClient`] trait so it can work with any HTTP backend
/// (e.g. reqwest, custom clients). Supports range requests, file identity
/// checking, and reusing previously opened responses.
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

/// The first byte offset a response body starts at, read from `Content-Range`.
///
/// Accepts `bytes <start>-<end>/<size>`; the `bytes */<size>` form (no body
/// range) and any malformed value yield `None`. The parsing rules — the
/// case-insensitive unit and the tolerated whitespace — live in `ContentRange`,
/// shared with the prefetch probe so the two cannot disagree.
pub(super) fn content_range_start(headers: &impl HttpHeaders) -> Option<u64> {
    let raw = headers.get("content-range").ok()?;
    ContentRange::parse(&raw)?.first_byte()
}

/// The complete length a response's `Content-Range` names, if any.
///
/// A whole-file request names no end of its own, but a `206` reply to it still
/// states the resource's size here — the only expected length such an attempt
/// can be checked against.
fn content_range_complete(headers: &impl HttpHeaders) -> Option<u64> {
    let raw = headers.get("content-range").ok()?;
    ContentRange::parse(&raw)?.complete_length()
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
        let mut current: GetResponse<Client> = match reused {
            Some(resp) => resp,
            None => self
                .client
                .get((*self.url).clone(), range.cloned())
                .send()
                .await
                .map_err(|(e, retry)| (HttpError::Request(e), retry))?,
        };

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

        // A ranged request must be answered with a body that starts where we asked.
        // A server that ignores `Range` replies `200` with the whole file; the
        // engine would write that body at `range.start`, silently corrupting the
        // output. The `Content-Range` offset is the only proof of the body's
        // position that reaches this layer.
        if let Some(expected) = range
            && expected.start > 0
            && content_range_start(current.headers()) != Some(expected.start)
        {
            return Err((
                HttpError::RangeIgnored {
                    start: expected.start,
                    resp: current,
                },
                None,
            ));
        }

        // The absolute offsets this attempt must deliver. A finite range names its
        // end directly. `u64::MAX` is the "to the end of the resource" sentinel
        // callers pass when the size is unknown (`0..u64::MAX`), and a whole-file
        // pull names no end at all — but a `206` reply to either still states the
        // resource's size in its `Content-Range`, which is then the only expected
        // end available. With neither, nothing says how long the body should be
        // and the end cannot be checked.
        let (start, expected_end) = range.map_or((0, None), |r| (r.start, Some(r.end)));
        let expected_end = expected_end
            .filter(|end| *end != u64::MAX)
            .or_else(|| content_range_complete(current.headers()));

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
            let mut yielded: u64 = 0;
            loop {
                match current.chunk().await {
                    Ok(Some(chunk)) => {
                        yielded = yielded.saturating_add(chunk.len() as u64);
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
    /// The start offset is rendered in the `Content-Range` form the parser reads.
    #[derive(Debug)]
    struct MockHeaders(Option<u64>);
    impl HttpHeaders for MockHeaders {
        type GetHeaderError = MockError;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            match (header, self.0) {
                ("content-range", Some(start)) => {
                    Ok(Cow::Owned(format!("bytes {start}-{start}/1000")))
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

    /// A raw `Content-Range` value, for the parser cases the mock client cannot
    /// build from an offset alone.
    struct RawHeaders(&'static str);
    impl HttpHeaders for RawHeaders {
        type GetHeaderError = MockError;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            if header == "content-range" {
                Ok(Cow::Borrowed(self.0))
            } else {
                Err(MockError)
            }
        }
    }

    #[test]
    fn content_range_start_parses_the_common_forms() {
        assert_eq!(
            content_range_start(&MockHeaders(Some(5))),
            Some(5),
            "the ranged form must yield its start offset"
        );
        assert_eq!(
            content_range_start(&RawHeaders("bytes */20")),
            None,
            "an unsatisfied range carries no body offset"
        );
        assert_eq!(
            content_range_start(&MockHeaders(None)),
            None,
            "a response without Content-Range has no offset"
        );
        assert_eq!(
            content_range_start(&RawHeaders("Bytes 5-9/20")),
            Some(5),
            "the range unit is a case-insensitive token"
        );
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
}
