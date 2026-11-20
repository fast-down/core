use crate::{PartialConfig, StateError, TaskError, WriteMethod};
use fast_down::{
    ProgressEntry, UrlInfo, WorkerId,
    http::HttpError,
    reqwest::{BuildClientError, SmartRedirectClient},
};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

/// Events emitted by a download run, consumed through the crossfire channel
/// returned by [`crate::create_channel`].
///
/// Events cover the full lifecycle: prefetch ([`Event::Prefetch`]), pipeline
/// setup ([`Event::Start`]), per-worker fetch/write progress
/// ([`Event::Pulling`] … [`Event::Finished`]), aggregated progress
/// ([`Event::Progress`]), resume ([`Event::Resumed`], [`Event::ResumeError`]),
/// and completion ([`Event::Renamed`]). Error variants (`*Error`) report failures
/// without aborting the stream, so a consumer can decide whether to retry,
/// cancel, or surface them in a UI.
///
/// Every run ends with exactly one [`Event::Terminated`], which is always the
/// last event on the channel. A consumer that only needs the outcome can wait
/// for it instead of draining until the channel disconnects.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    /// Emitted after the prefetch step resolves the remote file's metadata.
    ///
    /// Carries the [`UrlInfo`] (size, identity headers, range support) so a
    /// caller can inspect the remote before or while the download proceeds.
    Prefetch(UrlInfo),
    /// The prefetch request failed (server unreachable, non-2xx response, a
    /// request that outlived its time budget, a response that cannot be used as
    /// the resource's bytes, etc.).
    ///
    /// Fatal for the download: without [`UrlInfo`] the engine cannot plan ranges.
    PrefetchError(HttpError<SmartRedirectClient>),
    /// Failed to compute the output / `.part` / `.fd` paths.
    ///
    /// For example the target directory is not writable or the file name is
    /// invalid on this platform.
    GenPathError(std::io::Error),
    /// A new download has nowhere to write: [`Config::save_dir`](crate::Config)
    /// is empty and there is no `.fd` whose directory could be reused.
    ///
    /// Resuming a pinned state file does not need one: the pair's own directory
    /// is the destination.
    MissingSaveDir,
    /// Persisting or removing the `.fd` state file failed (see [`StateError`]).
    ///
    /// Non-fatal for the current run, but the download can no longer be resumed
    /// reliably if it is interrupted afterwards. On a completed download the
    /// failure means the finished run's `.fd` could not be removed, so the next
    /// run treats the output as a fresh download.
    StateSaveError(StateError),
    /// Building the HTTP client failed (TLS / backend initialization, etc.).
    BuildClientError(BuildClientError),
    /// A configured request header was rejected (invalid name or value) and is
    /// not sent.
    ///
    /// Non-fatal: the request proceeds without that header.
    InvalidHeader(String),
    /// Creating the output sink — opening the `.part` file — failed.
    BuildPusherError(std::io::Error),
    /// Disk space for the whole file is about to be reserved, carrying the
    /// target size in bytes.
    ///
    /// Emitted whenever the remote size is known, for both the required
    /// reservation ([`crate::Config::pre_alloc`]) and the best-effort one. A
    /// required reservation always ends in [`Event::Allocated`] or
    /// [`Event::AllocError`]; the best-effort one reports success only where the
    /// platform has a fast path, so it is sometimes followed by neither.
    ///
    /// The reservation itself can take a while where the platform has no fast
    /// path (`pre_alloc` then writes zeros across the whole file); this event
    /// exists so a UI can say so instead of appearing frozen.
    Allocating(u64),
    /// Disk space reservation for the whole file finished.
    ///
    /// The success counterpart to [`Event::Allocating`], carrying the reserved
    /// size. Only emitted when the file was actually reserved to that size: the
    /// best-effort (`pre_alloc = false`) path is a no-op where the platform has
    /// no fast-reservation path, so it reports nothing rather than a successful
    /// allocation that did not happen. A failure is [`Event::AllocError`].
    Allocated(u64),
    /// Reserving disk space failed.
    ///
    /// Non-fatal: the download continues and the file grows on demand. The
    /// trade-off is more fragmentation and the chance of running out of space
    /// mid-download rather than up front.
    AllocError(std::io::Error),
    /// The writer actually used for this run.
    ///
    /// The configured [`crate::Config::write_method`] can be lowered at runtime
    /// — a 32-bit target, an unresumable remote, or a non-ranged download all
    /// select the buffered `CacheFilePusher` regardless — so this reports the
    /// effective choice ([`WriteMethod::Mmap`] for the memory-mapped pusher,
    /// [`WriteMethod::Std`] for the cached one) rather than what was configured.
    WriteMethodChosen(WriteMethod),
    /// The final rename of the `.part` file to its destination failed.
    ///
    /// The success counterpart is [`Event::Renamed`]. The bytes are already on
    /// disk under the `.part` name, so they can still be resumed or retried.
    RenameFailed(std::io::Error),
    /// Emitted after the `.part` file is successfully renamed to its final
    /// destination. Carries the actual landing path, which in unique mode may
    /// differ from the originally-planned name (e.g. `xxx (1).mp4`) when the
    /// target got occupied during the download.
    Renamed(PathBuf),
    /// Emitted once the pipeline is set up and writing is about to begin.
    ///
    /// Carries the `.part` path, the `.fd` state-file path, and the resolved
    /// [`PartialConfig`] used for this run: the caller's overrides merged with
    /// any resumed progress, then folded against the crate defaults, so a field
    /// that is `None` is at its crate default rather than missing. Call
    /// `build()` on it for the effective configuration.
    ///
    /// `parsed_config` is boxed: `PartialConfig` is ~400 bytes, and inlining it
    /// would make **every** `Event` that large — including the per-chunk
    /// `PullProgress`/`PushProgress` hot path. `Start` fires once per run, so the
    /// one extra allocation is negligible next to shrinking `Event` by ~60%.
    Start {
        /// The `.part` file being written.
        tmp_path: PathBuf,
        /// The `.fd` state file being written.
        fd_path: PathBuf,
        /// The resolved [`PartialConfig`] actually used for this run.
        parsed_config: Box<PartialConfig>,
    },
    /// Emitted when a download resumes from a previously-saved state, before
    /// [`Event::Start`]. Carries the progress that will be continued from and
    /// the total file size, so a UI can show e.g. "resuming from 42%".
    Resumed {
        /// The `.fd` state file the resume continues from.
        fd_path: PathBuf,
        /// The byte ranges already recorded.
        progress: Vec<ProgressEntry>,
        /// The remote file size.
        size: u64,
    },
    /// Emitted when the start policy refuses to continue a `.fd`/`.part` pair
    /// that exists but can no longer be resumed.
    ///
    /// [`StartMode::Auto`](crate::StartMode::Auto) never emits it — it falls
    /// back to a full re-download instead. The modes that refuse a mismatch
    /// ([`StartMode::Resume`](crate::StartMode::Resume), and
    /// [`StartMode::Forced`](crate::StartMode::Forced) when a forced resume
    /// would be unsound) report it here so the caller can decide what to do.
    ResumeError(StateError),
    /// Another run already owns the same `.fd`/`.part` pair, in this process or
    /// in another one.
    ///
    /// Only one run may write a given state file: a second run would interleave
    /// writes to the `.part` and overwrite the `.fd` progress. The claim is one
    /// advisory OS lock per normalized `.fd` path (a file under the system temp
    /// directory), so a run in another process is refused exactly like a second
    /// run in this one.
    AlreadyRunning(PathBuf),
    /// The claim's OS lock could not be acquired: its lock file under the system
    /// temp directory could not be created, opened, or locked.
    ///
    /// Fatal for this run, because without the lock a second process could
    /// write the same pair. Distinct from [`Event::AlreadyRunning`], which says
    /// a live owner holds the lock for an unknown time: retrying this run later
    /// can succeed.
    ClaimUnavailable(std::io::Error),

    /// Worker `id` started fetching its assigned byte range from the network.
    Pulling(WorkerId),
    /// Worker `id` failed to fetch its assigned range (network / decode error).
    PullError(WorkerId, TaskError),
    /// Worker `id`'s fetch exceeded its time budget and was aborted.
    PullTimeout(WorkerId),
    /// No chunk arrived for [`crate::Config::stall_timeout`], so the run was
    /// cancelled instead of retrying forever. Carries the configured timeout.
    ///
    /// The accepted bytes stay in the `.part`, so a later resume continues from
    /// them.
    Stalled(Duration),
    /// Worker `id` pulled some bytes into memory.
    ///
    /// `ProgressEntry` describes the contiguous range that just arrived from the
    /// network but is not yet written to disk.
    PullProgress(WorkerId, ProgressEntry),
    /// Worker `id` is handing a pulled range to the sink (writing it to `.part`).
    ///
    /// `ProgressEntry` is the range being written.
    Pushing(WorkerId, ProgressEntry),
    /// Worker `id` failed to write `ProgressEntry` to the sink.
    PushError(WorkerId, ProgressEntry, TaskError),
    /// A range was written to the sink.
    ///
    /// `ProgressEntry` is the range persisted (at least to the OS page cache) by
    /// this write. This drives the progress bar and the resume bookkeeping; it is
    /// intentionally emitted before `fsync`, so it reflects "written", not
    /// "durably flushed".
    PushProgress(ProgressEntry),

    /// Aggregated download progress, emitted on a fixed cadence
    /// ([`crate::Config::progress_emit_gap`]) by a dedicated reporter task.
    ///
    /// Unlike [`Event::PushProgress`], which fires once per written byte range,
    /// this carries a full [`DownloadStats`] — the current snapshot plus
    /// per-direction transfer rates and per-worker stats — so a consumer can
    /// render a progress bar directly without re-accumulating individual ranges.
    ///
    /// The reporter runs on its own timer, so the cadence is driven purely by
    /// `progress_emit_gap` and is never delayed by flushing, state saving, event
    /// forwarding, or a slow consumer (the channel is unbounded). One final
    /// `Progress` is sent when the run ends (success, cancellation, or error).
    Progress(DownloadStats),
    /// The sink is being flushed and synced to the `.part` file.
    Flushing,
    /// Flushing / syncing the sink failed.
    FlushError(TaskError),
    /// Worker `id` completed its assigned range and exited.
    Finished(WorkerId),

    /// The run has ended. Sent exactly once, as the last event on the channel.
    ///
    /// Preceding `*Error` events carry the details of whatever went wrong; this
    /// one only reports the outcome.
    Terminated(TerminationReason),
}

