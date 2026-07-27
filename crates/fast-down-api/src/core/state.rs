use crate::{Config, PartialConfig};
use fast_down::{FileId, Merge, ProgressEntry, UrlInfo};
use inherit_config::{ConfigLayer, InheritConfig};
use path_helper::tokio::safe_replace;
use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::fs;
use url::Url;

#[derive(Debug, Clone, InheritConfig)]
pub struct DownloadStateInner {
    #[config(default = Url::parse("about:blank").unwrap())]
    pub url: Url,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    #[config(nest)]
    pub config: Config,
    pub progress: Vec<ProgressEntry>,
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
    config_path: PathBuf,
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
                progress: Some(Vec::new()),
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
    pub async fn load(config_path: &Path) -> anyhow::Result<Self> {
        let inner = fs::read(&config_path).await?;
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
    pub async fn store(&mut self) -> anyhow::Result<()> {
        if self.is_dirty {
            self.inner
                .simplify_from(&PartialDownloadStateInner::default());
            let inner = toml::to_string_pretty(&self.inner)?;
            safe_replace(&self.config_path, inner.as_bytes()).await?;
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
    #[must_use]
    pub fn validate(&self, info: &UrlInfo) -> bool {
        info.fast_download && self.size == Some(info.size) && self.file_id() == info.file_id
    }

    /// Merge a freshly-written byte range into the recorded progress.
    ///
    /// The progress list is the authoritative set of on-disk ranges; new ranges
    /// are merged, de-duplicated and normalized. Marks the state dirty.
    pub fn merge_progress(&mut self, range: ProgressEntry) {
        self.inner
            .progress
            .get_or_insert_with(Vec::new)
            .merge_progress(range);
        self.is_dirty = true;
    }

    /// Total number of bytes already downloaded, derived from `progress`.
    ///
    /// This value is intentionally not persisted; it is recomputed on demand
    /// from the recorded ranges.
    #[must_use]
    pub fn downloaded_bytes(&self) -> u64 {
        self.progress
            .as_ref()
            .map_or(0, |v| v.iter().map(|r| r.end - r.start).sum())
    }

    #[must_use]
    pub fn file_id(&self) -> FileId {
        FileId {
            etag: self.etag.clone().flatten(),
            last_modified: self.last_modified.clone().flatten(),
        }
    }
}

impl Deref for DownloadState {
    type Target = PartialDownloadStateInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
