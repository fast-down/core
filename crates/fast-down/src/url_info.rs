use std::sync::Arc;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlInfo {
    pub size: u64,
    /// 服务器返回的原始文件名，必须清洗掉不合法字符才能安全使用
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
