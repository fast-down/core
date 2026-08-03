//! An HTTP implementation of the [`fast_pull::Puller`] trait.
//!
//! [`HttpPuller`] builds range requests through the generic [`crate::http::HttpClient`]
//! trait and streams the response body back as a [`fast_pull::PullStream`]. It
//! verifies the server's `ETag` / `Last-Modified` headers against the expected
//! [`crate::FileId`] so that a changed file is reported as [`crate::http::HttpError::MismatchedBody`]
//! rather than silently corrupting an incremental download.

use crate::http::{
    FileId, GetRequestError, GetResponse, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder,
    HttpResponse,
};
use bytes::Bytes;
use fast_pull::{ProgressEntry, PullResult, PullStream, Puller};
use futures::Stream;
use parking_lot::Mutex;
use std::{
    fmt::Debug,
    future::Future,
    ops::Range,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
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

type ResponseFut<Client> = Pin<
    Box<
        dyn Future<
                Output = Result<GetResponse<Client>, (GetRequestError<Client>, Option<Duration>)>,
            > + Send,
    >,
>;

type ChunkStream<Client> = Pin<Box<dyn Stream<Item = Result<Bytes, HttpError<Client>>> + Send>>;

enum ResponseState<Client: HttpClient> {
    Pending(ResponseFut<Client>),
    Streaming(ChunkStream<Client>),
    None,
}

fn into_chunk_stream<Client: HttpClient>(resp: GetResponse<Client>) -> ChunkStream<Client> {
    Box::pin(futures::stream::try_unfold(resp, |mut r| async move {
        match r.chunk().await {
            Ok(Some(chunk)) => Ok(Some((chunk, r))),
            Ok(None) => Ok(None),
            Err(e) => Err(HttpError::Chunk(e, r)),
        }
    }))
}

impl<Client: HttpClient> Puller for HttpPuller<Client> {
    type Error = HttpError<Client>;
    fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> {
        let range = range.cloned().unwrap_or(0..u64::MAX);
        std::future::ready(Ok(RandRequestStream {
            client: self.client.clone(),
            url: self.url.clone(),
            state: if range.start == 0
                && let Some(resp) = &self.resp
                && let Some(resp) = resp.lock().take()
            {
                ResponseState::Streaming(into_chunk_stream(resp))
            } else if range.end == u64::MAX {
                let req = self.client.get((*self.url).clone(), None).send();
                ResponseState::Pending(Box::pin(req))
            } else {
                ResponseState::None
            },
            range,
            file_id: self.file_id.clone(),
        }))
    }
}
struct RandRequestStream<Client: HttpClient> {
    client: Client,
    url: Arc<Url>,
    range: Range<u64>,
    state: ResponseState<Client>,
    file_id: FileId,
}
impl<Client: HttpClient> Stream for RandRequestStream<Client> {
    type Item = Result<Bytes, (HttpError<Client>, Option<Duration>)>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            break match &mut self.state {
                ResponseState::Pending(resp) => match resp.as_mut().poll(cx) {
                    Poll::Ready(Ok(resp)) => {
                        let new_file_id = FileId::new(
                            resp.headers().get("etag").ok().as_deref(),
                            resp.headers().get("last-modified").ok().as_deref(),
                        );
                        if new_file_id == self.file_id {
                            self.state = ResponseState::Streaming(into_chunk_stream(resp));
                            continue;
                        }
                        self.state = ResponseState::None;
                        Poll::Ready(Some(Err((
                            HttpError::MismatchedBody(new_file_id, resp),
                            None,
                        ))))
                    }
                    Poll::Ready(Err((e, d))) => {
                        self.state = ResponseState::None;
                        Poll::Ready(Some(Err((HttpError::Request(e), d))))
                    }
                    Poll::Pending => Poll::Pending,
                },
                ResponseState::None => {
                    if self.range.end == u64::MAX {
                        break Poll::Ready(Some(Err((HttpError::Irrecoverable, None))));
                    }
                    let resp = self
                        .client
                        .get((*self.url).clone(), Some(self.range.clone()))
                        .send();
                    self.state = ResponseState::Pending(Box::pin(resp));
                    continue;
                }
                ResponseState::Streaming(stream) => match stream.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(chunk))) => {
                        self.range.start += chunk.len() as u64;
                        Poll::Ready(Some(Ok(chunk)))
                    }
                    Poll::Ready(Some(Err(e))) => {
                        self.state = ResponseState::None;
                        Poll::Ready(Some(Err((e, None))))
                    }
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Pending => Poll::Pending,
                },
            };
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::borrow::Cow;
    use std::sync::atomic::{AtomicU64, Ordering};

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
        fn send(
            self,
        ) -> impl Future<Output = Result<Self::Response, (Self::RequestError, Option<Duration>)>>
        {
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
        ) -> impl Future<Output = Result<Self::Response, (Self::RequestError, Option<Duration>)>> + Send
        {
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
        ) -> impl Future<Output = Result<Self::Response, (Self::RequestError, Option<Duration>)>> + Send
        {
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
        ) -> impl Future<Output = Result<Self::Response, (Self::RequestError, Option<Duration>)>> + Send
        {
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
        let url = Url::parse("http://localhost").unwrap();
        let client = MismatchClient;
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        let result = stream.try_next().await;
        assert!(matches!(
            result,
            Err((HttpError::MismatchedBody(_, _), None))
        ));
    }

    #[tokio::test]
    async fn test_http_puller_request_error() {
        let url = Url::parse("http://localhost").unwrap();
        let client = ReqErrClient;
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        let result = stream.try_next().await;
        assert!(matches!(result, Err((HttpError::Request(_), None))));
    }

    #[tokio::test]
    async fn test_http_puller_chunk_error_then_irrecoverable() {
        let url = Url::parse("http://localhost").unwrap();
        let client = ChunkErrClient;
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), client, None, file_id);
        let mut stream = Puller::pull(&mut puller, None).await.unwrap();
        // Full-file pull: Pending -> Ok(resp) -> file_id matches -> Streaming.
        // The first poll of the stream yields the chunk error.
        let first = stream.try_next().await;
        assert!(matches!(first, Err((HttpError::Chunk(_, _), None))));
        // The chunk error leaves state == None with range.end == u64::MAX, so the
        // next poll returns HttpError::Irrecoverable.
        let second = stream.try_next().await;
        assert!(matches!(second, Err((HttpError::Irrecoverable, None))));
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
        ) -> impl Future<Output = Result<Self::Response, (Self::RequestError, Option<Duration>)>> + Send
        {
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
    async fn test_resume_range_advances_after_chunk_error() {
        let url = Url::parse("http://localhost").unwrap();
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(Arc::new(url), ResumeClient, None, file_id);
        let range = 0..100;
        let mut stream = Puller::pull(&mut puller, Some(&range)).await.unwrap();
        // First chunk succeeds ("success" = 7 bytes), advancing range.start to 7.
        let first = stream.try_next().await;
        assert!(matches!(first, Ok(Some(_))));
        // Next poll: chunk error is returned, state resets to None.
        let second = stream.try_next().await;
        assert!(matches!(second, Err((HttpError::Chunk(_, _), None))));
        // Third poll: state None -> retry request issued with advanced start == 7.
        let third = stream.try_next().await;
        assert!(matches!(third, Ok(Some(_))));
        assert_eq!(RESUME_RANGE_START.load(Ordering::SeqCst), 7);
    }
}
