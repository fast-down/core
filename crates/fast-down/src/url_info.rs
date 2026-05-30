use std::sync::Arc;
use url::Url;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlInfo {
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
    pub supports_range: bool,
    pub fast_download: bool,
    pub final_url: Url,
    pub file_id: FileId,
    pub content_type: Option<String>,
}

#[cfg(feature = "sanitize-filename")]
impl UrlInfo {
    #[must_use]
    pub fn filename(&self) -> String {
        path_helper::sanitize_filename(&self.raw_name, 255)
    }
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileId {
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
}

impl FileId {
    pub fn new(etag: Option<&str>, last_modified: Option<&str>) -> Self {
        Self {
            etag: etag.map(Arc::from),
            last_modified: last_modified.map(Arc::from),
        }
    }
}
