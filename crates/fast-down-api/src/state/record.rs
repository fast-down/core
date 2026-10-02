use crate::{PartialConfig, StateError};
use fast_down::{FileId, Merge, ProgressEntry, Total, UrlInfo};
use inherit_config::ConfigLayer;
use path_helper::tokio::safe_replace;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};
use url::Url;

/// The on-disk `.fd` state of one download: the remote identity it was written
/// for, the byte ranges already fetched, and the accumulated active time.
///
/// The `.part` file it describes is **not** recorded here — it is always
/// [`crate::state::tmp_path_for`] of the `.fd` path, i.e. the same path with its
/// extension swapped to `.part`. Keeping the pairing implicit lets the two
/// files be renamed or moved together, and removes a hand-editable path from
/// the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// The durable initial URL (never a transient redirect target).
    pub url: Url,
    #[serde(default)]
    pub config: PartialConfig,
    #[serde(default)]
    pub size: u64,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    #[serde(default, with = "humantime_serde")]
    pub elapsed: Duration,
}

impl Record {
    /// A brand-new record for an unfetched URL: no identity, no progress.
    #[must_use]
    pub const fn from_url(url: Url, config: PartialConfig) -> Self {
        Self {
            url,
            config,
            size: 0,
            etag: None,
            last_modified: None,
            elapsed: Duration::ZERO,
        }
    }

    /// Read and decode a `.fd` file.
    ///
    /// A missing file is reported as [`StateError::NotFound`] rather than a
    /// generic I/O error, so callers can tell "this download never ran" (fine)
    /// apart from "the state exists but cannot be read" (not fine).
    ///
    /// # Errors
    /// Returns [`StateError::NotFound`] when there is no `.fd` at `fd_path`,
    /// [`StateError::Open`] for any other read failure, or
    /// [`StateError::Decode`] when the contents are not valid TOML.
    pub async fn load(fd_path: impl AsRef<Path>) -> Result<Self, StateError> {
        let fd_path = fd_path.as_ref();
        let bytes = tokio::fs::read(fd_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StateError::NotFound(fd_path.to_path_buf())
            } else {
                StateError::Open(e)
            }
        })?;
        Ok(toml::from_slice(&bytes)?)
    }

    /// The identity headers this record was saved with.
    ///
    /// `etag`/`last_modified` are already `Option<Arc<str>>`, so they map
    /// straight onto [`FileId`] without unwrapping.
    #[must_use]
    pub fn file_id(&self) -> FileId {
        FileId {
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
        }
    }

    /// Check this record still describes `info`, so its progress can be resumed.
    ///
    /// A resume is only safe when the recorded size and identity headers match
    /// the freshly prefetched ones. When *both* sides lack identity headers the
    /// [`FileId`]s are equal (`None == None`), so a server that sends neither
    /// `ETag` nor `Last-Modified` is treated as unchanged as long as the size
    /// matches.
    ///
    /// # Errors
    /// Returns [`StateError::FileChanged`] when the size or identity differs.
    pub fn validate(&self, info: &UrlInfo) -> Result<(), StateError> {
        let local_file_id = self.file_id();
        if self.size == info.size && local_file_id == info.file_id {
            Ok(())
        } else {
            Err(StateError::FileChanged {
                local_file_id,
                local_file_size: self.size,
                remote_file_id: info.file_id.clone(),
                remote_file_size: info.size,
            })
        }
    }

    /// Highest exclusive offset the recorded progress reaches; `0` when nothing
    /// has been downloaded.
    fn recorded_end(&self) -> u64 {
        self.config
            .downloaded_chunk
            .as_ref()
            .and_then(|ranges| ranges.iter().map(|r| r.end).max())
            .unwrap_or(0)
    }

    /// Classify the `.part` file at `tmp_path` against the recorded progress.
    ///
    /// [`PartState::Short`] is the dangerous case: continuing would let the sink
    /// extend the file with zeros over the missing span and never fetch it,
    /// corrupting the output. A stat failure is treated as
    /// [`PartState::Missing`] — an unreadable `.part` cannot be verified, so
    /// starting clean is the only safe choice, and a genuinely unopenable path
    /// surfaces later as a sink error.
    pub async fn part_state(&self, tmp_path: &Path) -> PartState {
        let Ok(meta) = tokio::fs::metadata(tmp_path).await else {
            return PartState::Missing;
        };
        if !meta.is_file() {
            return PartState::Missing;
        }
        let actual = meta.len();
        let recorded = self.recorded_end();
        if actual < recorded {
            PartState::Short { actual, recorded }
        } else {
            PartState::Ready
        }
    }

    /// The `.fd` state of a freshly-started download, written before any bytes
    /// flow so an interruption can be resumed.
    ///
    /// # Errors
    /// Returns [`StateError::Encode`] when the record cannot be serialized, or
    /// [`StateError::Save`] when the file cannot be written.
    #[allow(clippy::result_large_err)]
    pub async fn store(&self, fd_path: &Path) -> Result<(), StateError> {
        let text = toml::to_string_pretty(self)?;
        safe_replace(fd_path, text.as_bytes())
            .await
            .map_err(StateError::Save)?;
        Ok(())
    }

    /// Merge a freshly-written byte range into the recorded progress.
    pub fn merge_progress(&mut self, range: ProgressEntry) {
        self.config
            .downloaded_chunk
            .get_or_insert_default()
            .merge_progress(range);
    }

    /// Replace the recorded progress with `progress`.
    pub fn set_progress(&mut self, progress: Vec<ProgressEntry>) {
        self.config.downloaded_chunk = Some(progress);
    }

    /// Drop any recorded progress: the download will start from byte zero.
    pub fn clear_progress(&mut self) {
        if let Some(config) = self.config.downloaded_chunk.as_mut() {
            config.clear();
        }
    }

    /// Total bytes recorded as written.
    #[must_use]
    pub fn downloaded(&self) -> u64 {
        self.config
            .downloaded_chunk
            .as_ref()
            .map_or(0, Total::total)
    }

    /// A copy of the recorded progress ranges.
    #[must_use]
    pub fn progress(&self) -> Vec<ProgressEntry> {
        self.config.downloaded_chunk.clone().unwrap_or_default()
    }

    /// Whether every byte of a `total`-byte file has been recorded. A zero-size
    /// file counts as complete.
    #[must_use]
    pub fn is_complete(&self, total: u64) -> bool {
        total == 0
            || matches!(
                self.config.downloaded_chunk.as_deref(),
                Some([entry]) if entry == &(0..total)
            )
    }

    /// Point the record at the URL this run resolved, keeping the initial URL
    /// semantics (`url` is what a later `State::load` will re-resolve).
    pub fn set_url(&mut self, url: &Url) {
        self.url = url.clone();
    }

    /// Refresh the recorded identity to the values the current prefetch saw.
    ///
    /// Only call this once the record is known to still describe `info` (see
    /// [`Record::validate`]): overwriting the identity first would make a later
    /// validation compare a value with itself and always pass.
    pub fn refresh_identity(&mut self, info: &UrlInfo) {
        self.size = info.size;
        self.etag.clone_from(&info.file_id.etag);
        self.last_modified.clone_from(&info.file_id.last_modified);
    }

    /// Layer the caller's fresh overrides over the persisted configuration,
    /// keeping the recorded download progress.
    ///
    /// Progress lives in `config.downloaded_chunk`. It is dropped from the
    /// caller's layer before `inherit_from` (which only fills *empty* fields)
    /// so the persisted progress survives instead of being shadowed by the
    /// caller's (usually empty) copy.
    pub fn merge_config(&mut self, caller: &PartialConfig) {
        let mut merged = caller.clone();
        merged.downloaded_chunk = None;
        merged.inherit_from(&self.config);
        self.config = merged;
    }
}

