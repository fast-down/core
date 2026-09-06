use super::{
    DownloadPlan, Fallback, MismatchPlan, MismatchPolicy, PlanCommon, PlanError, PlanKind,
    ResumeOutcome, ResumePlan, plan_common, probe, probe_resume,
};
use crate::{PartialConfig, StateError, TerminationReason, Tx};
use path_helper::IterStemExt;
use std::path::PathBuf;
use tokio::fs;
use tokio_util::sync::CancellationToken;
use url::Url;

pub(super) struct FreshPlan {
    common: PlanCommon,
    outcome: ResumeOutcome,
}

impl FreshPlan {
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
        self.common.claim_and_run().await
    }
}

/// Prepare a download without touching the filesystem.
///
/// Prefetches the remote metadata, resolves the output path, and inspects the
/// `.fd`/`.part` pair that a previous run may have left behind. The returned
/// [`DownloadPlan`] reports what starting it would do; nothing is created until
/// it is started.
///
/// When `overwrite` is disabled the probe walks the `name`, `name (1)`,
/// `name (2)` … sequence looking for a resumable pair, and settles on the first
/// unclaimed name. A pair whose state exists but fails validation is reported as
/// [`ResumeOutcome::Mismatch`] instead of being skipped silently, so the caller
/// can offer to force the resume.
///
/// # Errors
///
/// Returns [`PlanError`] when the client cannot be built, the prefetch exhausts
/// its retries, or the output path cannot be computed.
#[allow(clippy::result_large_err)]
pub async fn plan(
    url: Url,
    mut partial_config: PartialConfig,
    tx: Tx,
    token: CancellationToken,
) -> Result<DownloadPlan, PlanError> {
    partial_config.downloaded_chunk = None;
    let probe = probe(&url, &partial_config, &tx).await?;
    let can_resume = probe.config.resume && probe.info.fast_download;

    if probe.config.overwrite {
        plan_overwrite(url, partial_config, probe, tx, token, can_resume).await
    } else {
        plan_without_overwrite(url, partial_config, probe, tx, token, can_resume).await
    }
}

#[allow(clippy::result_large_err)]
async fn plan_overwrite(
    url: Url,
    partial_config: PartialConfig,
    probe: super::Probe,
    tx: Tx,
    token: CancellationToken,
    can_resume: bool,
) -> Result<DownloadPlan, PlanError> {
    let cfg_path = probe.final_path.with_added_extension("fd");
    let tmp_path = probe.final_path.with_added_extension("part");
    let resume_probe = if can_resume {
        probe_resume(&url, &cfg_path, &tmp_path, &probe.info, &partial_config).await
    } else {
        (None, ResumeOutcome::Fresh)
    };
    let inner = match resume_probe {
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
            PlanKind::Resume(ResumePlan::new(common, &state, Fallback::Fresh))
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
            PlanKind::Mismatch(MismatchPlan::new(common, error, MismatchPolicy::Fresh))
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

#[allow(clippy::result_large_err)]
async fn plan_without_overwrite(
    url: Url,
    partial_config: PartialConfig,
    probe: super::Probe,
    tx: Tx,
    token: CancellationToken,
    can_resume: bool,
) -> Result<DownloadPlan, PlanError> {
    let mut mismatch: Option<(StateError, PathBuf, PathBuf)> = None;
    for base_path in probe.final_path.iter_stem() {
        let tmp_path = base_path.with_added_extension("part");
        let cfg_path = base_path.with_added_extension("fd");

        let tmp_exists = fs::try_exists(&tmp_path).await.unwrap_or(false);
        let cfg_exists = fs::try_exists(&cfg_path).await.unwrap_or(false);
        if !tmp_exists && !cfg_exists {
            // The first unclaimed name. If an earlier stem held a state that
            // failed validation, report that instead: the caller may prefer to
            // force it rather than download the file again under a new name.
            let inner = if let Some((error, cfg_path, tmp_path)) = mismatch {
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
                PlanKind::Mismatch(MismatchPlan::new(common, error, MismatchPolicy::Fresh))
            } else {
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
            };
            return Ok(DownloadPlan { inner });
        }

        if can_resume {
            match super::super::try_load_resume_state(
                &url,
                &cfg_path,
                &tmp_path,
                &probe.info,
                &partial_config,
            )
            .await
            {
                Ok(Some(state)) => {
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
                    return Ok(DownloadPlan {
                        inner: PlanKind::Resume(ResumePlan::new(common, &state, Fallback::Fresh)),
                    });
                }
                Err(e) if mismatch.is_none() => mismatch = Some((e, cfg_path, tmp_path)),
                Ok(None) | Err(_) => {}
            }
        }
    }
    unreachable!()
}
