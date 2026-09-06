//! Two-phase download entry point: probe the remote and the disk first, commit
//! to writing later.
//!
//! [`plan`] and [`plan_resume`] perform every step that has no lasting effect —
//! prefetching the remote metadata, resolving the output path, and inspecting
//! any `.fd`/`.part` pair already on disk — and hand back a [`DownloadPlan`]
//! describing what *would* happen. Nothing is created, truncated or renamed
//! until one of the `start*` methods is called, so a caller can show the plan to
//! a user, ask a question, and then either commit or drop the plan without
//! leaving anything behind.
//!
//! [`crate::download`] and [`crate::resume`] are the one-shot wrappers over this
//! pair: they plan and immediately start, reporting a [`PlanError`] as the
//! matching `*Error` event.
mod common;
mod fresh;
mod manifest;
mod mismatch;
mod resume;

use self::{
    common::{Fallback, PlanCommon},
    fresh::FreshPlan,
    manifest::ManifestFreshPlan,
    mismatch::{MismatchPlan, MismatchPolicy},
    resume::ResumePlan,
};
pub use self::{fresh::plan, manifest::plan_from_fd, resume::peek_resume, resume::plan_resume};
use super::try_load_resume_state;
use crate::{
    Config, DownloadState, Event, PartialConfig, StateError, TerminationReason, Tx, prefetch,
    utils::gen_path,
};
use fast_down::{ProgressEntry, Total, UrlInfo, reqwest::ReqwestResponseError};
use inherit_config::ConfigLayer;
use reqwest::Response;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Why a download plan could not be produced.
///
/// Every variant is fatal for the plan: no [`DownloadPlan`] exists, and nothing
/// has been written to disk. [`PlanError::emit`] maps each one onto the
/// equivalent [`Event`] for callers that consume the event stream rather than
/// the return value.
#[derive(Debug, thiserror::Error)]
#[allow(clippy::large_enum_variant)]
pub enum PlanError {
    /// The HTTP client could not be built (TLS / backend initialization, etc.).
    #[error("failed to build the HTTP client: {0}")]
    BuildClient(reqwest::Error),
    /// Prefetch exhausted its retry budget; this is the failure of the last
    /// attempt. Earlier attempts were reported as [`Event::PrefetchError`].
    #[error("failed to fetch the remote metadata: {0}")]
    Prefetch(ReqwestResponseError),
    /// The output path could not be computed (unwritable directory, a file name
    /// that is invalid on this platform, etc.).
    #[error("failed to compute the output path: {0}")]
    GenPath(std::io::Error),
    /// An explicit [`plan_resume`] could not continue the interrupted download.
    #[error(transparent)]
    Resume(StateError),
}

impl PlanError {
    /// Forward this error to the event channel as the matching `*Error` event.
    ///
    /// Used by the one-shot [`crate::download`] / [`crate::resume`] wrappers,
    /// whose callers only observe the event stream.
    pub fn emit(self, tx: &Tx) {
        let _ = match self {
            Self::BuildClient(e) => tx.send(Event::BuildClientError(e)),
            Self::Prefetch(e) => tx.send(Event::PrefetchError(e)),
            Self::GenPath(e) => tx.send(Event::GenPathError(e)),
            Self::Resume(e) => tx.send(Event::ResumeError(e)),
        };
    }
}

/// What the disk probe found for the download a [`DownloadPlan`] describes.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ResumeOutcome {
    /// Nothing to continue from: either no state was found, or resuming is
    /// disabled / unsupported by the server. Starting the plan downloads the
    /// whole file.
    Fresh,
    /// A `.fd`/`.part` pair was found and validates against the current remote
    /// file. Starting the plan continues from `progress`.
    Resumable {
        /// Path of the `.fd` state file backing the resume.
        config_path: PathBuf,
        /// Byte ranges already written to the `.part` file.
        progress: Vec<ProgressEntry>,
        /// Total bytes already on disk (the sum of the `progress` lengths).
        downloaded: u64,
    },
    /// A `.fd`/`.part` pair was found but cannot be continued — most often
    /// because the remote file changed since it was written.
    ///
    /// What [`DownloadPlan::start`] does depends on which entry point produced
    /// the plan. From [`plan`] it falls back to a fresh download (overwriting
    /// the stale `.part`, or claiming a new name when `overwrite` is disabled).
    /// From [`plan_resume`] the caller asked for a resume specifically, so it
    /// refuses instead: [`Event::ResumeError`] and
    /// [`TerminationReason::Failed`], leaving the `.part` and `.fd` untouched.
    ///
    /// Either way [`DownloadPlan::start_forced_resume`] continues from the
    /// stale state, but only when the mismatch is limited to the identity
    /// headers.
    Mismatch(StateError),
}

enum PlanKind {
    Fresh(FreshPlan),
    Resume(ResumePlan),
    ManifestFresh(ManifestFreshPlan),
    Mismatch(MismatchPlan),
}

