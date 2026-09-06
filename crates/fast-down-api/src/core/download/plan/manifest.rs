use super::super::open_create;
use super::{
    DownloadPlan, Fallback, PlanCommon, PlanError, PlanKind, ResumeOutcome, ResumePlan,
    inherit_persisted_config, plan_common, probe, state_url,
};
use crate::{DownloadState, Event, PartialConfig, StateError, TerminationReason, Tx, tx_err};
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

pub(super) struct ManifestFreshPlan {
    common: PlanCommon,
    outcome: ResumeOutcome,
}

impl ManifestFreshPlan {
    pub(super) const fn new(common: PlanCommon) -> Self {
        Self {
            common,
            outcome: ResumeOutcome::Fresh,
        }
    }

    pub(super) const fn common(&self) -> &PlanCommon {
        &self.common
    }

    pub(super) const fn outcome(&self) -> &ResumeOutcome {
        &self.outcome
    }

    pub(super) fn into_common(self) -> PlanCommon {
        self.common
    }

    pub(super) async fn run(self) -> TerminationReason {
        let state = match DownloadState::load(&self.common.cfg_path).await {
            Ok(state) => state,
            Err(e) => {
                let _ = self.common.tx.send(Event::ResumeError(e));
                return TerminationReason::Failed;
            }
        };
        let state = fresh_from_loaded(
            state,
            &self.common.partial_config,
            &self.common.url,
            &self.common.info,
        );
        if !self.common.ensure_parent().await {
            return TerminationReason::Failed;
        }
        tx_err!(
            open_create().open(&self.common.tmp_path).await,
            self.common.tx,
            BuildPusherError,
            TerminationReason::Failed
        );
        self.common.run_existing(state, false).await
    }
}

fn fresh_from_loaded(
    state: DownloadState,
    partial_config: &crate::PartialConfig,
    url: &url::Url,
    info: &fast_down::UrlInfo,
) -> DownloadState {
    state.update(|inner| {
        if let Some(c) = &mut inner.config {
            c.downloaded_chunk = None;
        }
        inner.elapsed = Some(Duration::ZERO);
    });
    let mut fresh_config = partial_config.clone();
    fresh_config.downloaded_chunk = None;
    state.merge_config(&fresh_config);
    state.refresh_identity(url, info);
    state
}

/// Prepare a download driven by a `.fd` state file used as a download manifest.
///
/// Unlike [`super::plan_resume`], which is given the `.part` and looks for its
/// companion `.fd`, this entry takes the `.fd` itself. The sibling `.part`
/// decides whether the run resumes recorded progress or reuses the manifest
/// configuration for a fresh download.
///
/// `url` is optional: when omitted, the durable initial URL recorded in the
/// `.fd` is used. Resume requires a range-capable server; without one the
/// manifest is still reused for a fresh, single-stream download.
///
/// # Errors
///
/// Returns [`PlanError::Resume`] when `fd_path` is not a `.fd` file, the `.fd`
/// cannot be read or decoded, no URL can be resolved, or the prefetch fails.
#[allow(clippy::result_large_err)]
pub async fn plan_from_fd(
    fd_path: impl AsRef<Path>,
    url: Option<Url>,
    mut partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) -> Result<DownloadPlan, PlanError> {
    let fd_path = fd_path.as_ref();
    if fd_path.extension() != Some(OsStr::new("fd")) {
        return Err(PlanError::Resume(StateError::Open(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd_path must end with .fd extension",
        ))));
    }
    partial_config.overwrite = Some(false);

    let loaded = DownloadState::load(fd_path)
        .await
        .map_err(PlanError::Resume)?;
    let Some(url) = state_url(url, &loaded) else {
        return Err(PlanError::Resume(StateError::NoUrl(fd_path.to_path_buf())));
    };

    inherit_persisted_config(&mut partial_config, &loaded);
    partial_config.overwrite = Some(false);
    partial_config.resume = Some(true);
    let probe = probe(&url, &partial_config, &tx).await?;

    let tmp_path = fd_path.with_extension("part");
    let state = if probe.info.fast_download {
        match super::super::try_load_resume_state(
            &url,
            fd_path,
            &tmp_path,
            &probe.info,
            &partial_config,
        )
        .await
        {
            Ok(Some(state)) => Some(state),
            _ => None,
        }
    } else {
        None
    };
    let common = plan_common(
        url,
        partial_config,
        probe,
        fd_path.to_path_buf(),
        tmp_path,
        tx,
        token,
        state.as_ref(),
    );
    let inner = match state {
        Some(state) => PlanKind::Resume(ResumePlan::new(common, &state, Fallback::ManifestFresh)),
        None => PlanKind::ManifestFresh(ManifestFreshPlan::new(common)),
    };
    Ok(DownloadPlan { inner })
}
