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
use fast_pull::{ProgressEntry, PullResult, PullerError};
use std::{borrow::Cow, fmt::Debug, future::Future, time::Duration};
use url::Url;

/// Abstraction over an HTTP client that can send GET requests with optional byte-range headers.
///
/// Implementations must return the response body **undecoded**: the downloader
/// writes body bytes at the absolute offsets it asks for, so a body that went
/// through a content coding is not the resource's bytes. Every request asks for
/// `Accept-Encoding: identity`, but a server may ignore that and a backend may
/// decode anyway, so [`HttpPuller`] and the prefetch check the response's
/// `Content-Encoding` and report
/// [`HttpError::ContentEncoding`]
/// instead of streaming it.
pub trait HttpClient: Clone + Send + Sync + Unpin + 'static {
    /// The request builder this client produces.
    type RequestBuilder: HttpRequestBuilder;
    /// Start a GET request, optionally restricted to `range`.
    fn get(&self, url: Url, range: Option<ProgressEntry>) -> Self::RequestBuilder;
}
/// Abstraction over an HTTP request builder that can be sent to produce a response.
pub trait HttpRequestBuilder {
    /// The response type produced on success.
    type Response: HttpResponse;
    /// The error type for a failed send.
    type RequestError: std::error::Error + Send + Sync + Unpin;
    /// Send the request, producing a response or a retryable error.
    fn send(self) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send;
}
/// Abstraction over an HTTP response that provides headers, final URL, and chunked body reading.
pub trait HttpResponse: Send + Sync + Debug + Unpin {
    /// The response's header map type.
    type Headers: HttpHeaders;
    /// The error type produced while reading a body chunk.
    type ChunkError: std::error::Error + Send + Sync + Unpin;
    /// The response headers.
    fn headers(&self) -> &Self::Headers;
    /// The URL the response was served from, after redirects.
    fn url(&self) -> &Url;
    /// Read the next body chunk, or `None` at end of body.
    fn chunk(&mut self) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send;
}
/// Abstraction over HTTP response headers, providing typed get-by-name access.
pub trait HttpHeaders {
    /// The error type for a failed header lookup.
    type GetHeaderError: std::error::Error + Send + Sync + Unpin;
    /// Get a header value by name.
    ///
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

/// A byte-range `Content-Range` header value (RFC 9110 §14.4).
///
/// The downloader and the prefetch probe ask different questions about the same
/// header, so they share this one parser: the downloader needs the body's first
/// byte, the probe needs to know whether the answer is the `bytes 0-0/<size>` it
/// asked for. One parser keeps the two answers consistent, so any value the
/// downloader reads an offset from also counts as range support.
///
/// The range unit is matched case-insensitively and whitespace is tolerated
/// around both the unit and the byte offsets, because servers in the wild send
/// `Bytes 0-0/1024`, `bytes  0-0/1024`, and `bytes 0-0 / 1024`.
///
/// The complete length after the slash is kept for both forms. It is the only
/// size a chunked `200` leaves behind: with no `Content-Length` to read, the
/// `bytes 0-0/<size>` answer to the probe is where the size comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentRange {
    /// The first byte offset of the enclosed range; `None` for the unsatisfied
    /// form `bytes */<size>`.
    first: Option<u64>,
    /// The last byte offset of the enclosed range, together with `first`.
    last: Option<u64>,
    /// The complete length the range is a part of, when the value names one;
    /// `None` for `bytes 0-0/*` or a value with no `/` part.
    complete: Option<u64>,
}

