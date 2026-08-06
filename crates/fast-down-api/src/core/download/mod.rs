use crate::core::download::overwrite::OverwriteOption;
use crate::utils::ForceSendExt;
use crate::{DownloadState, Event, StateError};
use crate::{PartialConfig, Tx, prefetch, tx_err, utils::gen_path};
use fast_down::UrlInfo;
use inherit_config::ConfigLayer;
use overwrite::overwrite;
use path_helper::IterStemExt;
use std::path::Path;
use tokio::fs::{self, OpenOptions};
use tokio_util::sync::CancellationToken;
use url::Url;

mod overwrite;
mod pipeline;
mod progress_reporter;

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
/// This helper consolidates the resume logic shared between `run_download` (overwrite and non-overwrite branches)
/// and `run_resume`. It checks if both `.fd` and `.part` exist, validates the state against the current server info,
/// and merges the new config into the loaded state.
///
/// Returns `Ok(Some(state))` if resume is possible, `Ok(None)` if no resume state exists (caller should start fresh),
/// or `Err(StateError)` if the state exists but is invalid.
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

    // Check that the .part file size is consistent with the recorded progress
    if let Ok(metadata) = fs::metadata(tmp_path).await {
        let actual_size = metadata.len();
        let recorded_progress = state.get_progress();
        let max_recorded_end = recorded_progress.iter().map(|r| r.end).max().unwrap_or(0);

        if actual_size < max_recorded_end {
            // The .part file is smaller than what we think is already downloaded.
            // This could lead to data corruption if we continue with resume.
            // Treat this as if no valid state exists and start fresh.
            return Ok(None);
        }
    }

    // Merge the new config into the loaded state
    state.merge_config(partial_config);
    state.refresh_identity(url, info);

    Ok(Some(state))
}

/// Spawn a detached background download task that resumes automatically when
/// possible.
///
/// The task first `prefetch`es metadata, then either resumes from a valid
/// `.fd`/`.part` state or starts a fresh download (falling back silently when
/// resume is impossible). Progress and lifecycle events are delivered through
/// `tx`.
///
/// Completion is observed through the [`Rx`](crate::Rx) you created alongside
/// `tx`: the spawned task holds the only `Tx` clones, so the receiver
/// disconnects once the task has fully finished — including the final
/// `overwrite`. Drain `rx` until it disconnects to await completion; keep the
/// [`CancellationToken`](crate::create_cancellation_token) you passed in if you
/// need to cancel.
pub fn download(url: Url, partial_config: PartialConfig, tx: Tx, token: CancellationToken) {
    tokio::spawn(
        async move {
            let token2 = token.clone();
            let opt = token
                .run_until_cancelled(
                    async move { run_download(url, partial_config, tx, token2).await },
                )
                .await
                .flatten();
            if let Some(opt) = opt {
                overwrite(opt).await;
            }
        }
        .force_send(),
    );
}

/// Spawn a detached task that resumes a previously interrupted download from its
/// `.part` file.
///
/// `url` is optional. When `Some`, the resume resolves and validates against
/// that URL exactly as before. When `None`, the task reuses the **initial URL**
/// persisted in the `.fd` state file — the one the original `download` recorded
/// (the durable initial URL, not the transient redirect/`final_url`). So a
/// caller can resume purely from the `.part` path; redirects are re-resolved
/// through a fresh prefetch on every resume.
///
/// If the download cannot be continued — the `.fd` state file is missing, the
/// server does not support range requests, or the remote file changed — the
/// task emits [`Event::ResumeError`](crate::Event::ResumeError) and returns
/// **without** falling back to a full re-download. If `tmp_path` itself does not
/// exist, the call falls back to a fresh download **only when a `url` is
/// available**; with `url = None` there is nothing to fetch, so it emits
/// `ResumeError(StateError::NoUrl)` instead. Likewise, when `url = None` but the
/// `.fd` carries no resolvable URL, the call reports `StateError::NoUrl`.
///
/// Completion is observed the same way as [`download`](crate::download): drain the `Rx` paired
/// with `tx` until it disconnects.
pub fn resume(
    tmp_path: impl AsRef<Path>,
    url: Option<Url>,
    partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) {
    let tmp_path = tmp_path.as_ref();
    if tmp_path.extension() != Some(std::ffi::OsStr::new("part")) {
        let _ = tx.send(Event::ResumeError(StateError::Open(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tmp_path must end with .part extension",
        ))));
        return;
    }
    let tmp_path = tmp_path.to_path_buf();

    tokio::spawn(
        async move {
            let token2 = token.clone();
            let opt = Box::pin(token.run_until_cancelled(async move {
                run_resume(&tmp_path, url, partial_config, tx, token2).await
            }))
            .await
            .flatten();
            if let Some(opt) = opt {
                overwrite(opt).await;
            }
        }
        .force_send(),
    );
}

