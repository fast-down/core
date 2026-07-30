//! Full-pipeline download driver: persists state, runs the engine, then renames
//! the `.part` file into place.
//!
//! [`overwrite`] is the shared core of both [`crate::DownloadHandle::download`]
//! and [`crate::DownloadHandle::resume`]. It takes a fully-prepared
//! [`OverwriteOption`] (state + paths + prefetch result + channel + token),
//! builds the pull/push pipeline, drives [`fast_down::multi::download_multi`] or
//! [`fast_down::single::download_single`], forwards engine events to the public
//! [`crate::Event`] stream, periodically saves progress, and on success renames
//! the `.part` file to its final destination (or a unique variant when
//! `overwrite` is disabled).
use super::progress_reporter::ProgressReporter;
use crate::{
    DownloadState, Event, PartialConfig, Tx, core::download::pipeline::build_pipeline, tx_err,
};
use fast_down::{UrlInfo, invert, multi::download_multi, single::download_single};
use inherit_config::ConfigLayer;
use path_helper::tokio::gen_unique_path;
use reqwest::Response;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::fs;
use tokio_util::sync::CancellationToken;

/// Fully-prepared inputs for [`overwrite`].
///
/// Callers build this once prefetch has succeeded and the [`DownloadState`] is
/// ready; [`overwrite`] then owns it and runs the download to completion.
pub struct OverwriteOption {
    /// The download state backing resume (progress, identity, `.fd` path).
    pub state: DownloadState,
    /// Intended final destination of the file (before unique-path adjustment).
    pub final_path: PathBuf,
    /// Prefetch metadata for the remote file (size, identity, range support).
    pub info: UrlInfo,
    /// The prefetch HTTP response, reused to seed the first range request.
    pub resp: Response,
    /// Channel to forward public [`crate::Event`]s to the consumer.
    pub tx: Tx,
    /// Cancellation token; cancelling stops fetching and leaves `.part`/`.fd`.
    pub token: CancellationToken,
}

