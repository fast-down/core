use crate::{args::DownloadArgs, fmt, path, persist::Database, progress};
use color_eyre::Result;
use crossbeam_channel::{Receiver, Sender};
use fast_down::{
    file::DownloadOptions, DownloadResult, Event, MergeProgress, ProgressEntry, Total, UrlInfo,
};
use reqwest::{
    blocking::Client,
    header::{self, HeaderValue},
    Proxy,
};
use std::{
    env,
    path::Path,
    sync::{Arc, RwLock},
    thread::{self, JoinHandle},
    time::Instant,
};
use url::Url;

pub enum Action {
    Stop(usize),
    Resume(usize),
    AddTask(String),
    RemoveTask(usize),
}

pub enum Message {
    ProgressUpdate(usize, Vec<ProgressEntry>),
    Stopped(usize),
    Started(usize),
    Info(usize, UrlInfo),
}

pub struct Manager {
    tx_ctrl: Sender<Action>,
    pub rx_recv: Receiver<Message>,
    pub handle: JoinHandle<()>,
}

pub struct ManagerData {
    result: DownloadResult,
    url: String,
}

fn download(
    args: &DownloadArgs,
    url: &str,
    tx: Sender<Message>,
    manager_data: Arc<RwLock<Option<ManagerData>>>,
    list: Arc<RwLock<Vec<Arc<RwLock<Option<ManagerData>>>>>>,
) -> Result<()> {
    let mut headers = args.headers.clone();
    if args.browser {
        {
            let url = Url::parse(&url)?;
            headers
                .entry(header::ORIGIN)
                .or_insert(HeaderValue::from_str(
                    url.origin().ascii_serialization().as_str(),
                )?);
        }
        headers
            .entry(header::REFERER)
            .or_insert(HeaderValue::from_str(&url)?);
    }
    dbg!(&args);
    dbg!(&headers);
    let mut client = Client::builder().default_headers(headers);
    if let Some(ref proxy) = args.proxy {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;
    let db = Database::new()?;

    let info = fast_down::get_url_info(&url, &client)?;

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
    let mut resume_download = false;
    let mut write_progress: Vec<ProgressEntry> = Vec::with_capacity(threads);

    if save_path.exists() {
        if info.can_fast_download {
            if let Ok(Some(progress)) = db.get_progress(&save_path_str) {
                let downloaded = progress.progress.total();
                if downloaded < info.file_size {
                    download_chunks = progress::invert(&progress.progress, info.file_size);
                    write_progress = progress.progress.clone();
                    resume_download = true;
                    println!("发现未完成的下载，将继续下载剩余部分");
                    println!(
                        "已下载: {} / {} ({}%)",
                        fmt::format_size(downloaded as f64),
                        fmt::format_size(info.file_size as f64),
                        downloaded * 100 / info.file_size
                    );
                }
            }
        }
        if !resume_download {
            save_path = path::find_available_path(&save_path)?;
            save_path_str = save_path.to_str().unwrap().to_string();
        }
    }

    println!(
        "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}\n线程数量: {}\nETag: {:?}\nLast-Modified: {:?}\n",
        info.file_name,
        fmt::format_size(info.file_size as f64),
        info.file_size,
        save_path_str,
        threads,
        info.etag,
        info.last_modified
    );

    let result = fast_down::file::download(
        &info.final_url,
        &save_path,
        DownloadOptions {
            threads,
            can_fast_download: info.can_fast_download,
            file_size: info.file_size,
            client,
            download_buffer_size: args.download_buffer_size,
            download_chunks,
            retry_gap: args.retry_gap,
            write_buffer_size: args.write_buffer_size,
        },
    )?;
    manager_data.write().unwrap().replace(ManagerData {
        result: result.clone(),
        url: url.to_string(),
    });

    let mut last_db_update = Instant::now();

    if !resume_download {
        db.init_progress(
            &save_path_str,
            info.file_size,
            info.etag,
            info.last_modified,
            &info.file_name,
            &info.final_url,
        );
    }

    let start = Instant::now();
    for e in &result {
        match e {
            Event::DownloadProgress(p) => {
                write_progress.merge_progress(p);
                if last_db_update.elapsed().as_secs() >= 1 {
                    db.update_progress(
                        &save_path_str,
                        &write_progress,
                        start.elapsed().as_millis() as u64,
                    );
                    last_db_update = Instant::now();
                }
                tx.send(Message::ProgressUpdate(
                    list.read()
                        .unwrap()
                        .iter()
                        .position(|e| Arc::ptr_eq(&e, &manager_data))
                        .unwrap(),
                    write_progress.clone(),
                ))?;
            }
            _ => {}
        }
    }
    db.update_progress(
        &save_path_str,
        &write_progress,
        start.elapsed().as_millis() as u64,
    );
    tx.send(Message::ProgressUpdate(
        list.read()
            .unwrap()
            .iter()
            .position(|e| Arc::ptr_eq(&e, &manager_data))
            .unwrap(),
        write_progress,
    ))?;
    result.join().unwrap();
    Ok(())
}

impl Manager {
    pub fn new(args: Arc<DownloadArgs>) -> Self {
        let (tx_ctrl, rx_ctrl) = crossbeam_channel::unbounded();
        let (tx_recv, rx_recv) = crossbeam_channel::unbounded();
        let handle = thread::spawn(move || {
            let list: Arc<RwLock<Vec<Arc<RwLock<Option<ManagerData>>>>>> =
                Arc::new(RwLock::new(Vec::new()));
            for event in rx_ctrl {
                let list = list.clone();
                match event {
                    Action::Stop(index) => {
                        let list = list.read().unwrap();
                        let mut res = list[index].write().unwrap();
                        if let Some(res) = res.take() {
                            res.result.cancel();
                        }
                    }
                    Action::Resume(index) => {
                        let manager_data = Arc::new(RwLock::new(None));
                        let mut list_inner = list.write().unwrap();
                        let origin_data = list_inner.remove(index);
                        let origin_data = origin_data.read().unwrap();
                        if let Some(origin_data) = origin_data.as_ref() {
                            origin_data.result.cancel();
                            let url = origin_data.url.clone();
                            list_inner.insert(index, manager_data.clone());
                            let args = args.clone();
                            let tx_recv = tx_recv.clone();
                            let list = list.clone();
                            thread::spawn(move || {
                                let tx_clone = tx_recv.clone();
                                let manager_data_clone = manager_data.clone();
                                let list_clone = list.clone();
                                download(&args, &url, tx_recv, manager_data, list).map_err(
                                    move |e| {
                                        eprintln!("Error: {e:#?}");
                                        tx_clone
                                            .send(Message::Stopped(
                                                list_clone
                                                    .read()
                                                    .unwrap()
                                                    .iter()
                                                    .position(|e| {
                                                        Arc::ptr_eq(&e, &manager_data_clone)
                                                    })
                                                    .unwrap(),
                                            ))
                                            .unwrap();
                                    },
                                )
                            });
                        }
                    }
                    Action::AddTask(url) => {
                        let manager_data = Arc::new(RwLock::new(None));
                        list.write().unwrap().insert(0, manager_data.clone());
                        let args = args.clone();
                        let tx_recv = tx_recv.clone();
                        thread::spawn(move || {
                            let tx_clone = tx_recv.clone();
                            let manager_data_clone = manager_data.clone();
                            let list_clone = list.clone();
                            download(&args, &url, tx_recv, manager_data, list).map_err(move |e| {
                                eprintln!("Error: {e:#?}");
                                tx_clone
                                    .send(Message::Stopped(
                                        list_clone
                                            .read()
                                            .unwrap()
                                            .iter()
                                            .position(|e| Arc::ptr_eq(&e, &manager_data_clone))
                                            .unwrap(),
                                    ))
                                    .unwrap();
                            })
                        });
                    }
                    Action::RemoveTask(index) => {
                        let mut list = list.write().unwrap();
                        let origin_data = list.remove(index);
                        let origin_data = origin_data.read().unwrap();
                        if let Some(origin_data) = origin_data.as_ref() {
                            origin_data.result.cancel();
                        }
                    }
                }
            }
        });
        Self {
            tx_ctrl,
            rx_recv,
            handle,
        }
    }

    pub fn stop(&self, index: usize) -> Result<(), crossbeam_channel::SendError<Action>> {
        self.tx_ctrl.send(Action::Stop(index))
    }

    pub fn resume(&self, index: usize) -> Result<(), crossbeam_channel::SendError<Action>> {
        self.tx_ctrl.send(Action::Resume(index))
    }

    pub fn add_task(&self, url: String) -> Result<(), crossbeam_channel::SendError<Action>> {
        self.tx_ctrl.send(Action::AddTask(url))
    }

    pub fn remove_task(&self, index: usize) -> Result<(), crossbeam_channel::SendError<Action>> {
        self.tx_ctrl.send(Action::RemoveTask(index))
    }
}