/// How a download run ended, carried by [`Event::Terminated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminationReason {
    /// Every byte was written and the `.part` file was renamed into place.
    /// [`Event::Renamed`] carries the path it landed on.
    Completed,
    /// The [`CancellationToken`](crate::CancellationToken) was
    /// triggered. A run cancelled before any byte was recorded removes the empty
    /// pair it had just created; otherwise the `.part` and `.fd` files are left
    /// on disk so a later resume can pick up where this run stopped.
    Cancelled,
    /// The run stopped on its own without completing the file — every worker
    /// gave up, for example because the connection kept failing. The `.part` and
    /// `.fd` files are left on disk, so the next resolve resumes from the
    /// recorded progress.
    Incomplete,
    /// A fatal error ended the run: metadata could not be fetched, the output
    /// file could not be opened, the rename failed, and so on.
    Failed,
}

/// Full snapshot of a run's progress and throughput, carried by
/// [`Event::Progress`].
///
/// Aggregates the engine's per-worker fetch/write events into one value a UI can
/// render directly: the written ranges, pull-side and push-side totals, and one
/// row per worker.
#[derive(Debug, Clone)]
pub struct DownloadStats {
    /// Already-written byte ranges — the **push** side, and the same source of
    /// truth used for resume bookkeeping in the `.fd` record (normalized and
    /// de-duplicated).
    pub progress: Vec<ProgressEntry>,
    /// Remote file size in bytes.
    pub total: u64,
    /// Total active download time accumulated across all resume runs, persisted
    /// in the `.fd` state so a resume continues the clock.
    pub elapsed: Duration,
    /// Aggregated pull-side stats: deduplicated coverage of fetched ranges.
    pub pull: TransferStats,
    /// Aggregated push-side stats: bytes written to disk.
    pub push: TransferStats,
    /// Per-worker pull stats, keyed by worker id (ordered).
    pub workers: BTreeMap<WorkerId, WorkerStats>,
}

