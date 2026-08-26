//! Bamep Domain: the final non-destructive Agent -> Server data-plane
//! transfer dispatch decision (Issue #40 "[WP] Commit non-destructive
//! transfer Attempts for dispatch").
//!
//! This module is structurally separate from [`crate::final_dispatch`]
//! (the destructive dispatch gate, Issue #25):
//! [`TransferDispatchInputs`] carries no target-fingerprint, trusted-
//! bootstrap, hardware-confidence, or credential/presence evidence at all —
//! not merely unused fields, but fields that do not exist on this type — so
//! [`evaluate_transfer_dispatch`] cannot silently call, copy, or weaken the
//! seven-item destructive-operation gate
//! (`m0-endpoint-identity-lifecycle.md`). This is the RF-005
//! `bamep.m1.data-plane-transfer` Agent -> Server dispatch path, which
//! `m1-simulated-vertical-slice-and-baseline-validation.md` classifies
//! non-destructive: it therefore uses only generic workflow/scheduler
//! authorization (`m0-job-lifecycle-and-scheduling.md` "Workflow/scheduler
//! authorization") plus the Transfer/Attempt-binding correlation Issue #36
//! introduced — never the destructive gate.
//!
//! This function performs no I/O: every fact it needs — durable Job/JobStep
//! state, whether an unresolved prior Attempt already exists, and the
//! durable pre-dispatch `Transfer` — is resolved by the caller
//! (`bamep_server::application::TransferDispatchService`) before this
//! function is invoked, following the same `lock -> freshly read -> Domain
//! decision -> persist -> commit` pattern `final_dispatch` already
//! established.
//!
//! On success this function returns exactly one fresh [`Attempt`] in
//! [`AttemptState::Dispatched`], the candidate `JobStep` advanced to
//! `Dispatching`, and the given `Transfer` bound to that exact Attempt
//! (`crate::transfer::bind_attempt`) — never a replacement `TransferId` or
//! `ArtifactId`. On failure, this Domain function — not its caller — owns
//! the exact resulting durable JobStep, mirroring `final_dispatch`'s
//! identical contract: a final-revalidation failure under an authoritative
//! `PreconditionsSatisfied` JobStep carries that JobStep explicitly in
//! `Pending` ([`TransferDispatchDenial::pending_job_step`]); a structural
//! mismatch carries `None`, because there is nothing to revert.
//!
//! This module never sends `ActionDispatch` and never touches PostgreSQL.

use crate::attempt::{Attempt, AttemptId, AttemptState};
use crate::job::{Job, JobState, JobStep, JobStepId, JobStepState};
use crate::transfer::{bind_attempt, Transfer, TransferBindingError};

/// Every already-resolved fact [`evaluate_transfer_dispatch`] needs. The
/// caller is responsible for resolving every field from durable state at
/// decision time. Deliberately excludes every destructive-only evidence
/// field `final_dispatch::FinalDispatchInputs` carries (target fingerprint,
/// trusted-bootstrap state, hardware confidence, credential/presence) — see
/// module docs.
#[derive(Debug, Clone)]
pub struct TransferDispatchInputs {
    /// The owning Job, including every ordered `JobStep`, freshly read under
    /// lock immediately before this decision.
    pub job: Job,
    /// The candidate JobStep's identity within `job`.
    pub step_id: JobStepId,
    /// Whether an Attempt already exists for this JobStep in a non-terminal
    /// state (`Dispatched`, `InProgress`, or `AwaitingReconciliation`) —
    /// workflow/scheduler authorization item 5
    /// (`m0-job-lifecycle-and-scheduling.md`).
    pub existing_active_attempt: bool,
    /// The durable pre-dispatch `Transfer` (Issue #36), freshly read under
    /// lock immediately before this decision. Must correlate exactly to
    /// `job`/`step_id` and must not already be bound to another Attempt.
    pub transfer: Transfer,
}

