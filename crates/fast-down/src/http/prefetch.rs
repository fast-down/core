use crate::{
    UrlInfo,
    http::{GetResponse, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder, HttpResponse},
    url_info::FileId,
};
use content_disposition;
use std::{borrow::Borrow, future::Future};
use url::Url;

pub trait Prefetch<Client: HttpClient> {
    type Error;
    fn prefetch(
        &self,
        url: Url,
    ) -> impl Future<Output = Result<(UrlInfo, GetResponse<Client>), Self::Error>> + Send;
}

impl<Client: HttpClient, BorrowClient: Borrow<Client> + Sync> Prefetch<Client> for BorrowClient {
    type Error = HttpError<Client>;
    async fn prefetch(&self, url: Url) -> Result<(UrlInfo, GetResponse<Client>), Self::Error> {
        prefetch(self.borrow(), url).await
    }
}

fn get_filename(headers: &impl HttpHeaders, url: &Url) -> String {
    headers
        .get("content-disposition")
        .ok()
        .and_then(|s| content_disposition::parse_content_disposition(s).filename_full())
        .map(|s| urlencoding::decode(&s).map(String::from).unwrap_or(s))
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|s| urlencoding::decode(s).unwrap_or(s.into()))
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| url.to_string())
}

async fn prefetch<Client: HttpClient>(
    client: &Client,
    url: Url,
) -> Result<(UrlInfo, GetResponse<Client>), HttpError<Client>> {
    let resp = client
        .get(url, None)
        .send()
        .await
        .map_err(HttpError::Request)?;
    let headers = resp.headers();
    let supports_range = headers
        .get("accept-ranges")
        .map(|v| v == "bytes")
        .unwrap_or(false);
    let size = headers
        .get("content-length")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let final_url = resp.url();
    Ok((
        UrlInfo {
            final_url: final_url.clone(),
            name: get_filename(headers, final_url),
            size,
            supports_range,
            fast_download: size > 0 && supports_range,
            file_id: FileId::new(headers.get("etag").ok(), headers.get("last-modified").ok()),
        },
        resp,
    ))
}
