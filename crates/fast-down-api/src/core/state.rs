use crate::{Config, PartialConfig};
use fast_down::{FileId, ProgressEntry, UrlInfo};
use inherit_config::{ConfigLayer, InheritConfig};
use parking_lot::Mutex;
use path_helper::tokio::safe_replace;
use reqwest::Response;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::fs;
use url::Url;

/// Errors that can occur when an explicit [`resume`](crate::resume)
/// cannot continue an interrupted download.
///
/// These are surfaced through [`crate::Event::ResumeError`] so the caller is notified
/// instead of silently falling back to a full re-download.
#[derive(Debug, thiserror::Error)]
#[allow(clippy::large_enum_variant)]
pub enum StateError {
    /// Failed to open or read the `.fd` state file from disk.
    ///
    /// This usually means the file is missing or unreadable (permission error,
    /// removed by another process, etc.).
    #[error("failed to open .fd state file: {0}")]
    Open(std::io::Error),
    /// Failed to persist the `.fd` state file to disk.
    ///
    /// The state was computed but could not be written (disk full, permission
    /// error, etc.).
    #[error("failed to save .fd state file: {0}")]
    Save(std::io::Error),
    /// Failed to deserialize the `.fd` state file.
    ///
    /// The file was read but is not valid TOML, or its schema no longer matches
    /// the current [`DownloadStateInner`] definition.
    #[error("failed to decode .fd state file: {0}")]
    Decode(#[from] toml::de::Error),
    /// Failed to serialize the download state into the `.fd` state file.
    ///
    /// The state could not be encoded as TOML (e.g. a field holds a value that
    /// has no valid TOML representation).
    #[error("failed to encode .fd state file: {0}")]
    Encode(#[from] toml::ser::Error),
    /// The remote file changed (size / etag / last-modified mismatch), so resuming would corrupt the output.
    #[error(
        "remote file changed, cannot resume\n  local:  size={local_file_size}, id={local_file_id:?}\n  remote: size={remote_file_size}, id={remote_file_id:?}"
    )]
    FileChanged {
        local_file_id: FileId,
        local_file_size: u64,
        remote_file_id: FileId,
        remote_file_size: u64,
    },
    /// The server does not support resumable (range) downloads.
    #[error(
        "server does not support resumable download\n  url_info: {:?}\n  url: {}\n  status: {}\n  headers: {:?}",
        .0, .1.url(), .1.status(), .1.headers()
    )]
    NotResumable(UrlInfo, Response),
}

/// Full (resolved) download state that is serialized into the `.fd` file.
///
/// `DownloadStateInner` is the non-partial form of the persisted state: every
/// field is present. It is encoded to TOML by [`DownloadState::store`] and read
/// back as the generated partial [`PartialDownloadStateInner`] on resume. The
/// `size` field is recorded for resume validation; older `.fd` files omit it and
/// read back as `None` in the partial form.
#[derive(Debug, Clone, InheritConfig)]
pub struct DownloadStateInner {
    #[config(default = Url::parse("about:blank").unwrap())]
    pub url: Url,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    #[config(nest)]
    pub config: Config,
    /// Total file size recorded at save time, compared against the server
    /// `UrlInfo.size` during resume validation.
    ///
    /// An older `.fd` written without this field reads back as `None`, which
    /// makes `validate` fail and safely fall back to a full re-download.
    pub size: u64,
    /// Total active download time accumulated across all resume runs.
    ///
    /// Persisted as a human-readable string (e.g. `"1h 30m 2s"`) via
    /// `humantime_serde`, so the `.fd` file stays easy to inspect and edit by
    /// hand. A zero elapsed is omitted from the file, and a missing `elapsed`
    /// field on load is tolerated (treated as zero). A resumed download
    /// continues this clock instead of restarting it from zero; the progress
    /// reporter folds it into its average-speed calculation, giving a speed that
    /// spans the whole download session rather than only the current run.
    #[config(default = Duration::ZERO)]
    #[config(partial_attr(serde(with = "humantime_serde::option")))]
    #[config(partial_attr(serde(default)))]
    pub elapsed: Duration,
}

