#![allow(deprecated)]

extern crate alloc;
use super::ReqwestPuller;
use crate::base::url::UrlInfo;
use alloc::string::{String, ToString};
use content_disposition;
use reqwest::{
    Client, IntoUrl, StatusCode, Url,
    header::{self, HeaderMap},
};
use sanitize_filename;

fn get_file_size(headers: &HeaderMap, status: &StatusCode) -> u64 {
    if *status == StatusCode::PARTIAL_CONTENT {
        headers
            .get(header::CONTENT_RANGE)
            .and_then(|hv| hv.to_str().ok())
            .and_then(|s| s.rsplit('/').next())
            .and_then(|total| total.parse().ok())
            .unwrap_or(0)
    } else {
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|hv| hv.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
}

fn get_header_str(headers: &HeaderMap, header_name: &header::HeaderName) -> Option<String> {
    headers
        .get(header_name)
        .and_then(|hv| hv.to_str().ok())
        .map(String::from)
}

fn get_filename(headers: &HeaderMap, final_url: &Url) -> String {
    let from_disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|hv| hv.to_str().ok())
        .and_then(|s| content_disposition::parse_content_disposition(s).filename_full())
        .filter(|s| !s.trim().is_empty());

    let from_url = final_url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|s| urlencoding::decode(s).ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    let raw_name = from_disposition
        .or(from_url)
        .unwrap_or_else(|| final_url.to_string());

    sanitize_filename::sanitize_with_options(
        &raw_name,
        sanitize_filename::Options {
            windows: true,
            truncate: true,
            replacement: "_",
        },
    )
}

#[deprecated(
    since = "3.1.0",
    note = "`Prefetch` is deprecated, and will be removed in 3.4.0, please use alternative from `fast-down` crate instead"
)]
pub trait Prefetch {
    fn prefetch(
        &self,
        url: impl IntoUrl + Send,
    ) -> impl Future<Output = Result<UrlInfo, reqwest::Error>> + Send;
}

impl Prefetch for Client {
    async fn prefetch(&self, url: impl IntoUrl + Send) -> Result<UrlInfo, reqwest::Error> {
        let url = url.into_url()?;
        let resp = match self.head(url.clone()).send().await {
            Ok(resp) => resp,
            Err(_) => return prefetch_fallback(url, self).await,
        };
        let resp = match resp.error_for_status() {
            Ok(resp) => resp,
            Err(_) => return prefetch_fallback(url, self).await,
        };

        let status = resp.status();
        let final_url = resp.url();

        let resp_headers = resp.headers();
        let size = get_file_size(resp_headers, &status);

        let supports_range = match resp.headers().get(header::ACCEPT_RANGES) {
            Some(accept_ranges) => accept_ranges
                .to_str()
                .ok()
                .map(|v| v.split(' '))
                .and_then(|supports| supports.into_iter().find(|&ty| ty == "bytes"))
                .is_some(),
            None => return prefetch_fallback(url, self).await,
        };

        Ok(UrlInfo {
            final_url: final_url.clone(),
            name: get_filename(resp_headers, final_url),
            size,
            supports_range,
            fast_download: size > 0 && supports_range,
            etag: get_header_str(resp_headers, &header::ETAG),
            last_modified: get_header_str(resp_headers, &header::LAST_MODIFIED),
        })
    }
}

impl Prefetch for ReqwestPuller {
    fn prefetch(
        &self,
        url: impl IntoUrl + Send,
    ) -> impl Future<Output = Result<UrlInfo, reqwest::Error>> + Send {
        self.client.prefetch(url)
    }
}

async fn prefetch_fallback(url: Url, client: &Client) -> Result<UrlInfo, reqwest::Error> {
    let resp = client
        .get(url)
        .header(header::RANGE, "bytes=0-")
        .send()
        .await?
        .error_for_status()?;
    let status = resp.status();
    let final_url = resp.url();

    let resp_headers = resp.headers();
    let size = get_file_size(resp_headers, &status);
    let supports_range = status == StatusCode::PARTIAL_CONTENT;
    Ok(UrlInfo {
        final_url: final_url.clone(),
        name: get_filename(resp_headers, final_url),
        size,
        supports_range,
        fast_download: size > 0 && supports_range,
        etag: get_header_str(resp_headers, &header::ETAG),
        last_modified: get_header_str(resp_headers, &header::LAST_MODIFIED),
    })
}
