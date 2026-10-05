use crate::{PartialConfig, StateError, Tx, engine::StartPolicy, state::Record};
use fast_down::{ProgressEntry, UrlInfo};
use reqwest::Response;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// What a resolved download will do when started, reported by [`crate::State::build`].
#[derive(Debug)]
pub enum ResumeOutcome {
    /// Nothing to continue from: `start` claims a free `.fd`/`.part` pair and
    /// downloads the whole file.
    Fresh,
    /// A validated `.fd` with a usable `.part`: `start` continues from the
    /// recorded progress.
    Resumable,
    /// State exists but cannot be continued (the remote changed, or the
    /// `.part` is truncated). What `start` does is up to the caller's mode.
    Mismatch(StateError),
}

/// A download resolved against the remote, ready to start.
///
/// Produced by [`crate::State::build`]. Holds the live prefetch [`Response`] so
/// `start` can seed its first range request without another round trip; that
/// field is never persisted. Inspect the public [`outcome`](Ready::outcome) (and
/// [`progress`](Ready::progress)) before committing, then call `start`.
#[derive(Debug)]
pub struct Ready {
    /// The caller's original overrides, kept so a fresh start can rebuild a
    /// clean configuration with any recorded progress cleared.
    pub origin_config: PartialConfig,
    /// The record to persist: identity refreshed, config merged, progress kept
    /// only when resuming.
    pub record: Record,
    /// `Some` when resuming a known `.fd` and its location should be reused;
    /// `None` when `start` must claim a free `.fd`/`.part` pair.
    pub fd_path: Option<PathBuf>,
    /// The base output path from `gen_path`, before any final-name uniquification.
    pub final_path: PathBuf,
    /// Prefetch metadata for the remote file.
    pub info: UrlInfo,
    /// The prefetch response, reused to seed the first range request.
    pub resp: Response,
    /// What `start` would do.
    pub outcome: ResumeOutcome,
}

impl Ready {
    /// Run this download to completion under the given start policy.
    ///
    /// Accepts either a [`StartMode`](crate::StartMode) preset or a
    /// fully-specified [`StartPolicy`] (both convert into `StartPolicy`).
    /// Consumes the plan.
    /// Emits events on the channel that produced it, and exactly one
    /// [`crate::Event::Terminated`] as the last event. Cancelling the token stops
    /// fetching and leaves `.part`/`.fd` in place.
    ///
    /// The returned future is `Send`, so it can be `tokio::spawn`ed directly:
    /// `fast-pull`'s `DownloadResult` is `Send` even when built inside an
    /// `async` body (via its internal `ForceSend` wrapper), and its event stream
    /// is awaited through the `Send`-typed `DownloadResult::recv` method. Both
    /// work around rustc's `async fn` auto-trait leak check, not a real
    /// `!Send`.
    pub async fn start(self, policy: impl Into<StartPolicy>, tx: Tx, token: &CancellationToken) {
        crate::engine::run(
            crate::engine::PreparedRun {
                record: self.record,
                origin_config: self.origin_config,
                fd_path: self.fd_path,
                final_path: self.final_path,
                info: self.info,
                resp: self.resp,
                outcome: self.outcome,
            },
            policy,
            tx,
            token,
        )
        .await;
    }

    /// The `.part` file this run writes to, derived from the `.fd` path.
    /// `None` when the `.fd` location is not decided yet.
    #[must_use]
    pub fn tmp_path(&self) -> Option<PathBuf> {
        self.fd_path.as_deref().map(|p| p.with_extension("part"))
    }

    /// Byte ranges recorded so far (empty for a fresh download).
    #[must_use]
    pub fn progress(&self) -> &[ProgressEntry] {
        self.record
            .config
            .downloaded_chunk
            .as_deref()
            .map_or(&[], |v| v)
    }
}
