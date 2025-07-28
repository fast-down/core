use crate::{args::DownloadArgs, fmt, home_page::ProgressData, path, persist::Database, progress};
use async_channel::{Receiver, SendError, Sender};
use color_eyre::Result;
use fast_down::{
    file::DownloadOptions, DownloadResult, Event, MergeProgress, ProgressEntry, Total,
};
use reqwest::{
    header::{self, HeaderValue},
    Client, Proxy,
};
use std::{env, path::Path, sync::Arc, time::Instant};
use tokio::sync::RwLock;
use url::Url;

#[derive(Debug, Clone)]
pub enum Action {
    Stop(usize),
    Resume(usize),
    AddTask(String),
    RemoveTask(usize),
}

#[derive(Debug, Clone)]
pub enum Message {
    ProgressUpdate(usize, Vec<ProgressData>),
    Stopped(usize),
    FileName(usize, String),
}

#[derive(Debug)]
pub struct Manager {
    tx_ctrl: Sender<Action>,
    pub rx_recv: Receiver<Message>,
}

#[derive(Clone)]
pub struct ManagerData {
    pub result: Option<DownloadResult>,
    pub url: String,
    pub file_path: Option<String>,
}

async fn download(
    args: &DownloadArgs,
    url: &str,
    tx: Sender<Message>,
    manager_data: Arc<RwLock<ManagerData>>,
    list: Arc<RwLock<Vec<Arc<RwLock<ManagerData>>>>>,
    is_resume: bool,
) -> Result<()> {
    let mut header = args.headers.clone();
    if args.browser {
        {
            let url = Url::parse(url)?;
            header
                .entry(header::ORIGIN)
                .or_insert(HeaderValue::from_str(
                    url.origin().ascii_serialization().as_str(),
                )?);
        }
        header
            .entry(header::REFERER)
            .or_insert(HeaderValue::from_str(url)?);
    }
    dbg!(&args);
    let mut client = Client::builder().default_headers(header);
    if let Some(ref proxy) = args.proxy {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;
    let db = Database::new().await?;

    let info = loop {
        match fast_down::get_url_info(url, &client).await {
            Ok(info) => break info,
            Err(err) => println!("获取文件信息失败: {}", err),
        }
        tokio::time::sleep(args.retry_gap).await;
    };
    let threads = if info.can_fast_download {
        args.threads
    } else {
        1
    };
    let mut save_path = Path::new(&args.save_folder).join(&info.file_name);
    if save_path.is_relative() {
        if let Ok(current_dir) = env::current_dir() {
            save_path = current_dir.join(save_path);
        }
    }
    save_path = path_clean::clean(save_path);
    let mut save_path_str = save_path.to_str().unwrap().to_string();

    let mut download_chunks = vec![0..info.file_size];
    let mut download_progress: Vec<ProgressEntry> = Vec::with_capacity(threads);
    let mut write_progress: Vec<ProgressEntry> = Vec::with_capacity(threads);

    if save_path.try_exists()? {
        if is_resume && info.can_fast_download {
            if let Ok(Some(entry)) = db.get_entry(save_path_str.clone()).await {
                let downloaded = entry.progress.total();
                if downloaded < info.file_size {
                    download_chunks = progress::invert(&entry.progress, info.file_size);
                    download_progress = entry.progress.clone();
                    write_progress = entry.progress.clone();
                    println!("发现未完成的下载，将继续下载剩余部分");
                    println!(
                        "已下载: {} / {} ({}%)",
                        fmt::format_size(downloaded as f64),
                        fmt::format_size(info.file_size as f64),
                        downloaded * 100 / info.file_size
                    );
                }
            }
        } else {
            save_path = path::find_available_path(&save_path)?;
            save_path_str = save_path.to_str().unwrap().to_string();
        }
    }
    let save_path_str = Arc::new(save_path_str);
    let file_name = save_path.file_name().unwrap().to_str().unwrap();
    tx.send(Message::FileName(
        list.read()
            .await
            .iter()
            .position(|e| Arc::ptr_eq(&e, &manager_data))
            .unwrap(),
        file_name.into(),
    ))
    .await?;
    println!(
        "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}\n线程数量: {}\nETag: {:?}\nLast-Modified: {:?}\n",
        file_name,
        fmt::format_size(info.file_size as f64),
        info.file_size,
        save_path_str,
        threads,
        info.etag,
        info.last_modified
    );
    let result = fast_down::file::download(
        info.final_url.clone(),
        &save_path,
        DownloadOptions {
            threads,
            can_fast_download: info.can_fast_download,
            file_size: info.file_size,
            client,
            download_chunks,
            retry_gap: args.retry_gap,
            write_buffer_size: args.write_buffer_size,
            write_channel_size: args.write_channel_size,
        },
    )
    .await?;
    manager_data.write().await.result.replace(result.clone());
    let mut last_db_update = Instant::now();
    if !is_resume {
        db.init_entry(
            save_path_str.clone(),
            info.file_size,
            info.etag,
            info.last_modified,
            info.file_name,
            info.final_url.to_string(),
        )
        .await?;
    }
    let start = Instant::now();
    while let Ok(e) = result.event_chain.recv().await {
        match e {
            Event::DownloadProgress(p) => {
                download_progress.merge_progress(p);
                tx.send(Message::ProgressUpdate(
                    list.read()
                        .await
                        .iter()
                        .position(|e| Arc::ptr_eq(&e, &manager_data))
                        .unwrap(),
                    progress::add_blank(&download_progress, info.file_size),
                ))
                .await?;
            }
            Event::WriteProgress(p) => {
                write_progress.merge_progress(p);
                if last_db_update.elapsed().as_secs() >= 1 {
                    db.update_entry(
                        save_path_str.clone(),
                        write_progress.clone(),
                        start.elapsed().as_millis() as u64,
                    )
                    .await?;
                    last_db_update = Instant::now();
                }
            }
            _ => {}
        }
    }
    db.update_entry(
        save_path_str.clone(),
        write_progress,
        start.elapsed().as_millis() as u64,
    )
    .await?;
    result.join().await?;
    tx.send(Message::Stopped(
        list.read()
            .await
            .iter()
            .position(|e| Arc::ptr_eq(&e, &manager_data))
            .unwrap(),
    ))
    .await?;
    Ok(())
}

impl Manager {
    pub fn new(args: Arc<DownloadArgs>, init: Vec<Arc<RwLock<ManagerData>>>) -> Self {
        let (tx_ctrl, rx_ctrl) = async_channel::unbounded();
        let (tx_recv, rx_recv) = async_channel::unbounded();
        tokio::spawn(async move {
            let list = Arc::new(RwLock::new(init));
            let db = Database::new().await.unwrap();
            while let Ok(e) = rx_ctrl.recv().await {
                let list = list.clone();
                match e {
                    Action::Stop(index) => {
                        let list = list.read().await;
                        let mut res = list[index].write().await;
                        if let Some(res) = res.result.take() {
                            res.cancel().await;
                        }
                    }
                    Action::Resume(index) => {
                        let mut list_inner = list.write().await;
                        let origin_data = list_inner.remove(index);
                        let mut origin_data = origin_data.write().await;
                        if let Some(res) = origin_data.result.take() {
                            res.cancel().await;
                        }
                        let manager_data = Arc::new(RwLock::new(ManagerData {
                            result: None,
                            url: origin_data.url.clone(),
                            file_path: origin_data.file_path.clone(),
                        }));
                        list_inner.insert(index, manager_data.clone());
                        let args = args.clone();
                        let tx_recv = tx_recv.clone();
                        let list = list.clone();
                        let url = origin_data.url.clone();
                        tokio::spawn(async move {
                            let tx_clone = tx_recv.clone();
                            let manager_data_clone = manager_data.clone();
                            let list_clone = list.clone();
                            download(&args, &url, tx_recv, manager_data, list, true)
                                .await
                                .map_err(async move |e| {
                                    eprintln!("Error: {e:#?}");
                                    tx_clone
                                        .send(Message::Stopped(
                                            list_clone
                                                .read()
                                                .await
                                                .iter()
                                                .position(|e| Arc::ptr_eq(&e, &manager_data_clone))
                                                .unwrap(),
                                        ))
                                        .await
                                        .unwrap();
                                })
                        });
                    }
                    Action::AddTask(url) => {
                        let manager_data = Arc::new(RwLock::new(ManagerData {
                            result: None,
                            url: url.clone(),
                            file_path: None,
                        }));
                        list.write().await.insert(0, manager_data.clone());
                        let args = args.clone();
                        let tx_recv = tx_recv.clone();
                        tokio::spawn(async move {
                            let tx_clone = tx_recv.clone();
                            let manager_data_clone = manager_data.clone();
                            let list_clone = list.clone();
                            download(&args, &url, tx_recv, manager_data, list, false)
                                .await
                                .map_err(async move |e| {
                                    eprintln!("Error: {e:#?}");
                                    tx_clone
                                        .send(Message::Stopped(
                                            list_clone
                                                .read()
                                                .await
                                                .iter()
                                                .position(|e| Arc::ptr_eq(&e, &manager_data_clone))
                                                .unwrap(),
                                        ))
                                        .await
                                        .unwrap();
                                })
                        });
                    }
                    Action::RemoveTask(index) => {
                        let mut list = list.write().await;
                        let origin_data = list.remove(index);
                        let mut origin_data = origin_data.write().await;
                        if let Some(res) = origin_data.result.take() {
                            res.cancel().await;
                        }
                        if let Some(file_path) = origin_data.file_path.as_ref() {
                            db.remove_entry(file_path.clone()).await.unwrap();
                        }
                    }
                }
            }
        });
        Self { tx_ctrl, rx_recv }
    }

    pub async fn stop(&self, index: usize) -> Result<(), SendError<Action>> {
        self.tx_ctrl.send(Action::Stop(index)).await
    }

    pub async fn resume(&self, index: usize) -> Result<(), SendError<Action>> {
        self.tx_ctrl.send(Action::Resume(index)).await
    }

    pub async fn add_task(&self, url: String) -> Result<(), SendError<Action>> {
        self.tx_ctrl.send(Action::AddTask(url)).await
    }

    pub async fn remove_task(&self, index: usize) -> Result<(), SendError<Action>> {
        self.tx_ctrl.send(Action::RemoveTask(index)).await
    }
}
