# fast-pull

![Latest commit (branch)](https://img.shields.io/github/last-commit/fast-down/core/main)
[![Test](https://github.com/fast-down/fast-pull/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![Latest version](https://img.shields.io/crates/v/fast-pull.svg)](https://crates.io/crates/fast-pull)
[![Documentation](https://docs.rs/fast-pull/badge.svg)](https://docs.rs/fast-pull)
![License](https://img.shields.io/crates/l/fast-pull.svg)

`fast-pull` **Fast** IO multiplex and demultiplexer!

## Usage

```rust
use core::{num::NonZeroUsize, time::Duration};
use fast_pull::{
    Event, MergeProgress, ProgressEntry,
    file::RandFileWriterMmap,
    multi::{self, download_multi},
    reqwest::{Prefetch, ReqwestReader},
};
use reqwest::Client;
use tokio::fs::OpenOptions;

#[tokio::main]
async fn main() {
    let url = "https://example.com/file.txt";
    let client = Client::new();
    let info = client.prefetch(url).await.unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(info.name)
        .await
        .unwrap();
    let reader = ReqwestReader::new(url.parse().unwrap(), client);
    let writer = RandFileWriterMmap::new(file, info.size, 8 * 1024)
        .await
        .unwrap();
    let download_chunks = vec![0..info.size as u64];
    let result = download_multi(
        reader,
        writer,
        multi::DownloadOptions {
            concurrent: NonZeroUsize::new(32).unwrap(),
            retry_gap: Duration::from_secs(1),
            write_queue_cap: 1024,
            download_chunks: download_chunks.clone(),
        },
    )
    .await;
    let mut download_progress: Vec<ProgressEntry> = Vec::new();
    let mut write_progress: Vec<ProgressEntry> = Vec::new();
    while let Ok(e) = result.event_chain.recv().await {
        match e {
            Event::ReadProgress(_, p) => {
                download_progress.merge_progress(p);
            }
            Event::WriteProgress(_, p) => {
                write_progress.merge_progress(p);
            }
            _ => {}
        }
    }
    result.join().await.unwrap();
}
```
