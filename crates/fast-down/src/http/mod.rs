use std::{future::Future, num::NonZeroU16};

mod prefetch;
mod puller;
use bytes::Bytes;
pub use prefetch::*;
pub use puller::*;
use url::Url;

pub trait HttpClient {
    type Response: HttpResponse + Unpin;
    type RequestError: Send;
    fn head(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<Self::Response, Self::RequestError>> + Send;
    fn get(
        &self,
        url: &str,
        headers: Option<Box<[(Box<[u8]>, Box<[u8]>)]>>,
    ) -> impl Future<Output = Result<Self::Response, Self::RequestError>> + Send;
}
pub trait HttpResponse: Send {
    type Headers: HttpHeaders;
    type StatusError: Send;
    type ChunkError: Send;
    fn status(&self) -> NonZeroU16;
    fn headers(&self) -> &Self::Headers;
    fn url(&self) -> &Url;
    fn error_for_status(&self) -> Result<Self, Self::StatusError>
    where
        Self: Sized;
    fn chunk(&self) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send;
}
pub trait HttpHeaders {
    type GetHeaderError: Send;
    fn get(&self, header: &str) -> Result<&str, Self::GetHeaderError>;
}

#[derive(thiserror::Error)]
pub enum HttpError<Client: HttpClient> {
    Request(Client::RequestError),
    Chunk(<Client::Response as HttpResponse>::ChunkError),
    Status(<Client::Response as HttpResponse>::StatusError),
    GetHeader(<<Client::Response as HttpResponse>::Headers as HttpHeaders>::GetHeaderError),
}
