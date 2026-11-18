//! Single-owner run loop: claims the `.fd`/`.part` pair, drives the engine, and
//! periodically persists the state.
//!
//! The loop owns the [`Record`] outright, so nothing has to be shared behind a
//! mutex. Persisting is handed to a background task that only ever sees a cloned
//! snapshot; the bounded snapshot channel makes a slow disk skip a tick instead
//! of stalling the event loop, and the worker reports save failures straight to
//! the public event channel.

use super::{
    build_pipeline,
    claim::{self, Claim},
    open_create, paths,
    policy::{IdentityRecovery, Recovery, StartPolicy},
    stats::StatsTracker,
};
use crate::{
    Config, Event, PartialConfig, TaskError, TerminationReason, Tx,
    state::{Record, ResumeOutcome, StateError},
};
use fast_down::{
    BoxPusher, UrlInfo,
    fast_puller::{CookieStore, FastDownPuller},
    invert,
    multi::download_multi,
    single::download_single,
};
use inherit_config::ConfigLayer;
use parking_lot::Mutex;
use reqwest::Response;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, sync::mpsc, task::JoinHandle, time::MissedTickBehavior};
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
const fn decide(policy: StartPolicy, outcome: &ResumeOutcome) -> Fate {
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
            // The size still matches, so the recorded ranges still describe
            // these bytes whatever the identity headers say.
            StateError::FileChanged { .. } if error.force_safe() => match policy.identity_changed {
                IdentityRecovery::Fresh => Fate::Fresh,
                IdentityRecovery::Fail => Fate::Fail,
                IdentityRecovery::Force => Fate::Resume,
            },
            StateError::FileChanged { .. } => match policy.size_changed {
                Recovery::Fresh => Fate::Fresh,
                Recovery::Fail => Fate::Fail,
            },
            _ => match policy.unreadable {
                Recovery::Fresh => Fate::Fresh,
                Recovery::Fail => Fate::Fail,
            },
        },
    }
}

/// Live control for a run: the cancellation token plus a handle to resize the
/// worker pool while the download is in flight.
///
/// One `Ctx` controls one run at a time: starting a second run with the same
/// handle replaces the first run's session for the control methods, and the
/// first run clears only the session it attached.
///
/// The engine session does not exist until the pipeline is built, so a
/// [`set_threads`](Self::set_threads) or [`set_min_chunk_size`](Self::set_min_chunk_size)
/// made before then is remembered and applied as soon as it does. [`RunHandle`]
/// wraps a `Ctx`, so the two share one state.
///
/// The worker count and the minimum chunk size are deliberately separate:
/// [`set_threads`](Self::set_threads) only changes the pool size, and
/// [`set_min_chunk_size`](Self::set_min_chunk_size) only changes the chunk
/// granularity. The granularity bounds how finely a range may be split to hand a
/// worker its share, and takes effect on the next steal.
#[derive(Debug, Clone)]
pub struct Ctx {
    token: CancellationToken,
    slot: Arc<Mutex<Slot>>,
}

#[derive(Debug, Default)]
struct Slot {
    session: Option<Session>,
    /// An explicit minimum chunk size requested before the session existed;
    /// `None` means the run's configured value is used.
    pending_min: Option<u64>,
    /// A pool size requested before the session existed.
    pending_threads: Option<usize>,
    /// Bumped on every attach, so a finishing run only clears its own session.
    generation: u64,
}

impl Ctx {
    /// Create a control handle with a fresh cancellation token.
    #[must_use]
    pub fn new() -> Self {
        Self::with_token(CancellationToken::new())
    }

    /// Create a control handle around an existing cancellation token.
    #[must_use]
    pub fn with_token(token: CancellationToken) -> Self {
        Self {
            token,
            slot: Arc::new(Mutex::new(Slot::default())),
        }
    }

