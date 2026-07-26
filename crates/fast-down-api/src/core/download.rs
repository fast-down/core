use crate::{
    Config, DownloadState, Event, PartialConfig, ResumeError, Tx, WriteMethod,
    prefetch::prefetch,
    tx_err,
    utils::{ForceSendExt, build_header, gen_path},
};
use fast_down::{
    BoxPusher, Merge, UrlInfo,
    fast_puller::{FastDownPuller, FastDownPullerOptions, build_client},
    file::{CacheFilePusher, MmapFilePusher},
    handle::SharedHandle,
    invert,
    multi::download_multi,
    reqwest::SmartRedirectClient,
    single::download_single,
};
use inherit_config::ConfigLayer;
use parking_lot::Mutex;
use path_helper::{IterStemExt, tokio::gen_unique_path};
use reqwest::Response;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::{
    fs::{self, File, OpenOptions},
    task::JoinError,
};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Persist the download state after at least this many freshly-written bytes.
const STATE_STORE_BYTES: u64 = 16 * 1024 * 1024;
/// Persist the download state after at least this many `PushProgress` events.
const STATE_STORE_EVENTS: usize = 512;

/// Outcome of trying to acquire a writable `.part` file + the matching resume state.
#[allow(clippy::large_enum_variant)]
enum Acquire {
    /// A file is ready to write, optionally carrying a previously-saved resume state.
    ///
    /// The large fields are boxed so the variant stays small (`large_enum_variant`).
    Ready {
        file: File,
        effective: Config,
        parsed: PartialConfig,
        resume_state: Option<DownloadState>,
    },
    /// Unique-name collision: the caller should regenerate the stem and retry.
    CollisionRetry,
    /// An unrecoverable error was already reported via `tx`; the caller should stop.
    Abort,
}

/// Outcome of probing an existing `.part`/`.fd` pair for resume eligibility.
///
/// The classification is pure (see [`classify_resume`]); this enum only names
/// the three possible results so the resume *contract* lives in one place.
#[allow(clippy::large_enum_variant)]
enum ResumeProbe {
    /// File + state both present and the state still matches the remote file.
    Valid { file: File, state: DownloadState },
    /// An explicit resume was requested but the pair is unusable (stale or
    /// missing); the caller must report and stop.
    GiveUp(ResumeError),
    /// Stale or missing state on a plain download; the caller drops the partial
    /// files and opens fresh.
    Discard,
}

/// Borrow the shared `OpenOptions` presets used to open the `.part` file.
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

pub struct DownloadHandle {
    handle: SharedHandle<()>,
}

impl DownloadHandle {
    /// Wait for the download to complete.
    ///
    /// # Panics
    /// Panics if the background download task exits unexpectedly.
    ///
    /// # Errors
    /// Returns `Arc<JoinError>` if the download task itself panics or is
    /// cancelled.
    pub async fn join(&self) -> Result<(), Arc<JoinError>> {
        self.handle.join().await
    }