/// Successful commitment produced by [`evaluate_transfer_dispatch`]: the
/// candidate JobStep advanced to `Dispatching`, exactly one fresh `Attempt`
/// in `Dispatched`, and `transfer` bound to that exact Attempt — the same
/// `TransferId`/`ArtifactId` as the input, never regenerated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferDispatchOutcome {
    pub job_step: JobStep,
    pub attempt: Attempt,
    pub transfer: Transfer,
}

/// Every independently identifiable reason [`evaluate_transfer_dispatch`]
/// may reject a candidate JobStep. Always carried inside a
/// [`TransferDispatchDenial`], which also states the exact durable effect
/// (if any) this rejection requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransferDispatchRejection {
    /// `step_id` does not identify a JobStep belonging to `job`. Structural:
    /// nothing to revert.
    #[error("job step not found in job")]
    StepNotFound,
    /// The candidate JobStep is not currently `PreconditionsSatisfied`.
    /// Structural: nothing to revert.
    #[error("job step is not PreconditionsSatisfied")]
    NotPreconditionsSatisfied,
    /// The candidate JobStep carries a `DestructiveIntent`
    /// (`crate::job::DestructiveIntent`) — it is structurally classified as
    /// the destructive M1 workflow path (Issue #31) and must never be
    /// silently reinterpreted as this non-destructive transfer action.
    /// Structural: nothing to revert.
    #[error("job step is structurally classified as the destructive path")]
    StepIsDestructive,
    /// `transfer` does not belong to the exact `job_id`/`step_id`/
    /// `endpoint_id` context under evaluation. Structural: nothing to
    /// revert — the wrong Transfer was presented for this JobStep, not a
    /// timing-sensitive revalidation failure.
    #[error("transfer does not correlate to this job/job step/endpoint context")]
    TransferCorrelationMismatch,
    /// The owning Job is not `Running` (workflow/scheduler authorization
    /// item 1). A final-revalidation failure under an authoritative
    /// `PreconditionsSatisfied` JobStep.
    #[error("job is not Running")]
    JobNotRunning,
    /// The candidate is not the structurally current active step: an
    /// earlier ordered JobStep is not yet `Succeeded` (workflow/scheduler
    /// authorization item 2).
    #[error("job step is not the current active step")]
    NotCurrentStep,
    /// An unresolved prior Attempt already exists for this JobStep
    /// (workflow/scheduler authorization item 5).
    #[error("an active or unresolved attempt already exists for this job step")]
    ExistingActiveAttempt,
    /// `transfer` is already bound to a different Attempt
    /// (`crate::transfer::TransferBindingError::ConflictingRebind`) — this
    /// initial-dispatch path may never rebind an already-dispatched
    /// Transfer.
    #[error("transfer is already bound to a different attempt")]
    TransferAlreadyBound,
}

/// The complete failure outcome of [`evaluate_transfer_dispatch`]: the typed
/// [`TransferDispatchRejection`] plus the exact durable JobStep result the
/// caller must persist, when one is required — mirrors
/// [`crate::final_dispatch::FinalDispatchDenial`]'s identical contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferDispatchDenial {
    pub rejection: TransferDispatchRejection,
    pub pending_job_step: Option<JobStep>,
}

impl TransferDispatchDenial {
    fn structural(rejection: TransferDispatchRejection) -> Self {
        Self {
            rejection,
            pending_job_step: None,
        }
    }

    fn revalidation_failure(rejection: TransferDispatchRejection, step: &JobStep) -> Self {
        Self {
            rejection,
            pending_job_step: Some(JobStep {
                state: JobStepState::Pending,
                ..step.clone()
            }),
        }
    }
}

