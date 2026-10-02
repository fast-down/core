//! Builds the pull/push pipeline for a `.part` file.

use super::open_existing;
use crate::{Config, Event, Tx, WriteMethod, utils::build_header};
use fast_down::{
    BoxPusher, CacheFilePusher, MmapFilePusher, UrlInfo,
    fast_puller::{FastDownPuller, FastDownPullerOptions},
};
use file_alloc::FileAlloc;
use parking_lot::Mutex;
use reqwest::Response;
use std::{path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

/// Construct the (puller, pusher) pipeline for `path`.
///
/// Returns `None` (after forwarding the failure as a public [`Event`]) if the
/// HTTP client or the output file cannot be created, or if `token` is cancelled
/// before construction finishes.
///
/// * `config` drives the puller (headers, proxy, cert handling, local bind
///   address, redirect limit) and the writer choice.
/// * `info` supplies the file identity used for range validation and selects
///   the writer: on 64-bit targets a resumable `info.fast_download` download
///   with [`WriteMethod::Mmap`] uses [`MmapFilePusher`]; otherwise
///   [`CacheFilePusher`] (buffered + out-of-order reordering).
/// * `resp` is the prefetch response, reused to seed the first range request.
/// * `path` is the `.part` file to write; `tx` receives error events.
///
/// With [`Config::pre_alloc`] enabled and a known size, the whole file is
/// reserved on disk right after it is opened ([`Event::Allocating`]); a failed
/// reservation is reported as [`Event::AllocError`] and does not abort.
pub async fn build_pipeline(
    config: &Config,
    info: &UrlInfo,
    resp: Response,
    path: &Path,
    tx: &Tx,
    token: &CancellationToken,
) -> Option<(FastDownPuller, BoxPusher)> {
    let resp = Some(Arc::new(Mutex::new(Some(resp))));
    let built = token
        .run_until_cancelled(async move {
            let puller = FastDownPuller::new(FastDownPullerOptions {
                url: info.final_url.clone(),
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
            .map_err(Event::BuildClientError)?;

            file_alloc::init_fast_alloc();
            let mut file = open_existing()
                .open(path)
                .await
                .map_err(Event::BuildPusherError)?;
            if info.size > 0 {
                let _ = tx.send(Event::Allocating(info.size));
                if config.pre_alloc {
                    if let Err(e) = file.allocate(info.size).await {
                        let _ = tx.send(Event::AllocError(e));
                    }
                } else if let Err(e) = file.try_allocate(info.size).await {
                    let _ = tx.send(Event::AllocError(e));
                }
            }
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
    match built {
        Some(Ok(built)) => Some(built),
        Some(Err(event)) => {
            let _ = tx.send(event);
            None
        }
        None => None,
    }
}