/// On-disk state for an in-progress download, backing resume support.
///
/// `DownloadState` pairs a [`PartialDownloadStateInner`] (the persisted config,
/// whose fields may be absent if the `.fd` file predates them) with the path of
/// the `.fd` file and a dirty flag. It is the bridge between a saved `.fd` file
/// and a fresh [`crate::PartialConfig`] handed to
/// [`crate::resume`]: [`DownloadState::merge_config`] folds the
/// loaded progress into the new request so a resumed download continues from the
/// correct byte offset instead of restarting from zero.
///
/// `DownloadState` derefs to [`PartialDownloadStateInner`], so the saved `url`,
/// `etag`, `last_modified`, `config` and `size` are reachable directly.
///
/// The inner state is wrapped in an `Arc<Mutex<PartialDownloadStateInner>>` to
/// provide a single source of truth shared between the engine loop, the
/// internal progress reporter, and any other readers/writers.
///
/// `DownloadState` is cheaply `Clone`able: the clone shares the same inner state
/// and the same dirty flag (both behind `Arc`), so a download driver can hand a
/// clone to a background task that performs the periodic disk persist without
/// blocking the engine event loop.
#[derive(Debug, Clone)]
pub struct DownloadState {
    inner: Arc<Mutex<PartialDownloadStateInner>>,
    is_dirty: Arc<AtomicBool>,
    pub config_path: PathBuf,
}

