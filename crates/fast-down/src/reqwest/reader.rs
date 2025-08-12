extern crate alloc;
use alloc::{boxed::Box, format};
use bytes::Bytes;
use core::{
    pin::{Pin, pin},
    task::{Context, Poll},
};
use fast_pull::{Puller, RandPuller};
use futures::{Stream, TryFutureExt, TryStream};
use reqwest::{Client, Response, header};
use url::Url;

#[derive(Clone)]
pub struct ReqwestPuller {
    pub(crate) client: Client,
    url: Url,
}

impl ReqwestPuller {
    pub fn new(url: Url, client: Client) -> Self {
        Self { client, url }
    }
}

impl RandPuller for ReqwestPuller {
    type Error = reqwest::Error;
    fn read(
        &mut self,
        range: &fast_pull::ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        ReqwestStream {
            client: self.client.clone(),
            url: self.url.clone(),
            start: range.start,
            end: range.end,
            resp: ResponseState::None,
        }
    }
}
type ResponseFut = Pin<Box<dyn Future<Output = Result<Response, reqwest::Error>> + Send>>;
enum ResponseState {
    Pending(ResponseFut),
    Ready(Response),
    None,
}
struct ReqwestStream {
    client: Client,
    url: Url,
    start: u64,
    end: u64,
    resp: ResponseState,
}
impl Stream for ReqwestStream {
    type Item = Result<Bytes, reqwest::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let chunk_global;
        match &mut self.resp {
            ResponseState::Pending(resp) => {
                return match resp.try_poll_unpin(cx) {
                    Poll::Ready(resp) => match resp {
                        Ok(resp) => {
                            self.resp = ResponseState::Ready(resp);
                            self.poll_next(cx)
                        }
                        Err(e) => {
                            self.resp = ResponseState::None;
                            Poll::Ready(Some(Err(e)))
                        }
                    },
                    Poll::Pending => Poll::Pending,
                };
            }
            ResponseState::None => {
                let resp = self
                    .client
                    .get(self.url.clone())
                    .header(
                        header::RANGE,
                        format!("bytes={}-{}", self.start, self.end - 1),
                    )
                    .send();
                self.resp = ResponseState::Pending(Box::pin(resp));
                return self.poll_next(cx);
            }
            ResponseState::Ready(resp) => {
                if let Err(e) = resp.error_for_status_ref() {
                    self.resp = ResponseState::None;
                    return Poll::Ready(Some(Err(e)));
                }
                let mut chunk = pin!(resp.chunk());
                match chunk.try_poll_unpin(cx) {
                    Poll::Ready(Ok(Some(chunk))) => chunk_global = Ok(chunk),
                    Poll::Ready(Ok(None)) => return Poll::Ready(None),
                    Poll::Ready(Err(e)) => chunk_global = Err(e),
                    Poll::Pending => return Poll::Pending,
                };
            }
        };
        match chunk_global {
            Ok(chunk) => {
                self.start += chunk.len() as u64;
                Poll::Ready(Some(Ok(chunk)))
            }
            Err(e) => {
                self.resp = ResponseState::None;
                Poll::Ready(Some(Err(e)))
            }
        }
    }
}

impl Puller for ReqwestPuller {
    type Error = reqwest::Error;
    fn pull(&mut self) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        let req = self.client.get(self.url.clone());
        Box::pin(async move {
            let resp = req.send().await?;
            Ok(resp.bytes_stream())
        })
        .try_flatten_stream()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{
        Event, MergeProgress, ProgressEntry,
        mock::{MockRandWriter, MockSeqWriter, build_mock_data},
        multi::{self, download_multi},
        reqwest::ReqwestPuller,
        single::{self, download_single},
    };
    use alloc::vec;
    use core::{num::NonZeroUsize, time::Duration};
    use reqwest::Client;
    use std::{dbg, println};
    use vec::Vec;

    #[tokio::test]
    async fn test_concurrent_download() {
        let mock_data = build_mock_data(300 * 1024 * 1024);
        let mut server = mockito::Server::new_async().await;
        let mock_body_clone = mock_data.clone();
        let _mock = server
            .mock("GET", "/concurrent")
            .with_status(206)
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
        let reader = ReqwestPuller::new(
            format!("{}/concurrent", server.url()).parse().unwrap(),
            Client::new(),
        );
        let writer = MockRandWriter::new(&mock_data);
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_multi(
            reader,
            writer.clone(),
            multi::DownloadOptions {
                concurrent: NonZero::new(32).unwrap(),
                retry_gap: Duration::from_secs(1),
                write_queue_cap: 1024,
                download_chunks: download_chunks.clone(),
            },
        )
        .await;

        let mut download_progress: Vec<ProgressEntry> = Vec::new();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::ReadProgress(_, p) => {
                    download_progress.merge_progress(p);
                }
                Event::WriteProgress(_, p) => {
                    write_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        dbg!(&download_progress);
        dbg!(&write_progress);
        assert_eq!(download_progress, download_chunks);
        assert_eq!(write_progress, download_chunks);

        result.join().await.unwrap();
        writer.assert().await;
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
        let reader = ReqwestPuller::new(
            format!("{}/sequential", server.url()).parse().unwrap(),
            Client::new(),
        );
        let writer = MockSeqWriter::new(&mock_data);
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_single(
            reader,
            writer.clone(),
            single::DownloadOptions {
                retry_gap: Duration::from_secs(1),
                write_queue_cap: 1024,
            },
        )
        .await;

        let mut download_progress: Vec<ProgressEntry> = Vec::new();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::ReadProgress(_, p) => {
                    download_progress.merge_progress(p);
                }
                Event::WriteProgress(_, p) => {
                    write_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        dbg!(&download_progress);
        dbg!(&write_progress);
        assert_eq!(download_progress, download_chunks);
        assert_eq!(write_progress, download_chunks);

        result.join().await.unwrap();
        writer.assert().await;
    }
}
