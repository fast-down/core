# fast-down-api

[![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/main)](https://github.com/fast-down/core/commits/main)
[![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![codecov](https://codecov.io/gh/fast-down/core/branch/main/graph/badge.svg)](https://codecov.io/gh/fast-down/core)
[![Latest version](https://img.shields.io/crates/v/fast-down-api.svg)](https://crates.io/crates/fast-down-api)
[![Documentation](https://docs.rs/fast-down-api/badge.svg)](https://docs.rs/fast-down-api)
[![License](https://img.shields.io/crates/l/fast-down-api.svg)](https://github.com/fast-down/core/blob/main/LICENSE)

A convenient, high-level wrapper around
[`fast-down`](https://github.com/fast-down/core/tree/main/crates/fast-down) that
turns the pull/push engine into a few lines of async code: resolve a download
against the remote, inspect what it will do, run it, drain progress events, and
cancel cooperatively.

- **Concurrent, resumable downloads** powered by the `fast-down` engine (work-stealing, range requests).
- **One state type, one source of truth**: a [`Record`] is the `.fd` file itself, and every entry point resolves into the same [`Ready`] value before anything is written.
- **Two-phase by construction**: [`State::build`] prefetches, resolves paths, and probes the disk _without writing a single byte_, returning a [`Ready`] you can inspect before committing.
- **Event stream**: a single channel carries prefetch, disk allocation, per-worker progress, resume, rename, and lifecycle events. Every run ends with exactly one `Event::Terminated(TerminationReason)`.
- **Cooperative cancellation**: cancelling mid-flight preserves the `.part` / `.fd` files so you can resume later.
- **Configurable**: threads, chunk size, write method (`Mmap` / `Std`), proxies, headers, retries, disk pre-allocation, and more via `PartialConfig`.
- **Concurrency-limited queue**: a [`Queue`] runs many downloads with at most N in
  flight, reporting every task on one key-tagged event stream.

## Quick start

```rust,no_run
use fast_down_api::{
    Ctx, Event, PartialConfig, ResumeOutcome, StartMode, State, create_channel,
};
use std::path::PathBuf;
use url::Url;

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
// 1. Event channel, plus the run's cancellation / control handle.
let (tx, rx) = create_channel();
let ctx = Ctx::new();

// 2. Configure the download. Every field is optional; unset fields
//    fall back to Config::default(). A new download needs `save_dir`;
//    a resumed `.fd` takes its directory from the state file instead.
let config = PartialConfig {
    save_dir: Some(PathBuf::from("./downloads")),
    threads: Some(16),
    ..Default::default()
};

// 3. Resolve the download. This prefetches the remote metadata and probes the
//    disk, but writes nothing: the returned `Ready` is a plan you can inspect.
let url = Url::parse("https://example.com/large-file.bin")?;
let ready = State::new(url, config).build(&tx).await?;

// 4. Decide based on what starting would do.
match &ready.outcome {
    ResumeOutcome::Resumable => println!("will continue from a previous run"),
    ResumeOutcome::Fresh => println!("will download the whole file"),
    ResumeOutcome::Mismatch(e) => println!("stale state, cannot resume: {e}"),
}
println!("final path: {:?}", ready.final_path);
println!("already fetched: {} ranges", ready.progress().len());

// 5. Start it. `start` consumes the plan and returns at once; events flow on
//    `rx` while the run proceeds.
let handle = ready.start(StartMode::Auto, tx, &ctx);

// 6. Drain until the terminal event. The contract guarantees exactly one
//    `Event::Terminated`, as the last event on the channel.
while let Ok(event) = rx.recv().await {
    match event {
        Event::Progress(p) => println!("{:5.1}%  {} B/s", p.push.percent, p.push.bps),
        Event::Renamed(path) => println!("renamed -> {}", path.display()),
        Event::Terminated(_) => break,
        _ => {}
    }
}

// 7. The outcome comes from the handle (a `Future<Output =
//    TerminationReason>`), not from the event stream.
println!("ended: {:?}", handle.await);
# Ok(())
# }
```

### Naming the output file

`PartialConfig::filename` is used verbatim unless `parse_filename` is also set.
With `parse_filename`, it becomes a template expanded in two passes:

1. chrono `strftime` `%`-sequences against the **local** time — `%Y-%m-%d`
   becomes today's date.
2. The `{...}` placeholders below, over the already-expanded string.

| Placeholder | Expands to |
| ----------- | ---------- |
| `{host}` | The URL's host, or `unknown` when it has none. |
| `{parent_path}` | The URL's directory segments joined with the platform separator (`.` when there are none). |
| `{file_name}` | The remote file name. |
| `{file_stem}` | `{file_name}` without its last extension. |
| `{file_ext}` | The last extension, including its dot. |

`{parent_path}` may add subdirectories under `save_dir`; a template can never
resolve outside it.

Time expansion is all-or-nothing: an invalid sequence (`%Q`, a lone trailing
`%`) skips the time pass and survives literally, while the placeholders still
expand. A `%` inside a placeholder's value stays literal too, because the
placeholders are substituted after the time pass.

> **A `%` in the template makes resume time-dependent.** The default state file
> is `<final>.fd`, derived from the resolved path, so a run started yesterday
> records its state under yesterday's date; today's `State::new` probes today's
> path, finds nothing, and silently downloads the whole file again. The same
> template also resolves differently in another time zone or on another day, so
> do not cache the resolved path. To resume such a run, pin its state file with
> `State::at_path`.

```rust,no_run
use fast_down_api::PartialConfig;
use std::path::PathBuf;

let config = PartialConfig {
    save_dir: Some(PathBuf::from("./downloads")),
    // e.g. ./downloads/2026/11/15/example.com/report.pdf
    filename: Some("%Y/%m/%d/{host}/{file_name}".to_string()),
    parse_filename: Some(true),
    ..Default::default()
};
# let _ = config;
```

### Resuming a specific `.fd`

When you already know where a state file lives — a UI rebuilding a list of
paused downloads, or a caller resuming one specific file — pin it with
`State::at_path`. Nothing is read there; `build` reads that one file and
classifies it against the remote, instead of probing the default `<final>.fd`.

The pinned path also decides where the finished file lands: the `.part` sits
next to the `.fd` and is renamed from there, so `build` resolves the destination
under the `.fd`'s directory and ignores `save_dir`. A `filename` template still
applies, relative to that directory — which is why the resume below needs no
`save_dir` at all.

```rust,no_run
use fast_down_api::{PartialConfig, State, create_channel};
use std::path::PathBuf;
use url::Url;

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let (tx, _rx) = create_channel();
let url = Url::parse("https://example.com/large-file.bin")?;
let fd = PathBuf::from("./downloads/large-file.bin.fd");
let ready = State::at_path(url, fd, PartialConfig::default())
    .build(&tx)
    .await?;
println!("resuming {:?}", ready.fd_path);
# Ok(())
# }
```

To inspect a `.fd` **without any network access** — list what is resumable, or
validate a file you located yourself — use `Record::load`, which decodes it and
touches no HTTP:

```rust,no_run
use fast_down_api::Record;

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let record = Record::load("./downloads/large-file.bin.fd").await?;
println!("{} bytes in {} ranges", record.downloaded(), record.progress().len());
# Ok(())
# }
```

### Cancelling cooperatively

```rust,no_run
use fast_down_api::Ctx;

let ctx = Ctx::new();
ctx.cancel(); // stops fetching, keeps .part / .fd so you can resume later
```

`RunHandle::abort()` cancels the same token (`RunHandle` wraps its `Ctx`). A
cancelled run resolves to `TerminationReason::Cancelled`.

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

### Running many downloads: the queue

[`Queue`] runs many downloads with a bounded concurrency. Push a [`State`] (plus
the [`StartMode`] / [`StartPolicy`] to start it with) under your own task key `K`
(a download manager's task id, say) and the queue resolves and runs it as soon as
a slot frees, so the limit also covers prefetching. Every task reports into one
[`QueueEvent`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.QueueEvent.html)
stream, tagged with its key. The queue only schedules: progress, speed and
outcome are each task's own [`Event`]s on that stream.

```rust,no_run
use fast_down_api::{
    Event, PartialConfig, Queue, QueueEvent, StartMode, State, create_queue_channel,
};
use std::path::PathBuf;
use url::Url;

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let (tx, rx) = create_queue_channel::<i32>();
let queue = Queue::<i32>::new(3, tx); // at most three downloads in flight

// `State::new` starts a *new* download, so it needs a `save_dir`; only a
// pinned `.fd` (`State::at_path`) takes its directory from the state file.
let config = PartialConfig {
    save_dir: Some(PathBuf::from("./downloads")),
    ..Default::default()
};

for (gid, url) in ["https://example.com/a.bin", "https://example.com/b.bin"]
    .into_iter()
    .enumerate()
{
    let state = State::new(Url::parse(url)?, config.clone());
    queue.push(gid as i32, state, StartMode::Auto)?;
}

// Drop the last handle and the stream ends once every task has.
drop(queue);
while let Ok(event) = rx.recv().await {
    match event {
        QueueEvent::Task(_, Event::Terminated(reason)) => println!("task ended: {reason:?}"),
        _ => {}
    }
}
# Ok(())
# }
```

Every event carries the caller's key, so the queue needs no id mapping of its
own. `push` requires its key to be unique while the task is alive: a re-used key
is rejected with `QueueError::AlreadyQueued`, so it cannot clobber a still-running
task.

Every task is acknowledged with `Queued` when pushed; a task only publishes its
run events once it has **started**: `Started` opens its run and its engine events
follow, each ending with exactly one `Event::Terminated`. A task that never
started — still waiting when every handle is dropped — emits only that `Queued`
and leaves no files on disk, so a download manager can keep its own task manifest
and re-adopt the waiting tasks on the next start. A waiting task removed with
`Queue::cancel` emits `QueueEvent::Cancelled` instead of `Started`/`Terminated`.

## API overview

| Item | Purpose |
| ---- | ------- |
| [`State`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.State.html) | A download that has not been resolved yet. `State::new` starts from a URL; `State::at_path` pins an existing `.fd`. Neither reads the disk. |
| [`State::build`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.State.html) | Prefetch + resolve paths + probe the disk, returning a [`Ready`] **without writing anything**. |
| [`Ready`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.Ready.html) | A resolved download. Inspect its public fields (`outcome`, `final_path`, `fd_path`, `info`) plus `progress()` / `tmp_path()`, then run it. |
| [`StartMode`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.StartMode.html) | A preset for how to start: `Auto`, `Resume`, `Fresh`, `Forced`. Accepted by `Ready::start`. |
| [`StartPolicy`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.StartPolicy.html) | Per-dimension start policy (what to do for each reason a resume might fail). Also accepted by `Ready::start`. |
| [`ResumeOutcome`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.ResumeOutcome.html) | `Fresh` (nothing to continue), `Resumable`, or `Mismatch(StateError)`. |
| [`Ctx`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.Ctx.html) | The run's cancellation token + live worker-pool control, passed to `Ready::start`: `cancel`, `set_threads`, `set_min_chunk_size`. |
| [`RunHandle`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.RunHandle.html) | Returned by `Ready::start`: delegates `set_threads` / `set_min_chunk_size` / `abort` to its `Ctx`, and `await`s to the `TerminationReason`. |
| [`Record`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.Record.html) | The `.fd` state file: URL, remote identity, elapsed time, and the merged config (which carries `downloaded_chunk`). `Record::load` reads it offline. |
| [`create_channel`](https://docs.rs/fast-down-api/latest/fast_down_api/fn.create_channel.html) | Create the `(Tx, Rx)` event channel. |
| [`Event`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.Event.html) | The event enum delivered over the channel. |
| [`PartialConfig`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.PartialConfig.html) | Layered, optional configuration for a download. |
| [`Queue`](https://docs.rs/fast-down-api/latest/fast_down_api/struct.Queue.html) | A bounded-concurrency queue of downloads keyed by your own id `K`: `push` a [`State`] under a key and it is resolved and run when a slot frees. |
| [`QueueEvent`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.QueueEvent.html) | The queue's key-tagged per-task event stream. |
| [`QueueError`](https://docs.rs/fast-down-api/latest/fast_down_api/enum.QueueError.html) | What a queue operation refused (`NoRuntime`, `AlreadyQueued`). |
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
2. It reads the candidate `.fd` **once** (the pinned one, or the default
   `<final>.fd`).
3. The record is validated against the freshly fetched identity **before** it is
   refreshed, and the `.part` is measured against the recorded progress:
   - matching identity and a long-enough `.part` → `Resumable`,
   - a changed remote or a truncated `.part` → `Mismatch`,
   - no `.fd` (or a missing `.part`) → `Fresh`.

The default `<final>.fd` is derived from the resolved output path, so a
`filename` template containing `%`-sequences moves it with the date (see
[Naming the output file](#naming-the-output-file)). Pin the state file with
`State::at_path` when resuming such a template.

Cancellation leaves both files in place, so a later resolve can pick up exactly
where the run stopped. A run cancelled before any byte was recorded removes the
empty pair it had just created instead.

Every run — whether it completes, is cancelled, stops incomplete, or fails —
ends with exactly one `Event::Terminated(TerminationReason)` as the last event
on the channel, so draining `Rx` until `Terminated` is the reliable way to know
a run has finished.

## License

MIT — see [LICENSE](https://github.com/fast-down/core/blob/main/LICENSE).
