mod prefetch;
mod puller;
pub use prefetch::*;
pub use puller::*;

use bytes::Bytes;
use fast_pull::ProgressEntry;
use std::{fmt::Debug, future::Future};
use url::Url;

pub trait HttpClient: Clone + Send + Sync + Unpin {
    type RequestBuilder: HttpRequestBuilder;
    fn get(&self, url: Url, range: Option<ProgressEntry>) -> Self::RequestBuilder;
}
pub trait HttpRequestBuilder {
    type Response: HttpResponse;
    type RequestError: Send + Debug;
    fn send(self) -> impl Future<Output = Result<Self::Response, Self::RequestError>> + Send;
}
pub trait HttpResponse: Send + Unpin {
    type Headers: HttpHeaders;
    type ChunkError: Send + Debug;
    fn headers(&self) -> &Self::Headers;
    fn url(&self) -> &Url;
    fn chunk(&mut self) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send;
}
pub trait HttpHeaders {
    type GetHeaderError: Send + Debug;
    fn get(&self, header: &str) -> Result<&str, Self::GetHeaderError>;
}

pub type GetResponse<Client> =
    <<Client as HttpClient>::RequestBuilder as HttpRequestBuilder>::Response;
pub type GetRequestError<Client> =
    <<Client as HttpClient>::RequestBuilder as HttpRequestBuilder>::RequestError;
pub type GetChunkError<Client> = <GetResponse<Client> as HttpResponse>::ChunkError;
pub type GetHeader<Client> = <GetResponse<Client> as HttpResponse>::Headers;
pub type GetHeaderError<Client> = <GetHeader<Client> as HttpHeaders>::GetHeaderError;

#[derive(thiserror::Error, Debug)]
pub enum HttpError<Client: HttpClient> {
    Request(GetRequestError<Client>),
    Chunk(GetChunkError<Client>),
    GetHeader(GetHeaderError<Client>),
}
