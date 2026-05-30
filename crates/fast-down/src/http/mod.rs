mod content_disposition;
mod prefetch;
mod puller;
pub use content_disposition::*;
pub use prefetch::*;
pub use puller::*;

use crate::url_info::FileId;
use bytes::Bytes;
use fast_pull::{ProgressEntry, PullerError};
use std::{borrow::Cow, fmt::Debug, future::Future, time::Duration};
use url::Url;

pub trait HttpClient: Clone + Send + Sync + Unpin + 'static {
    type RequestBuilder: HttpRequestBuilder;
    fn get(&self, url: Url, range: Option<ProgressEntry>) -> Self::RequestBuilder;
}
pub trait HttpRequestBuilder {
    type Response: HttpResponse;
    type RequestError: Send + Debug + Unpin;
    fn send(
        self,
    ) -> impl Future<Output = Result<Self::Response, (Self::RequestError, Option<Duration>)>> + Send;
}
pub trait HttpResponse: Send + Debug + Unpin {
    type Headers: HttpHeaders;
    type ChunkError: Send + Debug + Unpin;
    fn headers(&self) -> &Self::Headers;
    fn url(&self) -> &Url;
    fn chunk(&mut self) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send;
}
pub trait HttpHeaders {
    type GetHeaderError: Send + Debug + Unpin;
    /// # Errors
    /// Returns an error if the header cannot be retrieved
    fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError>;
}

pub type GetRequestBuilder<Client> = <Client as HttpClient>::RequestBuilder;
pub type GetResponse<Client> = <GetRequestBuilder<Client> as HttpRequestBuilder>::Response;
pub type GetRequestError<Client> = <GetRequestBuilder<Client> as HttpRequestBuilder>::RequestError;
pub type GetChunkError<Client> = <GetResponse<Client> as HttpResponse>::ChunkError;
pub type GetHeader<Client> = <GetResponse<Client> as HttpResponse>::Headers;
pub type GetHeaderError<Client> = <GetHeader<Client> as HttpHeaders>::GetHeaderError;

#[derive(thiserror::Error, Debug)]
pub enum HttpError<Client: HttpClient> {
    Request(GetRequestError<Client>),
    Chunk(GetChunkError<Client>, GetResponse<Client>),
    Irrecoverable,
    MismatchedBody(FileId, GetResponse<Client>),
}

impl<C: HttpClient> PullerError for HttpError<C> {
    fn is_irrecoverable(&self) -> bool {
        matches!(self, Self::Irrecoverable)
    }
}
