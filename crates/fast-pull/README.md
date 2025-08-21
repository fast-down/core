# fast-pull

[![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/stable)](https://github.com/fast-down/core/commits/stable)
[![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![Latest version](https://img.shields.io/crates/v/fast-pull.svg)](https://crates.io/crates/fast-pull)
[![Documentation](https://docs.rs/fast-pull/badge.svg)](https://docs.rs/fast-pull)
[![License](https://img.shields.io/crates/l/fast-pull.svg)](https://github.com/fast-down/core/blob/stable/crates/fast-pull/LICENSE)

`fast-pull` **Fastest** concurrent downloader!

**[Official Website (Simplified Chinese)](https://fast.s121.top/)**

## Features

1. **⚡️ Fastest Download**  
   We created [fast-steal](https://github.com/fast-down/fast-steal) With optimized Work Stealing, **1.43 x faster** than
   NDM.
2. **🔄 File consistency**  
   Switching Wi-Fi, Turn Off Wi-Fi, Switch proxies. **We guarantee the consistency**.
3. **⛓️‍💥 Resuming Downloads**  
   You can **interrupt** at any time, and **resume downloading** after.
4. **⛓️‍💥 Incremental Downloads**  
   1000 more lines server logs? Don't worry, we **only download new lines**.
5. **💰 Free and open-source**  
   The code stays free and open-source. Thanks to [share121](https://github.com/share121), [Cyan](https://github.com/CyanChanges) and other fast-down contributors.
6. **No std support**

## Usage

```rust
use core::{num::NonZeroUsize, time::Duration};
use fast_pull::{
    Event, MergeProgress, ProgressEntry,
    file::RandFilePusherMmap,
    multi::{self, download_multi},
    reqwest::{Prefetch, ReqwestPuller},
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
    let puller = ReqwestPuller::new(url.parse().unwrap(), client);
    let pusher = RandFilePusherMmap::new(file, info.size, 8 * 1024)
        .await
        .unwrap();
    let download_chunks = vec![0..info.size as u64];
    let result = download_multi(
        puller,
        pusher,
        multi::DownloadOptions {
            concurrent: NonZero::new(32).unwrap(),
            retry_gap: Duration::from_secs(1),
            push_queue_cap: 1024,
            download_chunks: download_chunks.clone(),
        },
    )
    .await;
    let mut pull_progress: Vec<ProgressEntry> = Vec::new();
    let mut push_progress: Vec<ProgressEntry> = Vec::new();
    while let Ok(e) = result.event_chain.recv().await {
        match e {
            Event::PullProgress(_, p) => {
                pull_progress.merge_progress(p);
            }
            Event::PushProgress(_, p) => {
                push_progress.merge_progress(p);
            }
            _ => {}
        }
    }
    result.join().await.unwrap();
}
```