/// Aggregate statistics for one direction, either pull or push.
#[derive(Debug, Clone, Copy)]
pub struct TransferStats {
    /// Bytes covered by this direction. The pull side can run ahead of the push
    /// side while the write queue is buffering.
    pub downloaded: u64,
    /// Smoothed recent rate in bytes/second (EMA, time constant ≈ 3s), sampled
    /// once per progress report. `0` on the first emit and on the final emit; a
    /// stalled transfer decays toward zero instead of freezing at its last value.
    pub bps: u64,
    /// Average rate in bytes/second over the whole download session (all resume
    /// runs combined): `downloaded / elapsed`. `0` while elapsed is too small.
    pub avg_bps: u64,
    /// Download percentage as a `0.0..=100.0` value (`downloaded * 100 /
    /// total`); `0.0` when `total == 0`.
    pub percent: f64,
    /// Estimated time remaining, computed as `(total - downloaded) / rate`
    /// where `rate` is the smoothed recent `bps` (preferred: it reflects the
    /// *current* speed, so the estimate stays honest after a resume at a
    /// different speed), falling back to `avg_bps` while `bps` is still zero.
    /// `None` until a rate can be measured and `Some(Duration::ZERO)` once
    /// `downloaded == total`.
    pub eta: Option<Duration>,
}

