use crate::{
    DownloadResult, Event, MergeProgress, ProgressEntry, UrlInfo,
    downloader::config::DownloadConfig,
    file::FilePusher,
    http::{HttpError, Prefetch},
    multi::{self, TokioExecutor, download_multi},
    single::{self, EmptyExecutor, download_single},
};
use aria2_gid::Gid;
use inherit_config::InheritAble;
use kanal::{AsyncReceiver, AsyncSender};
use reqwest::Client;
use spin::mutex::SpinMutex;
use std::{fmt::Debug, path::PathBuf, sync::Arc, time::Duration};
use tokio::{fs::OpenOptions, task::JoinHandle};
use url::Url;

pub enum DownloadEvent {
    GetHttpClientError(reqwest::Error),
    Prefetch(Result<Arc<UrlInfo>, (HttpError<Client>, Option<Duration>)>),
    NoSameFile,
    FilePath(tokio::io::Result<PathBuf>),
    CreatePullerError(reqwest::Error),
    CreatePusherError(std::io::Error),
    Download(Event<HttpError<Client>, std::io::Error>),
}

#[derive(Debug)]
pub struct DownloadEntryInner {
    pub url: Url,
    pub config: DownloadConfig,
    pub global_config: Arc<SpinMutex<DownloadConfig>>,
    pub info: Option<Arc<UrlInfo>>,
    pub path: Option<PathBuf>,
    pub event_chain: AsyncReceiver<DownloadEvent>,
    tx: AsyncSender<DownloadEvent>,
    download_result: Option<DownloadResultEnum>,
    handle: Option<JoinHandle<()>>,
}

