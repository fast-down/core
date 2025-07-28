use crate::file;
use crate::writer::file::SeqFileWriter;
use crate::{DownloadResult, ProgressEntry, auto};
use reqwest::{Client, IntoUrl};
use std::{io::ErrorKind, path::Path, time::Duration};
use tokio::fs::{self, OpenOptions};

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub concurrent: bool,
    pub write_buffer_size: usize,
    pub download_chunks: Vec<ProgressEntry>,
    pub retry_gap: Duration,
    pub file_size: u64,
    pub write_channel_size: usize,
}

#[derive(thiserror::Error, Debug)]
pub enum DownloadErrorKind {
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn download(
    url: impl IntoUrl,
    save_path: &Path,
    options: DownloadOptions,
) -> Result<DownloadResult, DownloadErrorKind> {
    let save_folder = save_path
        .parent()
        .ok_or(DownloadErrorKind::Io(ErrorKind::NotFound.into()))?;
    if let Err(e) = fs::create_dir_all(save_folder).await
        && e.kind() != ErrorKind::AlreadyExists
    {
        return Err(DownloadErrorKind::Io(e));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&save_path)
        .await
        .map_err(DownloadErrorKind::Io)?;
    let seq_file_writer = SeqFileWriter::new(
        file.try_clone().await.map_err(DownloadErrorKind::Io)?,
        options.write_buffer_size,
    );
    #[cfg(target_pointer_width = "64")]
    let rand_file_writer = file::rand_file_writer_mmap::RandFileWriter::new(
        file,
        options.file_size,
        options.write_buffer_size,
    )
    .await
    .map_err(DownloadErrorKind::Io)?;
    #[cfg(not(target_pointer_width = "64"))]
    let rand_file_writer = file::rand_file_writer_std::RandFileWriter::new(
        file,
        options.file_size,
        options.write_buffer_size,
    )
    .await
    .map_err(|e| DownloadErrorKind::Io(e))?;
    auto::download(
        url,
        seq_file_writer,
        rand_file_writer,
        auto::DownloadOptions {
            threads: options.threads,
            client: options.client,
            concurrent: options.concurrent,
            download_chunks: options.download_chunks,
            retry_gap: options.retry_gap,
            file_size: options.file_size,
            write_channel_size: options.write_channel_size,
        },
    )
    .await
    .map_err(DownloadErrorKind::Reqwest)
}
