use crate::{Config, Event, Tx, WriteMethod, utils::build_header};
use fast_down::{
    BoxPusher, UrlInfo,
    fast_puller::{FastDownPuller, FastDownPullerOptions},
    file::{CacheFilePusher, MmapFilePusher},
};
use parking_lot::Mutex;
use reqwest::Response;
use std::sync::Arc;
use tokio::fs::File;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Build the puller + pusher inside a `force_send` + `run_until_cancelled`
/// future (not provably `Send`; see `run` doc). Yields `None` on a build error
/// (the error event is already sent through `tx`) or on cancel-before-transfer;
/// the caller is responsible for persisting state and stopping.
pub(super) async fn build_pipeline(
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
