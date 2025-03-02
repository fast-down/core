mod get_url_info;

use get_url_info::get_url_info;
use reqwest::{header::HeaderMap, Client, Proxy};
use std::{
    error::Error,
    path::{Path, PathBuf},
};
use tokio::{fs, io::AsyncWriteExt};

#[derive(Debug)]
pub struct DownloadInfo<'a> {
    pub file_size: usize,
    pub file_name: &'a String,
    pub file_path: &'a PathBuf,
}

pub struct DownloadOptions<'a> {
    pub url: &'a str,
    pub save_folder: &'a str,
    pub threads: usize,
    pub file_name: Option<&'a str>,
    pub headers: Option<HeaderMap>,
    pub proxy: Option<&'a str>,
    pub on_info: Option<Box<dyn FnOnce(&DownloadInfo)>>,
    pub on_progress: Option<Box<dyn FnMut(&DownloadProgress)>>,
}

#[derive(Debug)]
pub struct DownloadResult {
    pub file_name: String,
    pub file_size: usize,
    pub file_path: PathBuf,
}

#[derive(Debug)]
pub struct DownloadProgress {
    pub start_bytes: usize,
    pub end_bytes: usize,
}

pub async fn download<'a>(
    mut options: DownloadOptions<'a>,
) -> Result<DownloadResult, Box<dyn Error>> {
    // 配置默认 Headers
    let mut client = Client::builder().default_headers(options.headers.unwrap_or(HeaderMap::new()));
    // 配置 Proxy
    if let Some(proxy) = options.proxy {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;

    // 获取 URL 信息
    let info = get_url_info(&client, options.url).await?;
    // let can_fast_download = info.file_size > 0 && info.can_use_range;

    // 创建保存文件夹
    if let Err(e) = fs::create_dir_all(options.save_folder).await {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }

    // 创建文件
    let file_path =
        Path::new(options.save_folder).join(options.file_name.unwrap_or(&info.file_name));
    let mut file = fs::File::create(&file_path).await?;

    if let Some(on_info) = options.on_info {
        on_info(&DownloadInfo {
            file_size: info.file_size,
            file_name: &info.file_name,
            file_path: &file_path,
        });
    }

    let mut response = client.get(info.final_url).send().await.unwrap();
    let mut downloaded = 0usize;
    while let Some(bytes) = response.chunk().await.unwrap() {
        let len = bytes.len();
        file.write(&bytes).await.unwrap();
        if let Some(ref mut on_progress) = options.on_progress {
            on_progress(&DownloadProgress {
                start_bytes: downloaded,
                end_bytes: downloaded + len,
            })
        }
        downloaded += len;
    }
    file.flush().await.unwrap();
    Ok(DownloadResult {
        file_name: info.file_name,
        file_size: info.file_size,
        file_path,
    })
}
