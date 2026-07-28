use crate::{PartialConfig, StateError};
use fast_down::{ProgressEntry, UrlInfo, WorkerId, reqwest::ReqwestResponseError};
use std::{path::PathBuf, sync::Arc};

/// Events emitted by a download run, consumed through the crossfire channel
/// returned by [`crate::create_channel`].
///
/// Events cover the full lifecycle: prefetch ([`Event::Prefetch`]), pipeline
/// setup ([`Event::Start`]), per-worker fetch/write progress
/// ([`Event::Pulling`] … [`Event::Finished`]), resume ([`Event::Resumed`],
/// [`Event::ResumeError`]), and completion ([`Event::Renamed`]). Error variants
/// (`*Error`) report failures without aborting the stream, so a consumer can
/// decide whether to retry, cancel, or surface them in a UI.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /// Emitted after the prefetch step resolves the remote file's metadata.
    ///
    /// Carries the [`UrlInfo`] (size, identity headers, range support) so a
    /// caller can inspect the remote before or while the download proceeds.
    Prefetch(UrlInfo),
    /// The prefetch request failed (server unreachable, non-2xx response, etc.).
    ///
    /// Fatal for the download: without [`UrlInfo`] the engine cannot plan ranges.
    PrefetchError(ReqwestResponseError),
    /// Failed to compute the output / `.part` / `.fd` paths.
    ///
    /// For example the target directory is not writable or the file name is
    /// invalid on this platform.
    GenPathError(std::io::Error),
    /// Persisting the `.fd` state file failed (see [`StateError`]).
    ///
    /// Non-fatal for the current run, but the download can no longer be resumed
    /// reliably if it is interrupted afterwards.
    StateSaveError(StateError),
    /// Building the HTTP client failed (TLS / backend initialization, etc.).
    BuildClientError(reqwest::Error),
    /// Creating the output sink — opening the `.part` file — failed.
    BuildPusherError(std::io::Error),
    /// A spawned download worker task panicked or was cancelled.
    ///
    /// The engine joins its worker tasks; if a join handle reports an error it is
    /// surfaced here rather than silently dropping the panic.
    JoinError(Arc<tokio::task::JoinError>),
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
    /// [`PartialConfig`] actually used for this run (after merging any resumed
    /// progress and applying defaults).
    Start {
        tmp_path: PathBuf,
        config_path: PathBuf,
        parsed_config: PartialConfig,
    },
    /// Emitted when a download resumes from a previously-saved state, before
    /// [`Event::Start`]. Carries the progress that will be continued from and
    /// the total file size, so a UI can show e.g. "resuming from 42%".
    Resumed {
        config_path: PathBuf,
        progress: Vec<ProgressEntry>,
        size: u64,
    },
    /// Emitted when an explicit `resume()` call cannot continue the download.
    /// Unlike `download()` (which silently falls back to a full re-download),
    /// `resume()` reports the failure so the caller can decide what to do.
    ResumeError(StateError),

    /// Worker `id` started fetching its assigned byte range from the network.
    Pulling(WorkerId),
    /// Worker `id` failed to fetch its assigned range (network / decode error).
    PullError(WorkerId, anyhow::Error),
    /// Worker `id`'s fetch exceeded its time budget and was aborted.
    PullTimeout(WorkerId),
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
    PushError(WorkerId, ProgressEntry, anyhow::Error),
    /// A range was written to the sink.
    ///
    /// `ProgressEntry` is the range persisted (at least to the OS page cache) by
    /// this write. This drives the progress bar and the resume bookkeeping; it is
    /// intentionally emitted before `fsync`, so it reflects "written", not
    /// "durably flushed".
    PushProgress(ProgressEntry),
    /// The sink is being flushed and synced to the `.part` file.
    Flushing,
    /// Flushing / syncing the sink failed.
    FlushError(anyhow::Error),
    /// Worker `id` completed its assigned range and exited.
    Finished(WorkerId),
}
