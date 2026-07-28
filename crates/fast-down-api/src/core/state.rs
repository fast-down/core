use crate::{Config, PartialConfig};
use fast_down::{FileId, ProgressEntry, UrlInfo};
use inherit_config::{ConfigLayer, InheritConfig};
use path_helper::tokio::safe_replace;
use reqwest::Response;
use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::fs;
use url::Url;

/// Errors that can occur when an explicit [`DownloadHandle::resume`](crate::DownloadHandle::resume)
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
}

/// On-disk state for an in-progress download, backing resume support.
///
/// `DownloadState` pairs a [`PartialDownloadStateInner`] (the persisted config,
/// whose fields may be absent if the `.fd` file predates them) with the path of
/// the `.fd` file and a dirty flag. It is the bridge between a saved `.fd` file
/// and a fresh [`crate::PartialConfig`] handed to
/// [`crate::DownloadHandle::resume`]: [`DownloadState::merge_config`] folds the
/// loaded progress into the new request so a resumed download continues from the
/// correct byte offset instead of restarting from zero.
///
/// `DownloadState` derefs to [`PartialDownloadStateInner`], so the saved `url`,
/// `etag`, `last_modified`, `config` and `size` are reachable directly.
#[derive(Debug)]
pub struct DownloadState {
    inner: PartialDownloadStateInner,
    is_dirty: bool,
    pub config_path: PathBuf,
}

impl DownloadState {
    /// Build a fresh, dirty download state from the initial prefetch metadata.
    ///
    /// `url` and `url_info` come from the prefetch step, `config` is the
    /// caller-provided [`PartialConfig`], and `config_path` is where the `.fd`
    /// file will be written. The returned state is marked dirty so the first
    /// [`DownloadState::store`] persists it.
    #[must_use]
    pub fn new(url: &Url, url_info: &UrlInfo, config: &PartialConfig, config_path: &Path) -> Self {
        Self {
            inner: PartialDownloadStateInner {
                url: Some(url.clone()),
                etag: Some(url_info.file_id.etag.clone()),
                last_modified: Some(url_info.file_id.last_modified.clone()),
                config: Some(config.clone()),
                size: Some(url_info.size),
            },
            is_dirty: true,
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
            inner,
            is_dirty: false,
            config_path: config_path.to_path_buf(),
        })
    }

    /// Persist the download state to disk when it is dirty.
    ///
    /// # Errors
    /// Returns an error if serializing or writing the state fails.
    #[allow(clippy::result_large_err)]
    pub async fn store(&mut self) -> Result<(), StateError> {
        if self.is_dirty {
            self.inner
                .simplify_from(&PartialDownloadStateInner::default());
            let inner = toml::to_string_pretty(&self.inner)?;
            safe_replace(&self.config_path, inner.as_bytes())
                .await
                .map_err(StateError::Save)?;
            self.is_dirty = false;
        }
        Ok(())
    }

    /// Apply a mutation to the inner partial state and mark it dirty.
    ///
    /// Any change made through `cb` (e.g. updating `etag`/`last_modified` or the
    /// nested `config`) schedules the next [`DownloadState::store`].
    pub fn update(&mut self, cb: impl FnOnce(&mut PartialDownloadStateInner)) {
        cb(&mut self.inner);
        self.is_dirty = true;
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
        let local_file_size = self.size.unwrap_or(0);
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
        self.config
            .as_ref()
            .and_then(|c| c.downloaded_chunk.clone())
            .unwrap_or_default()
    }

    /// Merge a freshly-written byte range into the recorded progress.
    ///
    /// The progress list is the authoritative set of on-disk ranges; new ranges
    /// are merged, de-duplicated and normalized. Marks the state dirty.
    pub fn merge_progress(&mut self, range: ProgressEntry) {
        self.inner
            .config
            .get_or_insert_default()
            .merge_progress(range);
        self.is_dirty = true;
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
    pub fn merge_config(&mut self, partial_config: &PartialConfig) {
        if let Some(config) = &self.config {
            let mut pc = partial_config.clone();
            if let Some(downloaded_chunk) = &config.downloaded_chunk {
                for i in downloaded_chunk {
                    pc.merge_progress(i.clone());
                }
            }
            pc.inherit_from(config);
            self.inner.config = Some(pc);
            self.is_dirty = true;
        }
    }

    /// Reconstruct the identity of the file this state was saved for.
    ///
    /// Returns a [`FileId`] from the stored `etag` / `last_modified`. Missing
    /// headers collapse to `None`, which makes [`DownloadState::validate`] treat
    /// "no identity on either side" as a match.
    #[must_use]
    pub fn file_id(&self) -> FileId {
        FileId {
            etag: self.etag.clone().flatten(),
            last_modified: self.last_modified.clone().flatten(),
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

impl Deref for DownloadState {
    type Target = PartialDownloadStateInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
