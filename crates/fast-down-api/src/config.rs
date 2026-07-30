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
    #[config(partial_attr(serde(with = "humantime_serde::option")))]
    #[config(partial_attr(serde(default)))]
    pub retry_gap: Duration,

    /// Pull timeout. Recommended: `5s`
    ///
    /// If no bytes are received within `pull_timeout` after sending the request,
    /// the connection is dropped and re-established. This helps TCP detect
    /// congestion and can improve download speed.
    #[config(default = Duration::from_secs(5))]
    #[config(partial_attr(serde(with = "humantime_serde::option")))]
    #[config(partial_attr(serde(default)))]
    pub pull_timeout: Duration,

    /// Minimum interval between [`crate::Event::Progress`] emissions. Recommended: `500ms`
    ///
    /// The progress reporter runs on its own timer, so this cadence is driven
    /// purely by `progress_emit_gap` and is never delayed by other download work
    /// (flushing, state saving, event forwarding) or by a slow consumer — the
    /// event channel is unbounded, so [`crate::Event::Progress`] is sent without
    /// blocking. Set it smaller for a smoother progress bar, larger to cut channel
    /// traffic. `Duration::ZERO` emits at the maximum rate (busy-loop, not recommended).
    #[config(default = Duration::from_millis(500))]
    #[config(partial_attr(serde(with = "humantime_serde::option")))]
    #[config(partial_attr(serde(default)))]
    pub progress_emit_gap: Duration,

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

    /// Already downloaded chunks (resume progress), stored as an HTTP `Range`-style
    /// list, e.g. `downloaded_chunk = "1-3,4-9"`. Ends are *inclusive*, matching the
    /// HTTP `Range` header (`bytes=1-3` covers bytes 1,2,3); the internal half-open
    /// `start..end` is mapped to `start-(end-1)` on disk. Absent when nothing has
    /// been downloaded yet.
    #[config(partial_attr(serde(with = "range_list")))]
    #[config(partial_attr(serde(default)))]
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

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn merge_progress_none_to_some() {
        let mut c = PartialConfig::default();
        assert_eq!(c.downloaded_chunk, None);
        c.merge_progress(1u64..5);
        assert_eq!(c.downloaded_chunk, Some(vec![1u64..5]));
    }

    #[test]
    fn merge_progress_coalesces() {
        let mut c = PartialConfig::default();
        c.merge_progress(1u64..5);
        c.merge_progress(5u64..10);
        c.merge_progress(10u64..20);
        assert_eq!(c.downloaded_chunk, Some(vec![1u64..20]));
    }

    #[test]
    fn merge_progress_empty_is_noop() {
        let mut c = PartialConfig::default();
        c.merge_progress(1u64..5);
        c.merge_progress(3u64..3);
        assert_eq!(c.downloaded_chunk, Some(vec![1u64..5]));
    }

    #[test]
    fn merge_progress_disjoint() {
        let mut c = PartialConfig::default();
        c.merge_progress(1u64..5);
        c.merge_progress(10u64..20);
        assert_eq!(c.downloaded_chunk, Some(vec![1u64..5, 10u64..20]));
    }
}

