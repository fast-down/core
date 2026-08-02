# fast-down-api

[![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/main)](https://github.com/fast-down/core/commits/main)
[![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![codecov](https://codecov.io/gh/fast-down/core/branch/main/graph/badge.svg)](https://codecov.io/gh/fast-down/core)
[![Latest version](https://img.shields.io/crates/v/fast-down-api.svg)](https://crates.io/crates/fast-down-api)
[![Documentation](https://docs.rs/fast-down-api/badge.svg)](https://docs.rs/fast-down-api)
[![License](https://img.shields.io/crates/l/fast-down-api.svg)](https://github.com/fast-down/core/blob/main/LICENSE)

A convenient, high-level wrapper around [`fast-down`](https://github.com/fast-down/fast-down)
that turns the pull/push engine into a few lines of async code: spawn a download,
drain progress events, resume after interruption, and cancel cooperatively.

- **Concurrent, resumable downloads** powered by the `fast-down` engine (work-stealing, range requests).
- **Two entry points**: `download` (auto-resume when possible) and `resume` (hard error if it can't continue).
- **Event stream**: a single channel carries prefetch, per-worker progress, rename, and error events.
- **Cooperative cancellation**: cancelling mid-flight preserves the `.part` / `.fd` files so you can resume later.
- **Configurable**: threads, chunk size, write method (`Mmap` / `Std`), proxies, headers, retries, and more via `PartialConfig`.

## Quick start

```rust,no_run
use fast_down_api::{create_cancellation_token, create_channel, download, Event, PartialConfig};
use std::path::PathBuf;
use url::Url;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Channel for progress / lifecycle events, plus a cancellation token.
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();

    // 2. Configure the download. Every field is optional; unset fields
    //    fall back to Config::default(). (save_dir is required at runtime.)
    let config = PartialConfig {
        save_dir: Some(PathBuf::from("./downloads")),
        overwrite: Some(true),
        threads: Some(16),
        ..Default::default()
    };

    // 3. Start the download. This spawns a detached task and returns at once.
    let url = Url::parse("https://example.com/large-file.bin")?;
    download(url, config, tx, token.clone());

    // 4. Drain events until the task finishes or is cancelled.
    while let Ok(event) = rx.recv().await {
        match event {
            // Aggregated progress on a fixed cadence (Config::progress_emit_gap).
            // Convenient for a progress bar — no need to re-accumulate ranges.
            Event::Progress(sample) => {
                // `downloaded`, `percent`, and `total` are pre-computed for you;
                // the fields below are equivalent to deriving them from `progress`.
                let written: u64 = sample.progress.iter().map(|r| r.end - r.start).sum();
                assert_eq!(written, sample.downloaded);
                let pct = if sample.total > 0 {
                    written * 100 / sample.total
                } else {
                    0
                };
                // `eta` is the estimated remaining time, or `None` until a rate
                // can be measured.
                let eta_str = sample
                    .eta
                    .map_or_else(|| "?".to_string(), |d| format!("{d:?}"));
                println!(
                    "progress: {pct}% ({:.1}%)  {written}/{} bytes  \
                     {} B/s (inst)  {} B/s (avg)  elapsed {:?}  eta {eta_str}",
                    sample.percent, sample.total, sample.bps, sample.avg_bps, sample.elapsed
                );
            }
            Event::PushProgress(p) => println!("wrote range: {p:?}"),
            Event::Renamed(path) => {
                println!("done -> {path:?}");
                break;
            }
            Event::RenameFailed(e) => {
                eprintln!("rename failed: {e}");
                break;
            }
            Event::ResumeError(e) => eprintln!("resume error: {e}"),
            _ => {}
        }
    }

    // 5. Draining `rx` above already waits for completion: the loop ends when the
    //    task drops its last sender, so no separate join handle is needed.
    Ok(())
}
```

### Resuming an interrupted download

`resume` targets the existing `.part` file. If the download cannot be continued
(no `.fd` state, no range support, or the remote file changed) it emits
`Event::ResumeError` and **does not** silently restart — unlike `download`,
which auto-resumes when it can and otherwise falls back to a fresh download.

```rust,ignore
resume(
    "./downloads/large-file.bin.part", // the .part file from a previous run
    url,
    config,
    tx,
    token,
);
```

### Cancelling cooperatively

```rust,ignore
token.cancel(); // stops fetching, keeps .part / .fd so you can resume later
```

## API overview

| Item                                                                                                                        | Purpose                                                                        |
| --------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| [`download`](https://docs.rs/fast-down-api/latest/fast_down_api/fn.download.html) | Start a download; auto-resume when a valid `.fd` + `.part` exist, else fresh. Observe completion by draining the `Rx` from `create_channel` until it disconnects. |
| [`resume`](https://docs.rs/fast-down-api/latest/fast_down_api/fn.resume.html)     | Resume a specific `.part` file; hard-error (`Event::ResumeError`) if it can't. Completion is observed the same way, by draining `Rx`. |
| [`create_channel`](https://docs.rs/fast-down-api/latest/fast_down_api/fn.create_channel.html)                               | Create the `(Tx, Rx)` event channel.                                           |
| [`create_cancellation_token`](https://docs.rs/fast-down-api/latest/fast_down_api/fn.create_cancellation_token.html)         | Create a `CancellationToken` for cooperative cancellation.                     |
| [`Event`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.Event.html)                                               | The event enum delivered over the channel.                                     |
| [`PartialConfig`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.PartialConfig.html)                             | Layered, optional configuration for a download.                                |
| [`StateError`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.StateError.html)                                     | Errors surfaced via `Event::ResumeError`.                                      |

## How resume works

During a download the engine periodically persists a `.fd` state file next to
the `.part` partial file. That state records the byte ranges already written
(`downloaded_chunk`) and the remote file identity (`etag` / `last_modified` /
size). On the next run:

1. `download` loads the `.fd`, validates it still matches the remote (size + identity), and — if the `.part` file is present — emits `Event::Resumed` and continues from the recorded offset.
2. If validation fails (remote changed) or there is no `.part`, it starts fresh.
3. `resume` applies the same checks but, instead of falling back, reports `Event::ResumeError`.

Cancellation leaves both files in place, so a later `resume` (or `download`) can pick up exactly where it stopped.

## License

MIT — see [LICENSE](https://github.com/fast-down/core/blob/main/LICENSE).
