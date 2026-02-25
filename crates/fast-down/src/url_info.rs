use std::sync::Arc;
use url::Url;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlInfo {
    pub size: u64,
    /// 服务器返回的原始文件名，必须清洗掉不合法字符才能安全使用
    #[cfg_attr(
        feature = "sanitize-filename",
        doc = "用 [`UrlInfo::filename()`] 方法处理文件名"
    )]
    #[cfg_attr(
        not(feature = "sanitize-filename"),
        doc = "开启 `sanitize-filename` 特性后，可使用 `filename()` 方法处理文件名。"
    )]
    pub raw_name: String,
    pub supports_range: bool,
    pub fast_download: bool,
    pub final_url: Url,
    pub file_id: FileId,
}

#[cfg(feature = "sanitize-filename")]
impl UrlInfo {
    #[must_use]
    pub fn filename(&self) -> String {
        sanitize_filename::sanitize_with_options(
            &self.raw_name,
            sanitize_filename::Options {
                windows: cfg!(windows),
                truncate: true,
                replacement: "_",
            },
        )
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
