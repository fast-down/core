use crate::http::{
    GetRequestError, GetResponse, HttpClient, HttpError, HttpRequestBuilder, HttpResponse,
};
use bytes::Bytes;
use fast_pull::{ProgressEntry, RandPuller, SeqPuller};
use futures::{Stream, TryFutureExt, TryStream};
use spin::mutex::SpinMutex;
use std::{
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
};
use url::Url;

#[derive(Clone)]
pub struct HttpPuller<Client: HttpClient> {
    pub(crate) client: Client,
    url: Url,
    resp: Arc<SpinMutex<Option<GetResponse<Client>>>>,
}
impl<Client: HttpClient> HttpPuller<Client> {
    pub fn new(url: Url, client: Client, resp: Option<GetResponse<Client>>) -> Self {
        Self {
            client,
            url,
            resp: Arc::new(SpinMutex::new(resp)),
        }
    }
}

type ResponseFut<Client> =
    Pin<Box<dyn Future<Output = Result<GetResponse<Client>, GetRequestError<Client>>> + Send>>;
enum ResponseState<Client: HttpClient> {
    Pending(ResponseFut<Client>),
    Ready(GetResponse<Client>),
    None,
}

impl<Client: HttpClient + 'static> RandPuller for HttpPuller<Client> {
    type Error = HttpError<Client>;
    fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        RandRequestStream {
            client: self.client.clone(),
            url: self.url.clone(),
            start: range.start,
            end: range.end,
            state: if range.start == 0
                && let Some(resp) = self.resp.lock().take()
            {
                ResponseState::Ready(resp)
            } else {
                ResponseState::None
            },
        }
    }
}
struct RandRequestStream<Client: HttpClient + 'static> {
    client: Client,
    url: Url,
    start: u64,
    end: u64,
    state: ResponseState<Client>,
}
impl<Client: HttpClient> Stream for RandRequestStream<Client> {
    type Item = Result<Bytes, HttpError<Client>>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let chunk_global;
        match &mut self.state {
            ResponseState::Pending(resp) => {
                return match resp.try_poll_unpin(cx) {
                    Poll::Ready(resp) => match resp {
                        Ok(resp) => {
                            self.state = ResponseState::Ready(resp);
                            self.poll_next(cx)
                        }
                        Err(e) => {
                            self.state = ResponseState::None;
                            Poll::Ready(Some(Err(HttpError::Request(e))))
                        }
                    },
                    Poll::Pending => Poll::Pending,
                };
            }
            ResponseState::None => {
                let resp = self
                    .client
                    .get(self.url.clone(), Some(self.start..self.end))
                    .send();
                self.state = ResponseState::Pending(Box::pin(resp));
                return self.poll_next(cx);
            }
            ResponseState::Ready(resp) => {
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
                self.state = ResponseState::None;
                Poll::Ready(Some(Err(HttpError::Chunk(e))))
            }
        }
    }
}

impl<Client: HttpClient + 'static> SeqPuller for HttpPuller<Client> {
    type Error = HttpError<Client>;
    fn pull(&mut self) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        match self.resp.lock().take() {
            Some(resp) => SeqRequestStream {
                state: ResponseState::Ready(resp),
            },
            None => {
                let req = self.client.get(self.url.clone(), None).send();
                SeqRequestStream {
                    state: ResponseState::Pending(Box::pin(req)),
                }
            }
        }
    }
}
struct SeqRequestStream<Client: HttpClient + 'static> {
    state: ResponseState<Client>,
}
impl<Client: HttpClient> Stream for SeqRequestStream<Client> {
    type Item = Result<Bytes, HttpError<Client>>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let chunk_global;
        match &mut self.state {
            ResponseState::Pending(resp) => {
                return match resp.try_poll_unpin(cx) {
                    Poll::Ready(resp) => match resp {
                        Ok(resp) => {
                            self.state = ResponseState::Ready(resp);
                            self.poll_next(cx)
                        }
                        Err(e) => {
                            self.state = ResponseState::None;
                            Poll::Ready(Some(Err(HttpError::Request(e))))
                        }
                    },
                    Poll::Pending => Poll::Pending,
                };
            }
            ResponseState::None => return Poll::Ready(Some(Err(HttpError::Irrecoverable))),
            ResponseState::Ready(resp) => {
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
            Ok(chunk) => Poll::Ready(Some(Ok(chunk))),
            Err(e) => {
                self.state = ResponseState::None;
                Poll::Ready(Some(Err(HttpError::Chunk(e))))
            }
        }
    }
}
