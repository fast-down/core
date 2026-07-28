use fast_down::{Merge, ProgressEntry, Proxy};
use inherit_config::InheritConfig;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::IpAddr, path::PathBuf, time::Duration};

/// File write method for downloaded data.
///
/// - `Mmap`: memory-mapped I/O (fastest, default)
/// - `Std`: buffered standard file I/O
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WriteMethod {
    #[default]
    Mmap,
    Std,
}

/// Configuration for a download task.
///
/// All fields have sensible defaults; see [`Config::default`] for values.
#[derive(Debug, Clone, Serialize, Deserialize, InheritConfig)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    /// 保存的文件夹
    pub save_dir: PathBuf,

    /// 文件名解析
    pub parse_filename: bool,

    /// 文件名
    pub filename: String,

    /// 用于在 prefetch 阶段生成占位文件名
    pub gid: String,

    /// Number of threads. Recommended: `32` / `16` / `8`. More threads does not always mean faster.
    #[config(default = 32)]
    pub threads: usize,

    /// Proxy setting. Supports https, http, and socks5 proxies.
    pub proxy: Proxy<String>,

    /// Custom request headers.
    pub headers: HashMap<String, String>,

    /// Minimum chunk size in bytes. Recommended: `8 * 1024 * 1024`
    ///
    /// - Chunks that are too small may cause heavy contention.
    /// - When chunking is no longer possible, speculative mode is used.
    #[config(default = 8 * 1024 * 1024)]
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
    #[config(default = 16 * 1024 * 1024)]
    pub write_buffer_size: usize,

    /// Cache high watermark in bytes. Recommended: `16 * 1024 * 1024`
    ///
    /// When the byte merge buffer reaches this size, a merge flush is triggered
    /// to reduce the buffer to `cache_low_watermark` or below.
    ///
    /// - Only effective for [`WriteMethod::Std`].
    /// - Not used for [`WriteMethod::Mmap`].
    #[config(default = 16 * 1024 * 1024)]
    pub cache_high_watermark: usize,

    /// Cache low watermark in bytes. Recommended: `8 * 1024 * 1024`
    ///
    /// After a merge flush, the byte merge buffer size is reduced to this level or below.
    ///
    /// - Only effective for [`WriteMethod::Std`].
    /// - Not used for [`WriteMethod::Mmap`].
    #[config(default = 8 * 1024 * 1024)]
    pub cache_low_watermark: usize,

    /// Write queue capacity. Recommended: `10240`
    ///
    /// If download threads fill the write queue, backpressure is applied to
    /// slow down downloads and prevent excessive memory usage.
    #[config(default = 10240)]
    pub write_queue_cap: usize,

    /// Default retry interval after a request failure. Recommended: `500ms`
    ///
    /// If the server returns a `Retry-After` header, that value takes precedence.
    #[config(default = Duration::from_millis(500))]
    pub retry_gap: Duration,

    /// Pull timeout. Recommended: `5s`
    ///
    /// If no bytes are received within `pull_timeout` after sending the request,
    /// the connection is dropped and re-established. This helps TCP detect
    /// congestion and can improve download speed.
    #[config(default = Duration::from_secs(5))]
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
    ///     2. Mmap requires the file size to be known and byte-range support from
    ///        the server; when the `fast_download` flag (set during prefetch) is false,
    ///        it falls back to [`WriteMethod::Std`].
    ///     3. In rare cases, the OS may cache all data in memory and flush it all
    ///        at once after the download completes, causing a long post-download delay.
    /// - [`WriteMethod::Std`] has the best compatibility. Out-of-order chunks are
    ///   re-ordered into sequential order by the cache layer before being written.
    pub write_method: WriteMethod,

    /// Number of retries for fetching metadata. Recommended: `10`. Note: this is not
    /// the retry count during download.
    #[config(default = 10)]
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
    #[config(default = 3)]
    pub max_speculative: usize,

    /// Already downloaded chunks. Pass `Vec::new()` to download the entire file.
    pub downloaded_chunk: Vec<ProgressEntry>,

    /// Smoothing window for downloaded chunks in bytes. Recommended: `8 * 1024`
    ///
    /// Filters out small gaps in `downloaded_chunk` that are smaller than
    /// `chunk_window` to reduce the number of HTTP requests.
    #[config(default = 8 * 1024)]
    pub chunk_window: u64,

    /// Maximum number of redirects. Recommended value: `20`
    #[config(default = 20)]
    pub max_redirects: usize,

    /// Enable cookie store. When `true`, the client will automatically save
    /// `Set-Cookie` headers from responses and send matching cookies in
    /// subsequent requests (including across redirects).
    pub cookie_store: bool,

    /// 是否尝试断点续传，推荐值: `true`
    #[config(default = true)]
    pub resume: bool,

    /// 是否覆盖已存在的文件，推荐值: `false`
    pub overwrite: bool,
}

impl PartialConfig {
    /// Merge a freshly-written byte range into this partial config's progress.
    ///
    /// The range is folded into `downloaded_chunk` (created if absent), keeping
    /// it normalized and de-duplicated. This is the in-memory counterpart of
    /// [`DownloadState::merge_progress`](crate::DownloadState::merge_progress):
    /// callers use it to record progress before handing the config to
    /// [`DownloadHandle::resume`](crate::DownloadHandle::resume).
    pub fn merge_progress(&mut self, progress: ProgressEntry) {
        self.downloaded_chunk
            .get_or_insert_default()
            .merge_progress(progress);
    }
}
