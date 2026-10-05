//! Single-owner run loop: claims the `.fd`/`.part` pair, drives the engine, and
//! periodically persists the state.
//!
//! The loop owns the [`Record`] outright, so nothing has to be shared behind a
//! mutex. Persisting is handed to a background task that only ever sees a cloned
//! snapshot; the bounded snapshot channel makes a slow disk skip a tick instead
//! of stalling the event loop, and the worker reports save failures straight to
//! the public event channel.

use super::{
    build_pipeline, open_create, paths,
    policy::{IdentityRecovery, Recovery, StartPolicy},
    stats::StatsTracker,
};
use crate::{
    Config, Event, PartialConfig, TerminationReason, Tx,
    state::{Record, ResumeOutcome, StateError},
};
use fast_down::{
    BoxPusher, UrlInfo, fast_puller::FastDownPuller, invert, multi::download_multi,
    single::download_single,
};
use inherit_config::ConfigLayer;
use reqwest::Response;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{fs, sync::mpsc, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// How often the `.fd` state is persisted while a run is in flight.
const PERSIST_GAP: Duration = Duration::from_secs(1);

/// The resolved decision for a single run.
enum Fate {
    /// Continue from the recorded state.
    Resume,
    /// Ignore the recorded state and download the whole file.
    Fresh,
    /// Report the mismatch and stop.
    Fail,
}

/// Resolve a [`StartPolicy`] against the probed [`ResumeOutcome`].
fn decide(policy: StartPolicy, outcome: &ResumeOutcome) -> Fate {
    if !policy.use_recorded {
        return Fate::Fresh;
    }
    match outcome {
        ResumeOutcome::Resumable => Fate::Resume,
        ResumeOutcome::Fresh => match policy.no_state {
            Recovery::Fresh => Fate::Fresh,
            Recovery::Fail => Fate::Fail,
        },
        ResumeOutcome::Mismatch(error) => match error {
            StateError::Truncated { .. } => match policy.truncated {
                Recovery::Fresh => Fate::Fresh,
                Recovery::Fail => Fate::Fail,
            },
            StateError::NotResumable => match policy.not_resumable {
                Recovery::Fresh => Fate::Fresh,
                Recovery::Fail => Fate::Fail,
            },
            StateError::FileChanged {
                local_file_size,
                remote_file_size,
                ..
            } => {
                if local_file_size == remote_file_size {
                    match policy.identity_changed {
                        IdentityRecovery::Fresh => Fate::Fresh,
                        IdentityRecovery::Fail => Fate::Fail,
                        IdentityRecovery::Force => Fate::Resume,
                    }
                } else {
                    match policy.size_changed {
                        Recovery::Fresh => Fate::Fresh,
                        Recovery::Fail => Fate::Fail,
                    }
                }
            }
            _ => match policy.unreadable {
                Recovery::Fresh => Fate::Fresh,
                Recovery::Fail => Fate::Fail,
            },
        },
    }
}

/// Run a resolved download to completion, emitting events and exactly one
/// [`Event::Terminated`] as the last event.
///
/// Claims a free `.fd`/`.part` pair when `fd_path` is `None`, or reuses the given
/// one; `policy` decides what becomes of any recorded progress.
pub async fn run(
    prepared: PreparedRun,
    policy: impl Into<StartPolicy>,
    tx: Tx,
    token: &CancellationToken,
) {
    let reason = drive(prepared, policy.into(), &tx, token).await;
    let _ = tx.send(Event::Terminated(reason));
}

/// Everything a resolved run needs before it starts. Currently assembled field
/// by field in [`Ready::start`](crate::Ready::start); named here so the run entry
/// points take one argument instead of ten.
pub struct PreparedRun {
    pub record: Record,
    pub origin_config: PartialConfig,
    pub fd_path: Option<PathBuf>,
    pub final_path: PathBuf,
    pub info: UrlInfo,
    pub resp: Response,
    pub outcome: ResumeOutcome,
}

async fn drive(
    prepared: PreparedRun,
    policy: StartPolicy,
    tx: &Tx,
    token: &CancellationToken,
) -> TerminationReason {
    let PreparedRun {
        record,
        origin_config,
        fd_path,
        final_path,
        info,
        resp,
        outcome,
    } = prepared;

    // Resolve the fate once: whether to resume, fresh-download, or fail.
    let resuming = match decide(policy, &outcome) {
        Fate::Resume => true,
        Fate::Fresh => false,
        Fate::Fail => {
            if let ResumeOutcome::Mismatch(e) = outcome {
                let _ = tx.send(Event::ResumeError(e));
            }
            return TerminationReason::Failed;
        }
    };

    // 1. Provision the `.fd`/`.part` pair and persist the starting state.
    let Target {
        record,
        fd_path,
        tmp_path,
    } = match resolve_target(
        record,
        &origin_config,
        fd_path,
        &final_path,
        &info,
        resuming,
        tx,
    )
    .await
    {
        Ok(target) => target,
        Err(reason) => return reason,
    };

    // 2. Announce a resumed run.
    if resuming {
        let _ = tx.send(Event::Resumed {
            config_path: fd_path.clone(),
            progress: record.progress().to_vec(),
            size: info.size,
        });
    }

    // 3. Build the run config and the pull/push pipeline.
    let config = record.config.clone().build();
    let Some((puller, pusher)) = build_pipeline(&config, &info, resp, &tmp_path, tx, token).await
    else {
        return TerminationReason::Failed;
    };
    let _ = tx.send(Event::Start {
        tmp_path: tmp_path.clone(),
        config_path: fd_path.clone(),
        parsed_config: Box::new(record.config.clone()),
    });

    // 4. Drive the engine session to completion.
    let session = start_session(&config, &info, puller, pusher);
    let record = drive_session(record, &fd_path, &info, &config, session, tx, token).await;

    if token.is_cancelled() {
        return TerminationReason::Cancelled;
    }
    if !record.is_complete(info.size) {
        return TerminationReason::Incomplete;
    }

    // 5. Rename the finished `.part` into place and drop the `.fd`.
    finalize(&config, &final_path, &tmp_path, &fd_path, tx).await
}

/// The `.fd`/`.part` pair a run will use, plus the record to persist.
struct Target {
    record: Record,
    fd_path: PathBuf,
    tmp_path: PathBuf,
}

/// Provision the output directory and the `.fd`/`.part` pair and persist the
/// starting `.fd`. Reports failures as the matching [`Event`] and returns
/// [`TerminationReason::Failed`].
async fn resolve_target(
    mut record: Record,
    origin_config: &PartialConfig,
    fd_path: Option<PathBuf>,
    final_path: &Path,
    info: &UrlInfo,
    resuming: bool,
    tx: &Tx,
) -> Result<Target, TerminationReason> {
    // Ensure the output directory exists before claiming a scratch pair (the
    // `.fd`/`.part` sit next to `final_path`, so `create_new` there would
    // otherwise fail when `final_path`'s parent — e.g. a template subdir — has
    // not been created yet).
    if !paths::ensure_parent(final_path, tx).await {
        return Err(TerminationReason::Failed);
    }

    // Resolve the `.fd`/`.part` pair.
    let (fd_path, tmp_path) = if resuming {
        // A resumable/mismatched state always carries its location.
        let Some(fd_path) = fd_path else {
            let _ = tx.send(Event::ResumeError(StateError::NotResumable));
            return Err(TerminationReason::Failed);
        };
        let tmp = fd_path.with_extension("part");
        (fd_path, tmp)
    } else if let Some(fd_path) = fd_path {
        // Fresh over an existing pair: reuse the location and the `.part`, but
        // start from byte zero. This is how a pinned `.fd` (from `State::load`)
        // is reused even when its `.part` is missing.
        let tmp = fd_path.with_extension("part");
        if let Err(e) = open_create().open(&tmp).await {
            let _ = tx.send(Event::BuildPusherError(e));
            return Err(TerminationReason::Failed);
        }
        record = fresh_over(&record, origin_config, info);
        (fd_path, tmp)
    } else {
        paths::claim_pair(final_path, tx).await?
    };

    if let Err(e) = record.store(&fd_path).await {
        let _ = tx.send(Event::StateSaveError(e));
        return Err(TerminationReason::Failed);
    }

    Ok(Target {
        record,
        fd_path,
        tmp_path,
    })
}

/// The concrete engine session both `download_single` and `download_multi`
/// return (they share one type, so the `if/else` in [`start_session`] needs no
/// boxing).
type Session = fast_down::DownloadResult<
    fast_down::multi::TokioExecutor<FastDownPuller, Box<dyn fast_down::AnyError>>,
    <FastDownPuller as fast_down::Puller>::Error,
    Box<dyn fast_down::AnyError>,
>;

/// Construct the engine session: multi-threaded when the server supports ranges,
/// single-threaded otherwise.
fn start_session(
    config: &Config,
    info: &UrlInfo,
    puller: FastDownPuller,
    pusher: BoxPusher,
) -> Session {
    if info.fast_download {
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
    }
}

/// Run the session to end: forward events, persist the state periodically, emit
/// progress, and—once the engine stops—flush the final `.fd` and one terminal
/// [`Event::Progress`]. Returns the record with its `elapsed` clock advanced.
async fn drive_session(
    mut record: Record,
    fd_path: &Path,
    info: &UrlInfo,
    config: &Config,
    res: Session,
    tx: &Tx,
    token: &CancellationToken,
) -> Record {
    let loaded_elapsed = record.elapsed;
    let start = Instant::now();
    let mut tracker = StatsTracker::new(info.size, loaded_elapsed, record.progress());

    let (snap_tx, snap_rx) = mpsc::channel::<Record>(1);
    let writer = tokio::spawn(persist_worker(snap_rx, fd_path.to_path_buf(), tx.clone()));

    let mut persist_tick = tokio::time::interval(PERSIST_GAP);
    persist_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // `progress_emit_gap == 0` disables periodic progress frames. The interval is
    // still created (with a harmless placeholder period) so the `select!` guard
    // can stay uniform, but its branch never runs while disabled.
    let progress_enabled = !config.progress_emit_gap.is_zero();
    let mut progress_tick = tokio::time::interval(if progress_enabled {
        config.progress_emit_gap
    } else {
        PERSIST_GAP
    });
    progress_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            () = token.cancelled() => {
                res.abort();
                break;
            }
            event = res.recv() => match event {
                Ok(event) => {
                    tracker.observe(&event, Instant::now());
                    if let fast_down::Event::PushProgress(range) = &event {
                        record.merge_progress(range.clone());
                    }
                    forward(tx, event);
                }
                Err(_) => break,
            },
            _ = persist_tick.tick() => {
                // Bounded to one snapshot: a slower-than-one-second disk
                // skips a tick instead of piling up snapshots. Check capacity
                // first so a full channel costs no throwaway `Record` clone.
                if snap_tx.capacity() > 0 {
                    // Stamp the snapshot with the live elapsed, so a run that is
                    // killed (not gracefully cancelled) still persists its active
                    // time instead of the value from before it started.
                    record.elapsed = loaded_elapsed.saturating_add(start.elapsed());
                    let _ = snap_tx.try_send(record.clone());
                }
            },
            _ = progress_tick.tick(), if progress_enabled => {
                let _ = tx.send(Event::Progress(tracker.snapshot()));
            }
        }
    }

    // Stop the writer, flush the final state, emit one terminal sample.
    drop(snap_tx);
    let _ = writer.await;
    let elapsed = loaded_elapsed.saturating_add(start.elapsed());
    record.elapsed = elapsed;
    if let Err(e) = record.store(fd_path).await {
        let _ = tx.send(Event::StateSaveError(e));
    }
    let sample = tracker.snapshot_terminal(elapsed);
    let _ = tx.send(Event::Progress(sample));

    record
}

