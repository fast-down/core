//! Resolution of downloadable-resource metadata from an initial HTTP request.
//!
//! After a `FastDownPuller` performs a prefetch, it produces a
//! [`UrlInfo`] describing the resource (size, name, content type, range support)
//! together with a [`FileId`] (derived from the `ETag` / `Last-Modified`
//! headers) used to detect when a previously-downloaded file is still valid for
//! incremental/resumable downloads.

use std::sync::Arc;
use url::Url;

/// Metadata about a downloadable resource, gathered from the initial HTTP request.
///
/// Includes file size, filename, content type, range support, and file identity
/// (`ETag` / `Last-Modified`) for incremental downloads.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlInfo {
    /// Total size of the resource in bytes, taken from the `Content-Length` header (0 if absent).
    pub size: u64,
    /// Raw filename returned by the server. Sanitize invalid characters before using it safely.
    #[cfg_attr(
        feature = "sanitize-filename",
        doc = "Use the [`UrlInfo::filename()`] method to sanitize the filename"
    )]
    #[cfg_attr(
        not(feature = "sanitize-filename"),
        doc = "Enable the `sanitize-filename` feature to use the `filename()` method for sanitization."
    )]
    pub raw_name: String,
    /// Whether the server supports HTTP range requests (used to split the download into concurrent chunks).
    pub supports_range: bool,
    /// Whether the resource can be downloaded with the optimized fast (multi-chunk) path.
    pub fast_download: bool,
    /// The URL the response was actually served from, after any redirects.
    pub final_url: Url,
    /// Stable identity of the file, used to validate incremental/resumable downloads.
    pub file_id: FileId,
    /// The `Content-Type` header value, if the server provided one.
    pub content_type: Option<String>,
}

#[cfg(feature = "sanitize-filename")]
impl UrlInfo {
    #[must_use]
    pub fn filename(&self) -> String {
        path_helper::sanitize_filename(&self.raw_name, 255)
    }
}

/// File identity used for incremental and resumable downloads.
///
/// Combines the `ETag` and `Last-Modified` headers into a stable identifier.
/// Two downloads of the same logical file share a [`FileId`] only if neither
/// header changed, which lets the engine resume instead of re-downloading.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileId {
    /// The `ETag` header value, if present.
    pub etag: Option<Arc<str>>,
    /// The `Last-Modified` header value, if present.
    pub last_modified: Option<Arc<str>>,
}

impl FileId {
    /// Build a [`FileId`] from borrowed header values, copying them into `Arc<str>`.
    pub fn new(etag: Option<&str>, last_modified: Option<&str>) -> Self {
        Self {
            etag: etag.map(Arc::from),
            last_modified: last_modified.map(Arc::from),
        }
    }
}