/// Run a complete download: persist state, drive the engine, rename into place.
///
/// This is the shared core of [`crate::DownloadHandle::download`] and
/// [`crate::DownloadHandle::resume`]. It:
/// 1. Saves the `.fd` state up front.
/// 2. Builds the pull/push pipeline for the `.part` file.
/// 3. Emits [`crate::Event::Start`] and runs `download_multi` (fast downloads)
///    or `download_single` (single-stream) according to `info.fast_download`.
/// 4. Forwards every engine event to the public [`crate::Event`] channel and
///    merges `PushProgress` ranges into the state.
/// 5. Periodically (≈1s) re-saves the `.fd` so progress survives interruption.
/// 6. On success, renames `.part` to `final_path` (or a unique variant when
///    `overwrite` is disabled) and emits [`crate::Event::Renamed`].
///
/// If the token is cancelled or the download did not complete, the `.part` and
/// `.fd` files are left in place so a later resume can continue.
#[allow(clippy::too_many_lines)]
pub async fn overwrite(option: OverwriteOption) {
    let OverwriteOption {
        state,
        final_path,
        info,
        resp,
        tx,
        token,
    } = option;
    tx_err!(state.store().await, tx, StateSaveError);
    let _ = state.take_dirty();

    let inner_state = state.lock_inner().clone();
    let parsed_config = inner_state.config.clone().unwrap_or_default();
    let inner_state = inner_state.build();
    let tmp_path = state.tmp_path();
    let url = &inner_state.url;
    let config = &inner_state.config;

    let pipeline = build_pipeline(url, config, &info, resp, &tmp_path, &tx, &token).await;
    let Some((puller, pusher)) = pipeline else {
        return;
    };

    let _ = tx.send(Event::Start {
        tmp_path: tmp_path.clone(),
        config_path: state.config_path.clone(),
        parsed_config,
    });

    let res = if info.fast_download {
        download_multi(
            puller,
            pusher,
            fast_down::multi::DownloadOptions {
                download_chunks: invert(
                    config.downloaded_chunk.iter().cloned(),
                    info.size,
                    config.chunk_window,
                ),
                concurrent: config.threads,
                retry_gap: config.retry_gap,
                pull_timeout: config.pull_timeout,
                push_queue_cap: config.write_queue_cap,
                min_chunk_size: config.min_chunk_size,
                max_speculative: config.max_speculative,
            },
        )
    } else {
        download_single(
            puller,
            pusher,
            fast_down::single::DownloadOptions {
                retry_gap: config.retry_gap,
                push_queue_cap: config.write_queue_cap,
            },
        )
    };

    let abort_handle = {
        let token = token.clone();
        let res = res.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            res.abort();
        })
    };

    let persist_token = token.child_token();
    let persist_task = {
        let st = state.clone();
        let tx = tx.clone();
        let stop = persist_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(1)) => {
                        if st.take_dirty() && let Err(e) = st.store().await {
                            let _ = tx.send(Event::StateSaveError(e));
                            st.mark_dirty(); // retry on the next cadence tick
                        }
                    }
                    () = stop.cancelled() => break,
                }
            }
        })
    };

    let reporter = ProgressReporter::new(inner_state.elapsed, info.size, state.share_inner());
    let progress_task = reporter.clone().spawn(&tx, config.progress_emit_gap);

    while let Ok(e) = res.event_chain().recv().await {
        if let fast_down::Event::PushProgress(range) = &e {
            state.merge_progress(range.clone());
        }
        let _ = match e {
            fast_down::Event::Pulling(id) => tx.send(Event::Pulling(id)),
            fast_down::Event::PullError(id, e) => tx.send(Event::PullError(id, anyhow::anyhow!(e))),
            fast_down::Event::PullTimeout(id) => tx.send(Event::PullTimeout(id)),
            fast_down::Event::PullProgress(id, range) => tx.send(Event::PullProgress(id, range)),
            fast_down::Event::Pushing(id, range) => tx.send(Event::Pushing(id, range)),
            fast_down::Event::PushError(id, range, e) => {
                tx.send(Event::PushError(id, range, anyhow::anyhow!(e)))
            }
            fast_down::Event::PushProgress(range) => tx.send(Event::PushProgress(range)),
            fast_down::Event::Flushing => tx.send(Event::Flushing),
            fast_down::Event::FlushError(e) => tx.send(Event::FlushError(anyhow::anyhow!(e))),
            fast_down::Event::Finished(id) => tx.send(Event::Finished(id)),
        };
    }

    progress_task.abort();
    let _ = progress_task.await;
    let sample = reporter.compute(Instant::now(), None);
    let elapsed = sample.elapsed;
    let _ = tx.send(Event::Progress(sample));
    persist_token.cancel();
    let _ = persist_task.await;

    if let Err(e) = res.join().await {
        let _ = tx.send(Event::JoinError(e));
    }

    abort_handle.abort();

    let download_complete = info.size == 0
        || matches!(&state.lock_inner().config, Some(PartialConfig { downloaded_chunk: Some(x), .. }) if x.len() == 1 && x[0] == (0..info.size));
    if token.is_cancelled() || !download_complete {
        state.set_elapsed(elapsed);
        if let Err(e) = state.store().await {
            let _ = tx.send(Event::StateSaveError(e));
        }
        return;
    }

    let final_path = if config.overwrite {
        final_path
    } else {
        tx_err!(gen_unique_path(final_path).await, tx, GenPathError)
    };
    if let Err(e) = fs::rename(tmp_path, &final_path).await {
        if !config.overwrite {
            let _ = fs::remove_file(&final_path).await;
        }
        let _ = tx.send(Event::RenameFailed(e));
        return;
    }
    let _ = fs::remove_file(&state.config_path).await;
    let _ = tx.send(Event::Renamed(final_path));
}