    /// The cancellation token this run observes.
    #[must_use]
    pub const fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// Cancel the run. A run that has not recorded any byte yet drops the empty
    /// `.part`/`.fd` pair it created; otherwise both files stay on disk for a
    /// later resume.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Resize the worker pool, keeping the current minimum chunk size (the run's
    /// configured value unless [`set_min_chunk_size`](Self::set_min_chunk_size)
    /// changed it).
    ///
    /// A no-op for single-threaded runs (unresumable or non-ranged downloads),
    /// which have no worker pool. A request made before the pipeline is built is
    /// remembered and applied as soon as it is.
    pub fn set_threads(&self, threads: usize) {
        let mut slot = self.slot.lock();
        if let Some(session) = &slot.session {
            session.set_threads(threads);
        } else {
            slot.pending_threads = Some(threads);
        }
    }

    /// Set the minimum chunk size (in bytes) used when a worker next steals.
    ///
    /// Bounds how finely a range may be split to hand a worker its share: a range
    /// is only halved when at least `min_chunk_size * 2` bytes remain. Applied to
    /// the live pool immediately; a request made before the pipeline is built is
    /// remembered and applied as soon as it is. A `0` is normalized to `1` by the
    /// engine, so no clamping is needed here.
    pub fn set_min_chunk_size(&self, bytes: u64) {
        let mut slot = self.slot.lock();
        if let Some(session) = &slot.session {
            session.set_min_chunk_size(bytes);
        } else {
            slot.pending_min = Some(bytes);
        }
    }

    /// Publish the freshly-built session: apply any granularity and pool size
    /// requested before the session existed.
    ///
    /// A pending minimum is applied to the live pool; without one the session
    /// already carries the run's configured granularity, so nothing is needed.
    fn attach(&self, session: &Session) -> AttachGuard<'_> {
        let mut slot = self.slot.lock();
        if let Some(min) = slot.pending_min.take() {
            session.set_min_chunk_size(min);
        }
        if let Some(threads) = slot.pending_threads.take() {
            session.set_threads(threads);
        }
        slot.generation = slot.generation.wrapping_add(1);
        slot.session = Some(session.clone());
        AttachGuard {
            ctx: self,
            generation: slot.generation,
        }
    }

    /// Forget the finished session, unless another run attached since.
    ///
    /// A later [`set_threads`](Self::set_threads) or
    /// [`set_min_chunk_size`](Self::set_min_chunk_size) is then remembered for
    /// the next run on this handle instead of being applied to a dead session
    /// and silently dropped.
    fn detach(&self, generation: u64) {
        let mut slot = self.slot.lock();
        if slot.generation == generation {
            slot.session = None;
        }
    }
}

/// Detaches a run's session when it ends, including on an early return.
struct AttachGuard<'a> {
    ctx: &'a Ctx,
    generation: u64,
}

impl Drop for AttachGuard<'_> {
    fn drop(&mut self) {
        self.ctx.detach(self.generation);
    }
}

impl Default for Ctx {
    fn default() -> Self {
        Self::new()
    }
}

/// A handle to a running download, returned by [`crate::Ready::start`].
///
/// It owns the cancellation token and session slot through its [`Ctx`], so
/// [`set_threads`](Self::set_threads) and [`abort`](Self::abort) delegate to it.
/// Await it (it is a [`Future`](std::future::Future)) to get the
/// [`TerminationReason`]; the run also reports on the event channel and ends with
/// exactly one [`Event::Terminated`].
#[derive(Debug)]
pub struct RunHandle {
    join: JoinHandle<TerminationReason>,
    ctx: Ctx,
}

impl RunHandle {
    /// The control handle, for e.g. sharing with another task.
    #[must_use]
    pub const fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// Resize the worker pool. Delegates to [`Ctx::set_threads`].
    pub fn set_threads(&self, threads: usize) {
        self.ctx.set_threads(threads);
    }

    /// Set the minimum chunk size used when a worker next steals. Delegates to
    /// [`Ctx::set_min_chunk_size`].
    pub fn set_min_chunk_size(&self, bytes: u64) {
        self.ctx.set_min_chunk_size(bytes);
    }

    /// Cancel the run. Delegates to [`Ctx::cancel`].
    pub fn abort(&self) {
        self.ctx.cancel();
    }
}

