use crate::{
    Config, DownloadState, Event, PartialConfig, ResumeError, Tx, WriteMethod,
    prefetch::prefetch,
    tx_err,
    utils::{ForceSendExt, build_header, gen_path},
};
use fast_down::{
    BoxPusher, Merge,
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
use path_helper::{FileStemExt, tokio::gen_unique_path};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::{
    fs::{self, File, OpenOptions},
    select,
    task::JoinError,
};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Persist the download state after at least this many freshly-written bytes.
const STATE_STORE_BYTES: u64 = 16 * 1024 * 1024;
/// Persist the download state after at least this many `PushProgress` events.
const STATE_STORE_EVENTS: usize = 512;

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
        let handle = tokio::spawn(Self::run(
            url,
            config,
            partial_config,
            client,
            tx,
            cancel_token,
            tmp_path,
        ));
        let handle = SharedHandle::new(handle);
        Ok(Self { handle })
    }

    /// The entire download pipeline, consolidated into a single function:
    ///
    /// 1. prefetch + resolve the destination path,
    /// 2. try to resume from an existing `.part`/`.fd` pair (or start fresh),
    /// 3. build the puller/pusher and run the transfer (multi or single),
    /// 4. forward engine events, persist the resume state with debounce,
    /// 5. on cancel keep `.part`+`.fd`; otherwise rename into place and drop
    ///    the state file.
    ///
    /// In `overwrite` mode (`unique = false`) a single create attempt is made;
    /// in `unique` mode (`unique = true`) a fresh `create_new` is retried with a
    /// regenerated stem (`xxx (1).mp4`, …) until a free path is found.
    ///
    /// The build step and the transfer step are each wrapped in `force_send`
    /// because the futures involved are not provably `Send` (they move the file
    /// handle / response into the engine builders, and the engine's event-
    /// channel receiver is `!Send`). They still run on this task, so the
    /// `unsafe impl Send` assertions are sound. The transfer step is kept
    /// *outside* `run_until_cancelled` so a mid-download cancel is handled by
    /// its own event loop + final state store, rather than being silently
    /// dropped by `run_until_cancelled`.
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
        // Probe the caller-provided `tmp_path` asynchronously so we never block
        // the executor thread with a synchronous `Path::exists()` (which would
        // stall a `current_thread` runtime or a slow network mount on the
        // calling worker). `try_exists` is the async, error-distinguishing
        // equivalent of `std::path::Path::exists`; we treat an undeterminable
        // probe (`Err`) the same as "absent" and fall back to a fresh download.
        // A present `.part` means we must force-resume this exact target.
        let tmp_exists = match &tmp_path {
            Some(p) => tokio::fs::try_exists(p).await.unwrap_or(false),
            None => false,
        };
        if tmp_exists {
            config.resume = true;
        }
        // When an explicit final path is given (resume with a caller-provided
        // `tmp_path`), use it verbatim and never auto-regenerate a unique name.
        let (origin_final, unique) = if tmp_exists {
            let base = tmp_path.as_ref().unwrap().with_extension("");
            (base, false)
        } else {
            (
                tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError),
                !config.overwrite,
            )
        };
        let can_resume = config.resume && info.fast_download;

        if tmp_exists && !info.fast_download {
            let _ = tx.send(Event::ResumeError(ResumeError::NotResumable));
            return;
        }

        let open_existing = {
            let mut o = OpenOptions::new();
            o.read(true).write(true).truncate(false).create(false);
            o
        };
        let open_create = {
            let mut o = OpenOptions::new();
            o.read(true).write(true).truncate(false).create(true);
            o
        };
        let open_create_new = {
            let mut o = OpenOptions::new();
            o.read(true).write(true).truncate(false).create_new(true);
            o
        };

        let mut final_path = origin_final.clone();
        let mut i = 0usize;
        loop {
            let tmp = final_path.with_added_extension("part");
            let cfg = final_path.with_added_extension("fd");

            // ---- 1. Try to resume from an existing `.part`/`.fd` pair ----
            let resumed: Option<(File, Config, PartialConfig, DownloadState)> = if can_resume {
                let (open_res, load_res) =
                    tokio::join!(open_existing.open(&tmp), DownloadState::load(&cfg));
                match (open_res, load_res) {
                    (Ok(file), Ok(state)) if state.validate(&info) => {
                        let mut pc = partial_config.clone();
                        if let Some(c) = &state.config {
                            pc.inherit_from(c);
                        }
                        let parsed = pc.clone();
                        let effective = pc.build();
                        Some((file, effective, parsed, state))
                    }
                    (Ok(_), Ok(_)) => {
                        if tmp_exists {
                            let _ = tx.send(Event::ResumeError(ResumeError::FileChanged));
                            return;
                        }
                        // Stale state: discard and start a fresh download.
                        let _ = fs::remove_file(&tmp).await;
                        let _ = fs::remove_file(&cfg).await;
                        None
                    }
                    _ => {
                        if tmp_exists {
                            let _ = tx.send(Event::ResumeError(ResumeError::NoStateFile));
                            return;
                        }
                        None
                    }
                }
            } else {
                None
            };

            // ---- 2. Pick the file to write + matching resume state ----
            let (file, mut effective, parsed, resume_state): (
                File,
                Config,
                PartialConfig,
                Option<DownloadState>,
            ) = if let Some((f, eff, par, st)) = resumed {
                (f, eff, par, Some(st))
            } else {
                let f = if unique {
                    open_create_new.open(&tmp).await.ok()
                } else {
                    Some(tx_err!(open_create.open(&tmp).await, tx, BuildPusherError))
                };
                if let Some(f) = f {
                    let parsed = partial_config.clone();
                    (f, config.clone(), parsed, None)
                } else {
                    if !unique {
                        return;
                    }
                    // Collision in unique mode: regenerate the stem and retry.
                    i += 1;
                    final_path = origin_final.with_added_file_stem_prefix(format!(" {i}"));
                    continue;
                }
            };

            // ===== attempt: start, download, persist, rename =====
            let is_resume = resume_state.is_some();
            let mut state =
                resume_state.unwrap_or_else(|| DownloadState::new(&url, &info, &parsed, &cfg));

            // Fold the saved progress into the engine's "already downloaded" set
            // so it only fetches the remaining bytes.
            if is_resume {
                if let Some(progress) = state.progress.clone() {
                    for p in progress {
                        effective.downloaded_chunk.merge_progress(p);
                    }
                }
                let _ = tx.send(Event::Resumed {
                    config_path: cfg.clone(),
                    progress: state.progress.clone().unwrap_or_default(),
                    size: info.size,
                });
            }
            let _ = tx.send(Event::Start {
                tmp_path: tmp.clone(),
                config_path: cfg.clone(),
                url_info: info.clone(),
                parsed_config: parsed,
            });

            // ---- 3a. Build the puller + pusher ----
            // Wrapped in `force_send` (not provably `Send`). The build itself is
            // fast and non-blocking, so a cancel landing here is rare and simply
            // ends the task without a transfer.
            let url_ref = &url;
            let config_ref = &effective;
            let info_ref = &info;
            let ct = cancel_token.clone();
            let resp = Some(Arc::new(Mutex::new(Some(resp))));
            let built = ct
                .run_until_cancelled(async move {
                    let puller = FastDownPuller::new(FastDownPullerOptions {
                        url: url_ref.clone(),
                        headers: build_header(&config_ref.headers).into(),
                        proxy: config_ref.proxy.as_deref(),
                        accept_invalid_certs: config_ref.accept_invalid_certs,
                        accept_invalid_hostnames: config_ref.accept_invalid_hostnames,
                        cookie_store: config_ref.cookie_store,
                        file_id: info_ref.file_id.clone(),
                        resp,
                        available_ips: config_ref.local_address.clone().into(),
                        max_redirects: config_ref.max_redirects,
                    })
                    .map_err(|e| Box::new(Event::BuildClientError(e)))?;
                    let pusher = if cfg!(target_pointer_width = "64")
                        && info_ref.fast_download
                        && config_ref.write_method == WriteMethod::Mmap
                    {
                        MmapFilePusher::new(&file, info_ref.size, config_ref.sync_all)
                            .await
                            .map(BoxPusher::new)
                    } else {
                        CacheFilePusher::new(
                            file,
                            info_ref.size,
                            config_ref.sync_all,
                            config_ref.cache_high_watermark,
                            config_ref.cache_low_watermark,
                            config_ref.write_buffer_size,
                        )
                        .await
                        .map(BoxPusher::new)
                    }
                    .map_err(|e| Box::new(Event::BuildPusherError(e)))?;
                    Ok::<_, Box<Event>>((puller, pusher))
                })
                .force_send()
                .await;
            let (puller, pusher) = match built {
                Some(Ok(b)) => b,
                Some(Err(e)) => {
                    let _ = tx.send(*e);
                    // Persist the (possibly empty) state so a later resume still
                    // finds a coherent `.fd`, then stop.
                    state.update(|_| {});
                    let _ = state.store().await;
                    return;
                }
                None => {
                    // Cancelled before the transfer started: keep the partial
                    // files and persist state so a later `resume()` can continue.
                    state.update(|_| {});
                    let _ = state.store().await;
                    return;
                }
            };

            // ---- 3b. Run the transfer ----
            // Multi-threaded when the server supports range requests, otherwise a
            // single sequential stream. This is a *separate* `force_send` future
            // (the engine's event-channel receiver is `!Send`) kept outside
            // `run_until_cancelled`, so a mid-download cancel is handled by this
            // future's own event loop + final state store — `run_until_cancelled`
            // would otherwise drop the inner future and skip persisting `.fd`.
            let res = if info.fast_download {
                download_multi(
                    puller,
                    pusher,
                    fast_down::multi::DownloadOptions {
                        download_chunks: invert(
                            effective.downloaded_chunk.iter().cloned(),
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

            let tx2 = tx.clone();
            let state_ref = &mut state;
            let ct2 = cancel_token.clone();
            (async move {
                // All events (including `PushProgress`) flow through the single
                // `event_chain`. `PushProgress` also updates `state` with
                // debounced store so the `.fd` always reflects the truth.
                let mut store_events: usize = 0;
                let mut store_bytes: u64 = 0;
                loop {
                    select! {
                        e = res.event_chain.recv() => {
                            match e {
                                Ok(e) => {
                                    if let fast_down::Event::PushProgress(range) = &e {
                                        store_events += 1;
                                        store_bytes += range.end - range.start;
                                        state_ref.merge_progress(range.clone());
                                    }
                                    let _ = match e {
                                        fast_down::Event::Pulling(id) => tx2.send(Event::Pulling(id)),
                                        fast_down::Event::PullError(id, e) => tx2.send(Event::PullError(id, anyhow::anyhow!(e))),
                                        fast_down::Event::PullTimeout(id) => tx2.send(Event::PullTimeout(id)),
                                        fast_down::Event::PullProgress(id, range) => tx2.send(Event::PullProgress(id, range)),
                                        fast_down::Event::Pushing(id, range) => tx2.send(Event::Pushing(id, range)),
                                        fast_down::Event::PushError(id, range, e) => tx2.send(Event::PushError(id, range, anyhow::anyhow!(e))),
                                        fast_down::Event::PushProgress(range) => tx2.send(Event::PushProgress(range)),
                                        fast_down::Event::Flushing => tx2.send(Event::Flushing),
                                        fast_down::Event::FlushError(e) => tx2.send(Event::FlushError(anyhow::anyhow!(e))),
                                        fast_down::Event::Finished(id) => tx2.send(Event::Finished(id)),
                                    };
                                    if store_events >= STATE_STORE_EVENTS
                                        || store_bytes >= STATE_STORE_BYTES
                                    {
                                        let _ = state_ref.store().await;
                                        store_events = 0;
                                        store_bytes = 0;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        () = ct2.cancelled() => break,
                    }
                }

                if let Err(e) = res.join().await {
                    let _ = tx2.send(Event::JoinError(e));
                }
                // Force a final store so the `.fd` always reflects the truth
                // (also covers the cancelled-before-finish case).
                let _ = state_ref.store().await;
            })
            .force_send()
            .await;

            // The `.fd` state file captures the current download progress (url,
            // config, size, etag) so a later resume can validate the remote file
            // identity. Called unconditionally even when cancelled before the
            // transfer finished.
            state.update(|_| {});
            let _ = state.store().await;
            // If the download was cancelled, do NOT rename the `.part` and do NOT
            // remove the `.fd` state file: keep both so a later `download()`/
            // `resume()` can continue from where it left off (design doc §8).
            if cancel_token.is_cancelled() {
                return;
            }
            // In unique mode, reserve a fresh unique destination right before
            // rename: the final file may have been created by someone else while
            // we were downloading. `gen_unique_path` atomically creates an empty
            // placeholder (create_new) which the rename below then replaces,
            // closing the TOCTOU gap.
            let dest = if unique {
                tx_err!(gen_unique_path(&final_path).await, tx, GenPathError)
            } else {
                final_path.clone()
            };
            if let Err(e) = fs::rename(&tmp, &dest).await {
                // Best-effort: drop the empty placeholder we just reserved so a
                // failed rename doesn't leave an orphan `xxx (1).mp4` behind.
                if unique {
                    let _ = fs::remove_file(&dest).await;
                }
                let _ = tx.send(Event::RenameFailed(e));
                return;
            }
            let _ = tx.send(Event::Renamed(dest));
            // Success: the download is complete and renamed, so the state file is
            // no longer needed. Best-effort cleanup only (kept on cancel).
            let _ = fs::remove_file(&cfg).await;
            return;
        }
    }
}
