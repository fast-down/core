use super::auto;
use crate::{Event, RandFileWriter, SeqFileWriter};
use color_eyre::eyre::Result;
use crossbeam_channel::Receiver;
use reqwest::{blocking::Client, IntoUrl};
use std::{
    fs::{self, OpenOptions},
    io::ErrorKind,
    ops::Range,
    path::Path,
    thread::JoinHandle,
    time::Duration,
};

pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub can_fast_download: bool,
    pub download_buffer_size: usize,
    pub write_buffer_size: usize,
    pub download_chunks: Vec<Range<usize>>,
    pub retry_gap: Duration,
    pub file_size: usize,
}

pub fn download_file(
    url: impl IntoUrl,
    save_path: &Path,
    options: DownloadOptions,
) -> Result<(Receiver<Event>, JoinHandle<()>)> {
    let save_folder = save_path.parent().unwrap();
    if let Err(e) = fs::create_dir_all(save_folder) {
        if e.kind() != ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&save_path)?;
    auto::download(
        url,
        SeqFileWriter::new(file.try_clone()?, options.write_buffer_size)?,
        RandFileWriter::new(file, options.file_size)?,
        auto::DownloadOptions {
            threads: options.threads,
            client: options.client,
            can_fast_download: options.can_fast_download,
            download_buffer_size: options.download_buffer_size,
            download_chunks: options.download_chunks,
            retry_gap: options.retry_gap,
            file_size: options.file_size,
        },
    )
}