impl DownloadState {
    /// Returns a reference to the shared inner state, locked for reading.
    /// Prefer this over the previous `Deref`-based field access.
    pub fn lock_inner(&self) -> parking_lot::MutexGuard<'_, PartialDownloadStateInner> {
        self.inner.lock()
    }

    /// Returns a clone of the shared `Arc<Mutex<PartialDownloadStateInner>>`,
    /// for sharing the authoritative state with other tasks
    /// (e.g. the internal progress reporter).
    #[must_use]
    pub fn share_inner(&self) -> Arc<Mutex<PartialDownloadStateInner>> {
        self.inner.clone()
    }

    /// Build a fresh, dirty download state from the initial prefetch metadata.
    ///
    /// `url` and `url_info` come from the prefetch step, `config` is the
    /// caller-provided [`PartialConfig`], and `config_path` is where the `.fd`
    /// file will be written. The returned state is marked dirty so the first
    /// [`DownloadState::store`] persists it.
    #[must_use]
    pub fn new(url: &Url, url_info: &UrlInfo, config: &PartialConfig, config_path: &Path) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PartialDownloadStateInner {
                url: Some(url.clone()),
                etag: Some(url_info.file_id.etag.clone()),
                last_modified: Some(url_info.file_id.last_modified.clone()),
                config: Some(config.clone()),
                size: Some(url_info.size),
                elapsed: Some(Duration::ZERO),
            })),
            is_dirty: Arc::new(AtomicBool::new(true)),
            config_path: config_path.to_path_buf(),
        }
    }

    /// Load a download state from disk.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or deserialized.
    #[allow(clippy::result_large_err)]
    pub async fn load(config_path: &Path) -> Result<Self, StateError> {
        let inner = fs::read(&config_path).await.map_err(StateError::Open)?;
        let inner: PartialDownloadStateInner = toml::from_slice(&inner)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            is_dirty: Arc::new(AtomicBool::new(false)),
            config_path: config_path.to_path_buf(),
        })
    }

    /// Persist the download state to disk.
    ///
    /// This serializes a snapshot of the shared inner state and atomically
    /// replaces the `.fd` file. It does **not** consult or clear the dirty flag;
    /// callers decide *when* to persist (typically via [`DownloadState::take_dirty`]
    /// on a fixed cadence, off the engine event loop). Safe to call from a
    /// background task because the heavy `safe_replace` (disk write + fsync) runs
    /// after the inner lock is released.
    ///
    /// # Errors
    /// Returns an error if serializing or writing the state fails.
    #[allow(clippy::result_large_err)]
    pub async fn store(&self) -> Result<(), StateError> {
        let serialized = {
            let mut guard = self.inner.lock();
            guard.simplify_from(&PartialDownloadStateInner::default());
            toml::to_string_pretty(&*guard)?
        };
        safe_replace(&self.config_path, serialized.as_bytes())
            .await
            .map_err(StateError::Save)?;
        Ok(())
    }

    /// Consume the dirty flag (swap to clean) and report whether it was dirty.
    ///
    /// Used by the engine loop on a fixed cadence: a `true` return means "a
    /// persist is due, hand the work to a background task". Because the flag is
    /// an `Arc<AtomicBool>` shared with any clones, the background task and the
    /// loop observe the same flag.
    #[must_use]
    pub fn take_dirty(&self) -> bool {
        self.is_dirty.swap(false, Ordering::SeqCst)
    }

    /// Mark the state dirty so the next persist cycle rewrites the `.fd`.
    ///
    /// Call this when a mutation lands (e.g. progress or identity changed) and
    /// when a background [`DownloadState::store`] fails and should be retried.
    pub fn mark_dirty(&self) {
        self.is_dirty.store(true, Ordering::SeqCst);
    }

    /// Apply a mutation to the inner partial state and mark it dirty.
    ///
    /// Any change made through `cb` (e.g. updating `etag`/`last_modified` or the
    /// nested `config`) schedules the next [`DownloadState::store`].
    pub fn update<F: FnOnce(&mut PartialDownloadStateInner) -> R, R>(&self, cb: F) -> R {
        let res = cb(&mut self.inner.lock());
        self.mark_dirty();
        res
    }

    /// Check whether the server-side file is still the same one this state was
    /// saved for, so a resumed download continues from the correct offset.
    ///
    /// The comparison requires the recorded `size` to match and (unless both
    /// sides are missing identity headers) the `FileId` (`etag` +
    /// `last_modified`) to be equal. Resumable downloads also require the
    /// server to support range requests.
    ///
    /// # Errors
    #[allow(clippy::result_large_err)]
    pub fn validate(&self, info: &UrlInfo) -> Result<(), StateError> {
        let local_file_id = self.file_id();
        let local_file_size = self.inner.lock().size.unwrap_or(0);
        let is_same = local_file_size == info.size && local_file_id == info.file_id;
        if is_same {
            Ok(())
        } else {
            Err(StateError::FileChanged {
                local_file_id,
                local_file_size,
                remote_file_id: info.file_id.clone(),
                remote_file_size: info.size,
            })
        }
    }

    /// Return the list of byte ranges already written to the `.part` file.
    ///
    /// This is the authoritative on-disk progress. It is empty for a brand new
    /// download and grows as [`DownloadState::merge_progress`] is called.
    #[must_use]
    pub fn get_progress(&self) -> Vec<ProgressEntry> {
        self.inner
            .lock()
            .config
            .as_ref()
            .and_then(|c| c.downloaded_chunk.clone())
            .unwrap_or_default()
    }

    /// Total active download time accumulated so far, across all prior resume
    /// runs. `Duration::ZERO` when nothing has been recorded yet.
    #[must_use]
    pub fn get_elapsed(&self) -> Duration {
        self.inner.lock().elapsed.unwrap_or(Duration::ZERO)
    }

    /// Set the total accumulated active download time (absolute, not additive).
    ///
    /// Called by the download driver as time accrues, so the value persisted on
    /// the next [`DownloadState::store`] reflects the full session. Marks the
    /// state dirty.
    pub fn set_elapsed(&self, elapsed: Duration) {
        self.update(|inner| inner.elapsed = Some(elapsed));
    }

    /// Merge a freshly-written byte range into the recorded progress.
    ///
    /// The progress list is the authoritative set of on-disk ranges; new ranges
    /// are merged, de-duplicated and normalized. Marks the state dirty.
    pub fn merge_progress(&self, range: ProgressEntry) {
        self.update(|inner| inner.config.get_or_insert_default().merge_progress(range));
    }

    /// Fold a freshly-built [`PartialConfig`] into this loaded state for resume.
    ///
    /// This is the key bridge that preserves download progress across a resume:
    /// the ranges already recorded in `self.config.downloaded_chunk` are merged
    /// into `partial_config` first, so that even if `partial_config` starts from
    /// an empty progress list the resumed download keeps the already-downloaded
    /// bytes. `partial_config` is then layered on top via
    /// `inherit_from`, and the merged result replaces the stored
    /// config. Marks the state dirty.
    pub fn merge_config(&self, partial_config: &PartialConfig) {
        self.update(|inner| {
            let mut pc = partial_config.clone();
            if let Some(config) = &inner.config {
                if let Some(downloaded_chunk) = &config.downloaded_chunk {
                    for i in downloaded_chunk {
                        pc.merge_progress(i.clone());
                    }
                }
                pc.inherit_from(config);
            }
            inner.config = Some(pc);
        });
    }

    /// Reconstruct the identity of the file this state was saved for.
    ///
    /// Returns a [`FileId`] from the stored `etag` / `last_modified`. Missing
    /// headers collapse to `None`, which makes [`DownloadState::validate`] treat
    /// "no identity on either side" as a match.
    #[must_use]
    pub fn file_id(&self) -> FileId {
        let inner = self.inner.lock();
        FileId {
            etag: inner.etag.clone().flatten(),
            last_modified: inner.last_modified.clone().flatten(),
        }
    }

    /// Path of the partial (`.part`) output file paired with this state.
    ///
    /// Derived from `config_path` by swapping the extension to `.part`.
    #[must_use]
    pub fn tmp_path(&self) -> PathBuf {
        self.config_path.with_extension("part")
    }
}

#[cfg(test)]
mod tests {
    use super::{DownloadState, PartialConfig};
    use fast_down::{FileId, UrlInfo};
    use std::path::Path;
    use std::time::Duration;
    use url::Url;