impl std::future::Future for RunHandle {
    type Output = TerminationReason;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // `JoinHandle` is `Unpin`, so we can poll it through `Pin::get_mut`. The
        // run always emits [`Event::Terminated`], but a panic (or abort) drops
        // `tx` and closes the channel without one; report `Failed` for that case
        // rather than propagating the foreign `JoinError` type.
        std::pin::Pin::new(&mut self.get_mut().join)
            .poll(cx)
            .map(|res| res.unwrap_or(TerminationReason::Failed))
    }
}

/// Spawn a resolved download on the ambient tokio runtime and return a
/// [`RunHandle`] immediately.
///
/// `tx` is owned by the spawned run: sending on it reports events, and dropping
/// it when the run ends disconnects the receiver. `ctx` carries the cancellation
/// token and live worker-pool control, shared with the returned handle. Requires
/// an active tokio runtime; panics otherwise.
pub fn run(prepared: PreparedRun, policy: impl Into<StartPolicy>, tx: Tx, ctx: &Ctx) -> RunHandle {
    let policy = policy.into();
    let join = {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let reason = drive(prepared, policy, &tx, &ctx).await;
            let _ = tx.send(Event::Terminated(reason));
            reason
        })
    };
    RunHandle {
        join,
        ctx: ctx.clone(),
    }
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
    /// The store the probe used, shared with every client the run builds.
    pub(crate) cookie_store: CookieStore,
}

async fn drive(
    prepared: PreparedRun,
    policy: StartPolicy,
    tx: &Tx,
    ctx: &Ctx,
) -> TerminationReason {
    let PreparedRun {
        record,
        origin_config,
        fd_path,
        final_path,
        info,
        resp,
        outcome,
        cookie_store,
    } = prepared;
    let token = ctx.token();

    // A cancel that arrived before the run was spawned must win over the start
    // policy: report Cancelled, not a policy failure.
    if token.is_cancelled() {
        return TerminationReason::Cancelled;
    }

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
    // A cancel that arrived while the policy was being resolved must not create
    // a pair it will immediately abandon.
    if token.is_cancelled() {
        return TerminationReason::Cancelled;
    }
    let Target {
        record,
        fd_path,
        tmp_path,
        _claim,
        fresh,
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
            fd_path: fd_path.clone(),
            progress: record.progress().to_vec(),
            size: info.size,
        });
    }

    // 3. Build the run config and the pull/push pipeline.
    let config = record.config.clone().build();
    let (puller, pusher) =
        match build_pipeline(&config, &info, resp, &tmp_path, tx, token, cookie_store).await {
            // `None` means the run was cancelled mid-build; a cancel must report
            // as such, not as a build failure. The pair this run just created holds
            // no bytes, so drop it rather than leave a 0% "resumable" task behind.
            None => {
                discard_unused_pair(fresh, &record, &fd_path, &tmp_path).await;
                return TerminationReason::Cancelled;
            }
            Some(Err(event)) => {
                let _ = tx.send(event);
                // A pair this run just created and never wrote to would otherwise
                // stay on disk as a permanent 0-byte "resumable" task.
                discard_unused_pair(fresh, &record, &fd_path, &tmp_path).await;
                return TerminationReason::Failed;
            }
            Some(Ok(built)) => built,
        };
    let _ = tx.send(Event::Start {
        tmp_path: tmp_path.clone(),
        fd_path: fd_path.clone(),
        parsed_config: Box::new(record.config.clone()),
    });

    // 4. Drive the engine session to completion.
    let session = start_session(&config, &info, puller, pusher);
    let _attachment = ctx.attach(&session);
    let (record, engine_aborted) =
        drive_session(record, &fd_path, &info, &config, session, tx, token).await;

    if token.is_cancelled() {
        // A cancel that arrived before any byte was recorded leaves nothing to
        // resume, so drop the empty pair instead of a permanent 0% task.
        discard_unused_pair(fresh, &record, &fd_path, &tmp_path).await;
        return TerminationReason::Cancelled;
    }
    if !record.is_complete(info.size) {
        discard_unused_pair(fresh, &record, &fd_path, &tmp_path).await;
        return TerminationReason::Incomplete;
    }
    if engine_aborted {
        discard_unused_pair(fresh, &record, &fd_path, &tmp_path).await;
        return TerminationReason::Failed;
    }

    // 5. Rename the finished `.part` into place and drop the `.fd`.
    finalize(&config, &final_path, &tmp_path, &fd_path, tx).await
}

