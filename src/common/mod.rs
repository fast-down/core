#[cfg(feature = "common.url")]
mod url;

use crate::{FetchResult, Fetcher, ProgressEntry, Puller, RandomPusher};
use std::num::NonZeroUsize;
use std::time::Duration;
#[cfg(feature = "common.url")]
pub use url::UrlInfo;

#[derive(Debug, Eq, PartialEq)]
pub struct DownloadOptions {
    pub concurrent: Option<NonZeroUsize>,
    pub retry_gap: Duration,
    pub push_queue_cap: usize,
}

pub trait AutoDownload {
    fn download<F, P>(
        &self,
        fetcher: F,
        pusher: P,
        maybe_chunks: Option<Vec<ProgressEntry>>,
        options: DownloadOptions,
    ) -> impl std::future::Future<
        Output = FetchResult<F::Error, <F::Puller as Puller>::Error, P::Error>,
    > + Send
    where
        F: Fetcher + Send + 'static,
        P: RandomPusher + Send + 'static;
}