/// Decides the final non-destructive transfer-dispatch commitment for
/// `inputs.step_id` under `inputs.job`, binding `inputs.transfer` to the
/// freshly minted Attempt. See the module documentation for the full
/// contract.
///
/// SUCCESS produces exactly one fresh [`AttemptId`] and
/// [`crate::attempt::ActionId`] — always distinct, always UUID v4 — the
/// candidate JobStep advanced to `Dispatching`, and `inputs.transfer` bound
/// to that Attempt. FAILURE produces a [`TransferDispatchDenial`]
/// identifying the first independent precondition checked that did not
/// hold, and the exact durable JobStep result (if any) the caller must
/// persist.
pub fn evaluate_transfer_dispatch(
    inputs: &TransferDispatchInputs,
) -> Result<TransferDispatchOutcome, TransferDispatchDenial> {
    let Some(step) = inputs.job.steps.iter().find(|s| s.id == inputs.step_id) else {
        return Err(TransferDispatchDenial::structural(
            TransferDispatchRejection::StepNotFound,
        ));
    };
    if step.state != JobStepState::PreconditionsSatisfied {
        return Err(TransferDispatchDenial::structural(
            TransferDispatchRejection::NotPreconditionsSatisfied,
        ));
    }
    if step.destructive_intent.is_some() {
        return Err(TransferDispatchDenial::structural(
            TransferDispatchRejection::StepIsDestructive,
        ));
    }
    let transfer = &inputs.transfer;
    if transfer.job_id != inputs.job.id
        || transfer.job_step_id != inputs.step_id
        || transfer.endpoint_id != inputs.job.endpoint_id
    {
        return Err(TransferDispatchDenial::structural(
            TransferDispatchRejection::TransferCorrelationMismatch,
        ));
    }

    // Every rejection from this point on is a final-revalidation failure
    // under an authoritative PreconditionsSatisfied JobStep with a
    // correctly correlated Transfer: this function itself owns the
    // resulting Pending JobStep, not the caller.
    let deny = |rejection: TransferDispatchRejection| {
        TransferDispatchDenial::revalidation_failure(rejection, step)
    };

    if inputs.job.state != JobState::Running {
        return Err(deny(TransferDispatchRejection::JobNotRunning));
    }
    let earlier_steps_all_succeeded = inputs
        .job
        .steps
        .iter()
        .filter(|s| s.order < step.order)
        .all(|s| s.state == JobStepState::Succeeded);
    if !earlier_steps_all_succeeded {
        return Err(deny(TransferDispatchRejection::NotCurrentStep));
    }
    if inputs.existing_active_attempt {
        return Err(deny(TransferDispatchRejection::ExistingActiveAttempt));
    }

    let attempt = Attempt {
        id: AttemptId::new(),
        job_step_id: step.id,
        action_id: crate::attempt::ActionId::new(),
        state: AttemptState::Dispatched,
    };
    let bound_transfer = match bind_attempt(transfer, &attempt) {
        Ok(bound) => bound,
        Err(TransferBindingError::ConflictingRebind) => {
            return Err(deny(TransferDispatchRejection::TransferAlreadyBound))
        }
        // Unreachable given the correlation check above (which already
        // verifies `transfer.job_step_id == inputs.step_id ==
        // attempt.job_step_id`), but handled exhaustively rather than
        // panicking on a Domain invariant this function itself established.
        Err(TransferBindingError::WrongJobStep) => {
            return Err(TransferDispatchDenial::structural(
                TransferDispatchRejection::TransferCorrelationMismatch,
            ))
        }
    };
    let dispatching_step = JobStep {
        state: JobStepState::Dispatching,
        ..step.clone()
    };
    Ok(TransferDispatchOutcome {
        job_step: dispatching_step,
        attempt,
        transfer: bound_transfer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_manifest::{ChunkSize, DigestAlgorithm};
    use crate::job::{create_workflow, satisfy_preliminary_preconditions};
    use crate::transfer::{create_transfer_context, SourceProvenance, TransferDirection};
    use crate::EndpointId;

    fn preconditions_satisfied_job(
        endpoint_id: crate::EndpointId,
        step_count: usize,
    ) -> (Job, JobStepId) {
        let job = create_workflow(endpoint_id, step_count).unwrap();
        let running = crate::job::admit_job(&job, chrono::Utc::now()).unwrap().job;
        let step_id = running.steps[0].id;
        let advanced = satisfy_preliminary_preconditions(&running, step_id).unwrap();
        let mut job = running;
        job.steps[0] = advanced;
        (job, step_id)
    }

    fn transfer_for(job: &Job, step_id: JobStepId) -> Transfer {
        create_transfer_context(
            job.endpoint_id,
            job.id,
            step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .transfer
    }

    fn all_pass_inputs() -> TransferDispatchInputs {
        let (job, step_id) = preconditions_satisfied_job(EndpointId::new(), 1);
        let transfer = transfer_for(&job, step_id);
        TransferDispatchInputs {
            job,
            step_id,
            existing_active_attempt: false,
            transfer,
        }
    }

    fn assert_revalidation_failure(
        result: Result<TransferDispatchOutcome, TransferDispatchDenial>,
        expected: TransferDispatchRejection,
        step_id: JobStepId,
    ) {
        let denial = result.expect_err("expected a rejection");
        assert_eq!(denial.rejection, expected);
        let pending_step = denial
            .pending_job_step
            .expect("a final-revalidation failure must supply the Pending JobStep to persist");
        assert_eq!(pending_step.id, step_id);
        assert_eq!(pending_step.state, JobStepState::Pending);
    }

    fn assert_structural_mismatch(
        result: Result<TransferDispatchOutcome, TransferDispatchDenial>,
        expected: TransferDispatchRejection,
    ) {
        let denial = result.expect_err("expected a rejection");
        assert_eq!(denial.rejection, expected);
        assert!(
            denial.pending_job_step.is_none(),
            "a structural mismatch must not supply a JobStep to persist"
        );
    }

    #[test]
    fn eligible_non_destructive_transfer_dispatch_succeeds() {
        let inputs = all_pass_inputs();
        let step_id = inputs.step_id;
        let transfer_id = inputs.transfer.id;
        let artifact_id = inputs.transfer.artifact_id;

        let outcome = evaluate_transfer_dispatch(&inputs).unwrap();

        assert_eq!(outcome.job_step.state, JobStepState::Dispatching);
        assert_eq!(outcome.job_step.id, step_id);
        assert_eq!(outcome.attempt.job_step_id, step_id);
        assert_eq!(outcome.attempt.state, AttemptState::Dispatched);
        assert_eq!(outcome.transfer.attempt_id, Some(outcome.attempt.id));
        assert_eq!(
            outcome.transfer.id, transfer_id,
            "TransferId must never be regenerated"
        );
        assert_eq!(
            outcome.transfer.artifact_id, artifact_id,
            "ArtifactId must never be regenerated"
        );
        assert_ne!(outcome.attempt.id.0, outcome.attempt.action_id.0);
        assert_eq!(outcome.attempt.id.0.get_version_num(), 4);
        assert_eq!(outcome.attempt.action_id.0.get_version_num(), 4);
    }

    #[test]
    fn two_independent_evaluations_never_share_attempt_or_action_identity() {
        let inputs_a = all_pass_inputs();
        let inputs_b = all_pass_inputs();
        let a = evaluate_transfer_dispatch(&inputs_a).unwrap();
        let b = evaluate_transfer_dispatch(&inputs_b).unwrap();

        assert_ne!(a.attempt.id, b.attempt.id);
        assert_ne!(a.attempt.action_id, b.attempt.action_id);
    }

    #[test]
    fn an_unknown_step_id_is_rejected_without_side_effects() {
        let mut inputs = all_pass_inputs();
        inputs.step_id = JobStepId::new();
        assert_structural_mismatch(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::StepNotFound,
        );
    }

    #[test]
    fn a_step_still_pending_is_rejected_structurally() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let running = crate::job::admit_job(&job, chrono::Utc::now()).unwrap().job;
        let step_id = running.steps[0].id;
        let transfer = transfer_for(&running, step_id);
        let inputs = TransferDispatchInputs {
            job: running,
            step_id,
            existing_active_attempt: false,
            transfer,
        };
        assert_structural_mismatch(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::NotPreconditionsSatisfied,
        );
    }

    #[test]
    fn a_job_that_is_no_longer_running_blocks_dispatch() {
        let mut inputs = all_pass_inputs();
        inputs.job.state = JobState::Cancelling;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::JobNotRunning,
            step_id,
        );
    }

    #[test]
    fn a_terminal_job_blocks_dispatch() {
        let mut inputs = all_pass_inputs();
        inputs.job.state = JobState::Failed;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::JobNotRunning,
            step_id,
        );
    }

    #[test]
    fn a_later_pending_step_cannot_skip_an_earlier_unfinished_step() {
        let job = create_workflow(EndpointId::new(), 2).unwrap();
        let running = crate::job::admit_job(&job, chrono::Utc::now()).unwrap().job;
        let first_step_id = running.steps[0].id;
        let second_step_id = running.steps[1].id;
        let advanced_first = satisfy_preliminary_preconditions(&running, first_step_id).unwrap();
        let mut job = running;
        job.steps[0] = advanced_first;
        job.steps[1] = JobStep {
            state: JobStepState::PreconditionsSatisfied,
            ..job.steps[1].clone()
        };
        let transfer = transfer_for(&job, second_step_id);

        let inputs = TransferDispatchInputs {
            job,
            step_id: second_step_id,
            existing_active_attempt: false,
            transfer,
        };
        assert_revalidation_failure(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::NotCurrentStep,
            second_step_id,
        );
    }

    #[test]
    fn an_existing_active_attempt_blocks_another_attempt() {
        let mut inputs = all_pass_inputs();
        inputs.existing_active_attempt = true;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::ExistingActiveAttempt,
            step_id,
        );
    }

    #[test]
    fn wrong_job_id_correlation_is_rejected_structurally() {
        let mut inputs = all_pass_inputs();
        inputs.transfer.job_id = crate::job::JobId::new();
        assert_structural_mismatch(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::TransferCorrelationMismatch,
        );
    }

    #[test]
    fn wrong_job_step_id_correlation_is_rejected_structurally() {
        let mut inputs = all_pass_inputs();
        inputs.transfer.job_step_id = JobStepId::new();
        assert_structural_mismatch(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::TransferCorrelationMismatch,
        );
    }

    #[test]
    fn wrong_endpoint_id_correlation_is_rejected_structurally() {
        let mut inputs = all_pass_inputs();
        inputs.transfer.endpoint_id = EndpointId::new();
        assert_structural_mismatch(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::TransferCorrelationMismatch,
        );
    }

    #[test]
    fn an_already_bound_transfer_is_rejected() {
        let mut inputs = all_pass_inputs();
        let other_attempt = Attempt {
            id: AttemptId::new(),
            job_step_id: inputs.step_id,
            action_id: crate::attempt::ActionId::new(),
            state: AttemptState::Dispatched,
        };
        inputs.transfer = bind_attempt(&inputs.transfer, &other_attempt).unwrap();
        let step_id = inputs.step_id;

        assert_revalidation_failure(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::TransferAlreadyBound,
            step_id,
        );
    }

    #[test]
    fn a_structurally_destructive_job_step_is_never_silently_treated_as_a_transfer_dispatch() {
        let (mut job, step_id) = preconditions_satisfied_job(EndpointId::new(), 1);
        job.steps[0].destructive_intent = Some(crate::job::DestructiveIntent {
            authorized_inventory_revision_id: crate::InventoryRevisionId(uuid::Uuid::new_v4()),
            authorized_target_fingerprint: crate::TargetFingerprint::new("disk-a"),
        });
        let transfer = transfer_for(&job, step_id);
        let inputs = TransferDispatchInputs {
            job,
            step_id,
            existing_active_attempt: false,
            transfer,
        };

        assert_structural_mismatch(
            evaluate_transfer_dispatch(&inputs),
            TransferDispatchRejection::StepIsDestructive,
        );
    }
}
