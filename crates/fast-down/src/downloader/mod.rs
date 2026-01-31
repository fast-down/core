pub mod config;
pub mod entry;
pub mod invert;
pub mod puller;
pub mod send_err;
pub mod unique_path;

use crate::downloader::{config::DownloadConfig, entry::DownloadEntry};
use aria2_gid::Gid;
use parking_lot::Mutex;
use std::sync::Arc;

pub struct Downloader {
    list: Arc<Mutex<Vec<DownloadEntry>>>,
    parallelism: Arc<Mutex<usize>>,
    pub config: Arc<Mutex<DownloadConfig>>,
}

impl Downloader {
    pub fn new(config: Arc<Mutex<DownloadConfig>>) -> Self {
        Self::with_capacity(config, 0)
    }
    pub fn with_capacity(config: Arc<Mutex<DownloadConfig>>, capacity: usize) -> Self {
        Self {
            list: Arc::new(Mutex::new(Vec::with_capacity(capacity))),
            parallelism: Arc::new(Mutex::new(0)),
            config,
        }
    }
    pub fn set_parallelism(self: Arc<Self>, parallelism: usize) {
        *self.parallelism.lock() = parallelism;
        self.run();
    }
    pub fn run(self: Arc<Self>) {
        let list = self.list.lock();
        let parallelism = *self.parallelism.lock();
        let running_count: usize = list
            .iter()
            .map(|e| if e.is_running() { 1 } else { 0 })
            .sum();
        if running_count < parallelism {
            let mut needed = parallelism - running_count;
            for entry in list.iter() {
                if needed == 0 {
                    break;
                }
                if entry.is_running() {
                    continue;
                }
                match entry.run() {
                    Ok(_) => {
                        log::debug!("downloader.run(): Run Gid {}, Entry {entry:?}", entry.gid);
                        needed -= 1;
                        let rx = entry.inner.lock().event_chain.clone();
                        let downloader = Arc::downgrade(&self);
                        tokio::spawn(async move {
                            while (rx.recv().await).is_ok() {}
                            if let Some(downloader) = downloader.upgrade() {
                                downloader.run();
                            }
                        });
                    }
                    Err(e) => log::error!(
                        "downloader.run(): Gid {}, Entry {entry:?}, Error: {e:?}",
                        entry.gid
                    ),
                }
            }
        } else if running_count > parallelism {
            let mut needed = parallelism - running_count;
            for entry in list.iter() {
                if needed == 0 {
                    break;
                }
                if entry.is_running() {
                    log::debug!("downloader.run(): Abort Gid {}, Entry {entry:?}", entry.gid);
                    needed -= 1;
                    entry.abort();
                }
            }
        }
    }
    pub fn add_task(self: Arc<Self>, options: AddOptions) -> Result<Gid, reqwest::Error> {
        let call_dbg = format!("downloader.add_task({options:?})");
        log::debug!("{call_dbg}");
        let mut list = self.list.lock();
        let gid = loop {
            let temp = Gid::new();
            if list.iter().all(|entry| entry != &temp) {
                break temp;
            }
            log::debug!("{call_dbg}: Gid collision, retrying");
        };
        log::debug!("{call_dbg}: Assigned Gid {gid}");
        let entry = DownloadEntry::new(gid, options, self.config.clone());
        log::debug!("{call_dbg}: Inserted entry {gid}: {entry:?}");
        list.push(entry);
        drop(list);
        self.run();
        Ok(gid)
    }
    pub fn remove(self: Arc<Self>, gid: Gid) -> Option<DownloadEntry> {
        log::debug!("downloader.remove({gid})");
        let mut list = self.list.lock();
        let pos = list.iter().position(|entry| entry == &gid);
        log::debug!("downloader.remove({gid}): position = {pos:?}");
        let entry = if let Some(pos) = pos {
            let removed = list.remove(pos);
            log::debug!("downloader.remove({gid}): {removed:?}");
            log::debug!("downloader.remove({gid}): Aborting");
            removed.abort();
            log::debug!("downloader.remove({gid}): Aborted");
            Some(removed)
        } else {
            None
        };
        drop(list);
        self.run();
        entry
    }
    pub fn stop(self: Arc<Self>, gid: Gid) {
        log::debug!("downloader.stop({gid})");
        let mut list = self.list.lock();
        let pos = list.iter().position(|entry| entry == &gid);
        log::debug!("downloader.stop({gid}): position = {pos:?}");
        if let Some(pos) = pos {
            let entry = list.remove(pos);
            log::debug!("downloader.stop({gid}): {entry:?}");
            log::debug!("downloader.stop({gid}): Aborting");
            entry.abort();
            log::debug!("downloader.stop({gid}): Aborted");
            list.push(entry);
            log::debug!("downloader.stop({gid}): Moved to end");
        }
        drop(list);
        self.run();
    }
    pub fn running_count(&self) -> usize {
        self.list
            .lock()
            .iter()
            .map(|e| if e.is_running() { 1 } else { 0 })
            .sum()
    }
}
