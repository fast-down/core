use crate::{Config, Event, Tx, tx_err, utils::build_header};
use fast_down::{UrlInfo, fast_puller::build_client, http::Prefetch};
use reqwest::Response;
use url::Url;

pub async fn prefetch(url: &Url, config: &Config, tx: &Tx) -> Option<(UrlInfo, Response)> {
    let client = build_client(
        build_header(&config.headers),
        config.proxy.as_deref(),
        config.accept_invalid_certs,
        config.accept_invalid_hostnames,
        config.cookie_store,
        config.local_address.first().copied(),
        config.max_redirects,
    );
    let client = tx_err!(client, tx, BuildClientError, None);
    let mut retry_count = 0;
    loop {
        match client.prefetch(url.clone()).await {
            Ok(t) => {
                let _ = tx.send(Event::Prefetch(t.0.clone()));
                break Some(t);
            }
            Err((e, t)) => {
                let _ = tx.send(Event::PrefetchError(e));
                retry_count += 1;
                if retry_count >= config.retry_times {
                    return None;
                }
                tokio::time::sleep(t.unwrap_or(config.retry_gap)).await;
            }
        }
    }
}
