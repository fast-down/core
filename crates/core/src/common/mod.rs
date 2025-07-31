#[cfg(feature = "common.url")]
mod url;

use std::num::NonZeroUsize;
use std::time::Duration;
#[cfg(feature = "common.url")]
pub use url::UrlInfo;
use crate::{FetchResult, Fetcher, ProgressEntry, Puller, RandomPusher};

#[derive(Debug, Eq, PartialEq)]
pub struct DownloadOptions {
    pub concurrent: Option<NonZeroUsize>,
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
}

pub trait AutoDownload {
    async fn download<F, P>(
        &self,
        fetcher: F,
        pusher: P,
        maybe_chunks: Option<Vec<ProgressEntry>>,
        options: DownloadOptions,
    ) -> FetchResult<F::Error, <F::Puller as Puller>::Error, P::Error>
    where
        F: Fetcher + Send + 'static,
        P: RandomPusher + Send + 'static;
}
