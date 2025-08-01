use super::{AutoDownload, DownloadOptions};
use crate::{FetchResult, Fetcher, ProgressEntry, Puller, RandomPusher, multi, single};
use std::num::NonZeroUsize;
use url::Url;

#[derive(Debug, Clone)]
pub struct UrlInfo {
    pub size: u64,
    pub name: Option<String>,
    pub supports_range: bool,
    pub fast_download: bool,
    pub final_url: Url,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl AutoDownload for UrlInfo {
    async fn download<F, P>(
        &self,
        fetcher: F,
        pusher: P,
        maybe_chunks: Option<Vec<ProgressEntry>>,
        options: DownloadOptions,
    ) -> FetchResult<F::Error, <F::Puller as Puller>::Error, P::Error>
    where
        F: Fetcher + Send + 'static,
        P: RandomPusher + Send + 'static,
    {
        if self.fast_download {
            multi::download_multi(
                fetcher,
                pusher,
                multi::DownloadOptions {
                    #[allow(clippy::single_range_in_vec_init)]
                    pull_chunks: maybe_chunks.unwrap_or_else(|| vec![0..self.size]),
                    concurrent: options
                        .concurrent
                        .unwrap_or(const { NonZeroUsize::new(1).unwrap() }),
                    retry_gap: options.retry_gap,
                    push_queue_cap: options.push_queue_cap,
                },
            )
            .await
        } else {
            single::download_single(
                fetcher,
                pusher,
                single::DownloadOptions {
                    retry_gap: options.retry_gap,
                    push_queue_cap: options.push_queue_cap,
                },
            )
            .await
        }
    }
}