/// The `.fd`/`.part` pair a run will use, plus the record to persist.
struct Target {
    record: Record,
    fd_path: PathBuf,
    tmp_path: PathBuf,
    /// Published for the whole run: dropping it releases the pair for the next
    /// run (or process).
    _claim: Claim,
    /// Whether this run created the pair, so a failed start may discard it.
    fresh: bool,
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

    // Resolve the `.fd`/`.part` pair. The claim for it is taken before either
    // file is created (here, or inside `claim_pair` for a new stem): a losing
    // run must not have touched anything, let alone delete a pair the winner may
    // already be writing to.
    let (claim, fd_path, tmp_path, fresh) = if resuming {
        // A resumable/mismatched state always carries its location.
        let Some(fd_path) = fd_path else {
            let _ = tx.send(Event::ResumeError(StateError::NotResumable));
            return Err(TerminationReason::Failed);
        };
        let tmp = crate::utils::part_path(&fd_path);
        let claim = claim_or_report(&fd_path, tx)?;
        (claim, fd_path, tmp, false)
    } else if let Some(fd_path) = fd_path {
        // Fresh over an existing pair: reuse the location and the `.part`, but
        // start from byte zero. This is how a pinned `.fd` (from `State::at_path`)
        // is reused even when its `.part` is missing.
        let claim = claim_or_report(&fd_path, tx)?;
        let tmp = crate::utils::part_path(&fd_path);
        if let Err(e) = open_create().open(&tmp).await {
            let _ = tx.send(Event::BuildPusherError(e));
            return Err(TerminationReason::Failed);
        }
        record = fresh_over(&record, origin_config, info);
        (claim, fd_path, tmp, false)
    } else {
        let (claim, fd_path, tmp_path) = paths::claim_pair(final_path, tx).await?;
        (claim, fd_path, tmp_path, true)
    };

    // Fold the configuration into a difference against the crate defaults before
    // the first write: `build` restores those defaults on load, so the `.fd`
    // keeps only the fields that differ from them. Done here once so the
    // periodic saves in `drive_session` — which re-serialize this same record —
    // stay lean too. `build` is unaffected: it maps every `None` back to the
    // very default the field was compared against.
    record.simplify_from(&PartialConfig::default());

    if let Err(e) = record.store(&fd_path).await {
        // The pair was just created and holds no bytes; do not leave it behind
        // as a 0% "resumable" task.
        discard_unused_pair(fresh, &record, &fd_path, &tmp_path).await;
        let _ = tx.send(Event::StateSaveError(e));
        return Err(TerminationReason::Failed);
    }

    Ok(Target {
        record,
        fd_path,
        tmp_path,
        _claim: claim,
        fresh,
    })
}

/// Take the claim for `fd_path`, reporting why it failed.
///
/// [`Event::AlreadyRunning`] means a live run owns the pair;
/// [`Event::ClaimUnavailable`] means the lock itself could not be set up.
/// Neither leaves anything for the caller to clean up: nothing has been created
/// under that path yet.
fn claim_or_report(fd_path: &Path, tx: &Tx) -> Result<Claim, TerminationReason> {
    match claim::claim(fd_path) {
        Ok(Some(claim)) => Ok(claim),
        Ok(None) => {
            let _ = tx.send(Event::AlreadyRunning(fd_path.to_path_buf()));
            Err(TerminationReason::Failed)
        }
        Err(e) => {
            let _ = tx.send(Event::ClaimUnavailable(e));
            Err(TerminationReason::Failed)
        }
    }
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
                stall_timeout: config.stall_timeout,
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
                stall_timeout: config.stall_timeout,
            },
        )
    }
}