impl ContentRange {
    /// Parse `raw`. `None` means "not a byte range at all": a different or
    /// missing range unit, or a value with no `first-last` part.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        // The unit is the leading run of ASCII letters. Finding it by character
        // instead of by a fixed byte index keeps every slice on a UTF-8
        // boundary, so a multi-byte header value can never panic the parser.
        let unit_end = trimmed
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(trimmed.len());
        if !trimmed[..unit_end].eq_ignore_ascii_case("bytes") {
            return None;
        }
        let rest = trimmed[unit_end..].trim_start();
        // The complete length is optional: a truncated value still yields the
        // offsets it does carry, and `*` names no length at all.
        let (range, complete) = rest.split_once('/').unwrap_or((rest, ""));
        let complete = complete.trim().parse().ok();
        // `bytes */<size>` answers an unsatisfiable range: a valid value, but
        // it describes no body offset.
        if range.trim() == "*" {
            return Some(Self {
                first: None,
                last: None,
                complete,
            });
        }
        let (first, last) = range.split_once('-')?;
        Some(Self {
            first: Some(first.trim().parse().ok()?),
            last: Some(last.trim().parse().ok()?),
            complete,
        })
    }

    /// The first byte offset the response body starts at.
    pub(crate) const fn first_byte(self) -> Option<u64> {
        self.first
    }

    /// The number of bytes this range encloses: `last - first + 1`, because
    /// `last` is inclusive.
    ///
    /// `None` for a value that names no body (`bytes */<size>`) and for one whose
    /// end precedes its start — that encloses no byte, and the subtraction must
    /// not wrap.
    pub(crate) fn span_len(self) -> Option<u64> {
        let last = self.last?;
        let first = self.first?;
        last.checked_sub(first)?.checked_add(1)
    }

    /// The complete length of the resource this range belongs to, when the
    /// header names one.
    pub(crate) const fn complete_length(self) -> Option<u64> {
        self.complete
    }

    /// Whether this is the `bytes 0-0/<size>` answer to a `Range: bytes=0-0`
    /// probe: a `bytes` range enclosing exactly byte 0.
    pub(crate) const fn is_zero_probe(self) -> bool {
        matches!((self.first, self.last), (Some(0), Some(0)))
    }
}

/// The content coding a response declares, when it changes the body's bytes.
///
/// `None` when the header is missing, empty, or `identity` (case-insensitive):
/// all three say the bytes on the wire are the resource's own. Any other value
/// (including a comma-separated list) is returned so the caller can report it —
/// the downloader writes body bytes at the absolute offsets it asked for, which
/// a coded representation does not line up with.
pub(crate) fn content_encoding(headers: &impl HttpHeaders) -> Option<String> {
    let raw = headers.get("content-encoding").ok()?;
    let encoding = raw.trim();
    if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        return None;
    }
    Some(encoding.to_string())
}

/// Errors that can occur during HTTP download operations.
///
/// Maps to the various stages of an HTTP request: building, streaming chunks,
/// detecting mismatched file identity, and irrecoverable failures.
///
/// `#[non_exhaustive]`: new ways for a response to be wrong get their own
/// variant, and a caller outside this crate must not fail to compile because of
/// one it has never seen.
#[derive(thiserror::Error)]
#[non_exhaustive]
pub enum HttpError<Client: HttpClient> {
    /// The HTTP request failed to send.
    #[error("HTTP request failed: {0}")]
    Request(#[source] GetRequestError<Client>),
    /// The request outlived the layer's own time budget without being answered.
    ///
    /// The backend may have timeouts of its own; this one bounds a request whose
    /// connection was accepted and then left silent. It surfaces as an ordinary
    /// failure, so a caller's retries apply exactly as they do to a reset.
    #[error("the HTTP request did not answer within {0:?}")]
    Timeout(Duration),
    /// Reading a response body chunk failed.
    #[error("HTTP chunk read failed: {0}\n  response: {1:?}")]
    Chunk(#[source] GetChunkError<Client>, GetResponse<Client>),
    /// A pull failure the engine must not retry.
    #[error("irrecoverable pull error")]
    Irrecoverable,
    /// The response's file identity did not match the expected one.
    #[error("body mismatch: expected file {0:?}, got different content\n  response: {1:?}")]
    MismatchedBody(FileId, GetResponse<Client>),
    /// The server answered a ranged request with a body that does not start at
    /// the requested offset — typically a whole-file `200` reply to a `Range`
    /// request. Writing it where the caller asked for would corrupt the output.
    #[error(
        "server ignored the range request: expected a body starting at byte {start}\n  response: {resp:?}"
    )]
    RangeIgnored {
        /// The offset the request asked for.
        start: u64,
        /// The response that did not honour it.
        resp: GetResponse<Client>,
    },
    /// The response body is longer than the span its `Content-Range` announced.
    /// The excess belongs to no offset the caller asked for, so the body cannot
    /// be streamed as the requested range.
    #[error(
        "body over-delivery: the response announced {expected} bytes but delivered more\n  response: {resp:?}"
    )]
    OverDelivery {
        /// The number of bytes the response's `Content-Range` named.
        expected: u64,
        /// The response that kept sending past it.
        resp: GetResponse<Client>,
    },
    /// The response body is not the resource's own bytes: it carries a
    /// `Content-Encoding` other than `identity`.
    ///
    /// The downloader writes body bytes at the absolute offsets named by the
    /// range it asked for, so a coded body cannot be used at all — its bytes
    /// are a representation, not the resource's bytes at those offsets.
    /// `identity` is requested on every request, but a server may ignore it, and
    /// the `reqwest` backend decodes a coded body regardless of what was asked
    /// for when its decompression features are enabled.
    #[error(
        "the response body is `{encoding}`-encoded, not the resource's bytes\n  response: {resp:?}"
    )]
    ContentEncoding {
        /// The `Content-Encoding` value the response declared.
        encoding: String,
        /// The response that carried it.
        resp: GetResponse<Client>,
    },
}