impl DownloadEntryInner {
    pub fn config(&self) -> DownloadConfig {
        self.config
            .inherit(&*self.global_config.lock())
            .inherit(&DownloadConfig::default())
    }
    pub fn is_running(&self) -> bool {
        self.download_result.is_some() || self.handle.is_some()
    }
    pub fn abort(&mut self) {
        if let Some(res) = self.download_result.take() {
            match res {
                DownloadResultEnum::Single(r) => r.abort(),
                DownloadResultEnum::Multiple(r) => r.abort(),
            }
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadEntry {
    pub gid: Gid,
    pub inner: Arc<SpinMutex<DownloadEntryInner>>,
    pub push_progress: Arc<SpinMutex<Vec<ProgressEntry>>>,
    pub add_options: AddOptions,
}
impl DownloadEntry {
    pub fn new(
        gid: Gid,
        option: AddOptions,
        global_config: Arc<SpinMutex<DownloadConfig>>,
    ) -> Self {
        let (tx, event_chain) = kanal::unbounded_async();
        Self {
            gid,
            inner: Arc::new(SpinMutex::new(DownloadEntryInner {
                url: option.url.clone(),
                config: option.config.clone(),
                global_config,
                info: None,
                path: None,
                event_chain,
                tx,
                download_result: None,
                handle: None,
            })),
            push_progress: Arc::new(SpinMutex::new(Vec::new())),
            add_options: option,
        }
    }
    pub fn abort(&self) {
        self.inner.lock().abort();
    }
    pub fn is_running(&self) -> bool {
        self.inner.lock().is_running()
    }
    pub fn run(&self) -> Result<(), reqwest::Error> {
        let inner = self.inner.clone();
        let push_progress = self.push_progress.clone();
        let handle = tokio::spawn(async move {
            let mut guard = inner.lock();
            guard.abort();
            let config = guard.config();
            let url = guard.url.clone();
            let tx = guard.tx.clone();
            drop(guard);
            let client = send_err2!(get_client(&config), tx, DownloadEvent::GetHttpClientError);
            let (info, resp) = send_err!(
                client.prefetch(url.clone()).await,
                tx,
                DownloadEvent::Prefetch
            );
            let info = Arc::new(info);
            let mut guard = inner.lock();
            if let Some(old_info) = guard.info.as_ref()
                && old_info.file_id != info.file_id
            {
                tx.send(DownloadEvent::NoSameFile).await.unwrap();
                return;
            }
            guard.info.replace(info.clone());
            drop(guard);
            tx.send(DownloadEvent::Prefetch(Ok(info.clone())))
                .await
                .unwrap();
            let path = {
                let mut guard = inner.lock();
                if let Some(path) = guard.path.as_ref() {
                    path.clone()
                } else {
                    let mut path = config.save_dir.unwrap().to_path_buf();
                    path.push(info.filename());
                    path = send_err!(gen_unique_path(path).await, tx, DownloadEvent::FilePath);
                    guard.path.replace(path.clone());
                    path
                }
            };
            tx.send(DownloadEvent::FilePath(Ok(path.clone())))
                .await
                .unwrap();
            let puller = send_err2!(
                FastDownPuller::new(FastDownPullerOptions {
                    url,
                    headers: config.headers.unwrap(),
                    proxy: &config.proxy.unwrap(),
                    multiplexing: config.multiplexing.unwrap(),
                    accept_invalid_certs: config.accept_invalid_certs.unwrap(),
                    accept_invalid_hostnames: config.accept_invalid_hostnames.unwrap(),
                    file_id: info.file_id.clone(),
                    resp: Some(Arc::new(SpinMutex::new(Some(resp)))),
                }),
                tx,
                DownloadEvent::CreatePullerError
            );
            let retry_gap = config.retry_gap.unwrap();
            let push_queue_cap = config.write_queue_cap.unwrap();
            let file = send_err2!(
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .read(false)
                    .open(&path)
                    .await,
                tx,
                DownloadEvent::CreatePusherError
            );
            let pusher = send_err2!(
                FilePusher::new(file, info.size, config.write_buffer_size.unwrap()).await,
                tx,
                DownloadEvent::CreatePusherError
            );
            let res = if info.fast_download {
                let res = download_multi(
                    puller,
                    pusher,
                    multi::DownloadOptions {
                        download_chunks: invert_progress(&push_progress.lock(), info.size),
                        concurrent: config.threads.unwrap(),
                        min_chunk_size: config.min_chunk_size.unwrap(),
                        retry_gap,
                        push_queue_cap,
                    },
                )
                .await;
                DownloadResultEnum::Multiple(res)
            } else {
                let res = download_single(
                    puller,
                    pusher,
                    single::DownloadOptions {
                        retry_gap,
                        push_queue_cap,
                    },
                )
                .await;
                DownloadResultEnum::Single(res)
            };
            inner.lock().download_result.replace(res.clone());
            let event_chain = match res {
                DownloadResultEnum::Single(res) => res.event_chain,
                DownloadResultEnum::Multiple(res) => res.event_chain,
            };
            while let Ok(event) = event_chain.recv().await {
                if let Event::PushProgress(_, range) = &event {
                    push_progress.lock().merge_progress(range.clone());
                }
                tx.send(DownloadEvent::Download(event)).await.unwrap();
            }
        });
        self.inner.lock().handle.replace(handle);
        Ok(())
    }
}

impl PartialEq for DownloadEntry {
    fn eq(&self, other: &Self) -> bool {
        self.gid == other.gid
    }
}
impl Eq for DownloadEntry {}
impl PartialEq<Gid> for DownloadEntry {
    fn eq(&self, other: &Gid) -> bool {
        self.gid == *other
    }
}

#[derive(Debug, Clone)]
pub struct AddOptions {
    pub url: Url,
    pub immediate_download: bool,
    pub config: DownloadConfig,
}

pub fn get_client(config: &DownloadConfig) -> Result<Client, reqwest::Error> {
    build_client(
        config.headers.as_ref().unwrap(),
        config.proxy.as_ref().unwrap(),
        config.accept_invalid_certs.unwrap(),
        config.accept_invalid_hostnames.unwrap(),
    )
}

#[derive(Debug, Clone)]
pub enum DownloadResultEnum {
    Single(DownloadResult<EmptyExecutor, HttpError<Client>, std::io::Error>),
    Multiple(
        DownloadResult<
            TokioExecutor<FastDownPuller, std::io::Error>,
            HttpError<Client>,
            std::io::Error,
        >,
    ),
}
