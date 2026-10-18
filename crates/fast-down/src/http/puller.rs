//! An HTTP implementation of the [`fast_pull::Puller`] trait.
//!
//! [`HttpPuller`] builds range requests through the generic [`crate::http::HttpClient`]
//! trait and streams the response body back as a [`fast_pull::PullStream`]. It
//! verifies the server's `ETag` / `Last-Modified` headers against the expected
//! [`crate::FileId`] so that a changed file is reported as [`crate::http::HttpError::MismatchedBody`]
//! rather than silently corrupting an incremental download.

use crate::http::{
    FileId, GetResponse, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder, HttpResponse,
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
        if new_file_id != self.file_id {
            // Content changed underneath us. Surface it instead of streaming a body
            // that belongs to a different resource.
            return Err((HttpError::MismatchedBody(new_file_id, current), None));
        }

        Ok(Box::pin(stream! {
            // One response per call. Retrying is the engine's job: a chunk error ends
            // this attempt, the engine rebuilds the puller (new client, next local
            // source IP) and calls `pull` again with its own advanced cursor.
            // Re-requesting here would pin the retry to the same client and could
            // never rotate away from a path the server keeps cutting.
            //
            // Stream contract: `Ok(None)` (clean termination) is reached ONLY on a
            // genuine EOF — when `current.chunk()` returns `Ok(None)`. A failed read
            // reports the failure and then keeps yielding `Irrecoverable`, so the
            // engine can never mistake a failed pull for a finished one.
            loop {
                match current.chunk().await {
                    Ok(Some(chunk)) => yield Ok(chunk),
                    Ok(None) => return,
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

    #[derive(Clone, Debug)]
    struct MockClient;
    impl HttpClient for MockClient {
        type RequestBuilder = MockRequestBuilder;
        fn get(&self, _url: Url, _range: Option<ProgressEntry>) -> Self::RequestBuilder {
            MockRequestBuilder
        }
    }
    struct MockRequestBuilder;
    impl HttpRequestBuilder for MockRequestBuilder {
        type Response = MockResponse;
        type RequestError = MockError;
        fn send(self) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> {
            std::future::ready(Ok(MockResponse::new()))
        }
    }
    #[derive(Debug)]
    struct MockResponse {
        headers: MockHeaders,
        url: Url,
    }
    impl MockResponse {
        fn new() -> Self {
            Self {
                headers: MockHeaders,
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
    #[derive(Debug)]
    struct MockHeaders;
    impl HttpHeaders for MockHeaders {
        type GetHeaderError = MockError;
        fn get(&self, _header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            Err(MockError)
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
                println!("Wait... [Mock: simulating network delay with Pending]");
                self.polled_once = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            println!("Done! [Mock: data arrived, Ready]");
            Poll::Ready(Ok(Some(Bytes::from_static(b"success"))))
        }
    }

    #[tokio::test]
    async fn test_http_puller_infinite_loop_fix() {
        let url = Url::parse("http://localhost").unwrap();
        let client = MockClient;
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let range = 0..7;
        let mut stream = Puller::pull(&mut puller, Some(&range))
            .await
            .expect("Failed to create stream");
        println!("--- Starting HttpPuller test ---");
        let result =
            tokio::time::timeout(Duration::from_secs(1), async { stream.try_next().await }).await;
        match result {
            Ok(Ok(Some(bytes))) => {
                println!("Received data: {bytes:?}");
                assert_eq!(bytes, Bytes::from_static(b"success"));
                println!("Test passed: HttpPuller correctly handled Pending state!");
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
            &MockHeaders
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
        let puller = HttpPuller::new(Arc::new(url), MockClient, None, FileId::new(None, None));
        let s = format!("{puller:?}");
        assert!(s.contains("HttpPuller"));
        assert!(s.contains("url"));
    }

    #[test]
    fn mock_response_url_is_accessible() {
        let r = MockResponse::new();
        assert_eq!(r.url().as_str(), "http://mock-url/");
    }

    #[tokio::test]
    async fn test_http_puller_mismatched_body() {
        // The identity check runs on the response head, so a changed resource is
        // reported by `pull` itself rather than by the stream.
        let url = Url::parse("http://localhost").unwrap();
        let client = MismatchClient;
        let file_id = FileId::new(None, None);
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
        let client = MockClient;
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
        // it directly (lines 119-123) instead of issuing a new request.
        let resp = Some(Arc::new(parking_lot::Mutex::new(Some(MockResponse::new()))));
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), MockClient, resp, file_id);
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
            if let Some(r) = range {
                RESUME_RANGE_START.store(r.start, Ordering::SeqCst);
            }
            ResumeRequestBuilder
        }
    }
    struct ResumeRequestBuilder;
    impl HttpRequestBuilder for ResumeRequestBuilder {
        type Response = ResumeResponse;
        type RequestError = MockError;
        fn send(
            self,
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
            std::future::ready(Ok(ResumeResponse::new()))
        }
    }
    #[derive(Debug)]
    struct ResumeResponse {
        url: Url,
        calls: u32,
    }
    impl ResumeResponse {
        fn new() -> Self {
            Self {
                url: Url::parse("http://mock-url").unwrap(),
                calls: 0,
            }
        }
    }
    impl HttpResponse for ResumeResponse {
        type Headers = MockHeaders;
        type ChunkError = MockError;
        fn headers(&self) -> &Self::Headers {
            &MockHeaders
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
}
