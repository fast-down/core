use std::sync::Arc;
use url::Url;

#[derive(Debug)]
pub struct UrlInfo {
    pub size: u64,
    pub name: String,
    pub supports_range: bool,
    pub fast_download: bool,
    pub final_url: Url,
    pub file_id: FileId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub fn empty() -> Self {
        Self {
            etag: None,
            last_modified: None,
        }
    }
}