/// Rename the finished `.part` into place and drop the `.fd`, honouring
/// `overwrite` (uniquify the final name when it is `false`).
async fn finalize(
    config: &Config,
    final_path: &Path,
    tmp_path: &Path,
    fd_path: &Path,
    tx: &Tx,
) -> TerminationReason {
    let target = if config.overwrite {
        final_path.to_path_buf()
    } else {
        match path_helper::tokio::gen_unique_path(final_path).await {
            Ok(path) => path,
            Err(e) => {
                let _ = tx.send(Event::GenPathError(e));
                return TerminationReason::Failed;
            }
        }
    };
    if let Err(e) = fs::rename(tmp_path, &target).await {
        if !config.overwrite {
            let _ = fs::remove_file(&target).await;
        }
        let _ = tx.send(Event::RenameFailed(e));
        return TerminationReason::Failed;
    }
    let _ = fs::remove_file(fd_path).await;
    let _ = tx.send(Event::Renamed(target));
    TerminationReason::Completed
}

/// Persist snapshots until the sender is dropped. Reports save failures straight
/// to the public event channel.
async fn persist_worker(mut rx: mpsc::Receiver<Record>, fd_path: PathBuf, tx: Tx) {
    while let Some(record) = rx.recv().await {
        if let Err(e) = record.store(&fd_path).await {
            let _ = tx.send(Event::StateSaveError(e));
        }
    }
}