impl PlanKind {
    const fn common(&self) -> &PlanCommon {
        match self {
            Self::Fresh(plan) => plan.common(),
            Self::Resume(plan) => plan.common(),
            Self::ManifestFresh(plan) => plan.common(),
            Self::Mismatch(plan) => plan.common(),
        }
    }

    const fn outcome(&self) -> &ResumeOutcome {
        match self {
            Self::Fresh(plan) => plan.outcome(),
            Self::Resume(plan) => plan.outcome(),
            Self::ManifestFresh(plan) => plan.outcome(),
            Self::Mismatch(plan) => plan.outcome(),
        }
    }

    fn into_common(self) -> PlanCommon {
        match self {
            Self::Fresh(plan) => plan.into_common(),
            Self::Resume(plan) => plan.into_common(),
            Self::ManifestFresh(plan) => plan.into_common(),
            Self::Mismatch(plan) => plan.into_common(),
        }
    }

    async fn run(self) -> TerminationReason {
        match self {
            Self::Fresh(plan) => Box::pin(plan.run()).await,
            Self::Resume(plan) => Box::pin(plan.run()).await,
            Self::ManifestFresh(plan) => Box::pin(plan.run()).await,
            Self::Mismatch(plan) => Box::pin(plan.run()).await,
        }
    }

    async fn run_forced(self) -> TerminationReason {
        match self {
            Self::Mismatch(plan) => Box::pin(plan.run_forced()).await,
            other => Box::pin(other.run()).await,
        }
    }

    /// Discard the variant and download the whole file from byte zero,
    /// ignoring any resumable state on disk.
    async fn run_fresh(self) -> TerminationReason {
        self.into_common().claim_and_run().await
    }
}

/// A prepared, not-yet-started download.
///
/// Produced by [`plan`] / [`plan_resume`] after the remote has been prefetched
/// and the disk inspected. Holding one keeps the prefetch [`Response`] open so
/// the first range request can reuse it; dropping one abandons the download
/// without having touched the filesystem.
#[must_use = "a DownloadPlan does nothing until one of the start methods is called; drop it to abandon the download"]
pub struct DownloadPlan {
    inner: PlanKind,
}

impl DownloadPlan {
    /// Metadata resolved for the remote file: size, identity headers, and
    /// whether the server supports ranged (and therefore parallel) requests.
    #[must_use]
    pub const fn info(&self) -> &UrlInfo {
        &self.inner.common().info
    }

    /// The fully-resolved configuration this plan will run with.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.inner.common().config
    }

    /// Where the finished file is intended to land.
    ///
    /// When `overwrite` is disabled the actual destination is uniquified at the
    /// very end, so the file may land on a `name (1).ext` variant instead;
    /// [`Event::Renamed`] carries the path it really landed on.
    #[must_use]
    pub fn final_path(&self) -> &Path {
        &self.inner.common().final_path
    }

    /// The `.part` file this plan inspected.
    ///
    /// For [`ResumeOutcome::Resumable`] and [`ResumeOutcome::Mismatch`] this is
    /// the existing file that was examined. For a normal [`ResumeOutcome::Fresh`]
    /// it is the name that was free at probe time — because planning claims
    /// nothing, a concurrent download can take it first, in which case starting
    /// the plan moves on to the next free name. A manifest-driven Fresh plan
    /// instead reports the `.part` paired with that manifest.
    #[must_use]
    pub fn tmp_path(&self) -> &Path {
        &self.inner.common().tmp_path
    }

    /// The `.fd` state file paired with [`DownloadPlan::tmp_path`].
    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.inner.common().cfg_path
    }

    /// What the disk probe found, and therefore what starting the plan will do.
    #[must_use]
    pub const fn resume_outcome(&self) -> &ResumeOutcome {
        self.inner.outcome()
    }

    /// Run the plan as probed.
    ///
    /// [`ResumeOutcome::Resumable`] continues from the saved progress and
    /// [`ResumeOutcome::Fresh`] downloads the whole file. A
    /// [`ResumeOutcome::Mismatch`] falls back to a fresh download, except for a
    /// plan made by [`plan_resume`], where it is reported as
    /// [`Event::ResumeError`] and ends the run — that caller asked to continue
    /// one specific file, not to fetch it again.
    ///
    /// Emits exactly one [`Event::Terminated`] as the last event on the channel.
    pub async fn start(self) {
        let tx = self.inner.common().tx.clone();
        let reason = self.inner.run().await;
        let _ = tx.send(Event::Terminated(reason));
    }

    /// Ignore any resumable state and download the whole file again.
    ///
    /// With `overwrite` enabled the existing `.part` is reused as scratch space
    /// and every byte is rewritten; otherwise a new, unclaimed `.part` name is
    /// taken so the existing one is left untouched.
    ///
    /// Emits exactly one [`Event::Terminated`] as the last event on the channel.
    pub async fn start_fresh(self) {
        let tx = self.inner.common().tx.clone();
        let reason = self.inner.run_fresh().await;
        let _ = tx.send(Event::Terminated(reason));
    }

    /// Continue from a [`ResumeOutcome::Mismatch`] state despite the failed
    /// validation.
    ///
    /// This is only safe when the byte layout is unchanged, so the forced
    /// resume is refused unless the mismatch is limited to the identity headers
    /// (etag / last-modified) and both sides agree on the file size. A size
    /// change means the recorded ranges describe different bytes, and reusing
    /// them would splice two versions of the file together.
    ///
    /// The `.part` is also measured against the recorded progress: a file
    /// shorter than the highest recorded offset is rejected with
    /// [`StateError::Truncated`], because the sink would extend it with zeros
    /// over the missing span and never fetch those bytes.
    ///
    /// A refusal is reported as [`Event::ResumeError`] followed by
    /// [`TerminationReason::Failed`]. Called on a plan that is not a mismatch,
    /// this behaves exactly like [`DownloadPlan::start`].
    ///
    /// Emits exactly one [`Event::Terminated`] as the last event on the channel.
    pub async fn start_forced_resume(self) {
        let tx = self.inner.common().tx.clone();
        let reason = self.inner.run_forced().await;
        let _ = tx.send(Event::Terminated(reason));
    }
}

