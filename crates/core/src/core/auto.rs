use super::{multi, single, DownloadResult};
use crate::{ProgressEntry, RandWriter, SeqWriter};
use core::time::Duration;
use reqwest::{Client, IntoUrl};

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub can_fast_download: bool,
    pub download_chunks: Vec<ProgressEntry>,
    pub retry_gap: Duration,
    pub file_size: u64,
}

pub async fn download(
    url: impl IntoUrl,
    seq_writer: impl SeqWriter + 'static,
    rand_writer: impl RandWriter + 'static,
    options: DownloadOptions,
) -> Result<DownloadResult, reqwest::Error> {
    if options.can_fast_download {
        multi::download(
            url,
            rand_writer,
            multi::DownloadOptions {
                client: options.client,
                threads: options.threads,
                download_chunks: options.download_chunks,
                retry_gap: options.retry_gap,
            },
        )
        .await
    } else {
        single::download(
            url,
            seq_writer,
            single::DownloadOptions {
                client: options.client,
                retry_gap: options.retry_gap,
            },
        )
        .await
    }
}
