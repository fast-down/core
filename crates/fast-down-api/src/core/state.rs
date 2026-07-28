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
/// These are surfaced through [`Event::ResumeError`] so the caller is notified
/// instead of silently falling back to a full re-download.
#[derive(Debug, thiserror::Error)]
#[allow(clippy::large_enum_variant)]
pub enum StateError {
    /// No `.fd` state file exists for this download, so there is nothing to resume from.
    #[error("no .fd state file to resume from")]
    Open(std::io::Error),
    /// No `.fd` state file exists for this download, so there is nothing to resume from.
    #[error("no .fd state file to resume from")]
    Save(std::io::Error),
    /// No `.fd` state file exists for this download, so there is nothing to resume from.
    #[error("no .fd state file to resume from")]
    Decode(#[from] toml::de::Error),
    #[error("no .fd state file to resume from")]
    Encode(#[from] toml::ser::Error),
    /// The remote file changed (size / etag / last-modified mismatch), so resuming would corrupt the output.
    #[error("remote file changed, cannot resume")]
    FileChanged {
        local_file_id: FileId,
        local_file_size: u64,
        remote_file_id: FileId,
        remote_file_size: u64,
    },
    /// The server does not support resumable (range) downloads.
    #[error("server does not support resumable download")]
    NotResumable(UrlInfo, Response),
}

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

#[derive(Debug)]
pub struct DownloadState {
    inner: PartialDownloadStateInner,
    is_dirty: bool,
    pub config_path: PathBuf,
}

impl DownloadState {
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

    #[must_use]
    pub fn file_id(&self) -> FileId {
        FileId {
            etag: self.etag.clone().flatten(),
            last_modified: self.last_modified.clone().flatten(),
        }
    }

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