/// Turn a validated state into the public [`ResumeOutcome::Resumable`] view.
fn resumable_outcome(state: &DownloadState, config_path: &Path) -> ResumeOutcome {
    let progress = state.get_progress();
    let downloaded = progress.total();
    ResumeOutcome::Resumable {
        config_path: config_path.to_path_buf(),
        progress,
        downloaded,
    }
}

/// The common, side-effect-free front half of every plan: resolve the config,
/// prefetch the remote metadata, and compute the output path.
struct Probe {
    config: Config,
    info: UrlInfo,
    resp: Response,
    final_path: PathBuf,
}

#[allow(clippy::result_large_err)]
async fn probe(url: &Url, partial_config: &PartialConfig, tx: &Tx) -> Result<Probe, PlanError> {
    let config = partial_config.clone().build();
    let (info, resp) = prefetch(url, &config, tx).await?;
    let final_path = gen_path(url, &info, &config)
        .await
        .map_err(PlanError::GenPath)?;
    Ok(Probe {
        config,
        info,
        resp,
        final_path,
    })
}

/// Probe the `.fd`/`.part` pair at `cfg_path`/`tmp_path` and map the outcome onto
/// the public [`ResumeOutcome`] view.
async fn probe_resume(
    url: &Url,
    cfg_path: &Path,
    tmp_path: &Path,
    info: &UrlInfo,
    partial_config: &PartialConfig,
) -> (Option<DownloadState>, ResumeOutcome) {
    match try_load_resume_state(url, cfg_path, tmp_path, info, partial_config).await {
        Ok(Some(state)) => {
            let outcome = resumable_outcome(&state, cfg_path);
            (Some(state), outcome)
        }
        Ok(None) => (None, ResumeOutcome::Fresh),
        Err(e) => (None, ResumeOutcome::Mismatch(e)),
    }
}

/// The caller's URL when given, otherwise the durable initial URL recorded in
/// the `.fd` state.
fn state_url(url: Option<Url>, loaded: &DownloadState) -> Option<Url> {
    url.or_else(|| {
        let guard = loaded.lock_inner();
        if let Some(url) = &guard.url
            && matches!(url.scheme(), "http" | "https")
        {
            Some(url.clone())
        } else {
            None
        }
    })
}

/// Layer caller overrides over the persisted configuration before prefetching
/// or computing paths. Persisted byte progress is intentionally excluded here:
/// it is restored only after the current `.fd`/`.part` pair validates.
fn inherit_persisted_config(config: &mut PartialConfig, loaded: &DownloadState) {
    let requested_progress = config.downloaded_chunk.take();
    let saved = loaded.lock_inner().config.clone();
    if let Some(saved) = saved {
        config.inherit_from(&saved);
    }
    config.downloaded_chunk = requested_progress;
}

fn resolved_plan_config(probe_config: Config, state: Option<&DownloadState>) -> Config {
    let Some(state) = state else {
        return probe_config;
    };
    state
        .lock_inner()
        .config
        .clone()
        .map_or(probe_config, ConfigLayer::build)
}

#[allow(clippy::too_many_arguments)]
fn plan_common(
    url: Url,
    partial_config: PartialConfig,
    probe: Probe,
    cfg_path: PathBuf,
    tmp_path: PathBuf,
    tx: Tx,
    token: CancellationToken,
    state: Option<&DownloadState>,
) -> PlanCommon {
    let Probe {
        config: probe_config,
        info,
        resp,
        final_path,
    } = probe;
    PlanCommon {
        url,
        partial_config,
        config: resolved_plan_config(probe_config, state),
        info,
        resp,
        final_path,
        cfg_path,
        tmp_path,
        tx,
        token,
    }
}

#[cfg(test)]
mod tests;
