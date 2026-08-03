//! Prefetch metadata for a downloadable URL over HTTP.
//!
//! [`Prefetch::prefetch`] issues the initial GET (and a range probe) through a
//! [`crate::http::HttpClient`], then assembles a [`crate::UrlInfo`] describing
//! the resource: its size, suggested filename, content type, range support, and
//! the [`crate::FileId`] used for resumable downloads.

use crate::{
    UrlInfo,
    http::{
        ContentDisposition, GetRequestError, GetResponse, HttpClient, HttpError, HttpHeaders,
        HttpRequestBuilder, HttpResponse,
    },
    url_info::FileId,
};
use std::{borrow::Borrow, future::Future, time::Duration};
use url::Url;

/// Result of a prefetch operation: the metadata ([`UrlInfo`]) and the initial HTTP response.
pub type PrefetchResult<Client> =
    Result<(UrlInfo, GetResponse<Client>), (GetRequestError<Client>, Option<Duration>)>;

/// Trait for fetching resource metadata (size, filename, range support) from a URL.
///
/// Implementors perform a GET request to gather [`UrlInfo`] and
/// the initial response for subsequent downloading.
pub trait Prefetch<Client: HttpClient> {
    fn prefetch(&self, url: Url) -> impl Future<Output = PrefetchResult<Client>> + Send;
}

impl<Client, BorrowClient> Prefetch<Client> for BorrowClient
where
    Client: HttpClient,
    BorrowClient: Borrow<Client> + Sync,
{
    async fn prefetch(&self, url: Url) -> PrefetchResult<Client> {
        prefetch(self.borrow(), url).await
    }
}

fn get_filename(headers: &impl HttpHeaders, url: &Url) -> String {
    headers
        .get("content-disposition")
        .ok()
        .and_then(|s| ContentDisposition::parse(s.as_ref()).filename)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|s| {
                    let s = urlencoding::decode_binary(s.as_bytes());
                    String::from_utf8_lossy(&s).into_owned()
                })
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| url.host_str().map(|s| s.replace('.', "_")))
        .unwrap_or_else(|| url.to_string().replace('.', "_"))
}

async fn prefetch<Client: HttpClient>(client: &Client, url: Url) -> PrefetchResult<Client> {
    let (no_range_fut, range_fut) = (
        prefetch_no_range(client, url.clone()),
        is_support_range(client, url),
    );
    let (result_no_range, result_range) = tokio::join!(no_range_fut, range_fut);
    let mut res = result_no_range?;
    if matches!(result_range, Ok(true)) {
        res.0.supports_range = true;
        if res.0.size != 0 {
            res.0.fast_download = true;
        }
    }
    Ok(res)
}

async fn prefetch_no_range<Client: HttpClient>(
    client: &Client,
    url: Url,
) -> PrefetchResult<Client> {
    let resp = client.get(url, None).send().await?;
    let headers = resp.headers();
    let size = headers
        .get("content-length")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let final_url = resp.url();
    Ok((
        UrlInfo {
            final_url: final_url.clone(),
            raw_name: get_filename(headers, final_url),
            size,
            supports_range: false,
            fast_download: false,
            file_id: FileId::new(
                headers.get("etag").ok().as_deref(),
                headers.get("last-modified").ok().as_deref(),
            ),
            content_type: headers.get("content-type").ok().map(String::from),
        },
        resp,
    ))
}

