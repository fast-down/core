use super::super::{
    open_create, open_create_new,
    overwrite::{OverwriteOption, overwrite},
};
use crate::{
    Config, DownloadState, Event, PartialConfig, StateError, TerminationReason, Tx, tx_err,
};
use fast_down::UrlInfo;
use path_helper::IterStemExt;
use reqwest::Response;
use std::io;
use std::path::PathBuf;
use tokio::fs;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone, Copy)]
pub(super) enum Fallback {
    Fresh,
    ManifestFresh,
    Fail,
}

/// Data shared by every planned action.
pub(super) struct PlanCommon {
    pub(super) url: Url,
    pub(super) partial_config: PartialConfig,
    pub(super) config: Config,
    pub(super) info: UrlInfo,
    pub(super) resp: Response,
    pub(super) final_path: PathBuf,
    pub(super) cfg_path: PathBuf,
    pub(super) tmp_path: PathBuf,
    pub(super) tx: Tx,
    pub(super) token: CancellationToken,
}

impl PlanCommon {
    pub(super) async fn ensure_parent(&self) -> bool {
        if let Some(parent) = self.final_path.parent()
            && let Err(e) = fs::create_dir_all(parent).await
        {
            let _ = self.tx.send(Event::BuildPusherError(e));
            return false;
        }
        true
    }

    fn emit_resumed(&self, state: &DownloadState) {
        let _ = self.tx.send(Event::Resumed {
            config_path: self.cfg_path.clone(),
            progress: state.get_progress(),
            size: self.info.size,
        });
    }

    async fn run_overwrite(self, state: DownloadState) -> TerminationReason {
        overwrite(OverwriteOption {
            state,
            final_path: self.final_path,
            info: self.info,
            resp: self.resp,
            tx: self.tx,
            token: self.token,
        })
        .await
    }

    pub(super) async fn run_existing(
        self,
        state: DownloadState,
        emit_resumed: bool,
    ) -> TerminationReason {
        if !self.ensure_parent().await {
            return TerminationReason::Failed;
        }
        if emit_resumed {
            self.emit_resumed(&state);
        }
        self.run_overwrite(state).await
    }

    /// Atomically claim a fresh `.part`/`.fd` pair and start from byte zero.
    pub(super) async fn claim_and_run(mut self) -> TerminationReason {
        self.partial_config.downloaded_chunk = None;
        if !self.ensure_parent().await {
            return TerminationReason::Failed;
        }

        if self.config.overwrite {
            tx_err!(
                open_create().open(&self.tmp_path).await,
                self.tx,
                BuildPusherError,
                TerminationReason::Failed
            );
            let state =
                DownloadState::new(&self.url, &self.info, &self.partial_config, &self.cfg_path);
            return self.run_overwrite(state).await;
        }

        for base_path in self.final_path.iter_stem() {
            let tmp_path = base_path.with_added_extension("part");
            let cfg_path = base_path.with_added_extension("fd");
            match open_create_new().open(&tmp_path).await {
                Ok(_) => match open_create_new().open(&cfg_path).await {
                    Ok(_) => {
                        let state = DownloadState::new(
                            &self.url,
                            &self.info,
                            &self.partial_config,
                            &cfg_path,
                        );
                        return self.run_overwrite(state).await;
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        let _ = fs::remove_file(&tmp_path).await;
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&tmp_path).await;
                        let _ = self.tx.send(Event::StateSaveError(StateError::Save(e)));
                        return TerminationReason::Failed;
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    let _ = self.tx.send(Event::BuildPusherError(e));
                    return TerminationReason::Failed;
                }
            }
        }
        unreachable!()
    }
}
