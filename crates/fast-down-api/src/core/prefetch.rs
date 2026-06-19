use crate::{Config, Event, Tx};
use fast_down::{UrlInfo, http::Prefetch, reqwest::SmartRedirectClient};
use reqwest::Response;
use url::Url;

pub async fn prefetch(
    url: &Url,
    config: &Config,
    client: &SmartRedirectClient,
    tx: &Tx,
) -> Option<(UrlInfo, Response)> {
    let mut retry_count = 0;
    loop {
        match client.prefetch(url.clone()).await {
            Ok(t) => break Some(t),
            Err((e, t)) => {
                let _ = tx.send(Event::PrefetchError(e.into()));
                retry_count += 1;
                if retry_count >= config.retry_times {
                    return None;
                }
                tokio::time::sleep(t.unwrap_or(config.retry_gap)).await;
            }
        }
    }
}
