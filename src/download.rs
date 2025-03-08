use crate::{display_progress, download_progress::DownloadProgress, get_chunks, get_url_info};
use memmap2::MmapOptions;
use reqwest::{
    header::{self, HeaderMap},
    Client, Proxy, StatusCode,
};
use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
};

pub struct DownloadInfo {
    pub file_size: u64,
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
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&file_path)
        .await?;
    file.set_len(info.file_size).await?;
    let (tx, rx) = mpsc::channel(100);

    if can_fast_download {
        let download_chunk = vec![DownloadProgress::new(0, info.file_size - 1)];
        let chunks = get_chunks::get_chunks(&download_chunk, options.threads);
        let final_url = Arc::new(info.final_url);
        let (tx_write, mut rx_write) = mpsc::channel(100);
        let client = Arc::new(client);
        for chunk in chunks {
            let client = client.clone();
            let final_url = final_url.clone();
            let tx_write = tx_write.clone();
            tokio::spawn(async move {
                let mut response = client
                    .get(&*final_url)
                    .header(
                        header::RANGE,
                        format!("bytes={}", display_progress::display_progress(&chunk)),
                    )
                    .send()
                    .await
                    .unwrap();
                if response.status() != StatusCode::PARTIAL_CONTENT {
                    panic!("Error: response code is {}, not 206", response.status());
                }
                let mut downloaded = 0;
                let mut i = 0;
                let mut start_pos = chunk[0].start;
                let mut size = chunk[0].size();
                while let Some(bytes) = response.chunk().await.unwrap() {
                    downloaded += bytes.len() as u64;
                    if downloaded >= size {
                        while downloaded >= size {
                            tx_write
                                .send((start_pos + downloaded, bytes.slice(0..=size as usize)))
                                .await
                                .unwrap();
                            i += 1;
                            start_pos = chunk[i].start;
                            downloaded -= size;
                            size = chunk[i].size();
                        }
                    } else {
                        tx_write
                            .send((start_pos + downloaded, bytes))
                            .await
                            .unwrap();
                    }
                }
            });
        }
        drop(tx_write);
        let mut mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        tokio::spawn(async move {
            while let Some((pos, bytes)) = rx_write.recv().await {
                let len = bytes.len() as u64;
                tx.send(DownloadProgress::new(pos, pos + len - 1))
                    .await
                    .unwrap();
                mmap[pos as usize..(pos + len) as usize].copy_from_slice(&bytes);
            }
            mmap.flush().unwrap();
        });
    } else {
        let mut writer = BufWriter::new(file);
        tokio::spawn(async move {
            let mut response = client.get(&info.final_url).send().await.unwrap();
            let mut downloaded = 0;
            while let Some(bytes) = response.chunk().await.unwrap() {
                let len = bytes.len();
                tx.send(DownloadProgress::new(
                    downloaded,
                    downloaded + len as u64 - 1,
                ))
                .await
                .unwrap();
                downloaded += len as u64;
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
