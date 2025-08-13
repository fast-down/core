extern crate alloc;
use alloc::string::String;
use url::Url;

#[deprecated(
    since = "3.1.0",
    note = "`UrlInfo` is deprecated, and will be removed in 3.4.0, please use `UrlInfo` from `fast-down` crate instead"
)]
#[derive(Debug, Clone)]
pub struct UrlInfo {
    pub size: u64,
    pub name: String,
    pub supports_range: bool,
    pub fast_download: bool,
    pub final_url: Url,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
