//! A backend-agnostic HTTP download layer.
//!
//! This module defines a set of small traits — [`HttpClient`],
//! [`HttpRequestBuilder`], [`HttpResponse`], and [`HttpHeaders`] — that abstract
//! over any HTTP client implementation (for example the `reqwest` wrapper in
//! the `reqwest` module). On top of them it provides:
//!
//! * [`HttpPuller`]: a [`fast_pull::Puller`] that streams bytes over HTTP, with
//!   range support and file-identity (resumability) checks.
//! * [`Prefetch`]: resolves a [`crate::UrlInfo`] for a URL via a prefetch request.
//! * [`ContentDisposition`]: parses the `Content-Disposition` header for filenames.
//! * [`manual_redirect`]: RFC 9110-aware `Referer` computation for redirect following.
//! * [`HttpError`]: the error type produced by this layer.
//!
//! Most users do not use these types directly; instead they use
//! `FastDownPuller` (from the `fast-puller` feature), which wraps
//! [`HttpPuller`] with a smart-redirecting `reqwest` client.

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
    #[error("HTTP request failed: {0:?}")]
    Request(GetRequestError<Client>),
    #[error("HTTP chunk read failed: {0:?}\n  response: {1:?}")]
    Chunk(GetChunkError<Client>, GetResponse<Client>),
    #[error("irrecoverable pull error")]
    Irrecoverable,
    #[error("body mismatch: expected file {0:?}, got different content\n  response: {1:?}")]
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{HttpClient, HttpError, HttpHeaders, HttpRequestBuilder, HttpResponse};
    use crate::url_info::FileId;
    use bytes::Bytes;
    use fast_pull::PullerError;
    use std::{borrow::Cow, future::Future, time::Duration};
    use url::Url;

    #[derive(Clone, Debug)]
    struct MockClient;
    impl HttpClient for MockClient {
        type RequestBuilder = MockRequestBuilder;
        fn get(&self, _url: Url, _range: Option<fast_pull::ProgressEntry>) -> Self::RequestBuilder {
            MockRequestBuilder
        }
    }
    struct MockRequestBuilder;
    impl HttpRequestBuilder for MockRequestBuilder {
        type Response = MockResponse;
        type RequestError = MockErr;
        fn send(
            self,
        ) -> impl Future<Output = Result<Self::Response, (Self::RequestError, Option<Duration>)>> + Send
        {
            std::future::ready(Ok(MockResponse::new()))
        }
    }
    #[derive(Debug)]
    struct MockResponse {
        url: Url,
    }
    impl MockResponse {
        fn new() -> Self {
            Self {
                url: Url::parse("http://mock-url").unwrap(),
            }
        }
    }
    impl HttpResponse for MockResponse {
        type Headers = MockHeaders;
        type ChunkError = MockErr;
        fn headers(&self) -> &Self::Headers {
            &MockHeaders
        }
        fn url(&self) -> &Url {
            &self.url
        }
        async fn chunk(&mut self) -> Result<Option<Bytes>, Self::ChunkError> {
            Ok(None)
        }
    }
    #[derive(Debug)]
    struct MockHeaders;
    impl HttpHeaders for MockHeaders {
        type GetHeaderError = MockErr;
        fn get(&self, _header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            Err(MockErr)
        }
    }
    #[derive(Debug, thiserror::Error)]
    #[error("MockError")]
    struct MockErr;

    #[test]
    fn debug_impl_formats_every_variant() {
        let request = HttpError::<MockClient>::Request(MockErr);
        assert!(format!("{request:?}").contains("Request"));

        let chunk = HttpError::<MockClient>::Chunk(MockErr, MockResponse::new());
        assert!(format!("{chunk:?}").contains("Chunk"));

        let irrecoverable = HttpError::<MockClient>::Irrecoverable;
        assert_eq!(format!("{irrecoverable:?}"), "Irrecoverable");

        let mismatched = HttpError::<MockClient>::MismatchedBody(
            FileId::new(Some("etag-x"), None),
            MockResponse::new(),
        );
        assert!(format!("{mismatched:?}").contains("MismatchedBody"));
    }

    #[test]
    fn is_irrecoverable_only_for_variant() {
        assert!(HttpError::<MockClient>::Irrecoverable.is_irrecoverable());
        assert!(!HttpError::<MockClient>::Request(MockErr).is_irrecoverable());
        assert!(!HttpError::<MockClient>::Chunk(MockErr, MockResponse::new()).is_irrecoverable());
        assert!(
            !HttpError::<MockClient>::MismatchedBody(
                FileId::new(Some("x"), None),
                MockResponse::new()
            )
            .is_irrecoverable()
        );
    }
}
