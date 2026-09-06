use super::{
    DownloadPlan, Fallback, FreshPlan, ManifestFreshPlan, MismatchPlan, MismatchPolicy, PlanCommon,
    PlanError, PlanKind, ResumeOutcome, inherit_persisted_config, plan_common, probe, probe_resume,
    resumable_outcome, state_url,
};
use crate::{DownloadState, Event, PartialConfig, StateError, TerminationReason, Tx};
use std::path::Path;
use tokio::fs;
use tokio_util::sync::CancellationToken;
use url::Url;

pub(super) struct ResumePlan {
    common: PlanCommon,
    outcome: ResumeOutcome,
    fallback: Fallback,
}

impl ResumePlan {
    pub(super) fn new(
        common: PlanCommon,
        state: &crate::DownloadState,
        fallback: Fallback,
    ) -> Self {
        let outcome = resumable_outcome(state, &common.cfg_path);
        Self {
            common,
            outcome,
            fallback,
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
        match super::super::try_load_resume_state(
            &self.common.url,
            &self.common.cfg_path,
            &self.common.tmp_path,
            &self.common.info,
            &self.common.partial_config,
        )
        .await
        {
            Ok(Some(state)) => self.common.run_existing(state, true).await,
            Ok(None) => Box::pin(self.run_fallback(None)).await,
            Err(error) => Box::pin(self.run_fallback(Some(error))).await,
        }
    }

    async fn run_fallback(self, error: Option<crate::StateError>) -> TerminationReason {
        match self.fallback {
            Fallback::Fresh => self.common.claim_and_run().await,
            Fallback::ManifestFresh => ManifestFreshPlan::new(self.common).run().await,
            Fallback::Fail => match error {
                Some(error) => {
                    let _ = self.common.tx.send(Event::ResumeError(error));
                    TerminationReason::Failed
                }
                // `Ok(None)` means the `.part` disappeared or became too short;
                // explicit resume preserves the established fresh fallback.
                None => self.common.claim_and_run().await,
            },
        }
    }
}

/// Prepare a resume of an interrupted download from its `.part` file.
///
/// `url` is optional: when omitted the durable initial URL recorded in the `.fd`
/// state is used, so a caller can resume from the `.part` path alone. Redirects
/// are always re-resolved through a fresh prefetch.
///
/// Unlike [`super::plan`], a state that exists but no longer describes the
/// remote file is not silently replaced by a full download. It comes back as
/// [`ResumeOutcome::Mismatch`], and starting such a plan reports
/// [`Event::ResumeError`] and stops — the caller asked to continue one specific
/// file, so it decides whether to force the resume, download afresh, or give up.
///
/// A `tmp_path` that does not exist at all is the one case that does defer to
/// [`super::plan`]: there is no partial file to continue, so with a `url`
/// available this is just a download.
///
/// # Errors
///
/// Returns [`PlanError::Resume`] when `tmp_path` is not a `.part` file, no URL
/// can be resolved, the `.fd` state cannot be read, or the server does not
/// support ranged requests. The [`super::plan`] errors apply as well.
#[allow(clippy::result_large_err)]
pub async fn plan_resume(
    tmp_path: impl AsRef<Path>,
    url: Option<Url>,
    mut partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) -> Result<DownloadPlan, PlanError> {
    let tmp_path = tmp_path.as_ref();
    if tmp_path.extension() != Some(std::ffi::OsStr::new("part")) {
        return Err(PlanError::Resume(StateError::Open(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tmp_path must end with .part extension",
        ))));
    }
    partial_config.overwrite = Some(false);

    if !fs::try_exists(tmp_path).await.unwrap_or(false) {
        let Some(url) = url else {
            return Err(PlanError::Resume(StateError::NoUrl(tmp_path.to_path_buf())));
        };
        partial_config.resume = Some(false);
        return super::fresh::plan(url, partial_config, tx, token).await;
    }

    let cfg_path = tmp_path.with_extension("fd");
    let loaded = DownloadState::load(&cfg_path)
        .await
        .map_err(PlanError::Resume)?;
    let Some(url) = state_url(url, &loaded) else {
        return Err(PlanError::Resume(StateError::NoUrl(tmp_path.to_path_buf())));
    };
    inherit_persisted_config(&mut partial_config, &loaded);
    partial_config.overwrite = Some(false);
    partial_config.resume = Some(true);
    drop(loaded);

    let probe = probe(&url, &partial_config, &tx).await?;
    if !probe.info.fast_download {
        return Err(PlanError::Resume(StateError::NotResumable(
            probe.info, probe.resp,
        )));
    }

    let (state, outcome) =
        probe_resume(&url, &cfg_path, tmp_path, &probe.info, &partial_config).await;
    let tmp_path = tmp_path.to_path_buf();
    let inner = match (state, outcome) {
        (Some(state), _) => {
            let common = plan_common(
                url,
                partial_config,
                probe,
                cfg_path,
                tmp_path,
                tx,
                token,
                Some(&state),
            );
            PlanKind::Resume(ResumePlan::new(common, &state, Fallback::Fail))
        }
        (None, ResumeOutcome::Mismatch(error)) => {
            let common = plan_common(
                url,
                partial_config,
                probe,
                cfg_path,
                tmp_path,
                tx,
                token,
                None,
            );
            PlanKind::Mismatch(MismatchPlan::new(common, error, MismatchPolicy::Fail))
        }
        (None, _) => {
            let common = plan_common(
                url,
                partial_config,
                probe,
                cfg_path,
                tmp_path,
                tx,
                token,
                None,
            );
            PlanKind::Fresh(FreshPlan::new(common))
        }
    };

    Ok(DownloadPlan { inner })
}

/// Read a previously-saved download state from disk without contacting the
/// network. The returned state is not validated against the current remote.
///
/// # Errors
/// Returns [`StateError::NotAPartFile`] when `tmp_path` is not a `.part` file,
/// or [`StateError::Open`] when its companion `.fd` cannot be read or decoded.
#[allow(clippy::result_large_err)]
pub async fn peek_resume(tmp_path: impl AsRef<Path>) -> Result<DownloadState, StateError> {
    let tmp_path = tmp_path.as_ref();
    if tmp_path.extension() != Some(std::ffi::OsStr::new("part")) {
        return Err(StateError::NotAPartFile(tmp_path.to_path_buf()));
    }
    DownloadState::load(&tmp_path.with_extension("fd")).await
}
