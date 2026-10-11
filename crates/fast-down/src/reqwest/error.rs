//! Errors produced by the `reqwest` HTTP backend.
//!
//! Each type wraps the underlying `reqwest`/`http` value privately and forwards
//! what a caller needs to render, classify, or inspect it, so code that does not
//! opt in is not tied to the exact client version. [`StatusCodeError::into_response`]
//! is the deliberate escape hatch for callers that do want the raw response.

use reqwest::Response;
use url::Url;

/// Errors that can occur when getting a header value from a reqwest response.
#[derive(thiserror::Error, Debug)]
pub enum ReqwestGetHeaderError {
    /// The requested header is not present.
    #[error("Header not found")]
    NotFound,
}

/// A failure while building the underlying HTTP client: an invalid proxy URL, a
/// TLS backend that failed to initialize, and so on.
///
/// Transparent over the underlying client error: it renders and sources through
/// to it, and derefs to it so its classification methods (for example
/// `is_timeout`) are callable without naming the client crate.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct BuildClientError(#[from] reqwest::Error);

impl std::ops::Deref for BuildClientError {
    type Target = reqwest::Error;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A transport-level failure from the underlying HTTP client.
///
/// Transparent over the underlying client error: it renders and sources through
/// to it, and derefs to it so its classification methods (for example
/// `is_timeout`) are callable without naming the client crate.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct RequestError(#[from] reqwest::Error);

impl std::ops::Deref for RequestError {
    type Target = reqwest::Error;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A non-success HTTP response, captured without exposing the response type of
/// the underlying HTTP client.
///
/// Status, URL, and headers are available synchronously. The body is **not**
/// read up front: the failed response is kept alive and [`body`](Self::body)
/// streams it on demand, so constructing the error is pure and does no I/O.
/// Since the response is consumed to read the body, `body` takes `self`.
///
/// Reading stops at [`MAX_ERROR_BODY`] bytes: an error response belongs to the
/// server, and a hostile or misconfigured one must not be able to make this
/// allocate an unbounded buffer.
#[derive(Debug, thiserror::Error)]
#[error(
    "Url: {}, Status Code: {}, Headers: {:?}",
    .resp.url(),
    .resp.status().as_u16(),
    .resp.headers()
)]
pub struct StatusCodeError {
    resp: Response,
}

/// Most bytes of an error body read by [`StatusCodeError::body`].
pub const MAX_ERROR_BODY: usize = 64 * 1024;

impl StatusCodeError {
    /// Capture a failed response. No body is read here.
    pub(crate) const fn from_response(resp: Response) -> Self {
        Self { resp }
    }

    /// The HTTP status code of the failed response.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.resp.status().as_u16()
    }

    /// The URL the failed response came from.
    #[must_use]
    pub fn url(&self) -> &Url {
        self.resp.url()
    }

    /// A header value from the failed response, if present and valid UTF-8.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.resp.headers().get(name).and_then(|v| v.to_str().ok())
    }

    /// Take back the raw failed response for full access to the underlying HTTP
    /// client.
    ///
    /// This is an opt-in escape hatch: the signature names `reqwest::Response`,
    /// but **only callers who use it** need to depend on `reqwest`. Code that
    /// does not call it is unaffected and can keep using [`body`](Self::body) /
    /// [`body_text`](Self::body_text) without naming the client crate.
    ///
    /// Consumes the error, since a response body can only be read once.
    #[must_use]
    pub fn into_response(self) -> Response {
        self.resp
    }

    /// Read the body of the failed response, capped at [`MAX_ERROR_BODY`] bytes.
    ///
    /// Consumes the response; a body can only be read once. Stops once the cap
    /// is reached instead of trusting a remote `Content-Length`, so a hostile
    /// server cannot force an unbounded allocation.
    pub async fn body(mut self) -> Vec<u8> {
        let mut body = Vec::new();
        while body.len() < MAX_ERROR_BODY {
            match self.resp.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = MAX_ERROR_BODY - body.len();
                    body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                Ok(None) | Err(_) => break,
            }
        }
        body
    }

    /// Read the body of the failed response and decode it as UTF-8, lossily.
    pub async fn body_text(self) -> String {
        String::from_utf8_lossy(&self.body().await).into_owned()
    }
}

/// Errors that can occur when sending a reqwest request.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ReqwestResponseError {
    /// The request could not be completed (connection, TLS, timeout, …).
    #[error(transparent)]
    Request(RequestError),
    /// The server answered with a non-success status code.
    #[error(transparent)]
    StatusCode(Box<StatusCodeError>),
}

impl ReqwestResponseError {
    /// Capture a failed response as a status-code variant. No body is read.
    pub(crate) fn from_response(resp: Response) -> Self {
        Self::StatusCode(Box::new(StatusCodeError::from_response(resp)))
    }

