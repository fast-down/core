use std::{error::Error, sync::mpsc, thread};

use reqwest::{
    blocking::Client,
    header::{self, HeaderMap},
    Proxy,
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
    pub content_type: String,
    pub file_name: Option<String>,
}

pub fn get_url_info(client: Client, url: &str) -> Result<UrlInfo, Box<dyn Error>> {
    let headers = HeaderMap::new();
    headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=0-"));
    let resp = client.get(url).headers(headers).send()?;
    if resp.status().is_success() {
        let headers = resp.headers()?;
    } else {
        return Err("Failed to get URL info".into());
    }
}
