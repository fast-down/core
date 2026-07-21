use crate::{
    Config, DownloadState, Event, PartialConfig, Tx, WriteMethod,
    prefetch::prefetch,
    tx_err,
    utils::{ForceSendExt, build_header, gen_path},
};
use fast_down::{
    BoxPusher, UrlInfo,
    fast_puller::{FastDownPuller, FastDownPullerOptions, build_client},
    file::{CacheFilePusher, MmapFilePusher},
    handle::SharedHandle,
    multi::download_multi,
    single::download_single,
};
use inherit_config::ConfigLayer;
use parking_lot::Mutex;
use path_helper::{FileStemExt, tokio::gen_unique_path};
use reqwest::Response;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::{
    fs::{self, File, OpenOptions},
    select,
};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Per-task context that stays constant across all resume/attempt iterations.
struct Ctx {
    url: Url,
    info: UrlInfo,
    tx: Tx,
    cancel_token: CancellationToken,
}

/// Paths used by a single download attempt.
struct AttemptPaths<'a> {
    tmp: &'a Path,
    config_path: &'a Path,
    final_path: &'a Path,
    /// When true (unique mode), regenerate a fresh unique destination right
    /// before rename so a concurrently-created final file is never overwritten.
    unique: bool,
}

pub struct DownloadHandle {
    handle: SharedHandle<()>,
}

