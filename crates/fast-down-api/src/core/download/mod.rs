use crate::core::download::overwrite::OverwriteOption;
use crate::utils::ForceSendExt;
use crate::{DownloadState, Event, StateError};
use crate::{PartialConfig, Tx, prefetch, tx_err, utils::gen_path};
use fast_down::handle::SharedHandle;
use inherit_config::ConfigLayer;
use overwrite::overwrite;
use path_helper::IterStemExt;
use std::path::Path;
use std::sync::Arc;
use tokio::fs::{self, OpenOptions};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use url::Url;

mod overwrite;
mod pipeline;

fn open_existing() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create(false);
    o
}
fn open_create() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create(true);
    o
}
fn open_create_new() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create_new(true);
    o
}

#[derive(Debug, Clone)]
pub struct DownloadHandle {
    handle: SharedHandle<()>,
}

impl DownloadHandle {
    /// Returns the join of this [`DownloadHandle`].
    ///
    /// # Errors
    ///
    /// This function will return an error if download thread panic
    pub async fn join(&self) -> Result<(), Arc<JoinError>> {
        self.handle.join().await
    }

    #[must_use]
    pub fn download(
        url: Url,
        partial_config: PartialConfig,
        tx: Tx,
        token: CancellationToken,
    ) -> Self {
        let handle = tokio::spawn(
            async move {
                let token2 = token.clone();
                let opt = token
                    .run_until_cancelled(
                        async move { download(url, partial_config, tx, token2).await },
                    )
                    .await
                    .flatten();
                if let Some(opt) = opt {
                    overwrite(opt).await;
                }
            }
            .force_send(),
        );
        Self {
            handle: SharedHandle::new(handle),
        }
    }

    pub fn resume(
        tmp_path: impl AsRef<Path>,
        url: Url,
        partial_config: PartialConfig,
        tx: Tx,
        token: CancellationToken,
    ) -> Self {
        let tmp_path = tmp_path.as_ref().to_path_buf();
        let handle = tokio::spawn(
            async move {
                let token2 = token.clone();
                let opt = Box::pin(token.run_until_cancelled(async move {
                    resume(&tmp_path, url, partial_config, tx, token2).await
                }))
                .await
                .flatten();
                if let Some(opt) = opt {
                    overwrite(opt).await;
                }
            }
            .force_send(),
        );
        Self {
            handle: SharedHandle::new(handle),
        }
    }
}

async fn download(
    url: Url,
    partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) -> Option<OverwriteOption> {
    let config = partial_config.clone().build();
    let (info, resp) = prefetch(&url, &config, &tx).await?;
    let can_resume = config.resume && info.fast_download;

    let origin_path = tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError, None);

    if config.overwrite {
        let cfg_path = origin_path.with_added_extension("fd");
        let tmp_path = origin_path.with_added_extension("part");

        let state = if can_resume
            && let Ok(mut s) = DownloadState::load(&cfg_path).await
            && s.validate(&info).is_ok()
            && fs::try_exists(&tmp_path).await.unwrap_or(false)
        {
            s.merge_config(&partial_config);
            let _ = tx.send(Event::Resumed {
                config_path: cfg_path,
                progress: s.get_progress(),
                size: info.size,
            });
            s
        } else {
            tx_err!(
                open_create().open(tmp_path).await,
                tx,
                BuildPusherError,
                None
            );
            DownloadState::new(&url, &info, &partial_config, &cfg_path)
        };
        return Some(OverwriteOption {
            state,
            final_path: origin_path,
            info,
            resp,
            tx,
            token,
        });
    }

    for base_path in origin_path.iter_stem() {
        let tmp_path = base_path.with_added_extension("part");
        let cfg_path = base_path.with_added_extension("fd");

        let state = if can_resume
            && let Ok(mut s) = DownloadState::load(&cfg_path).await
            && s.validate(&info).is_ok()
            && fs::try_exists(&tmp_path).await.unwrap_or(false)
        {
            s.merge_config(&partial_config);
            let _ = tx.send(Event::Resumed {
                config_path: cfg_path,
                progress: s.get_progress(),
                size: info.size,
            });
            s
        } else if open_create_new().open(&tmp_path).await.is_ok() {
            DownloadState::new(&url, &info, &partial_config, &cfg_path)
        } else {
            continue;
        };
        return Some(OverwriteOption {
            state,
            final_path: origin_path,
            info,
            resp,
            tx,
            token,
        });
    }
    unreachable!()
}

async fn resume(
    tmp_path: &Path,
    url: Url,
    mut partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) -> Option<OverwriteOption> {
    partial_config.overwrite = Some(false);
    let tmp_exits = fs::try_exists(tmp_path).await.unwrap_or(false);
    if !tmp_exits {
        partial_config.resume = Some(false);
        return download(url, partial_config, tx, token).await;
    }

    let cfg_path = tmp_path.with_extension("fd");
    let mut state = tx_err!(DownloadState::load(&cfg_path).await, tx, ResumeError, None);

    partial_config.resume = Some(true);
    let config = partial_config.clone().build();
    let (info, resp) = prefetch(&url, &config, &tx).await?;
    if !info.fast_download {
        let _ = tx.send(Event::ResumeError(StateError::NotResumable(info, resp)));
        return None;
    }
    tx_err!(state.validate(&info), tx, ResumeError, None);

    state.merge_config(&partial_config);
    let _ = tx.send(Event::Resumed {
        config_path: cfg_path,
        progress: state.get_progress(),
        size: info.size,
    });

    let final_path = tx_err!(gen_path(&url, &info, &config).await, tx, GenPathError, None);
    Some(OverwriteOption {
        state,
        final_path,
        info,
        resp,
        tx,
        token,
    })
}
