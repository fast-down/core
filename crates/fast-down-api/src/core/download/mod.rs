use crate::{
    Config, DownloadState, Event, PartialConfig, ResumeError, Tx, prefetch, tx_err,
    utils::{ForceSendExt, build_header, gen_path},
};
use fast_down::{
    Total, fast_puller::build_client, handle::SharedHandle, invert, multi::download_multi,
    reqwest::SmartRedirectClient, single::download_single,
};
use inherit_config::ConfigLayer;
use path_helper::IterStemExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Persist the download state after at least this many freshly-written bytes.
const STATE_STORE_BYTES: u64 = 16 * 1024 * 1024;
/// Persist the download state after at least this many `PushProgress` events.
const STATE_STORE_EVENTS: usize = 512;

mod acquire;
mod finalize;
mod pipeline;

use acquire::{Acquire, try_acquire_target};
use finalize::finalize;
use pipeline::build_pipeline;

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
        let tmp_path = if let Some(path) = tmp_path
            && fs::try_exists(&path).await.unwrap_or(false)
        {
            config.resume = true;
            Some(path)
        } else {
            None
        };

        // Resolve the destination path + whether we run in unique-name mode.
        let (origin_final, unique) = if let Some(path) = &tmp_path {
            (path.with_extension(""), false)
        } else {
            let p = tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError);
            (p, !config.overwrite)
        };

        if tmp_path.is_some() && !info.fast_download {
            let _ = tx.send(Event::ResumeError(ResumeError::NotResumable));
            return;
        }
        let can_resume = config.resume && info.fast_download;
        // `explicit_resume` is true only when this is an explicit `resume()` call
        // (a `tmp_path` was supplied and the `.part` exists); a plain `download()`
        // is `false` and must silently fall back instead of erroring.
        let explicit_resume = tmp_path.is_some();

        // `resp` is consumed exactly once, by `build_pipeline` on the iteration
        // that actually starts a transfer (collision retries `continue` before it).
        for final_path in origin_final.iter_stem() {
            let tmp = final_path.with_added_extension("part");
            let cfg = final_path.with_added_extension("fd");

            // ---- 1. Resume from / open the `.part` file (handles unique collisions) ----
            let acquired =
                try_acquire_target(&tx, can_resume, explicit_resume, unique, &info, &partial_config, &tmp, &cfg)
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
                    store_bytes += range.total();
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
