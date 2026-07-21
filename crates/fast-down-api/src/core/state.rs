use crate::{Config, PartialConfig};
use fast_down::{ProgressEntry, UrlInfo};
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
    #[config(default = Url::parse("").unwrap())]
    pub url: Url,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    #[config(nest)]
    pub config: Config,
    pub progress: Vec<ProgressEntry>,
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
            },
            is_dirty: false,
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
}

impl Deref for DownloadState {
    type Target = PartialDownloadStateInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