/// HTTP `Range`-style (de)serialization for [`Config::downloaded_chunk`].
///
/// The internal [`ProgressEntry`] is a half-open `start..end` range, while the
/// HTTP `Range` header uses an *inclusive* end (`bytes=1-3` covers bytes 1,2,3).
/// We follow that on-disk convention, so `start..end` is written as
/// `"{start}-{end-1}"` and parsed back as `{start}..{end+1}`. The round-trip is
/// lossless for any `u64` range and stays a single TOML string, e.g.
/// `downloaded_chunk = "1-3,4-9"`.
mod range_list {
    use fast_down::ProgressEntry;
    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(
        value: &Option<Vec<ProgressEntry>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            None => serializer.serialize_none(),
            Some(ranges) => {
                let text = ranges
                    .iter()
                    .map(|r| {
                        debug_assert!(
                            r.start < r.end,
                            "downloaded_chunk range must be non-empty (start < end), got {r:?}"
                        );
                        format!("{}-{}", r.start, r.end - 1)
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                serializer.serialize_str(&text)
            }
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<ProgressEntry>>, D::Error> {
        let raw = Option::<String>::deserialize(deserializer)?;
        match raw {
            None => Ok(None),
            Some(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Ok(Some(Vec::new()));
                }
                let mut ranges = Vec::new();
                for part in trimmed.split(',') {
                    let part = part.trim();
                    let (start_repr, end_repr) = part.split_once('-').ok_or_else(|| {
                        serde::de::Error::custom(format!(
                            "invalid range `{part}` in downloaded_chunk"
                        ))
                    })?;
                    let start: u64 = start_repr.trim().parse().map_err(|e| {
                        serde::de::Error::custom(format!("invalid range start `{start_repr}`: {e}"))
                    })?;
                    let end_inclusive: u64 = end_repr.trim().parse().map_err(|e| {
                        serde::de::Error::custom(format!("invalid range end `{end_repr}`: {e}"))
                    })?;
                    if end_inclusive < start {
                        return Err(serde::de::Error::custom(format!(
                            "invalid range `{part}` in downloaded_chunk: end {end_inclusive} < start {start}"
                        )));
                    }
                    ranges.push(start..end_inclusive.saturating_add(1));
                }
                Ok(Some(ranges))
            }
        }
    }
}

#[cfg(test)]
mod range_list_tests {
    use super::*;

    #[test]
    fn downloaded_chunk_round_trips_as_http_range_string() {
        let pc = PartialConfig {
            downloaded_chunk: Some(vec![1..4, 5..10, 100..101]),
            ..Default::default()
        };
        let toml = toml::to_string(&pc).unwrap();
        assert!(
            toml.contains("downloaded_chunk = \"1-3,5-9,100-100\""),
            "downloaded_chunk should be stored as an HTTP Range string, got:\n{toml}"
        );
        let back: PartialConfig = toml::from_str(&toml).unwrap();
        assert_eq!(back.downloaded_chunk, Some(vec![1..4, 5..10, 100..101]));
    }

    #[test]
    fn downloaded_chunk_absent_when_none() {
        let pc = PartialConfig::default();
        let toml = toml::to_string(&pc).unwrap();
        assert!(
            !toml.contains("downloaded_chunk"),
            "absent downloaded_chunk must not be serialized, got:\n{toml}"
        );
        let back: PartialConfig = toml::from_str(&toml).unwrap();
        assert_eq!(back.downloaded_chunk, None);
    }

    #[test]
    fn downloaded_chunk_empty_string_round_trips_to_empty_vec() {
        let pc = PartialConfig {
            downloaded_chunk: Some(Vec::new()),
            ..Default::default()
        };
        let toml = toml::to_string(&pc).unwrap();
        assert!(
            toml.contains("downloaded_chunk = \"\""),
            "an empty downloaded_chunk should serialize as an empty string, got:\n{toml}"
        );
        let back: PartialConfig = toml::from_str(&toml).unwrap();
        assert_eq!(back.downloaded_chunk, Some(Vec::new()));
    }

    #[test]
    fn downloaded_chunk_rejects_reversed_range() {
        let toml = "downloaded_chunk = \"3-1\"\n";
        let err = toml::from_str::<PartialConfig>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("3-1") && msg.contains("downloaded_chunk"),
            "reversed range must be rejected with a clear error, got: {msg}"
        );
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn downloaded_chunk_single_byte_range_round_trips() {
        // HTTP Range `5-5` is a single byte (offset 5), i.e. the half-open `5..6`.
        let pc = PartialConfig {
            downloaded_chunk: Some(vec![5..6]),
            ..Default::default()
        };
        let toml = toml::to_string(&pc).unwrap();
        assert!(
            toml.contains("downloaded_chunk = \"5-5\""),
            "single-byte range should keep its inclusive end, got:\n{toml}"
        );
        let back: PartialConfig = toml::from_str(&toml).unwrap();
        assert_eq!(back.downloaded_chunk, Some(vec![5..6]));
    }

    #[test]
    fn downloaded_chunk_rejects_range_without_dash() {
        // No '-' separator: `split_once('-')` fails (deserialize lines 299-303).
        let toml = "downloaded_chunk = \"5\"\n";
        let err = toml::from_str::<PartialConfig>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("downloaded_chunk"),
            "missing-dash range must be rejected with a clear error, got: {msg}"
        );
    }

    #[test]
    fn downloaded_chunk_rejects_bad_start() {
        // Non-numeric start: `start_repr.parse()` fails (lines 304-306).
        let toml = "downloaded_chunk = \"x-10\"\n";
        let err = toml::from_str::<PartialConfig>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("downloaded_chunk") && msg.contains('x'),
            "invalid start must be rejected with a clear error, got: {msg}"
        );
    }

    #[test]
    fn downloaded_chunk_rejects_bad_end() {
        // Non-numeric end: `end_repr.parse()` fails (lines 307-309).
        let toml = "downloaded_chunk = \"5-y\"\n";
        let err = toml::from_str::<PartialConfig>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("downloaded_chunk") && msg.contains('y'),
            "invalid end must be rejected with a clear error, got: {msg}"
        );
    }

    /// Covers the `None` arm of `range_list::serialize` (`config.rs` line 267).
    ///
    /// `PartialConfig` can never reach it: `inherit-config` injects
    /// `#[serde(skip_serializing_if = "Option::is_none")]` on every partial
    /// field, so serde skips the field entirely instead of calling the helper.
    /// A local wrapper without that attribute exercises the arm directly.
    #[test]
    fn range_list_serialize_none_emits_nothing() {
        #[derive(Serialize)]
        struct Wrapper {
            #[serde(with = "super::range_list")]
            downloaded_chunk: Option<Vec<ProgressEntry>>,
        }

        let toml = toml::to_string(&Wrapper {
            downloaded_chunk: None,
        })
        .unwrap();
        assert!(
            toml.is_empty(),
            "a `None` chunk list must serialize to nothing, got:\n{toml}"
        );
    }

    /// Covers the `None` arm of `range_list::deserialize` (`config.rs` line 290).
    ///
    /// TOML has no `null` and the partial field carries `serde(default)`, so a
    /// missing key never calls the helper; the arm is only reachable by feeding
    /// the helper a deserializer that yields `None` directly.
    #[test]
    fn range_list_deserialize_none_yields_none() {
        let de = serde::de::value::UnitDeserializer::<serde::de::value::Error>::new();
        let parsed = range_list::deserialize(de).unwrap();
        assert_eq!(parsed, None);
    }
}
