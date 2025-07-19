use crate::file;
use crate::writer::file::SeqFileWriter;
use crate::{auto, DownloadResult, ProgressEntry};
use reqwest::{Client, IntoUrl};
use std::{io::ErrorKind, path::Path, time::Duration};
use tokio::fs::{self, OpenOptions};

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub can_fast_download: bool,
    pub write_buffer_size: usize,
    pub download_chunks: Vec<ProgressEntry>,
    pub retry_gap: Duration,
    pub file_size: u64,
}

#[derive(Debug)]
pub enum DownloadErrorKind {
    Reqwest(reqwest::Error),
    Io(std::io::Error),
}
impl std::fmt::Display for DownloadErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadErrorKind::Reqwest(err) => write!(f, "HTTP request failed: {}", err),
            DownloadErrorKind::Io(err) => write!(f, "IO error: {}", err),
        }
    }
}

impl std::error::Error for DownloadErrorKind {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DownloadErrorKind::Reqwest(err) => Some(err),
            DownloadErrorKind::Io(err) => Some(err),
        }
    }
}

pub async fn download(
    url: impl IntoUrl,
    save_path: &Path,
    options: DownloadOptions,
) -> Result<DownloadResult, DownloadErrorKind> {
    let save_folder = save_path.parent().unwrap();
    if let Err(e) = fs::create_dir_all(save_folder).await {
        if e.kind() != ErrorKind::AlreadyExists {
            return Err(DownloadErrorKind::Io(e));
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&save_path)
        .await
        .map_err(|e| DownloadErrorKind::Io(e))?;
    let seq_file_writer = SeqFileWriter::new(
        file.try_clone()
            .await
            .map_err(|e| DownloadErrorKind::Io(e))?,
        options.write_buffer_size,
    );
    #[cfg(target_pointer_width = "64")]
    let rand_file_writer = file::rand_file_writer_mmap::RandFileWriter::new(
        file,
        options.file_size,
        options.write_buffer_size,
    )
    .await
    .map_err(|e| DownloadErrorKind::Io(e))?;
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
            can_fast_download: options.can_fast_download,
            download_chunks: options.download_chunks,
            retry_gap: options.retry_gap,
            file_size: options.file_size,
        },
    )
    .await
    .map_err(|e| DownloadErrorKind::Reqwest(e))
}
