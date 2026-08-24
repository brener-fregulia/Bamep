//! Bamep Domain: the final destructive-dispatch authorization gate (Issue
//! #25 "[WP] Schedule Jobs and enforce safe dispatch gate").
//!
//! This module composes independent workflow/scheduler authorization
//! (`m0-job-lifecycle-and-scheduling.md` "Workflow/scheduler authorization")
//! with the complete seven-item destructive-operation gate
//! (`m0-endpoint-identity-lifecycle.md` "Destructive-operation authorization
//! preconditions") into one pure decision: [`evaluate_final_destructive_dispatch`].
//! Every precondition is checked independently and none is inferred from
//! another — the returned [`FinalDispatchDenial`] identifies exactly which
//! [`FinalDispatchRejection`] failed, rather than collapsing everything into
//! one opaque boolean.
//!
//! This function performs no I/O: every fact it needs — durable Endpoint/Job
//! state, the current inventory revision, the current target fingerprint,
//! current Agent presence, and durable credential validity at "now" — is
//! resolved by the caller (`bamep_server::application::FinalDispatchService`)
//! before this function is invoked, following the `lock -> freshly read ->
//! Domain decision -> persist -> commit` pattern
//! (`m0-job-lifecycle-and-scheduling.md` "Final pre-dispatch revalidation").
//!
//! On success this function returns exactly one fresh [`Attempt`] in
//! [`AttemptState::Dispatched`] and the candidate `JobStep` advanced to
//! `Dispatching`. On failure, this Domain function — not its caller — owns
//! the exact resulting durable JobStep, when one is required: a
//! final-revalidation failure under an authoritative `PreconditionsSatisfied`
//! JobStep carries that JobStep explicitly in `Pending`
//! ([`FinalDispatchDenial::pending_job_step`]); a structural mismatch (the
//! JobStep was not, or is no longer, `PreconditionsSatisfied`) carries
//! `None`, because there is nothing to revert. The caller only persists
//! whatever this function already decided — it never independently encodes
//! the lifecycle rule that a revalidation failure means `Pending`.
//!
//! This module never sends `ActionDispatch` and never touches PostgreSQL, the
//! Runtime Presence Registry, or the target-revalidation Port directly.

use crate::attempt::{ActionId, Attempt, AttemptId, AttemptState};
use crate::hardware_confidence::HardwareConfidence;
use crate::inventory::InventoryRevisionId;
use crate::job::{Job, JobState, JobStep, JobStepId, JobStepState};
use crate::target_fingerprint::TargetFingerprint;

/// Every already-resolved fact [`evaluate_final_destructive_dispatch`] needs.
/// The caller is responsible for resolving every field from durable/transient
/// state at decision time — this type carries no authority of its own beyond
/// what its fields already state.
#[derive(Debug, Clone)]
pub struct FinalDispatchInputs {
    /// The owning Job, including every ordered `JobStep`, freshly read under
    /// lock immediately before this decision.
    pub job: Job,
    /// The candidate JobStep's identity within `job`.
    pub step_id: JobStepId,
    /// Whether an Attempt already exists for this JobStep in a non-terminal
    /// state (`Dispatched`, `InProgress`, or `AwaitingReconciliation`) —
    /// workflow/scheduler authorization item 5: "no unresolved prior Attempt
    /// requires an explicit decision before another Attempt may exist."
    pub existing_active_attempt: bool,
    /// Destructive precondition 1: persistent Endpoint identity is
    /// `Enrolled`.
    pub identity_enrolled: bool,
    /// Destructive precondition 2 (durable half): the applicable durable
    /// credential dimension is currently `CredentialActive`.
    pub credential_active: bool,
    /// Destructive precondition 2 (transient half): the Runtime Presence
    /// Registry currently reports at least one authenticated session for
    /// this Endpoint. Independent of `credential_active` — neither
    /// substitutes for the other.
    pub agent_present: bool,
    /// The Endpoint's current durable inventory revision, or `None` when no
    /// inventory has ever been recorded. Compared against the JobStep's
    /// `DestructiveIntent::authorized_inventory_revision_id` for precondition
    /// 4; a missing current revision can never satisfy that comparison.
    pub current_inventory_revision_id: Option<InventoryRevisionId>,
    /// The independently observed current target-disk fingerprint, or `None`
    /// when no current target evidence is available. Compared against the
    /// JobStep's `DestructiveIntent::authorized_target_fingerprint` for
    /// precondition 5, entirely independently of inventory equality.
    pub current_target_fingerprint: Option<TargetFingerprint>,
    /// Destructive precondition 6: the Endpoint's durable hardware-confidence
    /// state. Only `Consistent` passes.
    pub hardware_confidence: HardwareConfidence,
    /// Destructive precondition 7: whether an authoritative current boot
    /// exists and its trusted-bootstrap state is `Established`. `false`
    /// represents both "no current boot" and "current boot not yet
    /// established" — both fail closed identically.
    pub trusted_bootstrap_established: bool,
}

