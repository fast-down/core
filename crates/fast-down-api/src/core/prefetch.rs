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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::create_channel;
    use std::time::Duration;

    #[tokio::test]
    async fn prefetch_gives_up_after_retries_on_unreachable() {
        // Exercises the Err branch of prefetch (prefetch.rs lines 24-30): a
        // connection that is refused must emit `Event::PrefetchError`, retry up
        // to `retry_times`, then return `None`.
        let url = Url::parse("http://127.0.0.1:1/never").unwrap();
        let config = Config {
            retry_times: 2,
            retry_gap: Duration::ZERO,
            ..Default::default()
        };
        let (tx, rx) = create_channel();
        let result = prefetch(&url, &config, &tx).await;
        assert!(
            result.is_none(),
            "prefetch must give up after exhausting retries"
        );
        drop(tx);
        let mut errors = 0;
        while let Ok(e) = rx.recv().await {
            if matches!(e, Event::PrefetchError(_)) {
                errors += 1;
            }
        }
        assert!(errors >= 1, "expected at least one Event::PrefetchError");
    }
}
