#[cfg(test)]
mod tests {
    use crate::http::{
        FileId, HttpClient, HttpHeaders, HttpPuller, HttpRequestBuilder, HttpResponse,
    };
    use bytes::Bytes;
    use fast_pull::{ProgressEntry, RandPuller};
    use futures::{Future, TryStreamExt, task::Context};
    use spin::mutex::SpinMutex;
    use std::{pin::Pin, sync::Arc, task::Poll, time::Duration};
    use url::Url;

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
        let mut puller =
            HttpPuller::new(url, client, Some(Arc::new(SpinMutex::new(None))), file_id);
        let range = 0..7;
        let mut stream = puller.pull(&range).await.expect("Failed to create stream");
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
