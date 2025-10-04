use crate::http::{HttpClient, HttpHeaders, HttpRequestBuilder, HttpResponse};
use fast_pull::ProgressEntry;
use httpdate::parse_http_date;
use reqwest::{
    Client, RequestBuilder, Response,
    header::{self, HeaderMap, HeaderName, InvalidHeaderName},
};
use std::time::{Duration, SystemTime};
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
            let retry_after = res
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|r| r.to_str().ok())
                .and_then(|r| match r.parse() {
                    Ok(r) => Some(Duration::from_secs(r)),
                    Err(_) => match parse_http_date(r) {
                        Ok(target_time) => target_time.duration_since(SystemTime::now()).ok(),
                        Err(_) => None,
                    },
                });
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
    fn chunk(
        &mut self,
    ) -> impl Future<Output = Result<Option<bytes::Bytes>, Self::ChunkError>> + Send {
        Response::chunk(self)
    }
}

impl HttpHeaders for HeaderMap {
    type GetHeaderError = ReqwestGetHeaderError;
    fn get(&self, header: &str) -> Result<&str, Self::GetHeaderError> {
        let header_name: HeaderName = header
            .parse()
            .map_err(ReqwestGetHeaderError::InvalidHeaderName)?;
        let header_value = self
            .get(&header_name)
            .ok_or(ReqwestGetHeaderError::NotFound)?;
        header_value.to_str().map_err(ReqwestGetHeaderError::ToStr)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ReqwestGetHeaderError {
    #[error("Invalid header name {0:?}")]
    InvalidHeaderName(InvalidHeaderName),
    #[error("Failed to convert header value to string {0:?}")]
    ToStr(reqwest::header::ToStrError),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        http::{HttpError, HttpPuller, Prefetch},
        url_info::FileId,
    };
    use fast_pull::{
        Event, MergeProgress,
        mem::MemPusher,
        mock::build_mock_data,
        multi::{self, download_multi},
        single::{self, download_single},
    };
    use reqwest::{Client, StatusCode};
    use std::{num::NonZero, time::Duration};

    #[tokio::test]
    async fn test_redirect_and_content_range() {
        let mut server = mockito::Server::new_async().await;

        let mock_redirect = server
            .mock("GET", "/redirect")
            .with_status(301)
            .with_header("Location", "/%e4%bd%a0%e5%a5%bd.txt")
            .create_async()
            .await;

        let mock_file = server
            .mock("GET", "/%e4%bd%a0%e5%a5%bd.txt")
            .with_status(200)
            .with_header("Content-Length", "1024")
            .with_header("Accept-Ranges", "bytes")
            .with_body(vec![0; 1024])
            .create_async()
            .await;

        let client = Client::new();
        let url = Url::parse(&format!("{}/redirect", server.url())).unwrap();
        let (url_info, _) = client.prefetch(url).await.expect("Request should succeed");

        assert_eq!(
            url_info.final_url.as_str(),
            format!("{}/%e4%bd%a0%e5%a5%bd.txt", server.url())
        );
        assert_eq!(url_info.size, 1024);
        assert_eq!(url_info.name, "你好.txt");
        assert!(url_info.supports_range);

        mock_redirect.assert_async().await;
        mock_file.assert_async().await;
    }

    #[tokio::test]
    async fn test_filename_sources() {
        let mut server = mockito::Server::new_async().await;

        // Test with Content-Disposition header
        let mock1 = server
            .mock("GET", "/test1")
            .with_header("Content-Disposition", r#"attachment; filename="test.txt""#)
            .create_async()
            .await;
        let url = Url::parse(&format!("{}/test1", server.url())).unwrap();
        let (url_info, _) = Client::new().prefetch(url).await.unwrap();
        assert_eq!(url_info.name, "test.txt");
        mock1.assert_async().await;

        // Test URL path source
        let mock2 = server
            .mock("GET", "/test2/%E5%A5%BD%E5%A5%BD%E5%A5%BD.pdf")
            .create_async()
            .await;
        let url = Url::parse(&format!(
            "{}/test2/%E5%A5%BD%E5%A5%BD%E5%A5%BD.pdf",
            server.url()
        ))
        .unwrap();
        let (url_info, _) = Client::new().prefetch(url).await.unwrap();
        assert_eq!(url_info.name, "好好好.pdf");
        mock2.assert_async().await;
    }

    #[tokio::test]
    async fn test_error_handling() {
        let mut server = mockito::Server::new_async().await;
        let mock1 = server
            .mock("GET", "/404")
            .with_status(404)
            .create_async()
            .await;

        let client = Client::new();
        let url = Url::parse(&format!("{}/404", server.url())).unwrap();
        match client.prefetch(url).await {
            Ok(info) => unreachable!("404 status code should not success: {info:?}"),
            Err((err, _)) => match err {
                HttpError::Request(e) => match e {
                    ReqwestResponseError::Reqwest(error) => unreachable!("{error:?}"),
                    ReqwestResponseError::StatusCode(status_code) => {
                        assert_eq!(status_code, StatusCode::NOT_FOUND)
                    }
                },
                HttpError::Chunk(_) => unreachable!(),
                HttpError::GetHeader(_) => unreachable!(),
                HttpError::Irrecoverable => unreachable!(),
                HttpError::MismatchedBody(file_id) => {
                    unreachable!("404 status code should not return mismatched body: {file_id:?}")
                }
            },
        }
        mock1.assert_async().await;
    }

    #[tokio::test]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(300 * 1024 * 1024);
        let mut server = mockito::Server::new_async().await;
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
            Client::new(),
            None,
            FileId::empty(),
        );
        let pusher = MemPusher::with_capacity(mock_data.len());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_multi(
            puller,
            pusher.clone(),
            multi::DownloadOptions {
                concurrent: NonZero::new(32).unwrap(),
                retry_gap: Duration::from_secs(1),
                push_queue_cap: 1024,
                download_chunks: download_chunks.clone(),
                min_chunk_size: NonZero::new(1).unwrap(),
            },
        )
        .await;

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
        assert_eq!(&**pusher.receive.lock().await, mock_data);
    }

    #[tokio::test]
    async fn test_sequential_download() {
        let mock_data = build_mock_data(300 * 1024 * 1024);
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/sequential")
            .with_status(200)
            .with_body(mock_data.clone())
            .create_async()
            .await;
        let puller = HttpPuller::new(
            format!("{}/sequential", server.url()).parse().unwrap(),
            Client::new(),
            None,
            FileId::empty(),
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
        )
        .await;

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
        assert_eq!(&**pusher.receive.lock().await, mock_data);
    }
}
