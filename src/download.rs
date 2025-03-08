use crate::{download_progress::DownloadProgress, get_chunks, get_url_info};
use reqwest::{header::HeaderMap, Client, Proxy};
use std::{
    error::Error,
    path::{Path, PathBuf},
};
use tokio::{
    fs::{self},
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
};

pub struct DownloadInfo {
    pub file_size: usize,
    pub file_name: String,
    pub file_path: PathBuf,
    pub rx: mpsc::Receiver<DownloadProgress>,
}

pub struct DownloadOptions<'a> {
    pub url: &'a str,
    pub save_folder: &'a str,
    pub threads: usize,
    pub file_name: Option<&'a str>,
    pub headers: Option<HeaderMap>,
    pub proxy: Option<&'a str>,
}

pub async fn download<'a>(options: DownloadOptions<'a>) -> Result<DownloadInfo, Box<dyn Error>> {
    // 配置默认 Headers
    let mut client = Client::builder().default_headers(options.headers.unwrap_or(HeaderMap::new()));
    // 配置 Proxy
    if let Some(proxy) = options.proxy {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;

    // 获取 URL 信息
    let info = get_url_info::get_url_info(&client, options.url).await?;
    let can_fast_download = info.file_size > 0 && info.supports_range;

    // 创建保存文件夹
    if let Err(e) = fs::create_dir_all(options.save_folder).await {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }

    // 创建文件
    let file_path =
        Path::new(options.save_folder).join(options.file_name.unwrap_or(&info.file_name));
    let file = fs::File::create(&file_path).await?;
    let (tx, rx) = mpsc::channel(100);

    if can_fast_download {
        let download_chunk = vec![
            DownloadProgress::new(0, 1024),
            DownloadProgress::new(2048, 4096),
        ];
        let chunk = get_chunks::get_chunks(&download_chunk, options.threads);
        println!("{:#?}", chunk);
    } else {
        let mut writer = BufWriter::new(file);
        tokio::spawn(async move {
            let mut response = client.get(&info.final_url).send().await.unwrap();
            let mut downloaded = 0usize;
            while let Some(bytes) = response.chunk().await.unwrap() {
                let len = bytes.len();
                tx.send(DownloadProgress::new(downloaded, downloaded + len))
                    .await
                    .unwrap();
                downloaded += len;
                writer.write_all(&bytes).await.unwrap();
            }
            writer.flush().await.unwrap();
        });
    }

    Ok(DownloadInfo {
        file_size: info.file_size,
        file_name: info.file_name,
        file_path,
        rx,
    })
}
