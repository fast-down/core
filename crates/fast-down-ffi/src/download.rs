use crate::{Config, Error, Event, Tx};
use fast_down::{
    BoxPusher, Merge, UrlInfo,
    fast_puller::{FastDownPuller, FastDownPullerOptions, build_client},
    http::Prefetch,
    invert,
    multi::{self, download_multi},
    single::{self, download_single},
};
use parking_lot::Mutex;
use reqwest::{Response, header::HeaderMap};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio_util::sync::CancellationToken;
use url::Url;

/// A download task created by [`prefetch`], ready to be started.
///
/// Contains the resource metadata ([`UrlInfo`]), user configuration, and the
/// initial HTTP response for resumption. Call `start` (requires `file` feature),
/// `start_with_pusher`, or `start_in_memory` (requires `mem` feature) to begin
/// the download.
#[derive(Debug)]
pub struct DownloadTask {
    pub info: UrlInfo,
    pub config: Config,
    pub resp: Option<Arc<Mutex<Option<Response>>>>,
    pub tx: Tx,
    pub is_running: AtomicBool,
}

#[must_use]
fn parse_headers(headers: &HashMap<String, String>) -> Arc<HeaderMap> {
    headers
        .iter()
        .map(|(k, v)| (k.parse(), v.parse()))
        .filter_map(|(k, v)| k.ok().zip(v.ok()))
        .collect::<HeaderMap>()
        .into()
}

/// This function can be cancelled by dropping the returned Future.
///
/// # Errors
///
/// - Returns an error if proxy configuration fails.
/// - Returns an error if the maximum number of retries is reached.
pub async fn prefetch(url: Url, config: Config, tx: Tx) -> Result<DownloadTask, Error> {
    let headers = parse_headers(&config.headers);
    let local_addr: Arc<[_]> = config.local_address.clone().into();
    let client = build_client(
        headers.as_ref().clone(),
        config.proxy.as_deref(),
        config.accept_invalid_certs,
        config.accept_invalid_hostnames,
        local_addr.first().copied(),
        config.max_redirects,
    )?;
    let mut retry_count = 0;
    let (info, resp) = loop {
        match client.prefetch(url.clone()).await {
            Ok(t) => break t,
            Err((e, t)) => {
                let _ = tx.send(Event::PrefetchError(format!("{e:?}")));
                retry_count += 1;
                if retry_count >= config.retry_times {
                    return Err(Error::PrefetchTimeout(e));
                }
                tokio::time::sleep(t.unwrap_or(config.retry_gap)).await;
            }
        }
    };
    Ok(DownloadTask {
        config,
        resp: Some(Arc::new(Mutex::new(Some(resp)))),
        tx,
        info,
        is_running: AtomicBool::new(false),
    })
}

