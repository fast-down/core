use std::{error::Error, sync::mpsc, thread};

use content_disposition::parse_content_disposition;
use reqwest::{
    blocking::Client,
    header::{self, HeaderMap, HeaderValue},
    Proxy, StatusCode,
};

pub struct DownloadOptions<'a> {
    pub url: &'a str,
    pub save_folder: &'a str,
    pub threads: usize,
    pub file_name: Option<&'a str>,
    pub headers: Option<HeaderMap>,
    pub proxy: Option<&'a str>,
}

pub fn download(options: DownloadOptions) -> Result<String, Box<dyn Error>> {
    // 配置默认 Headers
    let client = Client::builder().default_headers(options.headers.unwrap_or(HeaderMap::new()));
    // 配置 Proxy
    if let Some(proxy) = options.proxy {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;

    // 获取 URL 信息
    let info = get_url_info(client, options.url)?;

    let (tx, rx) = mpsc::channel();

    for i in 0..8 {
        let tx = tx.clone();
        thread::spawn(move || {
            tx.send(i * 2).unwrap();
        });
    }

    for _ in 0..8 {
        println!("{}", rx.recv().unwrap());
    }
}

pub struct UrlInfo {
    pub file_size: usize,
    pub file_name: Option<String>,
    pub can_use_range: bool,
    pub final_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub fn get_url_info(client: Client, url: &str) -> Result<UrlInfo, Box<dyn Error>> {
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=0-"));
    let resp = client.get(url).headers(headers).send()?;
    let status = resp.status();
    if status.is_success() {
        let can_use_range = status == StatusCode::PARTIAL_CONTENT;
        let resp_headers = resp.headers();
        let file_size: usize = resp_headers
            .get(reqwest::header::CONTENT_LENGTH)
            .map_or(Ok("0"), |e| e.to_str())?
            .parse()?;
        let etag = resp_headers
            .get(reqwest::header::ETAG)
            .and_then(|e| e.to_str().ok())
            .map(|e| e.to_string());
        let file_name = resp_headers
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|e| e.to_str().ok())
            .and_then(|e| parse_content_disposition(e).filename_full());
        let last_modified = resp_headers
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|e| e.to_str().ok())
            .map(|e| e.to_string());
        return Ok(UrlInfo {
            file_name,
            file_size,
            can_use_range,
            etag,
            last_modified,
            final_url: resp.url().to_string(),
        });
    } else {
        return Err(format!(
            "Error: Failed to get URL info. status code: {}",
            status.as_str()
        )
        .into());
    }
}
