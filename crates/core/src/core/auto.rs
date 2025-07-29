use super::{DownloadResult, multi, single};
use crate::{ProgressEntry, RandWriter, SeqWriter};
use core::time::Duration;
use std::num::NonZeroUsize;
use reqwest::{Client, IntoUrl};

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub concurrent: Option<NonZeroUsize>,
    pub retry_gap: Duration,
    pub file_size: u64,
    pub write_channel_size: usize,
}

pub async fn download(
    client: Client,
    url: impl IntoUrl,
    download_chunks: Vec<ProgressEntry>,
    seq_writer: impl SeqWriter + 'static,
    rand_writer: impl RandWriter + 'static,
    options: DownloadOptions,
) -> Result<DownloadResult, reqwest::Error> {
    if let Some(threads) = options.concurrent {
        multi::download(
            client,
            url,
            download_chunks,
            rand_writer,
            multi::DownloadOptions {
                threads,
                retry_gap: options.retry_gap,
                write_channel_size: options.write_channel_size,
            },
        )
        .await
    } else {
        single::download(
            client,
            url,
            seq_writer,
            single::DownloadOptions {
                retry_gap: options.retry_gap,
                write_channel_size: options.write_channel_size,
            },
        )
        .await
    }
}
