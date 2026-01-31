use crate::http::{
    FileId, GetRequestError, GetResponse, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder,
    HttpResponse,
};
use bytes::Bytes;
use fast_pull::{ProgressEntry, PullResult, PullStream, RandPuller, SeqPuller};
use futures::Stream;
use parking_lot::Mutex;
use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use url::Url;

#[derive(Clone)]
pub struct HttpPuller<Client: HttpClient> {
    client: Client,
    url: Url,
    resp: Option<Arc<Mutex<Option<GetResponse<Client>>>>>,
    file_id: FileId,
}
impl<Client: HttpClient> HttpPuller<Client> {
    pub fn new(
        url: Url,
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
            Err(e) => Err(HttpError::Chunk(e)),
        }
    }))
}

impl<Client: HttpClient> RandPuller for HttpPuller<Client> {
    type Error = HttpError<Client>;
    async fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        Ok(RandRequestStream {
            client: self.client.clone(),
            url: self.url.clone(),
            start: range.start,
            end: range.end,
            state: if range.start == 0
                && let Some(resp) = &self.resp
                && let Some(resp) = resp.lock().take()
            {
                ResponseState::Streaming(into_chunk_stream(resp))
            } else {
                ResponseState::None
            },
            file_id: self.file_id.clone(),
        })
    }
}
struct RandRequestStream<Client: HttpClient> {
    client: Client,
    url: Url,
    start: u64,
    end: u64,
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
                            resp.headers().get("etag").ok(),
                            resp.headers().get("last-modified").ok(),
                        );
                        if new_file_id != self.file_id {
                            self.state = ResponseState::None;
                            Poll::Ready(Some(Err((HttpError::MismatchedBody(new_file_id), None))))
                        } else {
                            self.state = ResponseState::Streaming(into_chunk_stream(resp));
                            continue;
                        }
                    }
                    Poll::Ready(Err((e, d))) => {
                        self.state = ResponseState::None;
                        Poll::Ready(Some(Err((HttpError::Request(e), d))))
                    }
                    Poll::Pending => Poll::Pending,
                },
                ResponseState::None => {
                    let resp = self
                        .client
                        .get(self.url.clone(), Some(self.start..self.end))
                        .send();
                    self.state = ResponseState::Pending(Box::pin(resp));
                    continue;
                }
                ResponseState::Streaming(stream) => match stream.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(chunk))) => {
                        self.start += chunk.len() as u64;
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

impl<Client: HttpClient> SeqPuller for HttpPuller<Client> {
    type Error = HttpError<Client>;
    async fn pull(&mut self) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        Ok(SeqRequestStream {
            state: if let Some(resp) = &self.resp
                && let Some(resp) = resp.lock().take()
            {
                ResponseState::Streaming(into_chunk_stream(resp))
            } else {
                let req = self.client.get(self.url.clone(), None).send();
                ResponseState::Pending(Box::pin(req))
            },
            file_id: self.file_id.clone(),
        })
    }
}
struct SeqRequestStream<Client: HttpClient> {
    state: ResponseState<Client>,
    file_id: FileId,
}
impl<Client: HttpClient> Stream for SeqRequestStream<Client> {
    type Item = Result<Bytes, (HttpError<Client>, Option<Duration>)>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            break match &mut self.state {
                ResponseState::Pending(resp) => match resp.as_mut().poll(cx) {
                    Poll::Ready(Ok(resp)) => {
                        let new_file_id = FileId::new(
                            resp.headers().get("etag").ok(),
                            resp.headers().get("last-modified").ok(),
                        );
                        if new_file_id != self.file_id {
                            self.state = ResponseState::None;
                            Poll::Ready(Some(Err((HttpError::MismatchedBody(new_file_id), None))))
                        } else {
                            self.state = ResponseState::Streaming(into_chunk_stream(resp));
                            continue;
                        }
                    }
                    Poll::Ready(Err((e, d))) => {
                        self.state = ResponseState::None;
                        Poll::Ready(Some(Err((HttpError::Request(e), d))))
                    }
                    Poll::Pending => Poll::Pending,
                },
                ResponseState::None => {
                    break Poll::Ready(Some(Err((HttpError::Irrecoverable, None))));
                }
                ResponseState::Streaming(stream) => match stream.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(chunk))),
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
        async fn send(self) -> Result<Self::Response, (Self::RequestError, Option<Duration>)> {
            Ok(MockResponse::new())
        }
    }
    pub struct MockResponse {
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
    pub struct MockHeaders;
    impl HttpHeaders for MockHeaders {
        type GetHeaderError = MockError;
        fn get(&self, _header: &str) -> Result<&str, Self::GetHeaderError> {
            Err(MockError)
        }
    }
    #[derive(Debug)]
    pub struct MockError;

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
                println!("Wait... [Mock: 模拟网络延迟 Pending]");
                self.polled_once = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            println!("Done! [Mock: 数据到达 Ready]");
            Poll::Ready(Ok(Some(Bytes::from_static(b"success"))))
        }
    }

    #[tokio::test]
    async fn test_http_puller_infinite_loop_fix() {
        let url = Url::parse("http://localhost").unwrap();
        let client = MockClient;
        let file_id = FileId::new(None, None);
        let mut puller = HttpPuller::new(url, client, None, file_id);
        let range = 0..7;
        let mut stream = RandPuller::pull(&mut puller, &range)
            .await
            .expect("Failed to create stream");
        println!("--- 开始测试 HttpPuller ---");
        let result =
            tokio::time::timeout(Duration::from_secs(1), async { stream.try_next().await }).await;
        match result {
            Ok(Ok(Some(bytes))) => {
                println!("收到数据: {:?}", bytes);
                assert_eq!(bytes, Bytes::from_static(b"success"));
                println!("测试通过：HttpPuller 正确处理了 Pending 状态！");
            }
            e => {
                panic!(
                    "测试失败：超时！这表明 HttpPuller 可能在收到 Pending 后丢失了 Future 状态并陷入了死循环。 {e:?}"
                );
            }
        }
    }
}