struct RunGuard<'a>(&'a AtomicBool);
impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl DownloadTask {
    /// Cannot be cancelled by dropping the Future — doing so would leave the
    /// written content incomplete.
    /// Multiple calls will resume from where the last call left off.
    ///
    /// # Errors
    ///
    /// - Returns `Err` if the task is already running.
    /// - Returns `Err` if proxy configuration fails.
    /// - Returns `Err` if the download fails unexpectedly.
    pub async fn start_with_pusher(
        &self,
        pusher: BoxPusher,
        cancel_token: CancellationToken,
    ) -> Result<(), Error> {
        if self
            .is_running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(Error::AlreadyRunning);
        }
        let _guard = RunGuard(&self.is_running);
        let progress = self.config.downloaded_chunk.clone();
        let puller = FastDownPuller::new(FastDownPullerOptions {
            url: self.info.final_url.clone(),
            headers: parse_headers(&self.config.headers),
            proxy: self.config.proxy.as_deref(),
            available_ips: self.config.local_address.clone().into(),
            accept_invalid_certs: self.config.accept_invalid_certs,
            accept_invalid_hostnames: self.config.accept_invalid_hostnames,
            file_id: self.info.file_id.clone(),
            resp: self.resp.clone(),
            max_redirects: self.config.max_redirects,
        })?;
        let threads = if self.info.fast_download {
            self.config.threads.max(1)
        } else {
            1
        };
        let result = if self.info.fast_download {
            download_multi(
                puller,
                pusher,
                multi::DownloadOptions {
                    download_chunks: invert(
                        progress.lock().iter().cloned(),
                        self.info.size,
                        self.config.chunk_window,
                    ),
                    retry_gap: self.config.retry_gap,
                    concurrent: threads,
                    pull_timeout: self.config.pull_timeout,
                    push_queue_cap: self.config.write_queue_cap,
                    min_chunk_size: self.config.min_chunk_size,
                    max_speculative: self.config.max_speculative,
                },
            )
        } else {
            download_single(
                puller,
                pusher,
                single::DownloadOptions {
                    retry_gap: self.config.retry_gap,
                    push_queue_cap: self.config.write_queue_cap,
                },
            )
        };
        let mut cancelled = false;
        loop {
            tokio::select! {
                () = cancel_token.cancelled(), if !cancelled => {
                    result.abort();
                    cancelled = true;
                },
                e = result.event_chain.recv() => match e {
                    Ok(e) => {
                        let _ = self.tx.send((&e).into());
                        if let fast_down::Event::PushProgress(_, range) = e {
                            let mut p = progress.lock();
                            if range.start == 0 && !self.info.fast_download {
                                p.clear();
                            }
                            p.merge_progress(range);
                        }
                    },
                    Err(_) => break,
                }
            }
        }
        result.join().await?;
        Ok(())
    }

    /// Cannot be cancelled by dropping the Future — doing so would leave the
    /// written content incomplete.
    /// The pusher is determined by [`crate::WriteMethod`].
    ///
    /// # Errors
    ///
    /// Returns `Err` if opening or creating the file fails. Otherwise see
    /// [`crate::DownloadTask::start_with_pusher`].
    #[cfg(feature = "file")]
    pub async fn start(
        &self,
        save_path: std::path::PathBuf,
        cancel_token: CancellationToken,
    ) -> Result<(), Error> {
        let pusher = get_pusher(
            &self.info,
            self.config.write_method.clone(),
            self.config.sync_all,
            self.config.cache_high_watermark,
            self.config.cache_low_watermark,
            self.config.write_buffer_size,
            &save_path,
        );
        let pusher = tokio::select! {
              () = cancel_token.cancelled() => return Ok(()),
              pusher = pusher => pusher.map_err(Error::Io)?,
        };
        self.start_with_pusher(pusher, cancel_token).await
    }

    #[allow(clippy::missing_panics_doc)]
    /// Cannot be cancelled by dropping the Future — doing so would leave the
    /// written content incomplete.
    ///
    /// # Errors
    ///
    /// Same as [`crate::DownloadTask::start_with_pusher`].
    #[cfg(feature = "mem")]
    pub async fn start_in_memory(&self, cancel_token: CancellationToken) -> Result<Vec<u8>, Error> {
        #[allow(clippy::cast_possible_truncation)]
        let pusher = fast_down::mem::MemPusher::with_capacity(self.info.size as usize);
        self.start_with_pusher(BoxPusher::new(pusher.clone()), cancel_token)
            .await?;
        Ok(Arc::try_unwrap(pusher.receive).unwrap().into_inner())
    }
}

#[cfg(feature = "file")]
/// # Errors
///
/// Returns an error if opening or creating the file fails.
pub async fn get_pusher(
    info: &UrlInfo,
    write_method: crate::WriteMethod,
    sync_all: bool,
    high_watermark: usize,
    low_watermark: usize,
    buffer_size: usize,
    save_path: &std::path::Path,
) -> Result<BoxPusher, String> {
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(false)
        .open(&save_path)
        .await
        .map_err(|e| format!("{e:?}"))?;
    #[cfg(target_pointer_width = "64")]
    if info.fast_download && write_method == crate::WriteMethod::Mmap {
        use fast_down::file::MmapFilePusher;
        let pusher = BoxPusher::new(
            MmapFilePusher::new(file, info.size, sync_all)
                .await
                .map_err(|e| format!("{e:?}"))?,
        );
        return Ok(pusher);
    }
    let pusher = fast_down::file::CacheFilePusher::new(
        file,
        info.size,
        sync_all,
        high_watermark,
        low_watermark,
        buffer_size,
    )
    .await
    .map_err(|e| format!("{e:?}"))?;
    Ok(BoxPusher::new(pusher))
}