async fn is_support_range<Client: HttpClient>(
    client: &Client,
    url: Url,
) -> Result<bool, (HttpError<Client>, Option<Duration>)> {
    let resp = client
        .get(url, Some(0..1))
        .send()
        .await
        .map_err(|(e, d)| (HttpError::Request(e), d))?;
    let headers = resp.headers();
    let supports_range = headers
        .get("content-range")
        .is_ok_and(|v| v.trim_start().to_lowercase().starts_with("bytes 0-0/"));
    Ok(supports_range)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::borrow::Cow;
    use std::collections::HashMap;

    struct MapHeaders(HashMap<String, String>);
    impl HttpHeaders for MapHeaders {
        type GetHeaderError = std::convert::Infallible;
        fn get(&self, header: &str) -> Result<Cow<'_, str>, Self::GetHeaderError> {
            Ok(Cow::Borrowed(self.0.get(header).map_or("", String::as_str)))
        }
    }

    #[test]
    fn get_filename_filename_star_not_double_decoded() {
        // Hypothesis A: `ContentDisposition::parse` already percent-decodes
        // `filename*` ("%25100" -> "%100"). `get_filename` then calls
        // `urlencoding::decode_binary` on the decoded string, double-decoding
        // it. A server that intends the literal filename "%100" (sent as
        // `filename*=UTF-8''%25100`) must survive unchanged.
        let mut h = HashMap::new();
        h.insert(
            "content-disposition".to_string(),
            "attachment; filename*=UTF-8''%25100".to_string(),
        );
        let headers = MapHeaders(h);
        let url = Url::parse("http://example.com/").unwrap();
        assert_eq!(get_filename(&headers, &url), "%100");
    }

    #[test]
    fn get_filename_filename_star_no_percent_passthrough() {
        // Control: when the decoded `filename*` contains no literal '%' byte,
        // `decode_binary` leaves it untouched, so a normal UTF-8 name is correct.
        let mut h = HashMap::new();
        h.insert(
            "content-disposition".to_string(),
            "attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.txt".to_string(),
        );
        let headers = MapHeaders(h);
        let url = Url::parse("http://example.com/").unwrap();
        assert_eq!(get_filename(&headers, &url), "测试.txt");
    }

    #[test]
    fn get_filename_plain_filename_percent_stays_literal() {
        // Plain `filename` is no longer percent-decoded (RFC 6266: quoted-string
        // is literal). "%E6%B5%8B.txt" stays as-is, unlike the old behavior.
        let mut h = HashMap::new();
        h.insert(
            "content-disposition".to_string(),
            "attachment; filename=\"%E6%B5%8B.txt\"".to_string(),
        );
        let headers = MapHeaders(h);
        let url = Url::parse("http://example.com/").unwrap();
        assert_eq!(get_filename(&headers, &url), "%E6%B5%8B.txt");
    }

    #[test]
    fn get_filename_url_path_fallback_decodes_percent() {
        // No content-disposition → falls back to the last URL path segment,
        // which IS percent-decoded (URL paths are always encoded).
        let headers = MapHeaders(HashMap::new());
        let url = Url::parse("http://example.com/dir/%E6%B5%8B%E8%AF%95.txt").unwrap();
        assert_eq!(get_filename(&headers, &url), "测试.txt");
    }

    #[test]
    fn get_filename_url_path_invalid_utf8_uses_replacement() {
        // %FF is not valid UTF-8; from_utf8_lossy produces U+FFFD.
        let headers = MapHeaders(HashMap::new());
        let url = Url::parse("http://example.com/%FF.txt").unwrap();
        assert_eq!(get_filename(&headers, &url), "\u{FFFD}.txt");
    }

    #[test]
    fn get_filename_whitespace_cd_falls_through_to_url() {
        // Whitespace-only filename from CD is filtered out → URL path is used.
        let mut h = HashMap::new();
        h.insert(
            "content-disposition".to_string(),
            "attachment; filename=\"   \"".to_string(),
        );
        let headers = MapHeaders(h);
        let url = Url::parse("http://example.com/report.pdf").unwrap();
        assert_eq!(get_filename(&headers, &url), "report.pdf");
    }

    #[test]
    fn get_filename_empty_path_falls_back_to_host() {
        // Root URL (empty path segment) → host with dots replaced by underscores.
        let headers = MapHeaders(HashMap::new());
        let url = Url::parse("http://cdn.example.com/").unwrap();
        assert_eq!(get_filename(&headers, &url), "cdn_example_com");
    }

    #[test]
    fn get_filename_path_segment_spaces_fall_back_to_host() {
        // A path segment that is only spaces decodes to whitespace → filtered →
        // host fallback.
        let headers = MapHeaders(HashMap::new());
        let url = Url::parse("http://cdn.example.com/%20%20").unwrap();
        assert_eq!(get_filename(&headers, &url), "cdn_example_com");
    }
}
