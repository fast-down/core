use std::{error::Error, sync::mpsc, thread};

use reqwest::{
    blocking::Client,
    header::{self, HeaderMap},
};

pub struct DownloadOptions<'a> {
    pub url: &'a str,
    pub save_folder: &'a str,
    pub threads: usize,
    pub file_name: Option<&'a str>,
    pub headers: Option<HeaderMap>,
    pub proxy: Option<&'a str>,
}

pub fn download(options: DownloadOptions) {
    let info = get_url_info(GetUrlInfoOptions {
        url: options.url,
        headers: options.headers,
        proxy: options.proxy,
    });

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

pub struct GetUrlInfoOptions<'a> {
    pub url: &'a str,
    pub headers: Option<HeaderMap>,
    pub proxy: Option<&'a str>,
}

pub struct UrlInfo {
    pub file_size: usize,
    pub content_type: String,
    pub file_name: Option<String>,
}

pub fn get_url_info(options: GetUrlInfoOptions) -> Result<UrlInfo, Box<dyn Error>> {
    let client = Client::new();
    let headers = options.headers.unwrap_or(HeaderMap::new());
    headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=0-"));
    let response = client.get(options.url).headers(headers).send()?;
    if response.status().is_success() {
        let body = response.text()?;
        println!("Response: {}", body);
    } else {
        println!("Request failed with status: {}", response.status());
    }
}
