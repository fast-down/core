//! Builds the pull/push pipeline shared by `download` and `resume`.
//!
//! [`build_pipeline`] constructs a [`FastDownPuller`] (network side) and a
//! [`BoxPusher`] (file side) for the `.part` file, choosing the memory-mapped
//! writer on 64-bit targets when the server supports fast (resumable) downloads
//! and `Mmap` writing is configured, and the buffered/cache writer otherwise.
use crate::{Config, Event, Tx, WriteMethod, core::download::open_existing, utils::build_header};
use fast_down::{
    BoxPusher, UrlInfo,
    fast_puller::{FastDownPuller, FastDownPullerOptions},
    file::{CacheFilePusher, MmapFilePusher},
};
use parking_lot::Mutex;
use reqwest::Response;
use std::{path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Construct the (puller, pusher) pipeline for a `.part` file.
///
/// Returns `None` (after forwarding the failure as a public
/// [`crate::Event`]) if the HTTP client or the output file cannot be created,
/// or if `token` is cancelled before construction finishes.
///
/// * `url` / `config` drive the puller (headers, proxy, cert handling, range
///   identity, local bind address, redirect limit).
/// * `info` supplies the file identity used for range validation and selects
///   the writer: on 64-bit targets a resumable `info.fast_download` download
///   with [`WriteMethod::Mmap`] uses [`MmapFilePusher`]; otherwise
///   [`CacheFilePusher`] (buffered + out-of-order reordering).
/// * `resp` is the prefetch response, reused to seed the first range request
///   without an extra round-trip.
/// * `path` is the `.part` file; `tx` receives error events; `token` makes
///   construction cancellable.
pub async fn build_pipeline(
    url: &Url,
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
            .map_err(Event::BuildClientError)?;

            let file = open_existing()
                .open(path)
                .await
                .map_err(Event::BuildPusherError)?;
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
        Some(Ok(b)) => Some(b),
        Some(Err(e)) => {
            let _ = tx.send(e);
            None
        }
        None => None,
    }
}
