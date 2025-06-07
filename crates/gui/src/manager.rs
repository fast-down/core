use std::{
    env,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    thread::{self, JoinHandle},
    time::Instant,
};

use crossbeam_channel::{Receiver, Sender};
use fast_down::{file::DownloadOptions, Event, MergeProgress, ProgressEntry, Total, UrlInfo};
use reqwest::{
    blocking::Client,
    header::{self, HeaderValue},
    Proxy,
};
use url::Url;

use crate::{args::DownloadArgs, fmt, path, persist::Database, progress};

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
    progress: Vec<ProgressEntry>,
    cancel_fn: Box<dyn FnOnce()>,
}

impl Manager {
    pub fn new(args: DownloadArgs) -> Self {
        let (tx_ctrl, rx_ctrl) = crossbeam_channel::unbounded();
        let (tx_recv, rx_recv) = crossbeam_channel::unbounded();
        let handle = thread::spawn(move || {
            let list = Arc::new(RwLock::new(Vec::new()));
            for event in rx_ctrl {
                match event {
                    Action::Stop(_) => todo!(),
                    Action::Resume(_) => todo!(),
                    Action::AddTask(url_str) => {
                        let index = list.read().unwrap().len();
                        list.write().unwrap().push(None);
                        thread::spawn(move || {
                            if args.browser {
                                let url = Url::parse(&url_str).unwrap();
                                args.headers.entry(header::ORIGIN).or_insert(
                                    HeaderValue::from_str(
                                        url.origin().ascii_serialization().as_str(),
                                    )
                                    .unwrap(),
                                );
                                args.headers
                                    .entry(header::REFERER)
                                    .or_insert(HeaderValue::from_str(&url_str).unwrap());
                            }
                            dbg!(&args);
                            let mut client = Client::builder().default_headers(args.headers);
                            if let Some(ref proxy) = args.proxy {
                                client = client.proxy(Proxy::all(proxy).unwrap());
                            }
                            let client = client.build().unwrap();
                            let db = Database::new().unwrap();

                            let info = loop {
                                match fast_down::get_url_info(&url_str, &client) {
                                    Ok(info) => break info,
                                    Err(err) => {
                                        println!("获取文件信息失败: {}", err);
                                        thread::sleep(args.retry_gap);
                                    }
                                }
                            };

                            let threads = if info.can_fast_download {
                                args.threads
                            } else {
                                1
                            };
                            let mut save_path = Path::new(&args.save_folder).join(&info.file_name);
                            if save_path.is_relative() {
                                save_path = env::current_dir().unwrap().join(save_path);
                            }
                            save_path = path_clean::clean(save_path);
                            let save_path_str = save_path.to_str().unwrap().to_string();

                            let mut download_chunks = vec![0..info.file_size];
                            let mut resume_download = false;
                            let mut write_progress: Vec<ProgressEntry> =
                                Vec::with_capacity(threads);

                            if save_path.exists() {
                                if info.can_fast_download {
                                    if let Ok(Some(progress)) = db.get_progress(&save_path_str) {
                                        let downloaded = progress.progress.total();
                                        if downloaded < info.file_size {
                                            download_chunks = progress::invert(
                                                &progress.progress,
                                                info.file_size,
                                            );
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
                                    save_path = path::find_available_path(&save_path).unwrap();
                                }
                            }

                            println!(
                                "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}\n线程数量: {}\nETag: {:?}\nLast-Modified: {:?}\n",
                                info.file_name,
                                fmt::format_size(info.file_size as f64),
                                info.file_size,
                                save_path.to_str().unwrap(),
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
                            )
                            .unwrap();

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
                                        if last_db_update.elapsed().as_secs() > 1 {
                                            db.update_progress(
                                                &save_path_str,
                                                &write_progress,
                                                start.elapsed().as_millis() as u64,
                                            );
                                            last_db_update = Instant::now();
                                        }
                                        tx_recv
                                            .send(Message::ProgressUpdate(index, write_progress))
                                            .unwrap();
                                    }
                                    _ => {}
                                }
                            }
                            db.update_progress(
                                &save_path_str,
                                &write_progress,
                                start.elapsed().as_millis() as u64,
                            );
                            tx_recv
                                .send(Message::ProgressUpdate(index, write_progress))
                                .unwrap();
                            result.join().unwrap();
                        });
                    }
                    Action::RemoveTask(_) => todo!(),
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
