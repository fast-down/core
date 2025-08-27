use crate::http::{HttpClient, HttpError, HttpResponse};
use bytes::Bytes;
use fast_pull::{ProgressEntry, RandPuller};
use futures::{Stream, TryFutureExt, TryStream};
use std::{
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Clone)]
pub struct HttpPuller<Client: HttpClient> {
    pub(crate) client: Client,
    url: Arc<str>,
}

impl<Client: HttpClient> HttpPuller<Client> {
    pub fn new(url: &str, client: Client) -> Self {
        Self {
            client,
            url: url.into(),
        }
    }
}

impl<Client: HttpClient + Clone + Send + Sync + Unpin> RandPuller for HttpPuller<Client> {
    type Error = HttpError<Client>;
    fn pull(
        &mut self,
        range: &ProgressEntry,
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

type ResponseFut<Client: HttpClient> =
    Pin<Box<dyn Future<Output = Result<Client::Response, Client::RequestError>> + Send>>;
enum ResponseState<Client: HttpClient> {
    Pending(ResponseFut<Client>),
    Ready(Client::Response),
    None,
}

struct ReqwestStream<Client: HttpClient> {
    client: Client,
    url: Arc<str>,
    start: u64,
    end: u64,
    resp: ResponseState<Client>,
}
impl<Client: HttpClient> Stream for ReqwestStream<Client> {
    type Item = Result<Bytes, HttpError<Client>>;
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
                            Poll::Ready(Some(Err(HttpError::Request(e))))
                        }
                    },
                    Poll::Pending => Poll::Pending,
                };
            }
            ResponseState::None => {
                let range = format!("bytes={}-{}", self.start, self.end - 1);
                let resp = self.client.get(
                    &self.url,
                    Some([(b"range"[..].into(), range.as_bytes().into())].into()),
                );
                self.resp = ResponseState::Pending(Box::pin(resp));
                return self.poll_next(cx);
            }
            ResponseState::Ready(resp) => {
                if let Err(e) = resp.error_for_status() {
                    self.resp = ResponseState::None;
                    return Poll::Ready(Some(Err(HttpError::Status(e))));
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
                Poll::Ready(Some(Err(HttpError::Chunk(e))))
            }
        }
    }
}

// impl SeqPuller for HttpPuller {
//     type Error = reqwest::Error;
//     fn pull(&mut self) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
//         let req = self.client.get(self.url.clone());
//         Box::pin(async move {
//             let resp = req.send().await?;
//             log::debug!("[ReqwestPuller] {}: {:?}", self.url, resp);
//             Ok(resp.bytes_stream())
//         })
//         .try_flatten_stream()
//     }
// }

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use fast_pull::{
//         Event, MergeProgress,
//         mem::MemPusher,
//         mock::build_mock_data,
//         multi::{self, download_multi},
//         single::{self, download_single},
//     };
//     use std::{num::NonZero, time::Duration};

//     #[tokio::test]
//     async fn test_concurrent_download() {
//         let mock_data = build_mock_data(300 * 1024 * 1024);
//         let mut server = mockito::Server::new_async().await;
//         let mock_body_clone = mock_data.clone();
//         let _mock = server
//             .mock("GET", "/concurrent")
//             .with_status(206)
//             .with_body_from_request(move |request| {
//                 if !request.has_header("Range") {
//                     return mock_body_clone.clone();
//                 }
//                 let range = request.header("Range")[0];
//                 println!("range: {range:?}");
//                 range
//                     .to_str()
//                     .unwrap()
//                     .rsplit('=')
//                     .next()
//                     .unwrap()
//                     .split(',')
//                     .map(|p| p.trim().splitn(2, '-'))
//                     .map(|mut p| {
//                         let start = p.next().unwrap().parse::<usize>().unwrap();
//                         let end = p.next().unwrap().parse::<usize>().unwrap();
//                         start..=end
//                     })
//                     .flat_map(|p| mock_body_clone[p].to_vec())
//                     .collect()
//             })
//             .create_async()
//             .await;
//         let puller = HttpPuller::new(
//             format!("{}/concurrent", server.url()).parse().unwrap(),
//             Client::new(),
//         );
//         let pusher = MemPusher::with_capacity(mock_data.len());
//         #[allow(clippy::single_range_in_vec_init)]
//         let download_chunks = vec![0..mock_data.len() as u64];
//         let result = download_multi(
//             puller,
//             pusher.clone(),
//             multi::DownloadOptions {
//                 concurrent: NonZero::new(32).unwrap(),
//                 retry_gap: Duration::from_secs(1),
//                 push_queue_cap: 1024,
//                 download_chunks: download_chunks.clone(),
//                 min_chunk_size: NonZero::new(1).unwrap(),
//             },
//         )
//         .await;

//         let mut pull_progress: Vec<ProgressEntry> = Vec::new();
//         let mut push_progress: Vec<ProgressEntry> = Vec::new();
//         while let Ok(e) = result.event_chain.recv().await {
//             match e {
//                 Event::PullProgress(_, p) => {
//                     pull_progress.merge_progress(p);
//                 }
//                 Event::PushProgress(_, p) => {
//                     push_progress.merge_progress(p);
//                 }
//                 _ => {}
//             }
//         }
//         dbg!(&pull_progress);
//         dbg!(&push_progress);
//         assert_eq!(pull_progress, download_chunks);
//         assert_eq!(push_progress, download_chunks);

//         result.join().await.unwrap();
//         assert_eq!(&**pusher.receive.lock(), mock_data);
//     }

//     #[tokio::test]
//     async fn test_sequential_download() {
//         let mock_data = build_mock_data(300 * 1024 * 1024);
//         let mut server = mockito::Server::new_async().await;
//         let _mock = server
//             .mock("GET", "/sequential")
//             .with_status(200)
//             .with_body(mock_data.clone())
//             .create_async()
//             .await;
//         let puller = HttpPuller::new(
//             format!("{}/sequential", server.url()).parse().unwrap(),
//             Client::new(),
//         );
//         let pusher = MemPusher::with_capacity(mock_data.len());
//         #[allow(clippy::single_range_in_vec_init)]
//         let download_chunks = vec![0..mock_data.len() as u64];
//         let result = download_single(
//             puller,
//             pusher.clone(),
//             single::DownloadOptions {
//                 retry_gap: Duration::from_secs(1),
//                 push_queue_cap: 1024,
//             },
//         )
//         .await;

//         let mut pull_progress: Vec<ProgressEntry> = Vec::new();
//         let mut push_progress: Vec<ProgressEntry> = Vec::new();
//         while let Ok(e) = result.event_chain.recv().await {
//             match e {
//                 Event::PullProgress(_, p) => {
//                     pull_progress.merge_progress(p);
//                 }
//                 Event::PushProgress(_, p) => {
//                     push_progress.merge_progress(p);
//                 }
//                 _ => {}
//             }
//         }
//         dbg!(&pull_progress);
//         dbg!(&push_progress);
//         assert_eq!(pull_progress, download_chunks);
//         assert_eq!(push_progress, download_chunks);

//         result.join().await.unwrap();
//         assert_eq!(&**pusher.receive.lock(), mock_data);
//     }
// }
