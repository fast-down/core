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