    fn make_state(path: &Path) -> DownloadState {
        let url = Url::parse("https://example.com/file.bin").unwrap();
        let url_info = UrlInfo {
            size: 1024,
            raw_name: "file.bin".to_string(),
            supports_range: true,
            fast_download: true,
            final_url: url.clone(),
            file_id: FileId::new(Some("etag-1"), None),
            content_type: Some("application/octet-stream".to_string()),
        };
        DownloadState::new(&url, &url_info, &PartialConfig::default(), path)
    }

    #[tokio::test]
    async fn elapsed_stored_as_human_readable_string_and_round_trips() {
        let path = std::env::temp_dir().join(format!(
            "fd_elapsed_test_{}.fd",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = make_state(&path);
        let value = Duration::from_secs(3661); // 1h 1m 1s
        state.set_elapsed(value);
        state.store().await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        // Human-readable: a quoted string carrying h/m/s, not raw nanoseconds.
        assert!(
            content.contains("elapsed = \"")
                && content.contains('h')
                && content.contains('m')
                && content.contains('s'),
            "elapsed should be stored as a human-readable string, got:\n{content}"
        );
        assert!(
            !content.contains("3661000000000"),
            "elapsed must not be stored as raw nanoseconds, got:\n{content}"
        );

        let loaded = DownloadState::load(&path).await.unwrap();
        assert_eq!(loaded.get_elapsed(), value);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn elapsed_zero_is_not_persisted() {
        let path = std::env::temp_dir().join(format!(
            "fd_elapsed_zero_{}.fd",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = make_state(&path); // fresh state => elapsed is ZERO
        state.store().await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !content.contains("elapsed"),
            "a zero elapsed should be omitted from the .fd file, got:\n{content}"
        );

        let loaded = DownloadState::load(&path).await.unwrap();
        assert_eq!(loaded.get_elapsed(), Duration::ZERO);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn merge_config_adopts_fresh_config_when_loaded_state_has_none() {
        // Regression guard for the bug where a resumed `.fd` that deserializes with
        // `inner.config = None` (which is reachable: `store` omits an all-default
        // `[config]` table via `skip_serializing_if`) made `merge_config` a silent
        // no-op — it marked the state dirty but never adopted the fresh request's
        // config, so the user's resume settings were dropped. The fix makes the
        // `None` arm adopt the fresh config (a real mutation), so the dirty flag is
        // always earned. This test pins that behavior.
        let path = std::env::temp_dir().join(format!(
            "fd_merge_none_{}.fd",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // A `.fd` with NO `[config]` section: only the fields a valid state needs.
        tokio::fs::write(
            &path,
            "url = \"https://example.com/file.bin\"\nsize = 1024\n",
        )
        .await
        .unwrap();

        let state = DownloadState::load(&path).await.unwrap();
        assert!(
            state.lock_inner().config.is_none(),
            "precondition: a .fd without [config] must load with inner.config = None"
        );

        let fresh = PartialConfig {
            min_chunk_size: Some(1234),
            ..Default::default()
        };
        state.merge_config(&fresh);

        assert!(
            state.lock_inner().config.is_some(),
            "merge_config must adopt the fresh config even when the loaded state had none \
             (regression: it used to be a silent no-op that dropped the resume request)"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    #[allow(clippy::single_range_in_vec_init)]
    async fn merge_config_preserves_loaded_progress_into_fresh() {
        // Regression guard for the `Some(downloaded_chunk)` arm of
        // `merge_config` (state.rs lines 313-317): when the loaded `.fd` already
        // records progress, that progress must be folded into the fresh request so a
        // resumed download keeps the already-downloaded bytes, and the fresh
        // request's own overrides (here `min_chunk_size`) must still apply.
        let path = std::env::temp_dir().join(format!(
            "fd_merge_progress_{}.fd",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = make_state(&path);
        // Simulate a loaded state that already recorded two contiguous ranges.
        state.inner.lock().config = Some(PartialConfig {
            downloaded_chunk: Some(vec![0u64..10, 10..20]),
            ..Default::default()
        });

        let fresh = PartialConfig {
            min_chunk_size: Some(2048),
            ..Default::default()
        };
        state.merge_config(&fresh);

        let merged = state.inner.lock().config.clone().unwrap();
        assert_eq!(
            merged.downloaded_chunk,
            Some(vec![0u64..20]),
            "loaded progress must be preserved and merged into the fresh config"
        );
        assert_eq!(
            merged.min_chunk_size,
            Some(2048),
            "fresh request overrides must still be applied via inherit_from"
        );

        let _ = std::fs::remove_file(&path);
    }
}
