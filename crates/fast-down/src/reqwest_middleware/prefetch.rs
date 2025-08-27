use crate::{
    UrlInfo,
    reqwest::{get_file_size, get_filename, get_header_str},
};
use reqwest::{IntoUrl, StatusCode, Url, header};
use reqwest_middleware::ClientWithMiddleware;
use std::future::Future;

pub trait Prefetch {
    fn prefetch(
        &self,
        url: impl IntoUrl + Send,
    ) -> impl Future<Output = Result<UrlInfo, reqwest_middleware::Error>> + Send;
}

impl Prefetch for ClientWithMiddleware {
    async fn prefetch(
        &self,
        url: impl IntoUrl + Send,
    ) -> Result<UrlInfo, reqwest_middleware::Error> {
        prefetch(self, url).await
    }
}

async fn prefetch(
    client: &ClientWithMiddleware,
    url: impl IntoUrl,
) -> Result<UrlInfo, reqwest_middleware::Error> {
    let url = url.into_url()?;
    let resp = match client.head(url.clone()).send().await {
        Ok(resp) => resp,
        Err(_) => return prefetch_fallback(url, client).await,
    };
    log::debug!("Prefetch {url}: {resp:?}");
    let resp = match resp.error_for_status() {
        Ok(resp) => resp,
        Err(_) => return prefetch_fallback(url, client).await,
    };
    let supports_range = match resp.headers().get(header::ACCEPT_RANGES) {
        Some(accept_ranges) => accept_ranges
            .to_str()
            .ok()
            .map(|v| v.split(' '))
            .and_then(|supports| supports.into_iter().find(|&ty| ty == "bytes"))
            .is_some(),
        None => return prefetch_fallback(url, client).await,
    };
    let status = resp.status();
    let resp_headers = resp.headers();
    let size = get_file_size(resp_headers, &status);
    let final_url = resp.url();
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

async fn prefetch_fallback(
    url: Url,
    client: &ClientWithMiddleware,
) -> Result<UrlInfo, reqwest_middleware::Error> {
    let resp = client
        .get(url.clone())
        .header(header::RANGE, "bytes=0-")
        .send()
        .await?;
    log::debug!("Prefetch fallback {url}: {resp:?}");
    let resp = resp.error_for_status()?;
    let status = resp.status();
    let resp_headers = resp.headers();
    let size = get_file_size(resp_headers, &status);
    let supports_range = status == StatusCode::PARTIAL_CONTENT;
    let final_url = resp.url();
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

#[cfg(test)]
mod tests {
    use reqwest_middleware::ClientBuilder;

    use super::*;

    #[tokio::test]
    async fn test_redirect_and_content_range() {
        let mut server = mockito::Server::new_async().await;

        let mock_redirect = server
            .mock("GET", "/redirect")
            .with_status(301)
            .with_header("Location", "/real-file.txt")
            .create_async()
            .await;

        let mock_file = server
            .mock("GET", "/real-file.txt")
            .with_status(206)
            .with_header("Content-Range", "bytes 0-1023/2048")
            .with_body(vec![0; 1024])
            .create_async()
            .await;

        let client = ClientBuilder::new(reqwest::Client::new()).build();
        let url_info = client
            .prefetch(&format!("{}/redirect", server.url()))
            .await
            .expect("Request should succeed");

        assert_eq!(url_info.size, 2048);
        assert_eq!(url_info.name, "real-file.txt");
        assert_eq!(
            url_info.final_url.as_str(),
            format!("{}/real-file.txt", server.url())
        );
        assert!(url_info.supports_range);

        mock_redirect.assert_async().await;
        mock_file.assert_async().await;
    }

    #[tokio::test]
    async fn test_content_range_priority() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/file")
            .with_status(206)
            .with_header("Content-Range", "bytes 0-1023/2048")
            .create_async()
            .await;

        let client = ClientBuilder::new(reqwest::Client::new()).build();
        let url_info = client
            .prefetch(&format!("{}/file", server.url()))
            .await
            .expect("Request should succeed");

        assert_eq!(url_info.size, 2048);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_filename_sources() {
        let mut server = mockito::Server::new_async().await;

        // Test Content-Disposition source
        let mock1 = server
            .mock("GET", "/test1")
            .with_header("Content-Disposition", "attachment; filename=\"test.txt\"")
            .create_async()
            .await;
        let url_info = ClientBuilder::new(reqwest::Client::new())
            .build()
            .prefetch(&format!("{}/test1", server.url()))
            .await
            .unwrap();
        assert_eq!(url_info.name, "test.txt");
        mock1.assert_async().await;

        // Test URL path source
        let mock2 = server.mock("GET", "/test2/file.pdf").create_async().await;
        let url_info = ClientBuilder::new(reqwest::Client::new())
            .build()
            .prefetch(&format!("{}/test2/file.pdf", server.url()))
            .await
            .unwrap();
        assert_eq!(url_info.name, "file.pdf");
        mock2.assert_async().await;

        // Test sanitization
        let mock3 = server
      .mock("GET", "/test3")
      .with_header(
        "Content-Disposition",
        "attachment; filename*=UTF-8''%E6%82%AA%E3%81%84%3C%3E%E3%83%95%E3%82%A1%E3%82%A4%E3%83%AB%3F%E5%90%8D.txt",
      )
      .create_async()
      .await;
        let url_info = ClientBuilder::new(reqwest::Client::new())
            .build()
            .prefetch(&format!("{}/test3", server.url()))
            .await
            .unwrap();
        assert_eq!(url_info.name, "悪い<>ファイル?名.txt");
        mock3.assert_async().await;
    }

    #[tokio::test]
    async fn test_error_handling() {
        let mut server = mockito::Server::new_async().await;
        let mock1 = server
            .mock("GET", "/404")
            .with_status(404)
            .create_async()
            .await;

        let client = ClientBuilder::new(reqwest::Client::new()).build();
        match client.prefetch(&format!("{}/404", server.url())).await {
            Ok(info) => panic!("404 status code should not success: {info:?}"),
            Err(err) => {
                assert!(err.is_status(), "should be error about status code");
                assert_eq!(err.status(), Some(StatusCode::NOT_FOUND));
            }
        }

        mock1.assert_async().await;
    }
}
