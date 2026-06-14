mod content_disposition;
pub mod manual_redirect;
mod prefetch;
mod puller;
pub use content_disposition::*;
pub use manual_redirect::*;
pub use prefetch::*;
pub use puller::*;

use crate::url_info::FileId;
use bytes::Bytes;
use fast_pull::{ProgressEntry, PullerError};
use std::{borrow::Cow, fmt::Debug, future::Future, time::Duration};
use url::Url;

/// Abstraction over an HTTP client that can send GET requests with optional byte-range headers.
pub trait HttpClient: Clone + Send + Sync + Unpin + 'static {
    type RequestBuilder: HttpRequestBuilder;
    fn get(&self, url: Url, range: Option<ProgressEntry>) -> Self::RequestBuilder;
}
/// Abstraction over an HTTP request builder that can be sent to produce a response.
pub trait HttpRequestBuilder {
    type Response: HttpResponse;
    type RequestError: std::error::Error + Send + Sync + Unpin;
    fn send(
        self,
    ) -> impl Future<Output = Result<Self::Response, (Self::RequestError, Option<Duration>)>> + Send;
}
/// Abstraction over an HTTP response that provides headers, final URL, and chunked body reading.
pub trait HttpResponse: Send + Sync + Debug + Unpin {
    type Headers: HttpHeaders;
    type ChunkError: std::error::Error + Send + Sync + Unpin;
    fn headers(&self) -> &Self::Headers;
    fn url(&self) -> &Url;
    fn chunk(&mut self) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send;
}
/// Abstraction over HTTP response headers, providing typed get-by-name access.
pub trait HttpHeaders {
    type GetHeaderError: std::error::Error + Send + Sync + Unpin;
    /// # Errors
    /// Returns an error if the header cannot be retrieved
    fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError>;
}

/// Projected [`HttpClient::RequestBuilder`] type for a given client.
pub type GetRequestBuilder<Client> = <Client as HttpClient>::RequestBuilder;
/// Projected [`HttpResponse`] type for a given request builder.
pub type GetResponse<Client> = <GetRequestBuilder<Client> as HttpRequestBuilder>::Response;
/// Projected request error type for a given client.
pub type GetRequestError<Client> = <GetRequestBuilder<Client> as HttpRequestBuilder>::RequestError;
/// Projected chunk error type for a given client.
pub type GetChunkError<Client> = <GetResponse<Client> as HttpResponse>::ChunkError;
/// Projected headers type for a given client.
pub type GetHeader<Client> = <GetResponse<Client> as HttpResponse>::Headers;
/// Projected header-get error type for a given client.
pub type GetHeaderError<Client> = <GetHeader<Client> as HttpHeaders>::GetHeaderError;

/// Errors that can occur during HTTP download operations.
///
/// Maps to the various stages of an HTTP request: building, streaming chunks,
/// detecting mismatched file identity, and irrecoverable failures.
#[derive(thiserror::Error)]
pub enum HttpError<Client: HttpClient> {
    #[error("HTTP request failed")]
    Request(GetRequestError<Client>),
    #[error("HTTP chunk read failed")]
    Chunk(GetChunkError<Client>, GetResponse<Client>),
    #[error("irrecoverable pull error")]
    Irrecoverable,
    #[error("body mismatch: expected file {0:?}, got different content")]
    MismatchedBody(FileId, GetResponse<Client>),
}

impl<Client: HttpClient> std::fmt::Debug for HttpError<Client> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(e) => f.debug_tuple("Request").field(e).finish(),
            Self::Chunk(e, r) => f.debug_tuple("Chunk").field(e).field(r).finish(),
            Self::Irrecoverable => f.write_str("Irrecoverable"),
            Self::MismatchedBody(id, r) => {
                f.debug_tuple("MismatchedBody").field(id).field(r).finish()
            }
        }
    }
}

impl<C: HttpClient> PullerError for HttpError<C> {
    fn is_irrecoverable(&self) -> bool {
        matches!(self, Self::Irrecoverable)
    }
}
