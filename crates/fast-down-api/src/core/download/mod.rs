use crate::utils::ForceSendExt;
use crate::{DownloadState, Event, StateError};
use crate::{PartialConfig, TerminationReason, Tx};
use fast_down::UrlInfo;
use std::path::Path;
use tokio::fs::{self, OpenOptions};
use tokio_util::sync::CancellationToken;
use url::Url;

mod overwrite;
mod pipeline;
mod plan;
mod progress_reporter;

pub use plan::*;

fn open_existing() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create(false);
    o
}
fn open_create() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create(true);
    o
}
fn open_create_new() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create_new(true);
    o
}

/// Attempt to load and validate a resume state from disk.
///
/// This checks that both the `.fd` and `.part` exist, validates the state
/// against the current server info, and merges the new config into the loaded
/// state.
///
/// Returns `Ok(Some(state))` if resume is possible, `Ok(None)` if there is
/// nothing usable to resume from (the pair is incomplete, or the `.part` is
/// shorter than the recorded progress), or `Err(StateError)` if the state exists
/// but does not describe the current remote file.
#[allow(clippy::result_large_err)]
async fn try_load_resume_state(
    url: &Url,
    cfg_path: &Path,
    tmp_path: &Path,
    info: &UrlInfo,
    partial_config: &PartialConfig,
) -> Result<Option<DownloadState>, StateError> {
    // Check if both .fd and .part exist
    let fd_exists = fs::try_exists(cfg_path).await.unwrap_or(false);
    let tmp_exists = fs::try_exists(tmp_path).await.unwrap_or(false);

    if !fd_exists || !tmp_exists {
        return Ok(None);
    }

    // Load and validate the state
    let state = DownloadState::load(cfg_path).await?;

    // Validate the state against current server info
    state.validate(info)?;

    // Merge the new config into the loaded state
    state.merge_config(partial_config);

    // Check after merging so caller-supplied progress is validated too. A
    // `.part` shorter than any claimed range would otherwise be extended with
    // zeros while the download engine skipped those bytes.
    if state.part_shortfall(tmp_path).await.is_some() {
        return Ok(None);
    }

    state.refresh_identity(url, info);

    Ok(Some(state))
}

/// Spawn a detached background download task that resumes automatically when
/// possible.
///
/// This is the one-shot form of [`plan`] followed by [`DownloadPlan::start`]:
/// the task prefetches metadata, then either resumes from a valid `.fd`/`.part`
/// state or starts a fresh download (falling back silently when resume is
/// impossible). Use [`plan`] directly when the decision should be shown to a
/// user first. Progress and lifecycle events are delivered through `tx`.
///
/// The run always ends with exactly one [`Event::Terminated`], which is the last
/// event on the channel — including when planning itself fails or is cancelled.
/// A caller can wait for it instead of draining `rx` until it disconnects; the
/// channel still disconnects afterwards, because the spawned task holds the only
/// `Tx` clones. Keep the
/// [`CancellationToken`](crate::create_cancellation_token) you passed in if you
/// need to cancel.
pub fn download(url: Url, partial_config: PartialConfig, tx: Tx, token: CancellationToken) {
    tokio::spawn(
        async move {
            let token2 = token.clone();
            let planned =
                Box::pin(token.run_until_cancelled(plan(url, partial_config, tx.clone(), token2)))
                    .await;
            Box::pin(drive(planned, &tx)).await;
        }
        .force_send(),
    );
}

/// Spawn a detached task that resumes a previously interrupted download from its
/// `.part` file.
///
/// This is the one-shot form of [`plan_resume`] followed by
/// [`DownloadPlan::start`]. `url` is optional. When `Some`, the resume resolves
/// and validates against that URL exactly as before. When `None`, the task
/// reuses the **initial URL** persisted in the `.fd` state file — the one the
/// original `download` recorded (the durable initial URL, not the transient
/// redirect/`final_url`). So a caller can resume purely from the `.part` path;
/// redirects are re-resolved through a fresh prefetch on every resume.
///
/// If the download cannot be continued — `tmp_path` is not a `.part` file, the
/// `.fd` state file is missing, the server does not support range requests, or
/// the remote file changed — the task emits
/// [`Event::ResumeError`](crate::Event::ResumeError) and stops **without**
/// falling back to a full re-download. If `tmp_path` itself does not exist, the
/// call falls back to a fresh download **only when a `url` is available**; with
/// `url = None` there is nothing to fetch, so it emits
/// `ResumeError(StateError::NoUrl)` instead. Likewise, when `url = None` but the
/// `.fd` carries no resolvable URL, the call reports `StateError::NoUrl`.
///
/// Completion is observed the same way as [`download`]: wait for the single
/// [`Event::Terminated`], or drain the `Rx` until it disconnects.
pub fn resume(
    tmp_path: impl AsRef<Path>,
    url: Option<Url>,
    partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) {
    let tmp_path = tmp_path.as_ref().to_path_buf();
    tokio::spawn(
        async move {
            let token2 = token.clone();
            let planned = Box::pin(token.run_until_cancelled(plan_resume(
                tmp_path,
                url,
                partial_config,
                tx.clone(),
                token2,
            )))
            .await;
            Box::pin(drive(planned, &tx)).await;
        }
        .force_send(),
    );
}

/// Spawn a detached task that downloads from a `.fd` state file used as a
/// download manifest.
///
/// This is the one-shot form of [`plan_from_fd`]: the task loads the `.fd`,
/// resolves and validates against the remote file, then either resumes from a
/// present `.part` or — when the `.part` is missing — reuses the `.fd`'s url /
/// config and downloads the whole file from scratch. See [`plan_from_fd`] for
/// the full contract.
///
/// Completion is observed the same way as [`download`]: wait for the single
/// [`Event::Terminated`], or drain the `Rx` until it disconnects.
pub fn download_from_fd(
    fd_path: impl AsRef<Path>,
    url: Option<Url>,
    partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) {
    let fd_path = fd_path.as_ref().to_path_buf();
    tokio::spawn(
        async move {
            let token2 = token.clone();
            let planned = Box::pin(token.run_until_cancelled(plan_from_fd(
                fd_path,
                url,
                partial_config,
                tx.clone(),
                token2,
            )))
            .await;
            Box::pin(drive(planned, &tx)).await;
        }
        .force_send(),
    );
}

/// Start a freshly-made plan, or report why there is none.
///
/// `None` means the cancellation token fired while planning. Either way exactly
/// one [`Event::Terminated`] reaches the channel.
async fn drive(planned: Option<Result<DownloadPlan, PlanError>>, tx: &Tx) {
    match planned {
        None => {
            let _ = tx.send(Event::Terminated(TerminationReason::Cancelled));
        }
        Some(Err(e)) => {
            e.emit(tx);
            let _ = tx.send(Event::Terminated(TerminationReason::Failed));
        }
        Some(Ok(prepared)) => Box::pin(prepared.start()).await,
    }
}