    /// # Errors
    pub fn download(
        url: Url,
        partial_config: PartialConfig,
        tx: Tx,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<Self> {
        Self::spawn(url, partial_config, tx, cancel_token, None)
    }

    /// Explicitly resume an interrupted download from the given temporary file.
    ///
    /// `tmp_path` is the `.part` file left behind by a previous (cancelled or
    /// crashed) download.
    ///
    /// - When `tmp_path` **exists**, the download continues from it
    ///   (`force_resume = true`, and the `resume` config flag is forced on).
    /// - When `tmp_path` does **not** exist, the call falls back to a fresh
    ///   [`DownloadHandle::download`] (`force_resume = false`) and starts writing
    ///   at `tmp_path` from scratch.
    ///
    /// Note: even when `tmp_path` exists, if the `.fd` state file is missing or
    /// the remote file changed, an [`Event::ResumeError`] is still emitted —
    /// this is the `resume` contract (unlike `download`, it never silently
    /// falls back to a full re-download).
    ///
    /// # Errors
    pub fn resume(
        tmp_path: impl AsRef<Path>,
        url: Url,
        partial_config: PartialConfig,
        tx: Tx,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<Self> {
        // Hand the path down verbatim; `run` probes its existence asynchronously
        // via `tokio::fs::metadata` (never a synchronous `Path::exists()`, which
        // would block the calling executor thread on a slow/network mount).
        Self::spawn(
            url,
            partial_config,
            tx,
            cancel_token,
            Some(tmp_path.as_ref().to_path_buf()),
        )
    }

    /// Shared entry point for [`DownloadHandle::download`] and
    /// [`DownloadHandle::resume`]. Builds the HTTP client (errors here propagate
    /// to the caller via `?`), then spawns the background task that runs the
    /// entire download pipeline in [`Self::run`].
    ///
    /// # Errors
    fn spawn(
        url: Url,
        partial_config: PartialConfig,
        tx: Tx,
        cancel_token: CancellationToken,
        tmp_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let config = partial_config.clone().build();
        let client = build_client(
            build_header(&config.headers),
            config.proxy.as_deref(),
            config.accept_invalid_certs,
            config.accept_invalid_hostnames,
            config.cookie_store,
            config.local_address.first().copied(),
            config.max_redirects,
        )?;
        let handle = tokio::spawn(
            Self::run(
                url,
                config,
                partial_config,
                client,
                tx,
                cancel_token,
                tmp_path,
            )
            .force_send(),
        );
        let handle = SharedHandle::new(handle);
        Ok(Self { handle })
    }

    /// The entire download pipeline, consolidated into a single function:
    ///
    /// 1. prefetch + resolve the destination path,
    /// 2. per-iteration: try to resume from an existing `.part`/`.fd` pair (or
    ///    start fresh), then build the puller/pusher and run the transfer
    ///    (multi or single),
    /// 3. forward engine events, persist the resume state with debounce,
    /// 4. on cancel keep `.part`+`.fd`; otherwise rename into place and drop
    ///    the state file.
    ///
    /// In `overwrite` mode (`unique = false`) a single create attempt is made;
    /// in `unique` mode (`unique = true`) a fresh `create_new` is retried with a
    /// regenerated stem (`xxx (1).mp4`, …) until a free path is found.
    ///
    /// The whole `run` future is wrapped in `force_send` at the `spawn` site
    /// because the futures it drives are not provably `Send`: they move the
    /// file handle / response into the engine builders, and the engine's
    /// event-channel receiver is `!Send`. The future still runs on this task,
    /// so the `unsafe impl Send` assertions are sound. The transfer event loop
    /// is kept *outside* `run_until_cancelled` (only the puller/pusher build
    /// uses it) so a mid-download cancel is handled by the loop's final state
    /// store rather than being silently dropped.
    #[allow(clippy::too_many_lines)]
    async fn run(
        url: Url,
        mut config: Config,
        partial_config: PartialConfig,
        client: SmartRedirectClient,
        tx: Tx,
        cancel_token: CancellationToken,
        tmp_path: Option<PathBuf>,
    ) {
        let Some((info, resp)) = prefetch(&url, &config, &client, &tx).await else {
            return;
        };

        // Resolve whether we are resuming from an explicit temp file (resume())
        // or starting a fresh download (download()). Never a synchronous
        // `Path::exists()`, which would block the executor on a slow mount.
        let (tmp_path, tmp_exists) = match tmp_path {
            Some(path) => {
                if fs::try_exists(&path).await.unwrap_or(false) {
                    config.resume = true;
                    (Some(path), true)
                } else {
                    (None, false)
                }
            }
            None => (None, false),
        };

        // Resolve the destination path + whether we run in unique-name mode.
        let (origin_final, unique) = if let Some(path) = &tmp_path {
            (path.with_extension(""), false)
        } else {
            let p = tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError);
            (p, !config.overwrite)
        };

        if tmp_exists && !info.fast_download {
            let _ = tx.send(Event::ResumeError(ResumeError::NotResumable));
            return;
        }
        let can_resume = config.resume && info.fast_download;

        // `resp` is consumed exactly once, by `build_pipeline` on the iteration
        // that actually starts a transfer (collision retries `continue` before it).
        for final_path in origin_final.iter_stem() {
            let tmp = final_path.with_added_extension("part");
            let cfg = final_path.with_added_extension("fd");

            // ---- 1. Resume from / open the `.part` file (handles unique collisions) ----
            let acquired = try_acquire_target(
                &tx,
                can_resume,
                tmp_exists,
                unique,
                &info,
                &partial_config,
                &config,
                &tmp,
                &cfg,
            )
            .await;
            let (file, effective, parsed, resume_state) = match acquired {
                Acquire::CollisionRetry => continue,
                Acquire::Abort => return,
                Acquire::Ready {
                    file,
                    effective,
                    parsed,
                    resume_state,
                } => (file, effective, parsed, resume_state),
            };

            // ===== attempt: build state, emit events, transfer, persist, rename =====
            let is_resume = resume_state.is_some();
            let mut state =
                resume_state.unwrap_or_else(|| DownloadState::new(&url, &info, &parsed, &cfg));

            // Announce a resumed download. The progress ranges were already folded
            // into `effective.downloaded_chunk` inside `try_acquire_target`, so here
            // we only notify the consumer.
            if is_resume {
                let _ = tx.send(Event::Resumed {
                    config_path: cfg.clone(),
                    progress: state.progress.clone().unwrap_or_default(),
                    size: info.size,
                });
            }
            let _ = tx.send(Event::Start {
                tmp_path: tmp.clone(),
                config_path: cfg.clone(),
                parsed_config: parsed,
            });

            // ---- 3a. Build the puller + pusher (force_send; not provably Send) ----
            // The build itself is fast and non-blocking, so a cancel landing here
            // is rare and simply ends the task without a transfer.
            let Some((puller, pusher)) = build_pipeline(
                &url,
                &effective,
                &info,
                file,
                resp,
                cancel_token.clone(),
                &tx,
            )
            .await
            else {
                // Build failed (errored or cancelled before transfer): persist the
                // (possibly empty) state so a later resume still finds a coherent .fd.
                let _ = state.store().await;
                return;
            };

            // ---- 3b. Run the transfer + forward engine events with debounced store ----
            // Multi-threaded when the server supports range requests, otherwise a
            // single sequential stream. Kept outside `run_until_cancelled` (see
            // `run` doc) so a mid-download cancel is handled by this loop's final
            // state store rather than being silently dropped.
            let res = if info.fast_download {
                download_multi(
                    puller,
                    pusher,
                    fast_down::multi::DownloadOptions {
                        download_chunks: invert(
                            effective.downloaded_chunk.into_iter(),
                            info.size,
                            effective.chunk_window,
                        ),
                        concurrent: effective.threads,
                        retry_gap: effective.retry_gap,
                        pull_timeout: effective.pull_timeout,
                        push_queue_cap: effective.write_queue_cap,
                        min_chunk_size: effective.min_chunk_size,
                        max_speculative: effective.max_speculative,
                    },
                )
            } else {
                download_single(
                    puller,
                    pusher,
                    fast_down::single::DownloadOptions {
                        retry_gap: effective.retry_gap,
                        push_queue_cap: effective.write_queue_cap,
                    },
                )
            };

            // All events (including `PushProgress`) flow through the single
            // `event_chain`. `PushProgress` also updates `state` with debounced
            // store so the `.fd` always reflects the truth.
            let mut store_events: usize = 0;
            let mut store_bytes: u64 = 0;
            while let Ok(e) = res.event_chain.recv().await {
                if let fast_down::Event::PushProgress(range) = &e {
                    store_events += 1;
                    store_bytes += range.end - range.start;
                    state.merge_progress(range.clone());
                }
                let _ = match e {
                    fast_down::Event::Pulling(id) => tx.send(Event::Pulling(id)),
                    fast_down::Event::PullError(id, e) => {
                        tx.send(Event::PullError(id, anyhow::anyhow!(e)))
                    }
                    fast_down::Event::PullTimeout(id) => tx.send(Event::PullTimeout(id)),
                    fast_down::Event::PullProgress(id, range) => {
                        tx.send(Event::PullProgress(id, range))
                    }
                    fast_down::Event::Pushing(id, range) => tx.send(Event::Pushing(id, range)),
                    fast_down::Event::PushError(id, range, e) => {
                        tx.send(Event::PushError(id, range, anyhow::anyhow!(e)))
                    }
                    fast_down::Event::PushProgress(range) => tx.send(Event::PushProgress(range)),
                    fast_down::Event::Flushing => tx.send(Event::Flushing),
                    fast_down::Event::FlushError(e) => {
                        tx.send(Event::FlushError(anyhow::anyhow!(e)))
                    }
                    fast_down::Event::Finished(id) => tx.send(Event::Finished(id)),
                };
                if store_events >= STATE_STORE_EVENTS || store_bytes >= STATE_STORE_BYTES {
                    let _ = state.store().await;
                    store_events = 0;
                    store_bytes = 0;
                }
            }

            if let Err(e) = res.join().await {
                let _ = tx.send(Event::JoinError(e));
            }

            // The `.fd` state file captures the current download progress (url,
            // config, size, etag) so a later resume can validate the remote file
            // identity. Called unconditionally even when cancelled before the
            // transfer finished.
            let _ = state.store().await;
            // If the download was cancelled, do NOT rename the `.part` and do NOT
            // remove the `.fd` state file: keep both so a later `download()`/
            // `resume()` can continue from where it left off (design doc §8).
            if cancel_token.is_cancelled() {
                return;
            }
            finalize(&tx, unique, &tmp, &cfg, &final_path).await;
            return;
        }
    }
}

/// Try to resume from an existing `.part`/`.fd` pair, or open a fresh `.part`
/// file. Returns:
///
/// - [`Acquire::Ready`] with a writable `file` + the effective config + the
///   resume state (if any);
/// - [`Acquire::CollisionRetry`] in unique mode when `create_new` failed
///   (treats the failure as a name collision and asks the caller to retry);
/// - [`Acquire::Abort`] when an unrecoverable error has already been reported
///   through `tx` (e.g. a `resume()` contract violation, or a non-unique open
///   failure), and the caller should stop.
///
/// Classify the result of probing an existing `.part`/`.fd` pair for resume.
///
/// Pure: no I/O, no event emission. The two probe results (`open` the `.part`,
/// `load` the `.fd`) map onto one of three outcomes so the resume *contract*
/// lives in a single, unit-testable place. The outcomes are
/// [`ResumeProbe::Valid`] (file + state present and still match the remote
/// file; resume from it), [`ResumeProbe::GiveUp`] (an explicit resume was
/// requested but the pair is unusable; report and stop), and
/// [`ResumeProbe::Discard`] (stale or missing state on a plain download; drop
/// the partial files and open fresh).
fn classify_resume<E>(
    open_res: std::io::Result<File>,
    load_res: Result<DownloadState, E>,
    tmp_exists: bool,
    info: &UrlInfo,
) -> ResumeProbe {
    match (open_res, load_res) {
        (Ok(file), Ok(state)) if state.validate(info) => ResumeProbe::Valid { file, state },
        (Ok(_), Ok(_)) if tmp_exists => ResumeProbe::GiveUp(ResumeError::FileChanged),
        _ if tmp_exists => ResumeProbe::GiveUp(ResumeError::NoStateFile),
        _ => ResumeProbe::Discard,
    }
}

#[allow(clippy::too_many_arguments)]
async fn try_acquire_target(
    tx: &Tx,
    can_resume: bool,
    tmp_exists: bool,
    unique: bool,
    info: &UrlInfo,
    partial_config: &PartialConfig,
    config: &Config,
    tmp: &Path,
    cfg: &Path,
) -> Acquire {
    // ---- 1. Try to resume from an existing `.part`/`.fd` pair ----
    if can_resume {
        let opener = open_existing();
        let (open_res, load_res) = tokio::join!(opener.open(tmp), DownloadState::load(cfg));
        match classify_resume(open_res, load_res, tmp_exists, info) {
            ResumeProbe::Valid { file, state } => {
                let mut pc = partial_config.clone();
                if let Some(c) = &state.config {
                    pc.inherit_from(c);
                }
                let parsed = pc.clone();
                let mut effective = pc.build();
                // Fold the saved progress into the engine's "already downloaded"
                // set so it only fetches the remaining bytes. This is the other
                // half of reconstructing `effective` from the resume state; the
                // `config` half above comes from `state.config`.
                if let Some(progress) = state.progress.clone() {
                    for p in progress {
                        effective.downloaded_chunk.merge_progress(p);
                    }
                }
                return Acquire::Ready {
                    file,
                    effective,
                    parsed,
                    resume_state: Some(state),
                };
            }
            ResumeProbe::GiveUp(err) => {
                // Explicit resume target but the `.part`/`.fd` pair is unusable
                // (stale or missing): report and stop rather than silently
                // re-downloading (resume contract).
                let _ = tx.send(Event::ResumeError(err));
                return Acquire::Abort;
            }
            ResumeProbe::Discard => {
                // Stale or missing state on a plain download: drop the partial
                // files and fall through to a fresh open below.
                let _ = fs::remove_file(tmp).await;
                let _ = fs::remove_file(cfg).await;
            }
        }
    }

    // ---- 2. Fresh open (also the fall-through after discarding a stale state) ----
    let f = if unique {
        open_create_new().open(tmp).await.ok()
    } else {
        match open_create().open(tmp).await {
            Ok(f) => Some(f),
            Err(e) => {
                let _ = tx.send(Event::BuildPusherError(e));
                return Acquire::Abort;
            }
        }
    };
    f.map_or_else(
        || Acquire::CollisionRetry,
        |file| Acquire::Ready {
            file,
            effective: config.clone(),
            parsed: partial_config.clone(),
            resume_state: None,
        },
    )
}

/// Build the puller + pusher inside a `force_send` + `run_until_cancelled`
/// future (not provably `Send`; see `run` doc). Yields `None` on a build error
/// (the error event is already sent through `tx`) or on cancel-before-transfer;
/// the caller is responsible for persisting state and stopping.
async fn build_pipeline(
    url: &Url,
    effective: &Config,
    info: &UrlInfo,
    file: File,
    resp: Response,
    cancel_token: CancellationToken,
    tx: &Tx,
) -> Option<(FastDownPuller, BoxPusher)> {
    let ct = cancel_token;
    let resp = Some(Arc::new(Mutex::new(Some(resp))));
    let built = ct
        .run_until_cancelled(async move {
            let puller = FastDownPuller::new(FastDownPullerOptions {
                url: url.clone(),
                headers: build_header(&effective.headers).into(),
                proxy: effective.proxy.as_deref(),
                accept_invalid_certs: effective.accept_invalid_certs,
                accept_invalid_hostnames: effective.accept_invalid_hostnames,
                cookie_store: effective.cookie_store,
                file_id: info.file_id.clone(),
                resp,
                available_ips: effective.local_address.clone().into(),
                max_redirects: effective.max_redirects,
            })
            .map_err(Event::BuildClientError)?;
            let pusher = if cfg!(target_pointer_width = "64")
                && info.fast_download
                && effective.write_method == WriteMethod::Mmap
            {
                MmapFilePusher::new(&file, info.size, effective.sync_all)
                    .await
                    .map(BoxPusher::new)
            } else {
                CacheFilePusher::new(
                    file,
                    info.size,
                    effective.sync_all,
                    effective.cache_high_watermark,
                    effective.cache_low_watermark,
                    effective.write_buffer_size,
                )
                .await
                .map(BoxPusher::new)
            }
            .map_err(Event::BuildPusherError)?;
            Ok::<_, Event>((puller, pusher))
        })
        .await;
    match built {
        Some(Ok(b)) => Some(b),
        Some(Err(e)) => {
            let _ = tx.send(e);
            None
        }
        None => None,
    }
}

/// Rename the finished `.part` into place and drop the `.fd` state file.
///
/// In unique mode, a fresh unique destination is reserved right before rename
/// via `gen_unique_path` (atomic `create_new`), closing the TOCTOU gap where
/// the final file could have been created by someone else during the download.
/// On any failure the relevant error event is sent through `tx`.
async fn finalize(tx: &Tx, unique: bool, tmp: &Path, cfg: &Path, final_path: &Path) {
    let dest = if unique {
        match gen_unique_path(final_path).await {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.send(Event::GenPathError(e));
                return;
            }
        }
    } else {
        final_path.to_path_buf()
    };
    if let Err(e) = fs::rename(tmp, &dest).await {
        // Best-effort: drop the empty placeholder we just reserved so a failed
        // rename doesn't leave an orphan `xxx (1).mp4` behind.
        if unique {
            let _ = fs::remove_file(&dest).await;
        }
        let _ = tx.send(Event::RenameFailed(e));
        return;
    }
    let _ = tx.send(Event::Renamed(dest));
    // Success: the download is complete and renamed, so the state file is no
    // longer needed. Best-effort cleanup only (kept on cancel).
    let _ = fs::remove_file(cfg).await;
}