/// Run the session to end: forward events, persist the state periodically, emit
/// progress, and—once the engine stops—flush the final `.fd` and one terminal
/// [`Event::Progress`]. Returns the record with its `elapsed` clock advanced,
/// plus whether the engine aborted itself (for example a final flush that kept
/// failing) rather than the caller cancelling.
///
/// On a cancel the loop does not break: it stops aborting and keeps draining
/// events until `recv` sees the channel close, which happens only once every
/// worker and the push thread have dropped their senders, i.e. once the sink
/// (and any memory mapping or file handle it holds) is released. Only then does
/// the run persist the final `.fd` and report `Terminated`, so a caller that
/// immediately reopens the `.part` (to truncate it, or resume it) cannot race a
/// still-live mapping — which on Windows fails with `ERROR_USER_MAPPED_FILE`.
async fn drive_session(
    mut record: Record,
    fd_path: &Path,
    info: &UrlInfo,
    config: &Config,
    res: Session,
    tx: &Tx,
    token: &CancellationToken,
) -> (Record, bool) {
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
            // Cancel once, then let the loop fall through to draining
            // `res.recv()`. The guard is what makes this terminate: once fired,
            // `token.cancelled()` is ready forever, and under `biased` an
            // unguarded arm would win every poll and never reach `recv`.
            () = token.cancelled(), if !res.is_aborted() => res.abort(),
            event = res.recv() => match event {
                Ok(event) => {
                    tracker.observe(&event);
                    if let fast_down::Event::PushProgress(range) = &event {
                        record.merge_progress(range.clone());
                        tracker.set_written(record.downloaded());
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
                let _ = tx.send(Event::Progress(
                    tracker.snapshot(Instant::now(), record.progress()),
                ));
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
    let sample = tracker.snapshot_terminal(elapsed, record.progress());
    let _ = tx.send(Event::Progress(sample));

    let engine_aborted = res.is_aborted() && !token.is_cancelled();
    (record, engine_aborted)
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
            // `gen_unique_path` reserved this name by creating an empty file;
            // drop that placeholder so a later retry can still use the intended
            // name instead of falling back to `name (1)`.
            let _ = fs::remove_file(&target).await;
        }
        let _ = tx.send(Event::RenameFailed(e));
        return TerminationReason::Failed;
    }
    if let Err(e) = fs::remove_file(fd_path).await {
        // The download completed, but the stale `.fd` makes the next run see a
        // finished pair whose `.part` is gone, so it downloads the file again
        // under a unique name. Report the cleanup failure; the bytes are safe.
        let _ = tx.send(Event::StateSaveError(StateError::Remove(e)));
    }
    let _ = tx.send(Event::Renamed(target));
    TerminationReason::Completed
}

/// Drop a pair this run created when it holds no recorded progress.
///
/// Both conditions matter: `fresh` keeps a resumed or pinned pair (never ours to
/// delete) while the progress check keeps a cancelled mid-download pair, whose
/// recorded bytes are still worth resuming. The removal lives here rather than
/// in a standalone helper so there is no unchecked way to delete a pair.
async fn discard_unused_pair(fresh: bool, record: &Record, fd_path: &Path, tmp_path: &Path) {
    if fresh && record.progress().is_empty() {
        let _ = fs::remove_file(tmp_path).await;
        let _ = fs::remove_file(fd_path).await;
    }
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
        fast_down::Event::PullError(id, e) => tx.send(Event::PullError(id, TaskError::new(e))),
        fast_down::Event::PullTimeout(id) => tx.send(Event::PullTimeout(id)),
        fast_down::Event::Stalled(timeout) => tx.send(Event::Stalled(timeout)),
        fast_down::Event::PullProgress(id, range) => tx.send(Event::PullProgress(id, range)),
        fast_down::Event::Pushing(id, range) => tx.send(Event::Pushing(id, range)),
        fast_down::Event::PushError(id, range, e) => {
            tx.send(Event::PushError(id, range, TaskError::new(e)))
        }
        fast_down::Event::PushProgress(range) => tx.send(Event::PushProgress(range)),
        fast_down::Event::Flushing => tx.send(Event::Flushing),
        fast_down::Event::FlushError(e) => tx.send(Event::FlushError(TaskError::new(e))),
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