    /// The status code of a non-success response, or `None` for a transport
    /// failure.
    #[must_use]
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Request(_) => None,
            Self::StatusCode(resp) => Some(resp.status()),
        }
    }

    /// The URL of a non-success response, or `None` for a transport failure.
    #[must_use]
    pub fn url(&self) -> Option<&Url> {
        match self {
            Self::Request(_) => None,
            Self::StatusCode(resp) => Some(resp.url()),
        }
    }

    /// Whether the failure was a timeout (never true for a status code).
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Request(e) => e.is_timeout(),
            Self::StatusCode(_) => false,
        }
    }

    /// Whether the failure was a connection error (never true for a status
    /// code).
    #[must_use]
    pub fn is_connect(&self) -> bool {
        match self {
            Self::Request(e) => e.is_connect(),
            Self::StatusCode(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::significant_drop_tightening
    )]
    use super::*;
    use crate::http::Prefetch;
    use reqwest::{Client, StatusCode};

    #[tokio::test]
    async fn test_error_handling() {
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let _mock1 = server
            .mock("GET", "/404")
            .with_status(404)
            .create_async()
            .await;

        let url = Url::parse(&format!("{}/404", server.url())).unwrap();
        match client.prefetch(url).await {
            Ok(info) => unreachable!("404 status code should not success: {info:?}"),
            Err((err, _)) => match err {
                ReqwestResponseError::Request(error) => unreachable!("{error:?}"),
                ReqwestResponseError::StatusCode(resp) => {
                    assert_eq!(resp.status(), StatusCode::NOT_FOUND.as_u16());
                }
            },
        }
    }

    #[tokio::test]
    async fn status_code_error_exposes_headers_and_body() {
        // A non-success response must be captured losslessly: the caller can
        // read the status, a response header, and the raw body without naming
        // reqwest.
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let _mock = server
            .mock("GET", "/boom")
            .with_status(502)
            .with_header("x-reason", "upstream down")
            .with_body("bad gateway")
            .create_async()
            .await;

        let url = Url::parse(&format!("{}/boom", server.url())).unwrap();
        let (err, _) = client.prefetch(url).await.expect_err("502 must fail");
        assert_eq!(err.status_code(), Some(502));
        assert_eq!(err.url().unwrap().path(), "/boom");
        match err {
            ReqwestResponseError::StatusCode(resp) => {
                assert_eq!(resp.status(), 502);
                assert_eq!(resp.header("x-reason"), Some("upstream down"));
                assert_eq!(resp.body_text().await, "bad gateway");
            }
            ReqwestResponseError::Request(e) => unreachable!("{e:?}"),
        }
    }

    #[tokio::test]
    async fn status_code_error_into_response_escape_hatch() {
        // The raw `reqwest::Response` is retrievable for callers that opt in,
        // with full access to the underlying client's API.
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let _mock = server
            .mock("GET", "/raw")
            .with_status(503)
            .with_body("unavailable")
            .create_async()
            .await;

        let url = Url::parse(&format!("{}/raw", server.url())).unwrap();
        let (err, _) = client.prefetch(url).await.expect_err("503 must fail");
        let ReqwestResponseError::StatusCode(resp) = err else {
            unreachable!("expected a status code error")
        };
        let raw = resp.into_response();
        assert_eq!(raw.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(raw.text().await.unwrap(), "unavailable");
    }

    #[tokio::test]
    async fn status_code_error_caps_body() {
        // The body read is bounded even when the server sends more than
        // `MAX_ERROR_BODY` bytes.
        let mut server = mockito::Server::new_async().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let payload = vec![b'x'; MAX_ERROR_BODY + 4096];
        let _mock = server
            .mock("GET", "/big")
            .with_status(500)
            .with_body(payload)
            .create_async()
            .await;

        let url = Url::parse(&format!("{}/big", server.url())).unwrap();
        let (err, _) = client.prefetch(url).await.expect_err("500 must fail");
        match err {
            ReqwestResponseError::StatusCode(resp) => {
                assert_eq!(resp.body().await.len(), MAX_ERROR_BODY);
            }
            ReqwestResponseError::Request(e) => unreachable!("{e:?}"),
        }
    }

    #[tokio::test]
    async fn request_error_classifies_transport_failure() {
        // A dropped connection surfaces as `Request`, and the classification
        // helpers must report it without the caller naming reqwest.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                drop(stream);
            }
        });
        let client = Client::builder().no_proxy().build().unwrap();
        let url = Url::parse(&format!("http://{addr}/x")).unwrap();
        let (err, _) = client
            .prefetch(url)
            .await
            .expect_err("dropped conn must fail");
        match err {
            ReqwestResponseError::Request(e) => {
                assert!(!e.is_timeout());
                assert!(e.is_connect() || e.is_request());
                assert_eq!(e.status(), None);
            }
            ReqwestResponseError::StatusCode(resp) => unreachable!("{}", resp.status()),
        }
    }
}
