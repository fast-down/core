use crate::http::{
    FileId, GetRequestError, GetResponse, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder,
    HttpResponse,
};
use bytes::Bytes;
use fast_pull::{ProgressEntry, PullResult, PullStream, RandPuller, SeqPuller};
use futures::{Stream, TryFutureExt};
use spin::mutex::SpinMutex;
use std::{
    fmt::Debug,
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use url::Url;

#[derive(Clone)]
pub struct HttpPuller<Client: HttpClient> {
    pub(crate) client: Client,
    url: Url,
    resp: Option<Arc<SpinMutex<Option<GetResponse<Client>>>>>,
    file_id: FileId,
}
impl<Client: HttpClient> HttpPuller<Client> {
    pub fn new(
        url: Url,
        client: Client,
        resp: Option<Arc<SpinMutex<Option<GetResponse<Client>>>>>,
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
            .field("client", &"...")
            .field("url", &self.url)
            .field("resp", &"...")
            .field("file_id", &self.file_id)
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
enum ResponseState<Client: HttpClient> {
    Pending(ResponseFut<Client>),
    Ready(GetResponse<Client>),
    None,
}

impl<Client: HttpClient + 'static> RandPuller for HttpPuller<Client> {
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
                ResponseState::Ready(resp)
            } else {
                ResponseState::None
            },
            file_id: self.file_id.clone(),
        })
    }
}
struct RandRequestStream<Client: HttpClient + 'static> {
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
        let chunk_global;
        match &mut self.state {
            ResponseState::Pending(resp) => {
                return match resp.try_poll_unpin(cx) {
                    Poll::Ready(resp) => match resp {
                        Ok(resp) => {
                            let new_file_id = FileId::new(
                                resp.headers().get("etag").ok(),
                                resp.headers().get("last-modified").ok(),
                            );
                            if new_file_id != self.file_id {
                                self.state = ResponseState::None;
                                Poll::Ready(Some(Err((
                                    HttpError::MismatchedBody(new_file_id),
                                    None,
                                ))))
                            } else {
                                self.state = ResponseState::Ready(resp);
                                self.poll_next(cx)
                            }
                        }
                        Err((e, d)) => {
                            self.state = ResponseState::None;
                            Poll::Ready(Some(Err((HttpError::Request(e), d))))
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
                Poll::Ready(Some(Err((HttpError::Chunk(e), None))))
            }
        }
    }
}

impl<Client: HttpClient + 'static> SeqPuller for HttpPuller<Client> {
    type Error = HttpError<Client>;
    async fn pull(&mut self) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        Ok(SeqRequestStream {
            state: if let Some(resp) = &self.resp
                && let Some(resp) = resp.lock().take()
            {
                ResponseState::Ready(resp)
            } else {
                let req = self.client.get(self.url.clone(), None).send();
                ResponseState::Pending(Box::pin(req))
            },
            file_id: self.file_id.clone(),
        })
    }
}
struct SeqRequestStream<Client: HttpClient + 'static> {
    state: ResponseState<Client>,
    file_id: FileId,
}
impl<Client: HttpClient> Stream for SeqRequestStream<Client> {
    type Item = Result<Bytes, (HttpError<Client>, Option<Duration>)>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let chunk_global;
        match &mut self.state {
            ResponseState::Pending(resp) => {
                return match resp.try_poll_unpin(cx) {
                    Poll::Ready(resp) => match resp {
                        Ok(resp) => {
                            let new_file_id = FileId::new(
                                resp.headers().get("etag").ok(),
                                resp.headers().get("last-modified").ok(),
                            );
                            if new_file_id != self.file_id {
                                self.state = ResponseState::None;
                                Poll::Ready(Some(Err((
                                    HttpError::MismatchedBody(new_file_id),
                                    None,
                                ))))
                            } else {
                                self.state = ResponseState::Ready(resp);
                                self.poll_next(cx)
                            }
                        }
                        Err((e, d)) => {
                            self.state = ResponseState::None;
                            Poll::Ready(Some(Err((HttpError::Request(e), d))))
                        }
                    },
                    Poll::Pending => Poll::Pending,
                };
            }
            ResponseState::None => return Poll::Ready(Some(Err((HttpError::Irrecoverable, None)))),
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
                Poll::Ready(Some(Err((HttpError::Chunk(e), None))))
            }
        }
    }
}
