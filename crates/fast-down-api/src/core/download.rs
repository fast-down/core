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
use path_helper::FileStemExt;
use reqwest::Response;
use std::sync::Arc;
use tokio::{
    fs::{self, File, OpenOptions},
    select,
};
use tokio_util::sync::CancellationToken;
use url::Url;

pub struct DownloadHandle {
    handle: SharedHandle<()>,
}

impl DownloadHandle {
    /// # Errors
    pub fn download(
        url: Url,
        mut partial_config: PartialConfig,
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
            let origin_final_path = tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError);
            let mut tmp_path = origin_final_path.with_added_extension("part");
            let mut config_path = origin_final_path.with_added_extension("fd");
            let can_resume = config.resume && info.fast_download;
            let mut no_create_option = OpenOptions::new();
            no_create_option
                .read(true)
                .write(true)
                .truncate(false)
                .create(false);
            let mut create_option = OpenOptions::new();
            create_option
                .read(true)
                .write(true)
                .truncate(false)
                .create(true);
            let mut only_create_option = OpenOptions::new();
            only_create_option
                .read(true)
                .write(true)
                .truncate(false)
                .create_new(true);
            if config.overwrite {
                if can_resume
                    && let (Ok(file), Ok(state)) = tokio::join!(
                        no_create_option.open(&tmp_path),
                        DownloadState::load(&config_path)
                    )
                {
                    if let Some(config) = &state.config {
                        partial_config.inherit_from(config);
                    }
                    let _ = tx.send(Event::Start {
                        tmp_path: tmp_path.clone(),
                        config_path,
                        url_info: info.clone(),
                        parsed_config: partial_config.clone(),
                    });
                    Self::overwrite(
                        file,
                        url,
                        partial_config.build(),
                        info,
                        Some(Arc::new(Mutex::new(Some(resp)))),
                        tx.clone(),
                        cancel_token,
                    )
                    .force_send()
                    .await;
                } else {
                    let file = tx_err!(create_option.open(&tmp_path).await, tx, BuildPusherError);
                    let _ = tx.send(Event::Start {
                        tmp_path: tmp_path.clone(),
                        config_path,
                        url_info: info.clone(),
                        parsed_config: partial_config,
                    });
                    Self::overwrite(
                        file,
                        url,
                        config,
                        info,
                        Some(Arc::new(Mutex::new(Some(resp)))),
                        tx.clone(),
                        cancel_token,
                    )
                    .force_send()
                    .await;
                }
                tx_err!(
                    fs::rename(tmp_path, origin_final_path).await,
                    tx,
                    RenameFailed
                );
                return;
            }
            let mut i = 0;
            let mut final_path = origin_final_path.clone();
            loop {
                if can_resume
                    && let (Ok(file), Ok(state)) = tokio::join!(
                        no_create_option.open(&tmp_path),
                        DownloadState::load(&config_path)
                    )
                {
                    if let Some(config) = &state.config {
                        partial_config.inherit_from(config);
                    }
                    let _ = tx.send(Event::Start {
                        tmp_path: tmp_path.clone(),
                        config_path,
                        url_info: info.clone(),
                        parsed_config: partial_config.clone(),
                    });
                    Self::overwrite(
                        file,
                        url,
                        partial_config.build(),
                        info,
                        Some(Arc::new(Mutex::new(Some(resp)))),
                        tx.clone(),
                        cancel_token,
                    )
                    .force_send()
                    .await;
                    tx_err!(fs::rename(tmp_path, final_path).await, tx, RenameFailed);
                    return;
                } else if let Ok(file) = only_create_option.open(&tmp_path).await {
                    let _ = tx.send(Event::Start {
                        tmp_path: tmp_path.clone(),
                        config_path,
                        url_info: info.clone(),
                        parsed_config: partial_config,
                    });
                    Self::overwrite(
                        file,
                        url,
                        config,
                        info,
                        Some(Arc::new(Mutex::new(Some(resp)))),
                        tx.clone(),
                        cancel_token,
                    )
                    .force_send()
                    .await;
                    tx_err!(fs::rename(tmp_path, final_path).await, tx, RenameFailed);
                    return;
                }
                i += 1;
                final_path = origin_final_path.with_added_file_stem_prefix(format!(" {i}"));
                tmp_path = final_path.with_added_extension("part");
                config_path = final_path.with_added_extension("fd");
            }
        });
        let handle = SharedHandle::new(handle);
        Ok(Self { handle })
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
        let res = cancel_token
            .run_until_cancelled(async move {
                let puller = FastDownPuller::new(FastDownPullerOptions {
                    url,
                    headers: build_header(&config.headers).into(),
                    proxy: config.proxy.as_deref(),
                    accept_invalid_certs: config.accept_invalid_certs,
                    accept_invalid_hostnames: config.accept_invalid_hostnames,
                    cookie_store: config.cookie_store,
                    file_id: info.file_id.clone(),
                    resp,
                    available_ips: config.local_address.into(),
                    max_redirects: config.max_redirects,
                })
                .map_err(Event::BuildClientError)?;

                let pusher = if cfg!(target_pointer_width = "64")
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
                .map_err(Event::BuildPusherError)?;

                Ok::<_, Event>((puller, pusher))
            })
            .await;
        let (puller, pusher) = match res {
            Some(Ok(res)) => res,
            Some(Err(e)) => {
                let _ = tx.send(e);
                return;
            }
            None => return,
        };

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
}
