use crate::{
    UrlInfo,
    http::{
        ContentDisposition, GetResponse, HttpClient, HttpError, HttpHeaders, HttpRequestBuilder,
        HttpResponse,
    },
    url_info::FileId,
};
use std::{borrow::Borrow, future::Future, time::Duration};
use url::Url;

pub type PrefetchResult<Client> =
    Result<(UrlInfo, GetResponse<Client>), (HttpError<Client>, Option<Duration>)>;

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
        .map(|s| {
            let s = urlencoding::decode_binary(s.as_bytes());
            String::from_utf8_lossy(&s).into_owned()
        })
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
        .or_else(|| url.domain().map(ToString::to_string))
        .unwrap_or_else(|| url.to_string())
}

async fn prefetch<Client: HttpClient>(client: &Client, url: Url) -> PrefetchResult<Client> {
    let (no_range_fut, range_fut) = (
        prefetch_no_range(client, url.clone()),
        is_support_range(client, url),
    );
    let (result_no_range, result_range) = tokio::join!(no_range_fut, range_fut);
    let mut res = result_no_range?;
    if let Ok(supports_range) = result_range
        && supports_range
    {
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
    let resp = client
        .get(url, None)
        .send()
        .await
        .map_err(|(e, d)| (HttpError::Request(e), d))?;
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
