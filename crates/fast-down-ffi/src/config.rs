use fast_down::{ProgressEntry, Proxy};
use parking_lot::Mutex;
use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};

/// File write method for downloaded data.
///
/// - `Mmap`: memory-mapped I/O (fastest, default)
/// - `Std`: buffered standard file I/O
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WriteMethod {
    #[default]
    Mmap,
    Std,
}

/// Configuration for a download task.
///
/// All fields have sensible defaults; see [`Config::default`] for values.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct Config {
    /// Number of threads. Recommended: `32` / `16` / `8`. More threads does not always mean faster.
    pub threads: usize,
    /// Proxy setting. Supports https, http, and socks5 proxies.
    pub proxy: Proxy<String>,
    /// Custom request headers.
    pub headers: HashMap<String, String>,
    /// Minimum chunk size in bytes. Recommended: `8 * 1024 * 1024`
    ///
    /// - Chunks that are too small may cause heavy contention.
    /// - When chunking is no longer possible, speculative mode is used.
    pub min_chunk_size: u64,
    /// Whether to ensure data is fully flushed to disk. Recommended: `false`
    ///
    /// Set to `true` only if you need to power off immediately after download.
    pub sync_all: bool,
    /// Write buffer size in bytes. Recommended: `16 * 1024 * 1024`
    ///
    /// - Only effective for [`WriteMethod::Std`]. Reduces the number of `write` syscalls
    ///   by batching small writes into larger ones via `BufWriter`.
    /// - Not used for [`WriteMethod::Mmap`], as the buffer is managed by the OS.
    pub write_buffer_size: usize,
    /// Cache high watermark in bytes. Recommended: `16 * 1024 * 1024`
    ///
    /// When the byte merge buffer reaches this size, a merge flush is triggered
    /// to reduce the buffer to `cache_low_watermark` or below.
    ///
    /// - Only effective for [`WriteMethod::Std`].
    /// - Not used for [`WriteMethod::Mmap`].
    pub cache_high_watermark: usize,
    /// Cache low watermark in bytes. Recommended: `8 * 1024 * 1024`
    ///
    /// After a merge flush, the byte merge buffer size is reduced to this level or below.
    ///
    /// - Only effective for [`WriteMethod::Std`].
    /// - Not used for [`WriteMethod::Mmap`].
    pub cache_low_watermark: usize,
    /// Write queue capacity. Recommended: `10240`
    ///
    /// If download threads fill the write queue, backpressure is applied to
    /// slow down downloads and prevent excessive memory usage.
    pub write_queue_cap: usize,
    /// Default retry interval after a request failure. Recommended: `500ms`
    ///
    /// If the server returns a `Retry-After` header, that value takes precedence.
    pub retry_gap: Duration,
    /// Pull timeout. Recommended: `5000ms`
    ///
    /// If no bytes are received within `pull_timeout` after sending the request,
    /// the connection is dropped and re-established. This helps TCP detect
    /// congestion and can improve download speed.
    pub pull_timeout: Duration,
    /// Whether to accept invalid certificates (dangerous). Recommended: `false`
    pub accept_invalid_certs: bool,
    /// Whether to accept invalid hostnames (dangerous). Recommended: `false`
    pub accept_invalid_hostnames: bool,
    /// Write method. Recommended: [`WriteMethod::Mmap`]
    ///
    /// - [`WriteMethod::Mmap`] is fastest — it delegates writes to the OS, but:
    ///     1. On 32-bit systems, the maximum file size is 4 GB, so it automatically
    ///        falls back to [`WriteMethod::Std`].
    ///     2. The file size must be known, otherwise it falls back to [`WriteMethod::Std`].
    ///     3. In rare cases, the OS may cache all data in memory and flush it all
    ///        at once after the download completes, causing a long post-download delay.
    /// - [`WriteMethod::Std`] has the best compatibility. Out-of-order chunks are
    ///   re-ordered into sequential order by the cache layer before being written.
    #[cfg(feature = "file")]
    pub write_method: WriteMethod,
    /// Number of retries for fetching metadata. Recommended: `10`. Note: this is not
    /// the retry count during download.
    pub retry_times: usize,
    /// Local IP addresses to bind for outgoing requests. Recommended: `Vec::new()`
    ///
    /// If you have multiple network interfaces, you can provide their IP addresses;
    /// each time the puller is cloned (e.g. on retry or work-stealing) the next
    /// address in the list is used. This may not always improve speed.
    pub local_address: Vec<IpAddr>,
    /// Maximum number of speculative workers. Recommended: `3`
    ///
    /// When the remaining chunk is smaller than `min_chunk_size` and cannot be split,
    /// speculative mode is used. Up to `max_speculative` workers compete on the same
    /// chunk to prevent the download from stalling near 99%.
    pub max_speculative: usize,
    /// Already downloaded chunks. Pass `Vec::new()` to download the entire file.
    pub downloaded_chunk: Arc<Mutex<Vec<ProgressEntry>>>,
    /// Smoothing window for downloaded chunks in bytes. Recommended: `8 * 1024`
    ///
    /// Filters out small gaps in `downloaded_chunk` that are smaller than
    /// `chunk_window` to reduce the number of HTTP requests.
    pub chunk_window: u64,
    /// Maximum number of redirects. Recommended value: `20`
    pub max_redirects: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            retry_times: 10,
            threads: 32,
            proxy: Proxy::System,
            headers: HashMap::new(),
            sync_all: false,
            min_chunk_size: 8 * 1024 * 1024,
            write_buffer_size: 16 * 1024 * 1024,
            cache_high_watermark: 16 * 1024 * 1024,
            cache_low_watermark: 8 * 1024 * 1024,
            write_queue_cap: 10240,
            retry_gap: Duration::from_millis(500),
            pull_timeout: Duration::from_secs(5),
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            local_address: Vec::new(),
            max_speculative: 3,
            #[cfg(feature = "file")]
            write_method: WriteMethod::Mmap,
            downloaded_chunk: Arc::default(),
            chunk_window: 8 * 1024,
            max_redirects: 20,
        }
    }
}
