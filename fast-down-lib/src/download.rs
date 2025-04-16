extern crate std;
use crate::{
    download_multi_threads::{download_multi_threads, DownloadMultiThreadsOptions},
    download_single_thread::{download_single_thread, DownloadSingleThreadOptions},
    Event,
};
use std::{fs, io::ErrorKind, path::Path, thread::JoinHandle};
extern crate alloc;
use alloc::string::String;
use color_eyre::eyre::Result;
use fs::OpenOptions;
use reqwest::blocking::Client;

pub struct DownloadOptions<'a> {
    pub url: String,
    pub save_path: &'a Path,
    pub threads: usize,
    pub client: Client,
    pub file_size: usize,
    pub can_fast_download: bool,
    pub get_chunk_size: usize,
    pub write_chunk_size: usize,
}

pub fn download<'a>(
    options: DownloadOptions<'a>,
) -> Result<(crossbeam_channel::Receiver<Event>, JoinHandle<()>)> {
    let save_folder = options.save_path.parent().unwrap();
    if let Err(e) = fs::create_dir_all(save_folder) {
        if e.kind() != ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .open(&options.save_path)?;
    file.set_len(options.file_size as u64)?;

    if options.can_fast_download {
        download_multi_threads(DownloadMultiThreadsOptions {
            url: options.url,
            file,
            file_size: options.file_size,
            client: options.client,
            threads: options.threads,
            get_chunk_size: options.get_chunk_size,
            write_chunk_size: options.write_chunk_size,
        })
    } else {
        download_single_thread(DownloadSingleThreadOptions {
            url: options.url,
            file,
            client: options.client,
            get_chunk_size: options.get_chunk_size,
            write_chunk_size: options.write_chunk_size,
        })
    }
}
