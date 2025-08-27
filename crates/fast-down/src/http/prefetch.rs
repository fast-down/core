use crate::{
    UrlInfo,
    http::{HttpClient, HttpError, HttpHeaders, HttpResponse},
};
use content_disposition;
use std::{future::Future, num::NonZeroU16};
use url::Url;

pub trait Prefetch {
    type Error;
    fn prefetch(&self, url: &str) -> impl Future<Output = Result<UrlInfo, Self::Error>> + Send;
}

impl<Client: HttpClient + Sync> Prefetch for Client {
    type Error = HttpError<Client>;
    async fn prefetch(&self, url: &str) -> Result<UrlInfo, Self::Error> {
        prefetch(self, url).await
    }
}

fn get_file_size(headers: &impl HttpHeaders, status: NonZeroU16) -> u64 {
    if status.get() == 206 {
        headers
            .get("content-range")
            .ok()
            .and_then(|s| s.rsplit('/').next())
            .and_then(|total| total.parse().ok())
            .unwrap_or(0)
    } else {
        headers
            .get("content-length")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
}

fn get_filename(headers: &impl HttpHeaders, final_url: &Url) -> String {
    headers
        .get("content-disposition")
        .ok()
        .and_then(|s| content_disposition::parse_content_disposition(s).filename_full())
        .map(|s| urlencoding::decode(&s).map(String::from).unwrap_or(s))
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            final_url
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|s| urlencoding::decode(s).unwrap_or(s.into()))
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| final_url.to_string())
}

async fn prefetch<Client: HttpClient>(
    client: &Client,
    url: &str,
) -> Result<UrlInfo, HttpError<Client>> {
    let resp = match client.head(url).await {
        Ok(resp) => resp,
        Err(_) => return prefetch_fallback(client, url).await,
    };
    let resp = match resp.error_for_status() {
        Ok(resp) => resp,
        Err(_) => return prefetch_fallback(client, url).await,
    };
    let supports_range = match resp.headers().get("accept-ranges") {
        Ok(accept_ranges) => accept_ranges.split(' ').any(|ty| ty == "bytes"),
        Err(_) => return prefetch_fallback(client, url).await,
    };
    let status = resp.status();
    let resp_headers = resp.headers();
    let size = get_file_size(resp_headers, status);
    let final_url = resp.url();
    Ok(UrlInfo {
        final_url: final_url.clone(),
        name: get_filename(resp_headers, final_url),
        size,
        supports_range,
        fast_download: size > 0 && supports_range,
        etag: resp_headers.get("etag").ok().map(|s| s.to_string()),
        last_modified: resp_headers
            .get("last-modified")
            .ok()
            .map(|s| s.to_string()),
    })
}

async fn prefetch_fallback<Client: HttpClient>(
    client: &Client,
    url: &str,
) -> Result<UrlInfo, HttpError<Client>> {
    let resp = client
        .get(
            url,
            Some([(b"range"[..].into(), b"bytes=0-"[..].into())].into()),
        )
        .await
        .map_err(HttpError::Request)?;
    let resp = resp.error_for_status().map_err(HttpError::Status)?;
    let status = resp.status();
    let resp_headers = resp.headers();
    let size = get_file_size(resp_headers, status);
    let supports_range = status.get() == 206;
    let final_url = resp.url();
    Ok(UrlInfo {
        final_url: final_url.clone(),
        name: get_filename(resp_headers, final_url),
        size,
        supports_range,
        fast_download: size > 0 && supports_range,
        etag: resp_headers.get("etag").ok().map(|s| s.to_string()),
        last_modified: resp_headers
            .get("last-modified")
            .ok()
            .map(|s| s.to_string()),
    })
}

// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[tokio::test]
//     async fn test_redirect_and_content_range() {
//         let mut server = mockito::Server::new_async().await;

//         let mock_redirect = server
//             .mock("GET", "/redirect")
//             .with_status(301)
//             .with_header("Location", "/real-file.txt")
//             .create_async()
//             .await;

//         let mock_file = server
//             .mock("GET", "/real-file.txt")
//             .with_status(206)
//             .with_header("Content-Range", "bytes 0-1023/2048")
//             .with_body(vec![0; 1024])
//             .create_async()
//             .await;

//         let client = Client::new();
//         let url_info = client
//             .prefetch(&format!("{}/redirect", server.url()))
//             .await
//             .expect("Request should succeed");

//         assert_eq!(url_info.size, 2048);
//         assert_eq!(url_info.name, "real-file.txt");
//         assert_eq!(
//             url_info.final_url.as_str(),
//             format!("{}/real-file.txt", server.url())
//         );
//         assert!(url_info.supports_range);

//         mock_redirect.assert_async().await;
//         mock_file.assert_async().await;
//     }

//     #[tokio::test]
//     async fn test_content_range_priority() {
//         let mut server = mockito::Server::new_async().await;
//         let mock = server
//             .mock("GET", "/file")
//             .with_status(206)
//             .with_header("Content-Range", "bytes 0-1023/2048")
//             .create_async()
//             .await;

//         let client = Client::new();
//         let url_info = client
//             .prefetch(&format!("{}/file", server.url()))
//             .await
//             .expect("Request should succeed");

//         assert_eq!(url_info.size, 2048);
//         mock.assert_async().await;
//     }

//     #[tokio::test]
//     async fn test_filename_sources() {
//         let mut server = mockito::Server::new_async().await;

//         // Test Content-Disposition source
//         let mock1 = server
//             .mock("GET", "/test1")
//             .with_header("Content-Disposition", "attachment; filename=\"test.txt\"")
//             .create_async()
//             .await;
//         let url_info = Client::new()
//             .prefetch(&format!("{}/test1", server.url()))
//             .await
//             .unwrap();
//         assert_eq!(url_info.name, "test.txt");
//         mock1.assert_async().await;

//         // Test URL path source
//         let mock2 = server.mock("GET", "/test2/file.pdf").create_async().await;
//         let url_info = Client::new()
//             .prefetch(&format!("{}/test2/file.pdf", server.url()))
//             .await
//             .unwrap();
//         assert_eq!(url_info.name, "file.pdf");
//         mock2.assert_async().await;

//         // Test sanitization
//         let mock3 = server
//       .mock("GET", "/test3")
//       .with_header(
//         "Content-Disposition",
//         "attachment; filename*=UTF-8''%E6%82%AA%E3%81%84%3C%3E%E3%83%95%E3%82%A1%E3%82%A4%E3%83%AB%3F%E5%90%8D.txt",
//       )
//       .create_async()
//       .await;
//         let url_info = Client::new()
//             .prefetch(&format!("{}/test3", server.url()))
//             .await
//             .unwrap();
//         assert_eq!(url_info.name, "悪い<>ファイル?名.txt");
//         mock3.assert_async().await;
//     }

//     #[tokio::test]
//     async fn test_error_handling() {
//         let mut server = mockito::Server::new_async().await;
//         let mock1 = server
//             .mock("GET", "/404")
//             .with_status(404)
//             .create_async()
//             .await;

//         let client = Client::new();

//         match client.prefetch(&format!("{}/404", server.url())).await {
//             Ok(info) => panic!("404 status code should not success: {info:?}"),
//             Err(err) => {
//                 assert!(err.is_status(), "should be error about status code");
//                 assert_eq!(err.status(), Some(StatusCode::NOT_FOUND));
//             }
//         }

//         mock1.assert_async().await;
//     }
// }