/// Successful commitment produced by [`evaluate_final_destructive_dispatch`]:
/// the candidate JobStep advanced to `Dispatching`, and exactly one fresh
/// `Attempt` in `Dispatched`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalDispatchOutcome {
    pub job_step: JobStep,
    pub attempt: Attempt,
}

/// Every independently identifiable reason
/// [`evaluate_final_destructive_dispatch`] may reject a candidate JobStep.
/// Always carried inside a [`FinalDispatchDenial`], which also states the
/// exact durable effect (if any) this rejection requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FinalDispatchRejection {
    /// `step_id` does not identify a JobStep belonging to `job`. Structural:
    /// nothing to revert.
    #[error("job step not found in job")]
    StepNotFound,
    /// The candidate JobStep is not currently `PreconditionsSatisfied`
    /// (already `Dispatching`, still `Pending`, or terminal) — including the
    /// losing side of a concurrent dispatch race that already lost the row
    /// lock to a winner who committed first. Structural: nothing to revert.
    #[error("job step is not PreconditionsSatisfied")]
    NotPreconditionsSatisfied,
    /// The owning Job is not `Running` (workflow/scheduler authorization item
    /// 1). A final-revalidation failure under an authoritative
    /// `PreconditionsSatisfied` JobStep.
    #[error("job is not Running")]
    JobNotRunning,
    /// The candidate is not the structurally current active step: an earlier
    /// ordered JobStep is not yet `Succeeded` (workflow/scheduler
    /// authorization item 2).
    #[error("job step is not the current active step")]
    NotCurrentStep,
    /// An unresolved prior Attempt already exists for this JobStep
    /// (workflow/scheduler authorization item 5).
    #[error("an active or unresolved attempt already exists for this job step")]
    ExistingActiveAttempt,
    /// The candidate JobStep carries no `DestructiveIntent` — precondition 3
    /// ("authorized Job/action") requires the Server-authorized #31 intent.
    #[error("job step has no authorized destructive intent")]
    NoDestructiveIntent,
    /// Destructive precondition 1: identity is not `Enrolled`.
    #[error("endpoint identity is not Enrolled")]
    IdentityNotEnrolled,
    /// Destructive precondition 2 (durable half): credential is not
    /// `CredentialActive`.
    #[error("durable credential is not CredentialActive")]
    CredentialNotActive,
    /// Destructive precondition 2 (transient half): no currently
    /// authenticated Agent session for this Endpoint.
    #[error("no currently authenticated agent session")]
    AgentNotPresent,
    /// Destructive precondition 4: authorized inventory revision does not
    /// equal the Endpoint's current inventory revision (including when no
    /// current revision exists at all).
    #[error("authorized inventory revision is stale")]
    StaleInventory,
    /// Destructive precondition 5: authorized target fingerprint does not
    /// equal the currently revalidated target fingerprint (including when no
    /// current target evidence exists at all). Independent of precondition
    /// 4.
    #[error("authorized target fingerprint does not match current target")]
    TargetMismatch,
    /// Destructive precondition 6: hardware confidence is not `Consistent`
    /// (`LoweredConfidence` and `Conflict` both fail).
    #[error("hardware confidence is not Consistent")]
    HardwareConfidenceNotConsistent,
    /// Destructive precondition 7: no authoritative current boot with
    /// trusted-bootstrap `Established`. Independent of every credential/
    /// presence/workflow precondition above.
    #[error("trusted current bootstrap is not Established")]
    TrustedBootstrapNotEstablished,
}