impl TransferStats {
    /// Build one direction's stats from its running counters.
    ///
    /// This is the single place that relates the four derived figures:
    /// `avg_bps` is `downloaded / elapsed`, `percent` is `downloaded / total`,
    /// and `eta` divides the remaining bytes by `bps`, falling back to
    /// `avg_bps` until the smoothed rate warms up.
    #[must_use]
    pub fn new(downloaded: u64, bps: u64, total: u64, elapsed: Duration) -> Self {
        let avg_bps = Self::avg_bps(downloaded, elapsed);
        Self {
            downloaded,
            bps,
            avg_bps,
            percent: Self::percent(downloaded, total),
            eta: Self::eta(total, downloaded, bps, avg_bps),
        }
    }

    /// Percentage of `total` covered by `downloaded`; `0.0` for a zero-size file.
    fn percent(downloaded: u64, total: u64) -> f64 {
        if total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                downloaded as f64 / total as f64 * 100.0
            }
        }
    }

    /// Average rate in bytes/second over `elapsed`.
    fn avg_bps(downloaded: u64, elapsed: Duration) -> u64 {
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        downloaded
            .saturating_mul(1000)
            .checked_div(elapsed_ms)
            .unwrap_or(0)
    }

    /// Remaining time at the recent rate, falling back to the average rate.
    fn eta(total: u64, downloaded: u64, bps: u64, avg_bps: u64) -> Option<Duration> {
        let remaining = total.saturating_sub(downloaded);
        let rate = if bps > 0 { bps } else { avg_bps };
        if rate == 0 {
            None
        } else {
            remaining
                .saturating_mul(1000)
                .checked_div(rate)
                .map(Duration::from_millis)
        }
    }
}

/// One worker's pull-side statistics.
#[derive(Debug, Clone, Copy)]
pub struct WorkerStats {
    /// The worker's most recent activity.
    pub state: WorkerState,
    /// Total bytes this worker fetched from the network (raw, including any
    /// re-fetched ranges).
    pub pulled: u64,
    /// Smoothed recent pull rate for this worker in bytes/second. Not part of
    /// the aggregate: [`DownloadStats::pull`] counts deduplicated coverage,
    /// while this counts raw fetches, so the workers do not sum to it.
    pub bps: u64,
}

/// What a worker is currently doing, derived from the engine's lifecycle
/// events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkerState {
    /// Fetching its assigned range from the network.
    Pulling,
    /// Handing a pulled range to the writer.
    Pushing,
    /// Completed its work and exited.
    Finished,
    /// Its fetch exceeded the time budget; the range will be re-pulled.
    TimedOut,
}
