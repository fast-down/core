# fast-pull

[![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/main)](https://github.com/fast-down/core/commits/main)
[![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![codecov](https://codecov.io/gh/fast-down/core/branch/main/graph/badge.svg)](https://codecov.io/gh/fast-down/core)
[![Latest version](https://img.shields.io/crates/v/fast-pull.svg)](https://crates.io/crates/fast-pull)
[![Documentation](https://docs.rs/fast-pull/badge.svg)](https://docs.rs/fast-pull)
[![License](https://img.shields.io/crates/l/fast-pull.svg)](https://github.com/fast-down/core/blob/main/LICENSE)

`fast-pull` is a low-level concurrent **pull/push streaming engine** for moving
byte ranges from any source to any sink.

**[Official Website (Simplified Chinese)](https://fd.s121.top/)**

## Features

1. **⚡️ Concurrent pull/push**
   Built on [`fast-steal`](https://github.com/fast-down/fast-steal) with optimized
   work-stealing across worker tasks, plus a single-threaded sequential path
   (`download_single`).
2. **🔌 `Puller` / `Pusher` abstractions**
   A download is just a `Puller` (source) feeding a `Pusher` (sink). Bring your
   own, or use the built-ins. `Puller` must be `Clone` so work can be stolen and
   retried; `Pusher` reports partial failures so the engine can retry them.
3. **💾 Multiple write paths** (feature-gated)
   - `file` — `StdFilePusher` (raw `std::fs::File` random-access writes) and
     `MmapFilePusher` (memory-mapped zero-copy writes), plus the ready-made
     `CacheFilePusher` stack.
   - `mem` — `MemPusher`, an in-memory sink backed by a shared `Vec<u8>`.
4. **🧩 Out-of-order & buffered writes**
   Cache decorators `CacheDirectPusher`, `CacheMergePusher`, and
   `CacheSeqPusher` absorb out-of-order chunks (keyed by `range.start`) and
   flush runs once a watermark is reached; `BufWriterPusher` batches contiguous
   writes like `std::io::BufWriter`.
5. **📈 Progress & cancellation**
   Streaming `Event`s (pull/push progress, errors, completion) are delivered on
   `DownloadResult::event_chain`, and a session is cancelled by
   `DownloadResult::abort` or simply dropping the last handle clone.
6. **🧪 Testing-friendly**
   `MockPuller` + `build_mock_data` give you a deterministic in-memory source for
   tests — no network or disk required.

## Usage

```rust
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use fast_pull::{
    mock::{build_mock_data, MockPuller},
    single::{download_single, DownloadOptions},
    ProgressEntry, Pusher,
};

/// A minimal in-memory [`Pusher`] so this example compiles with **no** optional
/// features. In real code, prefer `fast_pull::mem::MemPusher` (feature `mem`) or
/// a file pusher (feature `file`).
#[derive(Clone, Default)]
struct VecPusher {
    data: Arc<Mutex<Vec<u8>>>,
}

impl Pusher for VecPusher {
    type Error = std::convert::Infallible;
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)> {
        let mut g = self.data.lock().unwrap();
        if g.len() < range.end as usize {
            g.resize(range.end as usize, 0);
        }
        g[range.start as usize..range.end as usize].copy_from_slice(&content);
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let expected = build_mock_data(1024);
    let puller = MockPuller::new(&expected);
    let pusher = VecPusher::default();
    let out = pusher.data.clone();

    let result = download_single(
        puller,
        pusher,
        DownloadOptions {
            retry_gap: std::time::Duration::from_secs(1),
            push_queue_cap: 16,
        },
    );
    while result.event_chain().recv().await.is_ok() {}

    assert_eq!(&*out.lock().unwrap(), &expected);
}
```

## License

Licensed under the same terms as the rest of the `fast-down` workspace. Thanks to
[share121](https://github.com/share121), [Cyan](https://github.com/CyanChanges)
and other `fast-down` contributors.
