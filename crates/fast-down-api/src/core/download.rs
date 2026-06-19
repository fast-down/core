use crate::{Config, Tx, prefetch::prefetch, utils::build_header};
use fast_down::{fast_puller::build_client, handle::SharedHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

pub struct DownloadHandle {
    handle: SharedHandle<()>,
}

impl DownloadHandle {
    /// # Errors
    pub fn new(
        url: Url,
        config: Config,
        tx: Tx,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<Self> {
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
            let Some((url_info, resp)) = prefetch(&url, &config, &client, &tx).await else {
                return;
            };
        });
        let handle = SharedHandle::new(handle);
        Ok(Self { handle })
    }
}
