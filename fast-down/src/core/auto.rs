use super::{multi, single};
use crate::{Event, RandWriter, SeqWriter};
use color_eyre::eyre::Result;
use core::{ops::Range, time::Duration};
use crossbeam_channel::Receiver;
use reqwest::{blocking::Client, IntoUrl};
use std::thread::JoinHandle;

pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub can_fast_download: bool,
    pub download_buffer_size: usize,
    pub download_chunks: Vec<Range<usize>>,
    pub retry_gap: Duration,
    pub file_size: usize,
}

pub fn download(
    url: impl IntoUrl,
    seq_writer: impl SeqWriter + 'static,
    rand_writer: impl RandWriter + 'static,
    options: DownloadOptions,
) -> Result<(Receiver<Event>, JoinHandle<()>)> {
    if !options.can_fast_download
        || options.threads < 2
            && options.download_chunks.len() == 1
            && options.download_chunks[0].start == 0
            && options.download_chunks[0].end == options.file_size
    {
        single::download(
            url,
            seq_writer,
            single::DownloadOptions {
                client: options.client,
                download_buffer_size: options.download_buffer_size,
                retry_gap: options.retry_gap,
            },
        )
    } else {
        multi::download(
            url,
            rand_writer,
            multi::DownloadOptions {
                client: options.client,
                threads: options.threads,
                download_buffer_size: options.download_buffer_size,
                download_chunks: options.download_chunks,
                retry_gap: options.retry_gap,
            },
        )
    }
}
