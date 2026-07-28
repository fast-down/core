# fast-down

[![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/main)](https://github.com/fast-down/core/commits/main)
[![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![Latest version](https://img.shields.io/crates/v/fast-down.svg)](https://crates.io/crates/fast-down)
[![Documentation](https://docs.rs/fast-down/badge.svg)](https://docs.rs/fast-down)
[![License](https://img.shields.io/crates/l/fast-down.svg)](https://github.com/fast-down/core/blob/main/LICENSE)

`fast-down` is a fast, concurrent file downloader library built on top of the
[`fast_pull`](https://crates.io/crates/fast-pull) pull/push engine, with first-class
HTTP support.

**[Official Website (Simplified Chinese)](https://fd.s121.top/)**

## What this crate provides

`fast-down` re-exports the entire [`fast_pull`](https://crates.io/crates/fast-pull)
pull/push engine (the `download_single` / `download_multi` entry points, the `Puller`
and `Pusher` traits, and the file/memory pushers) and adds the following on top of it:

1. **HTTP / reqwest puller** — `FastDownPuller` (with `FastDownPullerOptions`) is the
   default `Puller` for HTTP(S) sources. It builds on a `SmartRedirectClient` that
   follows redirects _manually_ so it can honor the `Referrer-Policy` header and strip
   resource-specific headers (`Origin` / `Authorization` / `Cookie`) on cross-origin
   hops, per RFC 9110 §15.4.
2. **URL info resolution** — `UrlInfo` and `FileId` capture a resource's size, suggested
   filename, content type, range support, and a stable identity derived from the
   `ETag` / `Last-Modified` headers, which powers incremental and resumable downloads.
3. **Proxy support** — the `Proxy` enum selects no proxy, the system proxy, or a custom
   proxy URL for outgoing requests.
4. **Task handles** — the re-exported `SharedHandle` task-handle type lets you await
   and abort an in-flight download from multiple owners.

Supporting building blocks (behind feature flags) include the backend-agnostic
`http` module (`HttpClient` traits, `HttpPuller`, `Prefetch`, `ContentDisposition`,
`HttpError`) and the `reqwest` module (`SmartRedirectClient`,
`ManualRedirectRequestBuilder`).

## Example

Download a file concurrently to disk. This requires the `reqwest` feature
(which also enables `http` and `fast-puller`) and a network connection, so the
block is marked `ignore` to keep doctests hermetic:

```rust,ignore
use std::sync::Arc;
use std::time::Duration;
use url::Url;

use fast_down::{FastDownPuller, FastDownPullerOptions, FileId, Proxy};
use fast_pull::file::StdFilePusher;
use fast_pull::multi::DownloadOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = Url::parse("https://example.com/large.bin")?;
    let file = tokio::fs::File::create("large.bin").await?;
    // Pre-size the file; pass `true` to fsync on flush.
    let pusher = StdFilePusher::new(file, /* size */ 0, /* sync_all */ false).await?;

    let puller = FastDownPuller::new(FastDownPullerOptions {
        url,
        headers: Arc::new(Default::default()),
        proxy: Proxy::System,
        accept_invalid_certs: false,
        accept_invalid_hostnames: false,
        cookie_store: false,
        file_id: FileId::default(),
        resp: None,
        available_ips: Arc::from(Vec::<std::net::IpAddr>::new()),
        max_redirects: 10,
    })?;

    let result = fast_pull::download_multi(
        puller,
        pusher,
        DownloadOptions {
            download_chunks: vec![0..u64::MAX].into_iter(),
            concurrent: 8,
            retry_gap: Duration::from_secs(1),
            pull_timeout: Duration::from_secs(30),
            push_queue_cap: 1024,
            min_chunk_size: 1 << 20,
            max_speculative: 3,
        },
    );

    result.join().await?;
    Ok(())
}
```

For a sequential, single-threaded download, swap `download_multi` for
`fast_pull::download_single` and use `fast_pull::single::DownloadOptions`
(which only has `retry_gap` and `push_queue_cap`).
