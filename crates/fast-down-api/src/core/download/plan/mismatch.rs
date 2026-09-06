use super::{PlanCommon, ResumeOutcome};
use crate::{Event, StateError, TerminationReason};
use std::io;
use tokio::fs;

#[derive(Clone, Copy)]
pub(super) enum MismatchPolicy {
    Fresh,
    Fail,
}

pub(super) struct MismatchPlan {
    common: PlanCommon,
    outcome: ResumeOutcome,
    policy: MismatchPolicy,
}

impl MismatchPlan {
    pub(super) const fn new(common: PlanCommon, error: StateError, policy: MismatchPolicy) -> Self {
        Self {
            common,
            outcome: ResumeOutcome::Mismatch(error),
            policy,
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
        match self.policy {
            MismatchPolicy::Fresh => self.common.claim_and_run().await,
            MismatchPolicy::Fail => {
                let ResumeOutcome::Mismatch(error) = self.outcome else {
                    unreachable!("MismatchPlan must contain a mismatch outcome")
                };
                let _ = self.common.tx.send(Event::ResumeError(error));
                TerminationReason::Failed
            }
        }
    }

    pub(super) async fn run_forced(self) -> TerminationReason {
        let ResumeOutcome::Mismatch(planned_error) = self.outcome else {
            unreachable!("MismatchPlan must contain a mismatch outcome")
        };
        if !matches!(
            &planned_error,
            StateError::FileChanged {
                local_file_size,
                remote_file_size,
                ..
            } if local_file_size == remote_file_size
        ) {
            let _ = self.common.tx.send(Event::ResumeError(planned_error));
            return TerminationReason::Failed;
        }

        let state = match crate::DownloadState::load(&self.common.cfg_path).await {
            Ok(state) => state,
            Err(e) => {
                let _ = self.common.tx.send(Event::ResumeError(e));
                return TerminationReason::Failed;
            }
        };
        match state.validate(&self.common.info) {
            Ok(()) => {}
            Err(StateError::FileChanged {
                local_file_size,
                remote_file_size,
                ..
            }) if local_file_size == remote_file_size => {}
            Err(e) => {
                let _ = self.common.tx.send(Event::ResumeError(e));
                return TerminationReason::Failed;
            }
        }
        if !fs::try_exists(&self.common.tmp_path).await.unwrap_or(false) {
            let _ = self
                .common
                .tx
                .send(Event::ResumeError(StateError::Open(io::Error::new(
                    io::ErrorKind::NotFound,
                    "the .part file disappeared after the plan was created",
                ))));
            return TerminationReason::Failed;
        }
        state.merge_config(&self.common.partial_config);
        if let Some((actual_size, recorded_size)) =
            state.part_shortfall(&self.common.tmp_path).await
        {
            let _ = self
                .common
                .tx
                .send(Event::ResumeError(StateError::Truncated {
                    actual_size,
                    recorded_size,
                }));
            return TerminationReason::Failed;
        }
        state.refresh_identity(&self.common.url, &self.common.info);
        self.common.run_existing(state, true).await
    }
}