/// Forward one engine event as the matching public event.
fn forward<PE, WE>(tx: &Tx, event: fast_down::Event<PE, WE>)
where
    PE: std::error::Error + Send + Sync + 'static,
    WE: std::error::Error + Send + Sync + 'static,
{
    let _ = match event {
        fast_down::Event::Pulling(id) => tx.send(Event::Pulling(id)),
        fast_down::Event::PullError(id, e) => tx.send(Event::PullError(id, anyhow::Error::new(e))),
        fast_down::Event::PullTimeout(id) => tx.send(Event::PullTimeout(id)),
        fast_down::Event::PullProgress(id, range) => tx.send(Event::PullProgress(id, range)),
        fast_down::Event::Pushing(id, range) => tx.send(Event::Pushing(id, range)),
        fast_down::Event::PushError(id, range, e) => {
            tx.send(Event::PushError(id, range, anyhow::Error::new(e)))
        }
        fast_down::Event::PushProgress(range) => tx.send(Event::PushProgress(range)),
        fast_down::Event::Flushing => tx.send(Event::Flushing),
        fast_down::Event::FlushError(e) => tx.send(Event::FlushError(anyhow::Error::new(e))),
        fast_down::Event::Finished(id) => tx.send(Event::Finished(id)),
    };
}

/// A clean record at the caller's configuration, carrying `info`'s identity and
/// no progress.
fn fresh_over(record: &Record, origin_config: &PartialConfig, info: &UrlInfo) -> Record {
    let mut fresh = Record::new(record.url.clone(), origin_config.clone());
    fresh.refresh_identity(info);
    fresh
}