/// The complete failure outcome of [`evaluate_final_destructive_dispatch`]:
/// the typed [`FinalDispatchRejection`] plus the exact durable JobStep result
/// the caller must persist, when one is required
/// (`m0-job-lifecycle-and-scheduling.md` "Final pre-dispatch revalidation").
///
/// `pending_job_step` is `Some(step)` — `step.state ==
/// JobStepState::Pending`, every other field unchanged — for a
/// final-revalidation failure under an authoritative `PreconditionsSatisfied`
/// JobStep, and `None` for a structural mismatch
/// ([`FinalDispatchRejection::StepNotFound`] /
/// [`FinalDispatchRejection::NotPreconditionsSatisfied`]), where the JobStep
/// was not (or is no longer) `PreconditionsSatisfied` to begin with, so there
/// is nothing to revert. The caller (`PostgresJobRepository`) persists
/// exactly this value and never independently decides "revalidation failure
/// means Pending" itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalDispatchDenial {
    pub rejection: FinalDispatchRejection,
    pub pending_job_step: Option<JobStep>,
}

impl FinalDispatchDenial {
    fn structural(rejection: FinalDispatchRejection) -> Self {
        Self {
            rejection,
            pending_job_step: None,
        }
    }

    fn revalidation_failure(rejection: FinalDispatchRejection, step: &JobStep) -> Self {
        Self {
            rejection,
            pending_job_step: Some(JobStep {
                state: JobStepState::Pending,
                ..step.clone()
            }),
        }
    }
}

