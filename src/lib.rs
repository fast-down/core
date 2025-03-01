mod get_url_info;

use get_url_info::get_url_info;
use reqwest::{blocking::Client, header::HeaderMap, Proxy};
use std::{
    error::Error,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

pub struct DownloadOptions<'a> {
    pub url: &'a str,
    pub save_folder: &'a str,
    pub threads: usize,
    pub file_name: Option<&'a str>,
    pub headers: Option<HeaderMap>,
    pub proxy: Option<&'a str>,
}
pub struct DownloadResult {
    pub file_name: String,
    pub file_size: usize,
    pub file_path: PathBuf,
    pub rx: mpsc::Receiver<Option<Result<DownloadProgress, Box<dyn Error>>>>,
}
pub struct DownloadProgress {
    pub start_bytes: usize,
    pub end_bytes: usize,
}

pub fn download(options: DownloadOptions) -> Result<DownloadResult, Box<dyn Error>> {
    // 配置默认 Headers
    let mut client = Client::builder().default_headers(options.headers.unwrap_or(HeaderMap::new()));
    // 配置 Proxy
    if let Some(proxy) = options.proxy {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;

    // 获取 URL 信息
    let info = get_url_info(&client, options.url)?;
    let can_fast_download = info.file_size > 0 && info.can_use_range;

    // 创建保存文件夹
    if let Err(e) = fs::create_dir_all(options.save_folder) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }

    // 创建文件
    let file_path =
        Path::new(options.save_folder).join(options.file_name.unwrap_or(&info.file_name));
    let mut file = fs::File::create(&file_path)?;

    let (tx, rx) = mpsc::channel::<Option<Result<DownloadProgress, Box<dyn Error>>>>();
    if !can_fast_download {
        thread::spawn(move || {
            let mut response = match client.get(info.final_url).send() {
                Ok(response) => response,
                Err(e) => {
                    tx.send(Some(Err(e.into()))).unwrap();
                    return;
                }
            };
            let mut buffer = [0; 1024];
            let mut downloaded = 0usize;
            while let Ok(bytes) = response.read(&mut buffer) {
                if bytes == 0 {
                    break;
                }
                if let Err(e) = file.write_all(&buffer[..bytes]) {
                    tx.send(Some(Err(e.into()))).unwrap();
                    return;
                }
                tx.send(Some(Ok(DownloadProgress {
                    start_bytes: downloaded,
                    end_bytes: downloaded + bytes,
                })))
                .unwrap();
                downloaded += bytes;
            }
            tx.send(None).unwrap();
        });
        Ok(DownloadResult {
            file_name: info.file_name,
            file_size: info.file_size,
            file_path,
            rx,
        })
    } else {
        for i in 0..8 {
            let tx = tx.clone();
            thread::spawn(move || {
                tx.send(i * 2).unwrap();
            });
        }

        // for _ in 0..8 {
        //     println!("{}", rx.recv().unwrap());
        // }
        Ok(DownloadResult {
            file_name: info.file_name,
            file_size: info.file_size,
            file_path,
            rx,
        })
    }
}
