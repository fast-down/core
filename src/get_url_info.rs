extern crate sanitize_filename;
use content_disposition::parse_content_disposition;
use reqwest::{
    blocking::Client,
    header::{self, HeaderMap, HeaderValue},
    StatusCode,
};
use std::error::Error;

#[derive(Debug)]
#[allow(dead_code)]
pub struct UrlInfo {
    pub file_size: usize,
    pub file_name: String,
    pub can_use_range: bool,
    pub final_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub fn get_url_info(client: &Client, url: &str) -> Result<UrlInfo, Box<dyn Error>> {
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
    let resp = client.get(url).headers(headers).send()?;
    let status = resp.status();
    if status.is_success() {
        let resp_headers = resp.headers();
        let final_url = resp.url();
        Ok(UrlInfo {
            final_url: final_url.to_string(),
            can_use_range: status == StatusCode::PARTIAL_CONTENT,
            file_name: sanitize_filename::sanitize_with_options(
                match resp_headers
                    .get(header::CONTENT_DISPOSITION)
                    .and_then(|e| e.to_str().ok())
                    .and_then(|e| parse_content_disposition(e).filename_full())
                    .or_else(|| {
                        url.split('/')
                            .last()
                            .and_then(|s| urlencoding::decode(s).ok())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| url.to_string())
                {
                    s if s.trim().is_empty() => final_url.to_string(),
                    s => s,
                },
                sanitize_filename::Options {
                    windows: true,
                    truncate: true,
                    replacement: "_",
                },
            ),
            file_size: resp_headers
                .get(header::CONTENT_LENGTH)
                .and_then(|e| e.to_str().ok())
                .and_then(|e| e.parse().ok())
                .unwrap_or(0),
            etag: resp_headers
                .get(header::ETAG)
                .and_then(|e| e.to_str().ok())
                .map(|e| e.to_string()),
            last_modified: resp_headers
                .get(header::LAST_MODIFIED)
                .and_then(|e| e.to_str().ok())
                .map(|e| e.to_string()),
        })
    } else {
        Err(format!(
            "Error: Failed to get URL info. status code: {}",
            status.as_str()
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;
    use reqwest::blocking::Client;

    #[test]
    fn test_get_url_info() {
        let mut server = Server::new();
        let url = server.url();
        let mock = server
            .mock("GET", "/file")
            .with_status(206)
            .with_header("content-length", "1024")
            .with_header("Content-Range", "bytes 0-1023")
            .with_header(
                "Content-Disposition",
                "attachment; filename=\"tecongegssdf.sg..e../\\/\\/\\/\\\\/t.txt\"",
            )
            .with_header("ETag", "\"12345\"")
            .with_header("Last-Modified", "Wed, 21 Oct 2023 07:28:00 GMT")
            .with_header("Accept-Ranges", "bytes")
            .with_body([1; 1024])
            .create();

        let req_url = format!("{}/file", url);

        let client = Client::new();
        let url_info = get_url_info(&client, &req_url).unwrap();

        assert_eq!(url_info.file_size, 1024);
        assert_eq!(
            url_info.file_name,
            "tecongegssdf.sg..e..__________t.txt".to_string()
        );
        assert_eq!(url_info.etag, Some("\"12345\"".to_string()));
        assert_eq!(
            url_info.last_modified,
            Some("Wed, 21 Oct 2023 07:28:00 GMT".to_string())
        );
        assert!(url_info.can_use_range);
        assert_eq!(url_info.final_url, req_url);

        mock.assert();
    }

    #[test]
    fn test_get_url_info_with_path_filename() {
        let mut server = Server::new();
        let url = server.url();
        let mock = server
            .mock(
                "GET",
                "/%F0%9F%A4%A6%E2%80%8D%E2%99%80%EF%B8%8F%F0%9F%99%8C%F0%9F%98%92%F0%9F%A4%A3",
            )
            .with_status(206)
            .with_header("content-length", "1024")
            .with_header("Content-Range", "bytes 0-1023")
            .with_header("ETag", "\"12345\"")
            .with_header("Last-Modified", "Wed, 21 Oct 2023 07:28:00 GMT")
            .with_header("Accept-Ranges", "bytes")
            .with_body([1; 1024])
            .create();

        let req_url = format!(
            "{}/%F0%9F%A4%A6%E2%80%8D%E2%99%80%EF%B8%8F%F0%9F%99%8C%F0%9F%98%92%F0%9F%A4%A3",
            url
        );

        let client = Client::new();
        let url_info = get_url_info(&client, &req_url).unwrap();

        assert_eq!(url_info.file_size, 1024);
        assert_eq!(url_info.file_name, "🤦‍♀️🙌😒🤣".to_string());
        assert_eq!(url_info.etag, Some("\"12345\"".to_string()));
        assert_eq!(
            url_info.last_modified,
            Some("Wed, 21 Oct 2023 07:28:00 GMT".to_string())
        );
        assert!(url_info.can_use_range);
        assert_eq!(url_info.final_url, req_url);

        mock.assert();
    }

    #[test]
    fn test_get_url_info_with_url_filename() {
        let mut server = Server::new();
        let url = server.url();
        let mock = server
            .mock("GET", "/")
            .with_status(206)
            .with_header("content-length", "1024")
            .with_header("Content-Range", "bytes 0-1023")
            .with_header("ETag", "\"12345\"")
            .with_header("Last-Modified", "Wed, 21 Oct 2023 07:28:00 GMT")
            .with_header("Accept-Ranges", "bytes")
            .with_body([1; 1024])
            .create();

        let req_url = format!("{}/", url);

        let client = Client::new();
        let url_info = get_url_info(&client, &req_url).unwrap();

        assert_eq!(url_info.file_size, 1024);
        assert_eq!(
            url_info.file_name,
            sanitize_filename::sanitize_with_options(
                &req_url,
                sanitize_filename::Options {
                    windows: true,
                    truncate: true,
                    replacement: "_"
                }
            )
        );
        assert_eq!(url_info.etag, Some("\"12345\"".to_string()));
        assert_eq!(
            url_info.last_modified,
            Some("Wed, 21 Oct 2023 07:28:00 GMT".to_string())
        );
        assert!(url_info.can_use_range);
        assert_eq!(url_info.final_url, req_url);

        mock.assert();
    }

    #[test]
    #[should_panic = "Error: Failed to get URL info. status code: 404"]
    fn test_get_url_info_with_error() {
        let mut server = Server::new();
        let url = server.url();
        let mock = server.mock("GET", "/not_found").with_status(404).create();

        let client = Client::new();
        get_url_info(&client, &format!("{}/not_found", url)).unwrap();

        mock.assert();
    }
}