/// Decides the final destructive-dispatch commitment for `inputs.step_id`
/// under `inputs.job`. See the module documentation for the full contract.
///
/// SUCCESS produces exactly one fresh [`AttemptId`] and [`ActionId`] — always
/// distinct, always UUID v4 — and the candidate JobStep advanced to
/// `Dispatching`. FAILURE produces a [`FinalDispatchDenial`] identifying the
/// first independent precondition checked that did not hold, and the exact
/// durable JobStep result (if any) the caller must persist; checks are
/// ordered but every one is evaluated against `inputs` alone, never inferred
/// from another.
pub fn evaluate_final_destructive_dispatch(
    inputs: &FinalDispatchInputs,
) -> Result<FinalDispatchOutcome, FinalDispatchDenial> {
    let Some(step) = inputs.job.steps.iter().find(|s| s.id == inputs.step_id) else {
        return Err(FinalDispatchDenial::structural(
            FinalDispatchRejection::StepNotFound,
        ));
    };
    if step.state != JobStepState::PreconditionsSatisfied {
        return Err(FinalDispatchDenial::structural(
            FinalDispatchRejection::NotPreconditionsSatisfied,
        ));
    }

    // Every rejection from this point on is a final-revalidation failure
    // under an authoritative PreconditionsSatisfied JobStep: this function
    // itself owns the resulting Pending JobStep, not the caller.
    let deny = |rejection: FinalDispatchRejection| {
        FinalDispatchDenial::revalidation_failure(rejection, step)
    };

    if inputs.job.state != JobState::Running {
        return Err(deny(FinalDispatchRejection::JobNotRunning));
    }
    let earlier_steps_all_succeeded = inputs
        .job
        .steps
        .iter()
        .filter(|s| s.order < step.order)
        .all(|s| s.state == JobStepState::Succeeded);
    if !earlier_steps_all_succeeded {
        return Err(deny(FinalDispatchRejection::NotCurrentStep));
    }
    if inputs.existing_active_attempt {
        return Err(deny(FinalDispatchRejection::ExistingActiveAttempt));
    }
    let Some(intent) = &step.destructive_intent else {
        return Err(deny(FinalDispatchRejection::NoDestructiveIntent));
    };

    if !inputs.identity_enrolled {
        return Err(deny(FinalDispatchRejection::IdentityNotEnrolled));
    }
    if !inputs.credential_active {
        return Err(deny(FinalDispatchRejection::CredentialNotActive));
    }
    if !inputs.agent_present {
        return Err(deny(FinalDispatchRejection::AgentNotPresent));
    }
    if inputs.current_inventory_revision_id != Some(intent.authorized_inventory_revision_id) {
        return Err(deny(FinalDispatchRejection::StaleInventory));
    }
    if inputs.current_target_fingerprint.as_ref() != Some(&intent.authorized_target_fingerprint) {
        return Err(deny(FinalDispatchRejection::TargetMismatch));
    }
    if inputs.hardware_confidence != HardwareConfidence::Consistent {
        return Err(deny(
            FinalDispatchRejection::HardwareConfidenceNotConsistent,
        ));
    }
    if !inputs.trusted_bootstrap_established {
        return Err(deny(FinalDispatchRejection::TrustedBootstrapNotEstablished));
    }

    let attempt = Attempt {
        id: AttemptId::new(),
        job_step_id: step.id,
        action_id: ActionId::new(),
        state: AttemptState::Dispatched,
    };
    let dispatching_step = JobStep {
        state: JobStepState::Dispatching,
        ..step.clone()
    };
    Ok(FinalDispatchOutcome {
        job_step: dispatching_step,
        attempt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{create_workflow, satisfy_preliminary_preconditions, DestructiveIntent};
    use crate::EndpointId;

    fn intent() -> DestructiveIntent {
        DestructiveIntent {
            authorized_inventory_revision_id: InventoryRevisionId(uuid::Uuid::new_v4()),
            authorized_target_fingerprint: TargetFingerprint::new("disk-a"),
        }
    }

    /// Builds a `Running` Job with one destructive JobStep at
    /// `PreconditionsSatisfied`, carrying `intent`.
    fn preconditions_satisfied_job(intent: DestructiveIntent) -> (Job, JobStepId) {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let running = crate::job::admit_job(&job, chrono::Utc::now()).unwrap().job;
        let step_id = running.steps[0].id;
        let advanced = satisfy_preliminary_preconditions(&running, step_id).unwrap();
        let mut job = running;
        job.steps[0] = JobStep {
            destructive_intent: Some(intent),
            ..advanced
        };
        (job, step_id)
    }

    /// Every precondition passing — the baseline every negative test starts
    /// from and flips exactly one field away from.
    fn all_pass_inputs() -> FinalDispatchInputs {
        let intent = intent();
        let (job, step_id) = preconditions_satisfied_job(intent.clone());
        FinalDispatchInputs {
            job,
            step_id,
            existing_active_attempt: false,
            identity_enrolled: true,
            credential_active: true,
            agent_present: true,
            current_inventory_revision_id: Some(intent.authorized_inventory_revision_id),
            current_target_fingerprint: Some(intent.authorized_target_fingerprint),
            hardware_confidence: HardwareConfidence::Consistent,
            trusted_bootstrap_established: true,
        }
    }

    /// Asserts `result` is a final-revalidation failure: the expected
    /// rejection, plus a `pending_job_step` that is exactly `step_id` moved
    /// to `Pending` — proving the Domain function itself, not the caller,
    /// decided that durable effect.
    fn assert_revalidation_failure(
        result: Result<FinalDispatchOutcome, FinalDispatchDenial>,
        expected: FinalDispatchRejection,
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

    /// Asserts `result` is a structural mismatch: the expected rejection,
    /// with no `pending_job_step` — proving there is nothing to persist.
    fn assert_structural_mismatch(
        result: Result<FinalDispatchOutcome, FinalDispatchDenial>,
        expected: FinalDispatchRejection,
    ) {
        let denial = result.expect_err("expected a rejection");
        assert_eq!(denial.rejection, expected);
        assert!(
            denial.pending_job_step.is_none(),
            "a structural mismatch must not supply a JobStep to persist"
        );
    }

    #[test]
    fn all_preconditions_passing_succeeds_with_one_fresh_attempt_dispatched() {
        let inputs = all_pass_inputs();
        let step_id = inputs.step_id;
        let outcome = evaluate_final_destructive_dispatch(&inputs).unwrap();

        assert_eq!(outcome.job_step.state, JobStepState::Dispatching);
        assert_eq!(outcome.job_step.id, step_id);
        assert_eq!(outcome.attempt.job_step_id, step_id);
        assert_eq!(outcome.attempt.state, AttemptState::Dispatched);
    }

    #[test]
    fn attempt_id_and_action_id_are_distinct_and_both_uuid_v4() {
        let inputs = all_pass_inputs();
        let outcome = evaluate_final_destructive_dispatch(&inputs).unwrap();

        assert_ne!(outcome.attempt.id.0, outcome.attempt.action_id.0);
        assert_eq!(outcome.attempt.id.0.get_version_num(), 4);
        assert_eq!(outcome.attempt.action_id.0.get_version_num(), 4);
    }

    #[test]
    fn two_independent_evaluations_never_share_attempt_or_action_identity() {
        let inputs_a = all_pass_inputs();
        let inputs_b = all_pass_inputs();
        let a = evaluate_final_destructive_dispatch(&inputs_a).unwrap();
        let b = evaluate_final_destructive_dispatch(&inputs_b).unwrap();

        assert_ne!(a.attempt.id, b.attempt.id);
        assert_ne!(a.attempt.action_id, b.attempt.action_id);
    }

    #[test]
    fn only_the_current_preconditions_satisfied_step_of_a_running_job_is_eligible() {
        // A step still Pending (never advanced) is rejected structurally,
        // never silently treated as eligible.
        let intent = intent();
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let running = crate::job::admit_job(&job, chrono::Utc::now()).unwrap().job;
        let step_id = running.steps[0].id;
        let mut inputs = all_pass_inputs();
        inputs.job = running;
        inputs.step_id = step_id;
        inputs.current_inventory_revision_id = Some(intent.authorized_inventory_revision_id);
        inputs.current_target_fingerprint = Some(intent.authorized_target_fingerprint);

        assert_structural_mismatch(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::NotPreconditionsSatisfied,
        );
    }

    #[test]
    fn an_unknown_step_id_is_rejected_without_side_effects() {
        let mut inputs = all_pass_inputs();
        inputs.step_id = JobStepId::new();
        assert_structural_mismatch(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::StepNotFound,
        );
    }

    #[test]
    fn a_job_that_is_no_longer_running_blocks_dispatch() {
        let mut inputs = all_pass_inputs();
        inputs.job.state = JobState::Cancelling;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::JobNotRunning,
            step_id,
        );
    }

    #[test]
    fn an_existing_active_attempt_blocks_another_attempt() {
        let mut inputs = all_pass_inputs();
        inputs.existing_active_attempt = true;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::ExistingActiveAttempt,
            step_id,
        );
    }

    #[test]
    fn identity_not_enrolled_fails_while_everything_else_passes() {
        let mut inputs = all_pass_inputs();
        inputs.identity_enrolled = false;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::IdentityNotEnrolled,
            step_id,
        );
    }

    #[test]
    fn credential_inactive_fails_while_presence_and_everything_else_passes() {
        let mut inputs = all_pass_inputs();
        inputs.credential_active = false;
        assert!(inputs.agent_present);
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::CredentialNotActive,
            step_id,
        );
    }

    #[test]
    fn missing_agent_presence_fails_while_credential_remains_active() {
        let mut inputs = all_pass_inputs();
        inputs.agent_present = false;
        assert!(inputs.credential_active);
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::AgentNotPresent,
            step_id,
        );
    }

    #[test]
    fn workflow_action_authorization_fails_while_endpoint_safety_facts_pass() {
        // No destructive intent attached: precondition 3 ("authorized
        // Job/action") fails while every Endpoint-owned safety dimension
        // still passes.
        let intent = intent();
        let (mut job, step_id) = preconditions_satisfied_job(intent.clone());
        job.steps[0].destructive_intent = None;
        let mut inputs = all_pass_inputs();
        inputs.job = job;
        inputs.step_id = step_id;

        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::NoDestructiveIntent,
            step_id,
        );
    }

    #[test]
    fn stale_authorized_inventory_fails_while_target_still_matches() {
        let mut inputs = all_pass_inputs();
        inputs.current_inventory_revision_id = Some(InventoryRevisionId(uuid::Uuid::new_v4()));
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::StaleInventory,
            step_id,
        );
    }

    #[test]
    fn target_mismatch_fails_while_inventory_still_matches() {
        let mut inputs = all_pass_inputs();
        inputs.current_target_fingerprint = Some(TargetFingerprint::new("disk-b"));
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::TargetMismatch,
            step_id,
        );
    }

    #[test]
    fn missing_current_inventory_is_treated_as_stale() {
        let mut inputs = all_pass_inputs();
        inputs.current_inventory_revision_id = None;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::StaleInventory,
            step_id,
        );
    }

    #[test]
    fn missing_current_target_is_treated_as_mismatch() {
        let mut inputs = all_pass_inputs();
        inputs.current_target_fingerprint = None;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::TargetMismatch,
            step_id,
        );
    }

    #[test]
    fn lowered_confidence_fails() {
        let mut inputs = all_pass_inputs();
        inputs.hardware_confidence = HardwareConfidence::LoweredConfidence;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::HardwareConfidenceNotConsistent,
            step_id,
        );
    }

    #[test]
    fn conflict_confidence_fails() {
        let mut inputs = all_pass_inputs();
        inputs.hardware_confidence = HardwareConfidence::Conflict;
        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::HardwareConfidenceNotConsistent,
            step_id,
        );
    }

    #[test]
    fn trusted_bootstrap_absent_fails_while_preconditions_one_through_six_all_pass() {
        let mut inputs = all_pass_inputs();
        inputs.trusted_bootstrap_established = false;

        assert!(inputs.identity_enrolled);
        assert!(inputs.credential_active);
        assert!(inputs.agent_present);
        assert_eq!(
            inputs.current_inventory_revision_id,
            inputs
                .job
                .steps
                .iter()
                .find(|s| s.id == inputs.step_id)
                .unwrap()
                .destructive_intent
                .as_ref()
                .map(|i| i.authorized_inventory_revision_id)
        );
        assert_eq!(inputs.hardware_confidence, HardwareConfidence::Consistent);

        let step_id = inputs.step_id;
        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::TrustedBootstrapNotEstablished,
            step_id,
        );
    }

    #[test]
    fn a_later_pending_step_cannot_skip_an_earlier_unfinished_step() {
        let intent_a = intent();
        let intent_b = DestructiveIntent {
            authorized_inventory_revision_id: InventoryRevisionId(uuid::Uuid::new_v4()),
            authorized_target_fingerprint: TargetFingerprint::new("disk-b"),
        };
        let job = create_workflow(EndpointId::new(), 2).unwrap();
        let running = crate::job::admit_job(&job, chrono::Utc::now()).unwrap().job;
        let first_step_id = running.steps[0].id;
        let second_step_id = running.steps[1].id;
        let advanced_first = satisfy_preliminary_preconditions(&running, first_step_id).unwrap();
        let mut job = running;
        job.steps[0] = JobStep {
            destructive_intent: Some(intent_a),
            ..advanced_first
        };
        // Second step is force-set to PreconditionsSatisfied directly (never
        // reachable through the real transition while the first step has not
        // succeeded) purely to prove the final-dispatch gate itself also
        // enforces the structural ordering rule, independently of
        // `satisfy_preliminary_preconditions` already enforcing it earlier.
        job.steps[1] = JobStep {
            state: JobStepState::PreconditionsSatisfied,
            destructive_intent: Some(intent_b.clone()),
            ..job.steps[1].clone()
        };

        let inputs = FinalDispatchInputs {
            job,
            step_id: second_step_id,
            existing_active_attempt: false,
            identity_enrolled: true,
            credential_active: true,
            agent_present: true,
            current_inventory_revision_id: Some(intent_b.authorized_inventory_revision_id),
            current_target_fingerprint: Some(intent_b.authorized_target_fingerprint),
            hardware_confidence: HardwareConfidence::Consistent,
            trusted_bootstrap_established: true,
        };

        assert_revalidation_failure(
            evaluate_final_destructive_dispatch(&inputs),
            FinalDispatchRejection::NotCurrentStep,
            second_step_id,
        );
    }
}
