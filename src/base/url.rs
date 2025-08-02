use url::Url;

#[derive(Debug, Clone)]
pub struct UrlInfo {
    pub size: u64,
    pub name: Option<String>,
    pub supports_range: bool,
    pub fast_download: bool,
    pub final_url: Url,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