impl DownloadHandle {
    /// # Errors
    pub fn download(
        url: Url,
        partial_config: PartialConfig,
        tx: Tx,
        cancel_token: CancellationToken,
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
        let handle = tokio::spawn(async move {
            let Some((info, resp)) = prefetch(&url, &config, &client, &tx).await else {
                return;
            };
            let origin_final = tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError);
            let ctx = Ctx {
                url,
                info,
                tx,
                cancel_token,
            };
            let resp = Some(Arc::new(Mutex::new(Some(resp))));
            if config.overwrite {
                Self::run_overwrite(&ctx, config, partial_config, resp, origin_final).await;
            } else {
                Self::run_unique(&ctx, config, partial_config, resp, origin_final).await;
            }
        });
        let handle = SharedHandle::new(handle);
        Ok(Self { handle })
    }

    async fn run_overwrite(
        ctx: &Ctx,
        config: Config,
        partial_config: PartialConfig,
        resp: Option<Arc<Mutex<Option<Response>>>>,
        origin_final: PathBuf,
    ) {
        let can_resume = config.resume && ctx.info.fast_download;
        let tmp = origin_final.with_added_extension("part");
        let cfg = origin_final.with_added_extension("fd");

        if can_resume
            && let no_create = Self::open_existing()
            && let (Ok(file), Ok(state)) =
                tokio::join!(no_create.open(&tmp), DownloadState::load(&cfg))
        {
            let mut partial_config = partial_config;
            if let Some(config) = &state.config {
                partial_config.inherit_from(config);
            }
            let parsed = partial_config.clone();
            let effective = partial_config.build();
            let paths = AttemptPaths {
                tmp: &tmp,
                config_path: &cfg,
                final_path: &origin_final,
                unique: false,
            };
            Self::attempt(ctx, file, &paths, effective, parsed, resp).await;
            return;
        }
        let create = Self::open_create();
        let file = tx_err!(create.open(&tmp).await, ctx.tx, BuildPusherError);
        let paths = AttemptPaths {
            tmp: &tmp,
            config_path: &cfg,
            final_path: &origin_final,
            unique: false,
        };
        Self::attempt(ctx, file, &paths, config, partial_config, resp).await;
    }

    async fn run_unique(
        ctx: &Ctx,
        config: Config,
        partial_config: PartialConfig,
        resp: Option<Arc<Mutex<Option<Response>>>>,
        origin_final: PathBuf,
    ) {
        let can_resume = config.resume && ctx.info.fast_download;
        let mut i = 0;
        let mut final_path = origin_final.clone();
        let mut tmp = origin_final.with_added_extension("part");
        let mut cfg = origin_final.with_added_extension("fd");
        let no_create = Self::open_existing();
        let only_create = Self::open_create_new();

        loop {
            if can_resume
                && let (Ok(file), Ok(state)) =
                    tokio::join!(no_create.open(&tmp), DownloadState::load(&cfg))
            {
                let mut partial_config = partial_config;
                if let Some(config) = &state.config {
                    partial_config.inherit_from(config);
                }
                let parsed = partial_config.clone();
                let effective = partial_config.build();
                let paths = AttemptPaths {
                    tmp: &tmp,
                    config_path: &cfg,
                    final_path: &final_path,
                    unique: true,
                };
                Self::attempt(ctx, file, &paths, effective, parsed, resp).await;
                return;
            }
            if let Ok(file) = only_create.open(&tmp).await {
                let paths = AttemptPaths {
                    tmp: &tmp,
                    config_path: &cfg,
                    final_path: &final_path,
                    unique: true,
                };
                Self::attempt(
                    ctx,
                    file,
                    &paths,
                    config.clone(),
                    partial_config.clone(),
                    resp,
                )
                .await;
                return;
            }
            i += 1;
            final_path = origin_final.with_added_file_stem_prefix(format!(" {i}"));
            tmp = final_path.with_added_extension("part");
            cfg = final_path.with_added_extension("fd");
        }
    }

    /// Emit `Event::Start`, run the actual download, then rename `.part` to its
    /// final destination and emit `Event::Renamed` with the real landing path.
    /// In unique mode the destination is regenerated via `gen_unique_path` right
    /// before rename, so a final file created concurrently during the download is
    /// never overwritten (it lands as `xxx (1).mp4` instead). `resp` is consumed
    /// here exactly once.
    async fn attempt(
        ctx: &Ctx,
        file: File,
        paths: &AttemptPaths<'_>,
        effective: Config,
        parsed: PartialConfig,
        resp: Option<Arc<Mutex<Option<Response>>>>,
    ) {
        let _ = ctx.tx.send(Event::Start {
            tmp_path: paths.tmp.to_path_buf(),
            config_path: paths.config_path.to_path_buf(),
            url_info: ctx.info.clone(),
            parsed_config: parsed,
        });
        Self::overwrite(
            file,
            ctx.url.clone(),
            effective,
            ctx.info.clone(),
            resp,
            ctx.tx.clone(),
            ctx.cancel_token.clone(),
        )
        .force_send()
        .await;
        // In unique mode, reserve a fresh unique destination right before rename:
        // the final file may have been created by someone else while we were
        // downloading. `gen_unique_path` atomically creates an empty placeholder
        // (create_new) which the rename below then replaces, closing the TOCTOU gap.
        let dest = if paths.unique {
            tx_err!(gen_unique_path(paths.final_path).await, ctx.tx, GenPathError)
        } else {
            paths.final_path.to_path_buf()
        };
        if let Err(e) = fs::rename(paths.tmp, &dest).await {
            // Best-effort: drop the empty placeholder we just reserved so a failed
            // rename doesn't leave an orphan `xxx (1).mp4` behind.
            if paths.unique {
                let _ = fs::remove_file(&dest).await;
            }
            let _ = ctx.tx.send(Event::RenameFailed(e));
            return;
        }
        let _ = ctx.tx.send(Event::Renamed(dest));
    }

    // pub fn resume() {}

    async fn overwrite(
        file: File,
        url: Url,
        config: Config,
        info: UrlInfo,
        resp: Option<Arc<Mutex<Option<Response>>>>,
        tx: Tx,
        cancel_token: CancellationToken,
    ) {
        let url_ref = &url;
        let config_ref = &config;
        let info_ref = &info;
        let built = cancel_token
            .run_until_cancelled(async move {
                let puller = Self::build_puller(url_ref, config_ref, info_ref, resp)?;
                let pusher = Self::build_pusher(file, config_ref, info_ref).await?;
                Ok::<_, Box<Event>>((puller, pusher))
            })
            .await;
        let (puller, pusher) = match built {
            Some(Ok(built)) => built,
            Some(Err(e)) => {
                let _ = tx.send(*e);
                return;
            }
            None => return,
        };
        Self::run_download(puller, pusher, config, info, tx, cancel_token).await;
    }

    fn build_puller(
        url: &Url,
        config: &Config,
        info: &UrlInfo,
        resp: Option<Arc<Mutex<Option<Response>>>>,
    ) -> Result<FastDownPuller, Box<Event>> {
        FastDownPuller::new(FastDownPullerOptions {
            url: url.clone(),
            headers: build_header(&config.headers).into(),
            proxy: config.proxy.as_deref(),
            accept_invalid_certs: config.accept_invalid_certs,
            accept_invalid_hostnames: config.accept_invalid_hostnames,
            cookie_store: config.cookie_store,
            file_id: info.file_id.clone(),
            resp,
            available_ips: config.local_address.clone().into(),
            max_redirects: config.max_redirects,
        })
        .map_err(|e| Box::new(Event::BuildClientError(e)))
    }

    async fn build_pusher(
        file: File,
        config: &Config,
        info: &UrlInfo,
    ) -> Result<BoxPusher, Box<Event>> {
        if cfg!(target_pointer_width = "64")
            && info.fast_download
            && config.write_method == WriteMethod::Mmap
        {
            MmapFilePusher::new(&file, info.size, config.sync_all)
                .await
                .map(BoxPusher::new)
        } else {
            CacheFilePusher::new(
                file,
                info.size,
                config.sync_all,
                config.cache_high_watermark,
                config.cache_low_watermark,
                config.write_buffer_size,
            )
            .await
            .map(BoxPusher::new)
        }
        .map_err(|e| Box::new(Event::BuildPusherError(e)))
    }

    async fn run_download(
        puller: FastDownPuller,
        pusher: BoxPusher,
        config: Config,
        info: UrlInfo,
        tx: Tx,
        cancel_token: CancellationToken,
    ) {
        let res = if info.fast_download {
            download_multi(
                puller,
                pusher,
                fast_down::multi::DownloadOptions {
                    download_chunks: config.downloaded_chunk.into_iter(),
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

        loop {
            select! {
                e = res.event_chain.recv() => {
                    match e {
                        Ok(e) => {
                            let _ = match e {
                                fast_down::Event::Pulling(id) => tx.send(Event::Pulling(id)),
                                fast_down::Event::PullError(id, e) => tx.send(Event::PullError(id, anyhow::anyhow!(e))),
                                fast_down::Event::PullTimeout(id) => tx.send(Event::PullTimeout(id)),
                                fast_down::Event::PullProgress(id, range) => tx.send(Event::PullProgress(id, range)),
                                fast_down::Event::Pushing(id, range) => tx.send(Event::Pushing(id, range)),
                                fast_down::Event::PushError(id, range, e) => tx.send(Event::PushError(id, range, anyhow::anyhow!(e))),
                                fast_down::Event::PushProgress(id, range) => tx.send(Event::PushProgress(id, range)),
                                fast_down::Event::Flushing => tx.send(Event::Flushing),
                                fast_down::Event::FlushError(e) => tx.send(Event::FlushError(anyhow::anyhow!(e))),
                                fast_down::Event::Finished(id) => tx.send(Event::Finished(id)),
                            };
                        },
                        Err(_) => break,
                    }
                }
                () = cancel_token.cancelled() => break,
            }
        }

        if let Err(e) = res.join().await {
            let _ = tx.send(Event::JoinError(e));
        }
    }

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
}
