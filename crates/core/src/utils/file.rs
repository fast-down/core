use crate::{auto, DownloadResult, Progress};
use crate::writer::file::{RandFileWriter, SeqFileWriter};
use color_eyre::eyre::Result;
use reqwest::{blocking::Client, IntoUrl};
use std::{
    fs::{self, OpenOptions},
    io::ErrorKind,
    path::Path,
    time::Duration,
};

pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub can_fast_download: bool,
    pub download_buffer_size: usize,
    pub write_buffer_size: usize,
    pub download_chunks: Vec<Progress>,
    pub retry_gap: Duration,
    pub file_size: u64,
}

pub fn download(
    url: impl IntoUrl,
    save_path: &Path,
    options: DownloadOptions,
) -> Result<DownloadResult> {
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
    let seq_file_writer = SeqFileWriter::new(file.try_clone()?, options.write_buffer_size)?;
    let rand_file_writer = RandFileWriter::new(file, options.file_size, options.write_buffer_size)?;
    auto::download(
        url,
        seq_file_writer,
        rand_file_writer,
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