impl<Client: HttpClient> std::fmt::Debug for HttpError<Client> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(e) => f.debug_tuple("Request").field(e).finish(),
            Self::Timeout(timeout) => f.debug_tuple("Timeout").field(timeout).finish(),
            Self::Chunk(e, r) => f.debug_tuple("Chunk").field(e).field(r).finish(),
            Self::Irrecoverable => f.write_str("Irrecoverable"),
            Self::MismatchedBody(id, r) => {
                f.debug_tuple("MismatchedBody").field(id).field(r).finish()
            }
            Self::RangeIgnored { start, resp } => f
                .debug_struct("RangeIgnored")
                .field("start", start)
                .field("resp", resp)
                .finish(),
            Self::OverDelivery { expected, resp } => f
                .debug_struct("OverDelivery")
                .field("expected", expected)
                .field("resp", resp)
                .finish(),
            Self::ContentEncoding { encoding, resp } => f
                .debug_struct("ContentEncoding")
                .field("encoding", encoding)
                .field("resp", resp)
                .finish(),
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
    use super::{
        ContentRange, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder, HttpResponse,
        prefetch::zero_range_answer,
    };
    use crate::url_info::FileId;
    use bytes::Bytes;
    use fast_pull::{PullResult, PullerError};
    use std::{borrow::Cow, future::Future};
    use url::Url;

    /// A header map holding a single `content-range` value; every other header
    /// reads as absent.
    struct RangeHeader(&'static str);
    impl HttpHeaders for RangeHeader {
        type GetHeaderError = std::convert::Infallible;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            Ok(Cow::Borrowed(if header == "content-range" {
                self.0
            } else {
                ""
            }))
        }
    }

    /// The shared parser's own contract, independent of either call site.
    #[test]
    fn content_range_parser_reads_units_and_offsets() {
        for raw in [
            "bytes 0-0/1024",
            "Bytes 0-0/1024",
            "bytes  0-0/1024",
            "bytes\t0-0/1024",
            " bytes 0-0/1024 ",
            "bytes 0 - 0 / 1024",
            "bytes0-0/1024",
        ] {
            let parsed = ContentRange::parse(raw).unwrap();
            assert_eq!(parsed.first_byte(), Some(0), "start offset of {raw:?}");
            assert!(parsed.is_zero_probe(), "{raw:?} encloses byte 0 only");
        }

        // The complete length is not part of either answer, but it must survive
        // parsing: it is the size a chunked metadata response otherwise lacks.
        let ranged = ContentRange::parse("bytes 5-9/20").unwrap();
        assert_eq!(ranged.first_byte(), Some(5));
        assert_eq!(ranged.complete_length(), Some(20));
        assert!(!ranged.is_zero_probe());

        // The unsatisfied form is a valid value that names no body offset.
        let unsatisfied = ContentRange::parse("bytes */1024").unwrap();
        assert_eq!(unsatisfied.first_byte(), None);
        assert_eq!(unsatisfied.complete_length(), Some(1024));
        assert!(!unsatisfied.is_zero_probe());

        // A wildcard or absent length parses as "unknown", never as 0.
        assert_eq!(
            ContentRange::parse("bytes 0-0/*")
                .unwrap()
                .complete_length(),
            None
        );
        assert_eq!(
            ContentRange::parse("bytes 0-0").unwrap().complete_length(),
            None
        );

        // A different unit is not a byte range at all.
        assert_eq!(ContentRange::parse("items 0-0/1024"), None);
        // Nor is a value with no range part.
        assert_eq!(ContentRange::parse("bytes"), None);
        assert_eq!(ContentRange::parse(""), None);

        // Multi-byte UTF-8 must be rejected, never sliced mid-character.
        assert_eq!(ContentRange::parse("字节 0-0/1024"), None);
        assert_eq!(ContentRange::parse("bytes 😀"), None);
    }

    /// Both call sites read the same header for the same server, so for every
    /// value they must agree on whether a `bytes 0-0` probe was answered: the
    /// probe is true exactly when the downloader sees an offset of 0.
    ///
    /// The bug this guards: the probe used to require the literal prefix
    /// `bytes 0-0/`, so `bytes  0-0/1024` was range-capable for the downloader
    /// and not for the probe, and a large file silently degraded to one stream.
    #[test]
    fn both_content_range_call_sites_agree() {
        let cases: &[(&str, Option<u64>)] = &[
            ("bytes 0-0/1024", Some(0)),
            ("Bytes 0-0/1024", Some(0)),
            ("bytes  0-0/1024", Some(0)),
            ("bytes\t0-0/1024", Some(0)),
            (" bytes 0-0/1024 ", Some(0)),
            ("bytes */1024", None),
            ("bytes 5-9/20", Some(5)),
            ("items 0-0/1024", None),
            ("", None),
            ("字节 0-0/1024", None),
        ];
        for &(raw, expected_start) in cases {
            let headers = RangeHeader(raw);
            let start = ContentRange::parse(raw).and_then(ContentRange::first_byte);
            assert_eq!(start, expected_start, "downloader's offset for {raw:?}");
            assert_eq!(
                zero_range_answer(&headers).is_some(),
                expected_start == Some(0),
                "the probe must accept exactly the 0-0 values the downloader accepts, for {raw:?}"
            );
        }
    }

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
        ) -> impl Future<Output = PullResult<Self::Response, Self::RequestError>> + Send {
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
        fn chunk(
            &mut self,
        ) -> impl Future<Output = Result<Option<Bytes>, Self::ChunkError>> + Send {
            std::future::ready(Ok(None))
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

        let timeout = HttpError::<MockClient>::Timeout(std::time::Duration::from_secs(30));
        assert!(format!("{timeout:?}").contains("Timeout"));

        let chunk = HttpError::<MockClient>::Chunk(MockErr, MockResponse::new());
        assert!(format!("{chunk:?}").contains("Chunk"));

        let irrecoverable = HttpError::<MockClient>::Irrecoverable;
        assert_eq!(format!("{irrecoverable:?}"), "Irrecoverable");

        let mismatched = HttpError::<MockClient>::MismatchedBody(
            FileId::new(Some("etag-x"), None),
            MockResponse::new(),
        );
        assert!(format!("{mismatched:?}").contains("MismatchedBody"));

        let range_ignored = HttpError::<MockClient>::RangeIgnored {
            start: 5,
            resp: MockResponse::new(),
        };
        assert!(format!("{range_ignored:?}").contains("RangeIgnored"));

        let over_delivery = HttpError::<MockClient>::OverDelivery {
            expected: 50,
            resp: MockResponse::new(),
        };
        assert!(format!("{over_delivery:?}").contains("OverDelivery"));
    }

    #[test]
    fn request_error_exposes_the_backend_error_as_source() {
        // `fast-down-api` walks the error chain to report the underlying cause;
        // an `HttpError::Request` with no `source()` truncates that chain to a
        // bare "HTTP request failed", losing what the backend actually said.
        let error = HttpError::<MockClient>::Request(MockErr);
        assert!(
            std::error::Error::source(&error).is_some(),
            "the request error must be reachable through std::error::Error::source"
        );
    }

    #[test]
    fn is_irrecoverable_only_for_variant() {
        assert!(HttpError::<MockClient>::Irrecoverable.is_irrecoverable());
        assert!(!HttpError::<MockClient>::Request(MockErr).is_irrecoverable());
        assert!(
            !HttpError::<MockClient>::Timeout(std::time::Duration::from_secs(30))
                .is_irrecoverable(),
            "a timed-out attempt is retried like a reset"
        );
        assert!(!HttpError::<MockClient>::Chunk(MockErr, MockResponse::new()).is_irrecoverable());
        assert!(
            !HttpError::<MockClient>::MismatchedBody(
                FileId::new(Some("x"), None),
                MockResponse::new()
            )
            .is_irrecoverable()
        );
        assert!(
            !HttpError::<MockClient>::RangeIgnored {
                start: 5,
                resp: MockResponse::new()
            }
            .is_irrecoverable(),
            "a range-ignoring server may answer correctly on the retry"
        );
        assert!(
            !HttpError::<MockClient>::OverDelivery {
                expected: 50,
                resp: MockResponse::new()
            }
            .is_irrecoverable(),
            "an over-delivering server may answer the retry with the right span"
        );
    }
}