async fn run_download(
    url: Url,
    partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) -> Option<OverwriteOption> {
    let config = partial_config.clone().build();
    let (info, resp) = prefetch(&url, &config, &tx).await?;
    let can_resume = config.resume && info.fast_download;

    let origin_path = tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError, None);

    if config.overwrite {
        let cfg_path = origin_path.with_added_extension("fd");
        let tmp_path = origin_path.with_added_extension("part");

        let state = if can_resume
            && let Ok(Some(s)) =
                try_load_resume_state(&url, &cfg_path, &tmp_path, &info, &partial_config).await
        {
            let _ = tx.send(Event::Resumed {
                config_path: cfg_path,
                progress: s.get_progress(),
                size: info.size,
            });
            s
        } else {
            tx_err!(
                open_create().open(tmp_path).await,
                tx,
                BuildPusherError,
                None
            );
            DownloadState::new(&url, &info, &partial_config, &cfg_path)
        };
        return Some(OverwriteOption {
            state,
            final_path: origin_path,
            info,
            resp,
            tx,
            token,
        });
    }

    for base_path in origin_path.iter_stem() {
        let tmp_path = base_path.with_added_extension("part");
        let cfg_path = base_path.with_added_extension("fd");

        let state = if can_resume
            && let Ok(Some(s)) =
                try_load_resume_state(&url, &cfg_path, &tmp_path, &info, &partial_config).await
        {
            let _ = tx.send(Event::Resumed {
                config_path: cfg_path,
                progress: s.get_progress(),
                size: info.size,
            });
            s
        } else {
            match open_create_new().open(&tmp_path).await {
                Ok(_) => DownloadState::new(&url, &info, &partial_config, &cfg_path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    let _ = tx.send(Event::BuildPusherError(e));
                    return None;
                }
            }
        };
        return Some(OverwriteOption {
            state,
            final_path: origin_path,
            info,
            resp,
            tx,
            token,
        });
    }
    unreachable!()
}

async fn run_resume(
    tmp_path: &Path,
    url: Option<Url>,
    mut partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) -> Option<OverwriteOption> {
    partial_config.overwrite = Some(false);
    let tmp_exists = fs::try_exists(tmp_path).await.unwrap_or(false);
    if !tmp_exists {
        // A missing tmp_path falls back to a fresh download, but that still needs a
        // URL to fetch. Without one there is nothing to resume against.
        let Some(url) = url else {
            let _ = tx.send(Event::ResumeError(StateError::NoUrl(
                tmp_path.to_path_buf(),
            )));
            return None;
        };
        partial_config.resume = Some(false);
        return run_download(url, partial_config, tx, token).await;
    }

    let cfg_path = tmp_path.with_extension("fd");
    let state = tx_err!(DownloadState::load(&cfg_path).await, tx, ResumeError, None);

    // Resolve the URL to prefetch/validate against: the caller's URL when given,
    // otherwise the durable initial URL recorded in the `.fd`. If neither exists
    // (url = None and an old `.fd` that stored no URL) we cannot resume.
    let Some(url) = url.or_else(|| {
        state
            .lock_inner()
            .url
            .clone()
            .filter(|s| matches!(s.scheme(), "http" | "https"))
    }) else {
        let _ = tx.send(Event::ResumeError(StateError::NoUrl(
            tmp_path.to_path_buf(),
        )));
        return None;
    };

    partial_config.resume = Some(true);
    let config = partial_config.clone().build();
    let (info, resp) = prefetch(&url, &config, &tx).await?;
    if !info.fast_download {
        let _ = tx.send(Event::ResumeError(StateError::NotResumable(info, resp)));
        return None;
    }

    match try_load_resume_state(&url, &cfg_path, tmp_path, &info, &partial_config).await {
        Ok(Some(state)) => {
            let _ = tx.send(Event::Resumed {
                config_path: cfg_path,
                progress: state.get_progress(),
                size: info.size,
            });

            let final_path = tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError, None);
            Some(OverwriteOption {
                state,
                final_path,
                info,
                resp,
                tx,
                token,
            })
        }
        Ok(None) => {
            // No valid state found, fall back to fresh download
            partial_config.resume = Some(false);
            run_download(url, partial_config, tx, token).await
        }
        Err(e) => {
            let _ = tx.send(Event::ResumeError(e));
            None
        }
    }
}
