# fast-down-api

[![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/main)](https://github.com/fast-down/core/commits/main)
[![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![codecov](https://codecov.io/gh/fast-down/core/branch/main/graph/badge.svg)](https://codecov.io/gh/fast-down/core)
[![Latest version](https://img.shields.io/crates/v/fast-down-api.svg)](https://crates.io/crates/fast-down-api)
[![Documentation](https://docs.rs/fast-down-api/badge.svg)](https://docs.rs/fast-down-api)
[![License](https://img.shields.io/crates/l/fast-down-api.svg)](https://github.com/fast-down/core/blob/main/LICENSE)

A convenient, high-level wrapper around [`fast-down`](https://github.com/fast-down/fast-down)
that turns the pull/push engine into a few lines of async code: resolve a
download against the remote, inspect what it will do, run it, drain progress
events, and cancel cooperatively.

- **Concurrent, resumable downloads** powered by the `fast-down` engine (work-stealing, range requests).
- **One state type, one source of truth**: a [`Record`] is the `.fd` file itself, and every entry point resolves into the same [`Ready`] value before anything is written.
- **Two-phase by construction**: [`State::build`] prefetches, resolves paths, and probes the disk _without writing a single byte_, returning a [`Ready`] you can inspect before committing.
- **Event stream**: a single channel carries prefetch, disk allocation, per-worker progress, resume, rename, and lifecycle events. Every run ends with exactly one `Event::Terminated(TerminationReason)`.
- **Cooperative cancellation**: cancelling mid-flight preserves the `.part` / `.fd` files so you can resume later.
- **Configurable**: threads, chunk size, write method (`Mmap` / `Std`), proxies, headers, retries, disk pre-allocation, and more via `PartialConfig`.

## Quick start

```rust,no_run
use fast_down_api::{PartialConfig, ResumeOutcome, State, create_cancellation_token, create_channel};
use std::path::PathBuf;
use url::Url;

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
// 1. Channel for progress / lifecycle events, plus a cancellation token.
let (tx, _rx) = create_channel();
let token = create_cancellation_token();

// 2. Configure the download. Every field is optional; unset fields
//    fall back to Config::default(). (save_dir is required at runtime.)
let config = PartialConfig {
    save_dir: Some(PathBuf::from("./downloads")),
    threads: Some(16),
    ..Default::default()
};

// 3. Resolve the download. This prefetches the remote metadata and probes the
//    disk, but writes nothing: the returned `Ready` is a plan you can inspect.
let url = Url::parse("https://example.com/large-file.bin")?;
let ready = State::new(url, config).build(tx).await?;

// 4. Decide based on what starting would do.
match ready.resume_outcome() {
    ResumeOutcome::Resumable => println!("will continue from a previous run"),
    ResumeOutcome::Fresh => println!("will download the whole file"),
    ResumeOutcome::Mismatch(e) => println!("stale state, cannot resume: {e}"),
}
println!("final path: {:?}", ready.final_path());
println!("already fetched: {} ranges", ready.progress().len());
# let _ = token;
# Ok(())
# }
```

Starting the resolved download, draining its events, and cancelling mid-flight
are provided by `Ready` and the event stream; see the crate documentation for
the run API.

### Loading an existing `.fd`

When you already know where a state file lives — a UI rebuilding a list of
paused downloads, or a caller resuming one specific file — load it directly.
`State::load` pins the location, so `build` probes exactly that file instead of
the default `<final>.fd`.

```rust,no_run
use fast_down_api::{PartialConfig, State, create_channel};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let (tx, _rx) = create_channel();
let state = State::load("./downloads/large-file.bin.fd").await?;
let ready = state.build(tx).await?;
println!("resuming {:?}", ready.config_path());
# Ok(())
# }
```

### Cancelling cooperatively

```rust,no_run
use fast_down_api::create_cancellation_token;

let token = create_cancellation_token();
token.cancel(); // stops fetching, keeps .part / .fd so you can resume later
```

### Choosing what `start` does

`Ready::start` takes a policy that decides what happens to any recorded
`.fd`/`.part`. The common combinations are presets ([`StartMode`]):

| `StartMode` | Behavior |
| ----------- | -------- |
| `Auto`      | Resume when valid, otherwise download the whole file. |
| `Resume`    | Resume this specific state; report `Event::ResumeError` when it no longer matches the remote. |
| `Fresh`     | Ignore any recorded progress and download the whole file. |
| `Forced`    | Continue from a mismatch when the size still matches (identity headers changed but the byte layout did not). |

Pass a [`StartPolicy`] when you need per-dimension control — each field is the
action for one independent reason a resume might not be possible:

```rust,no_run
use fast_down_api::{Recovery, StartPolicy};

let policy = StartPolicy {
    // Resume a truncated `.part`? No — treat it as "nothing to continue" and re-download.
    truncated: Recovery::Fresh,
    // A remote identity change: refuse rather than silently restart.
    identity_changed: fast_down_api::IdentityRecovery::Fail,
    ..StartPolicy::auto()
};
# let _ = policy;
```

| Field | When it applies | Values |
| ----- | --------------- | ------ |
| `use_recorded` | overall | `bool` — `false` ignores all recorded progress |
| `no_state` | no `.fd`, or its `.part` is gone | `Recovery` |
| `truncated` | `.part` shorter than the recorded progress | `Recovery` |
| `unreadable` | `.fd` cannot be read/decoded | `Recovery` |
| `size_changed` | remote size changed | `Recovery` |
| `not_resumable` | server does not support ranges | `Recovery` |
| `identity_changed` | identity headers changed, size unchanged | `IdentityRecovery` |

`Recovery` is `Fresh` (download the whole file) or `Fail` (emit
`Event::ResumeError`). `IdentityRecovery` adds `Force` (continue anyway) — only
offered where the byte layout is known intact, so a forced resume cannot splice
two versions together.

## API overview

| Item | Purpose |
| ---- | ------- |
| [`State`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.State.html) | A download that has not been resolved yet. `State::new` starts from a URL; `State::load` reads an existing `.fd`. |
| [`State::build`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.State.html) | Prefetch + resolve paths + probe the disk, returning a [`Ready`] **without writing anything**. |
| [`Ready`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.Ready.html) | A resolved download. Inspect with `resume_outcome`, `final_path`, `config_path`, `tmp_path`, `progress`, `info`, then run it. |
| [`StartMode`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.StartMode.html) | A preset for how to start: `Auto`, `Resume`, `Fresh`, `Forced`. Accepted by `Ready::start`. |
| [`StartPolicy`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.StartPolicy.html) | Per-dimension start policy (what to do for each reason a resume might fail). Also accepted by `Ready::start`. |
| [`ResumeOutcome`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.ResumeOutcome.html) | `Fresh` (nothing to continue), `Resumable`, or `Mismatch(StateError)`. |
| [`Record`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.Record.html) | The `.fd` state file: URL, remote identity, elapsed time, and the merged config (which carries `downloaded_chunk`). |
| [`create_channel`](https://docs.rs/fast-down-api/latest/fast_down_api/fn.create_channel.html) | Create the `(Tx, Rx)` event channel. |
| [`create_cancellation_token`](https://docs.rs/fast-down-api/latest/fast_down_api/fn.create_cancellation_token.html) | Create a `CancellationToken` for cooperative cancellation. |
| [`Event`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.Event.html) | The event enum delivered over the channel. |
| [`PartialConfig`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.PartialConfig.html) | Layered, optional configuration for a download. |
| [`StateError`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.StateError.html) / [`PlanError`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.PlanError.html) | Errors from loading/probing state and from resolving a plan. |

## How resume works

A download's progress lives in a `.fd` state file, written next to its `.part`
partial file. The state records the byte ranges already written
(`downloaded_chunk`), the remote file identity (`etag` / `last_modified` /
size), and the accumulated active time. The `.part` path is **not** stored — it
is always the `.fd` path with its extension swapped to `.part`, so the two move
together and no path can be hand-edited to redirect a write.

Resolving a download against the remote:

1. `State::build` prefetches the remote and computes the output path.
2. It loads the candidate `.fd` (the pinned one, or the default `<final>.fd`).
3. The record is validated against the freshly fetched identity **before** it is
   refreshed, and the `.part` is measured against the recorded progress:
   - matching identity and a long-enough `.part` → `Resumable`,
   - a changed remote or a truncated `.part` → `Mismatch`,
   - no `.fd` (or a missing `.part`) → `Fresh`.

Cancellation leaves both files in place, so a later resolve can pick up exactly
where the run stopped.

Every run — whether it completes, is cancelled, stops incomplete, or fails —
ends with exactly one `Event::Terminated(TerminationReason)` as the last event
on the channel, so draining `Rx` until `Terminated` is the reliable way to know
a run has finished.

## License

MIT — see [LICENSE](https://github.com/fast-down/core/blob/main/LICENSE).