/// What the `.part` paired with a [`Record`] looks like on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartState {
    /// No regular file at the derived path: nothing safe to continue from.
    Missing,
    /// The file exists but is shorter than the recorded progress.
    Short { actual: u64, recorded: u64 },
    /// The file exists and is at least as long as the recorded progress.
    Ready,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::single_range_in_vec_init)]
    use super::*;
    use fast_down::ProgressEntry;

    fn url() -> Url {
        Url::parse("https://example.com/video.mp4").unwrap()
    }

    fn info(size: u64, etag: &str) -> UrlInfo {
        UrlInfo {
            size,
            raw_name: "video.mp4".to_string(),
            supports_range: true,
            fast_download: true,
            final_url: url(),
            file_id: FileId::new(Some(etag), None),
            content_type: None,
        }
    }

    fn record_with(size: u64, etag: &str) -> Record {
        Record {
            url: url(),
            config: PartialConfig::default(),
            size,
            etag: Some(etag.into()),
            last_modified: None,
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn record_round_trips_through_toml() {
        let mut record = record_with(1000, "abc");
        record.elapsed = Duration::from_secs(3723);
        record.config.downloaded_chunk = Some(vec![0..500]);

        let text = toml::to_string(&record).unwrap();
        assert!(
            text.contains(r#"elapsed = "1h 2m 3s""#),
            "elapsed must be stored human-readably, got:\n{text}"
        );
        assert!(
            !text.contains("tmp_path"),
            "the .part path must not be persisted, got:\n{text}"
        );

        let back: Record = toml::from_slice(text.as_bytes()).unwrap();
        assert_eq!(back.size, 1000);
        assert_eq!(back.file_id().etag.as_deref(), Some("abc"));
        assert_eq!(back.elapsed, Duration::from_secs(3723));
        assert_eq!(back.config.downloaded_chunk, Some(vec![0..500]));
    }

    #[test]
    fn record_requires_url() {
        let err = toml::from_slice::<Record>(b"size = 1\n").unwrap_err();
        assert!(
            err.to_string().contains("url"),
            "a .fd without a url must be rejected, got: {err}"
        );
    }

    #[test]
    fn record_tolerates_missing_optional_fields() {
        let record: Record = toml::from_slice(b"url = \"https://e.com/f\"\n").unwrap();
        assert_eq!(record.size, 0);
        assert_eq!(record.etag, None);
        assert_eq!(record.last_modified, None);
        assert_eq!(record.elapsed, Duration::ZERO);
    }

    #[test]
    fn validate_matches_size_and_identity() {
        assert!(record_with(100, "e").validate(&info(100, "e")).is_ok());
    }

    #[test]
    fn validate_reports_both_sides_on_mismatch() {
        let err = record_with(100, "old")
            .validate(&info(200, "new"))
            .unwrap_err();
        match err {
            StateError::FileChanged {
                local_file_size,
                remote_file_size,
                local_file_id,
                remote_file_id,
            } => {
                assert_eq!((local_file_size, remote_file_size), (100, 200));
                assert_eq!(local_file_id.etag.as_deref(), Some("old"));
                assert_eq!(remote_file_id.etag.as_deref(), Some("new"));
            }
            other => panic!("expected FileChanged, got {other:?}"),
        }
    }

    #[test]
    fn validate_treats_absent_headers_on_both_sides_as_match() {
        let record = Record {
            url: url(),
            config: PartialConfig::default(),
            size: 100,
            etag: None,
            last_modified: None,
            elapsed: Duration::ZERO,
        };
        let info = UrlInfo {
            file_id: FileId::default(),
            ..info(100, "")
        };
        assert!(record.validate(&info).is_ok());
    }

    /// Guards the ordering bug `build` must avoid: refreshing the identity
    /// before validating makes the comparison compare a value with itself, so
    /// `validate` can never fail afterwards.
    #[test]
    fn validate_is_vacuous_after_refresh_identity() {
        let mut record = record_with(100, "old");
        let remote = info(200, "new");
        assert!(record.validate(&remote).is_err(), "precondition: stale");
        record.refresh_identity(&remote);
        assert!(
            record.validate(&remote).is_ok(),
            "after refresh the record trivially matches: callers must validate first"
        );
    }

    #[test]
    fn merge_config_keeps_recorded_progress() {
        let mut record = record_with(100, "e");
        record.config.downloaded_chunk = Some(vec![0..50]);

        let caller = PartialConfig {
            threads: Some(7),
            downloaded_chunk: Some(Vec::<ProgressEntry>::new()),
            ..Default::default()
        };
        record.merge_config(&caller);

        assert_eq!(record.config.downloaded_chunk, Some(vec![0..50]));
        assert_eq!(record.config.threads, Some(7));
    }

    #[test]
    fn merge_config_prefers_caller_overrides() {
        let mut record = record_with(100, "e");
        record.config.threads = Some(4);

        let caller = PartialConfig {
            threads: Some(9),
            ..Default::default()
        };
        record.merge_config(&caller);

        assert_eq!(record.config.threads, Some(9), "caller overrides must win");
    }

    #[tokio::test]
    async fn part_state_classifies_against_recorded_progress() {
        let dir = std::env::temp_dir().join(format!("fd_part_state_{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let part = dir.join("video.mp4.part");

        let mut record = record_with(100, "e");
        record.config.downloaded_chunk = Some(vec![0..50]);

        assert_eq!(record.part_state(&part).await, PartState::Missing);

        tokio::fs::write(&part, vec![0u8; 10]).await.unwrap();
        assert_eq!(
            record.part_state(&part).await,
            PartState::Short {
                actual: 10,
                recorded: 50
            }
        );

        tokio::fs::write(&part, vec![0u8; 50]).await.unwrap();
        assert_eq!(record.part_state(&part).await, PartState::Ready);

        tokio::fs::write(&part, vec![0u8; 80]).await.unwrap();
        assert_eq!(record.part_state(&part).await, PartState::Ready);

        tokio::fs::remove_file(&part).await.unwrap();
        tokio::fs::create_dir_all(&part).await.unwrap();
        assert_eq!(
            record.part_state(&part).await,
            PartState::Missing,
            "a directory is not a usable .part"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
