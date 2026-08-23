//! Bamep Domain: Job/JobStep workflow model
//! (`docs/specifications/m0-job-lifecycle-and-scheduling.md` "Domain model";
//! `docs/decisions/0006-job-jobstep-attempt-state-model-and-scheduling.md`).
//!
//! This module represents durable `Pending` workflow creation — identities,
//! `Job -> Endpoint`/`JobStep -> Job` correlations, explicit linear order, and
//! initial `Pending` states (Issue #24) — plus the destructive-operation
//! authorization snapshot attached to one eligible JobStep before dispatch
//! (Issue #31: [`DestructiveIntent`]), Job admission into `Running`
//! ([`admit_job`]), and the preliminary current-JobStep eligibility
//! transition ([`satisfy_preliminary_preconditions`]) (Issue #32). The
//! complete lifecycle vocabularies already reflect the Specification's later
//! states so later scheduling/dispatch Work Packages do not need a Domain
//! type change. This module performs no I/O, [`create_workflow`] constructs
//! no state beyond `Pending`, and no concrete Agent action type/version/
//! parameters or Attempt identity is represented here. Neither [`admit_job`]
//! nor [`satisfy_preliminary_preconditions`] creates an Attempt or evaluates
//! the destructive-operation gate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::events::DomainEvent;
use crate::inventory::InventoryRevisionId;
use crate::target_fingerprint::TargetFingerprint;
use crate::EndpointId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub Uuid);

impl JobId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobStepId(pub Uuid);

impl JobStepId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobStepId {
    fn default() -> Self {
        Self::new()
    }
}

/// Job lifecycle states (`m0-job-lifecycle-and-scheduling.md` "Job
/// lifecycle"). Issue #24 only ever constructs `Pending`; the remaining
/// variants are represented so later Work Packages do not need a Domain type
/// change, but no code in this checkpoint produces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Pending,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

/// JobStep lifecycle states (`m0-job-lifecycle-and-scheduling.md` "JobStep
/// lifecycle"). Issue #24 only ever constructs `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStepState {
    Pending,
    PreconditionsSatisfied,
    Dispatching,
    Succeeded,
    Failed,
    Cancelled,
}

/// The durable destructive-operation authorization snapshot for one JobStep
/// (Issue #31 "Persist simulated destructive JobStep intent";
/// `docs/specifications/m0-endpoint-identity-lifecycle.md` "Destructive-
/// operation authorization preconditions" 4 and 5). Presence of this value on
/// a [`JobStep`] classifies that step as destructive for the M1 workflow
/// path. Both fields are captured once, from Server-owned facts current at
/// authorization time, and are never refreshed from later inventory/target
/// observations — later staleness is detected by comparison, not by rewriting
/// this snapshot (`m0-job-lifecycle-and-scheduling.md` "Final pre-dispatch
/// revalidation", owned by #25).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestructiveIntent {
    pub authorized_inventory_revision_id: InventoryRevisionId,
    pub authorized_target_fingerprint: TargetFingerprint,
}

/// Rejections from [`authorize_destructive_intent`]. None of these represent
/// a JobStep state change — a rejected call leaves the JobStep exactly as it
/// was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DestructiveIntentError {
    #[error("job step does not belong to the specified job")]
    WrongJob,
    #[error("job step is not eligible for destructive intent authorization")]
    NotEligible,
    #[error("job step already has a destructive intent")]
    AlreadyAuthorized,
}

/// One ordered linear stage of its owning [`Job`]. `order` is the explicit
/// stable linear position required by the accepted linear-workflow baseline
/// (`m0-job-lifecycle-and-scheduling.md`: "The baseline workflow is linear;
/// branching/parallel JobSteps are outside this contract"). This module owns
/// workflow identity/correlation/order/state (Issue #24) and the
/// destructive-operation authorization snapshot (Issue #31); no concrete
/// Agent action type/version/parameters or Attempt identity is represented
/// yet — an action-specific contract will extend this still-pre-baseline
/// shape when it is introduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStep {
    pub id: JobStepId,
    pub job_id: JobId,
    pub order: i32,
    pub state: JobStepState,
    /// Single-assignment for this M1 boundary (Issue #31): `None` until one
    /// [`authorize_destructive_intent`] call succeeds, then bound for the
    /// life of this JobStep. A future explicit lifecycle/authorization
    /// operation — not introduced here — would be required to replace an
    /// already-attached intent.
    pub destructive_intent: Option<DestructiveIntent>,
}

/// One workflow targeting one Endpoint, composed of an ordered sequence of
/// [`JobStep`]s (`m0-job-lifecycle-and-scheduling.md` "Domain model").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub endpoint_id: EndpointId,
    pub state: JobState,
    /// Ordered by `JobStep::order`, ascending, with no gaps or duplicates —
    /// the invariant [`create_workflow`] establishes and the PostgreSQL
    /// Adapter's `UNIQUE (job_id, step_order)` constraint protects durably.
    pub steps: Vec<JobStep>,
}

/// An empty workflow is invalid: a Job is "composed of an ordered sequence of
/// JobSteps" (`m0-job-lifecycle-and-scheduling.md` "Domain model") — never
/// zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a Job requires at least one JobStep")]
pub struct EmptyWorkflow;

/// Constructs a new linear workflow targeting `endpoint_id`: fresh, stable,
/// distinct `Job`/`JobStep` identities, `step_count` ordered `JobStep`s
/// (positions `0..step_count`), and every state `Pending`
/// (`m0-job-lifecycle-and-scheduling.md` "Job lifecycle", "JobStep
/// lifecycle"). Pure — performs no I/O and does not verify that the target
/// Endpoint exists or is `Enrolled`; that verification requires durable
/// Endpoint state and belongs to the Application/Adapter boundary that calls
/// this function before persisting its result.
pub fn create_workflow(endpoint_id: EndpointId, step_count: usize) -> Result<Job, EmptyWorkflow> {
    if step_count == 0 {
        return Err(EmptyWorkflow);
    }
    let job_id = JobId::new();
    let steps = (0..step_count)
        .map(|order| JobStep {
            id: JobStepId::new(),
            job_id,
            order: order as i32,
            state: JobStepState::Pending,
            destructive_intent: None,
        })
        .collect();
    Ok(Job {
        id: job_id,
        endpoint_id,
        state: JobState::Pending,
        steps,
    })
}

/// Decides whether `step` may receive one durable [`DestructiveIntent`]
/// snapshot (Issue #31 "Application / internal harness path"). Pure — takes
/// the already Server-derived `authorized_inventory_revision_id` and
/// `authorized_target_fingerprint` as given facts; this function only
/// verifies eligibility and correlation, never derives or validates the
/// evidence itself. `job_id` is the caller's claimed owning Job, checked
/// against `step.job_id` so a caller cannot authorize a step under the wrong
/// Job by mistake.
///
/// Eligible means: `step` belongs to `job_id`, `step.state` is `Pending`, and
/// `step.destructive_intent` is not already set. Attaching an intent never
/// changes `step.state` — the returned value is only the intent to attach;
/// the caller remains responsible for persisting it without mutating state.
pub fn authorize_destructive_intent(
    step: &JobStep,
    job_id: JobId,
    authorized_inventory_revision_id: InventoryRevisionId,
    authorized_target_fingerprint: TargetFingerprint,
) -> Result<DestructiveIntent, DestructiveIntentError> {
    if step.job_id != job_id {
        return Err(DestructiveIntentError::WrongJob);
    }
    if step.state != JobStepState::Pending {
        return Err(DestructiveIntentError::NotEligible);
    }
    if step.destructive_intent.is_some() {
        return Err(DestructiveIntentError::AlreadyAuthorized);
    }
    Ok(DestructiveIntent {
        authorized_inventory_revision_id,
        authorized_target_fingerprint,
    })
}

/// Outcome of successfully admitting a `Pending` Job into `Running`
/// (`m0-job-lifecycle-and-scheduling.md` "Job lifecycle": `Pending -> Running`
/// "acquire the Job-scoped Endpoint-exclusivity lease"; Issue #32 "Job
/// admission and durable Endpoint exclusivity"). Durable Endpoint exclusivity
/// itself is represented by the persisted `Running` state under the
/// Adapter's active-Job uniqueness constraint, not by anything this pure
/// function does — [`admit_job`] only decides legality and produces the
/// exactly-one required [`DomainEvent::JobStarted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobAdmissionOutcome {
    pub job: Job,
    pub event: DomainEvent,
}

/// Rejection from [`admit_job`]. Never represents a partial/half-admitted
/// Job — a rejected call leaves the Job exactly as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JobAdmissionError {
    #[error("job is not eligible for admission")]
    NotPending,
}

/// Decides `Pending -> Running` for `job` at `now`
/// (`m0-job-lifecycle-and-scheduling.md` "Job lifecycle"). Only a `Pending`
/// Job may be admitted — a repeated admission attempt against an
/// already-`Running` (or any other non-`Pending`) Job is rejected rather than
/// silently re-emitting `JobStarted`. Pure: this function neither verifies
/// nor acquires Job-scoped Endpoint exclusivity itself — that durable
/// guarantee belongs to the Adapter's active-Job uniqueness constraint,
/// enforced when the returned [`JobAdmissionOutcome`] is persisted.
pub fn admit_job(job: &Job, now: DateTime<Utc>) -> Result<JobAdmissionOutcome, JobAdmissionError> {
    if job.state != JobState::Pending {
        return Err(JobAdmissionError::NotPending);
    }
    let admitted = Job {
        state: JobState::Running,
        ..job.clone()
    };
    let event = DomainEvent::JobStarted {
        event_id: Uuid::new_v4(),
        job_id: job.id,
        endpoint_id: job.endpoint_id,
        occurred_at: now,
    };
    Ok(JobAdmissionOutcome {
        job: admitted,
        event,
    })
}

/// Rejection from [`satisfy_preliminary_preconditions`]. Never represents a
/// state change — a rejected call leaves the JobStep exactly as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JobStepEligibilityError {
    #[error("job is not Running")]
    JobNotRunning,
    #[error("job step not found in this job")]
    StepNotFound,
    #[error("job step is not the current eligible step")]
    NotCurrent,
}

/// Decides the narrow preliminary eligibility transition `Pending ->
/// PreconditionsSatisfied` for `step_id` under `job`
/// (`m0-job-lifecycle-and-scheduling.md` "JobStep lifecycle": "preliminary
/// JobStep-specific eligibility passes; this does not establish workflow
/// authorization or the destructive gate"; Issue #32 "Current ordered
/// JobStep preliminary eligibility"). Eligible means: `job.state` is
/// `Running`, `step_id` identifies a `Pending` JobStep belonging to `job`,
/// and every earlier ordered JobStep (`order` less than the candidate's) is
/// already `Succeeded` — the structural, deterministic linear-workflow
/// "current step" rule. A later step can never skip an earlier unfinished
/// one. This function does not evaluate the seven destructive-operation
/// preconditions, does not create an Attempt, and never produces
/// `Dispatching`.
pub fn satisfy_preliminary_preconditions(
    job: &Job,
    step_id: JobStepId,
) -> Result<JobStep, JobStepEligibilityError> {
    if job.state != JobState::Running {
        return Err(JobStepEligibilityError::JobNotRunning);
    }
    let Some(candidate) = job.steps.iter().find(|step| step.id == step_id) else {
        return Err(JobStepEligibilityError::StepNotFound);
    };
    if candidate.state != JobStepState::Pending {
        return Err(JobStepEligibilityError::NotCurrent);
    }
    let earlier_steps_all_succeeded = job
        .steps
        .iter()
        .filter(|step| step.order < candidate.order)
        .all(|step| step.state == JobStepState::Succeeded);
    if !earlier_steps_all_succeeded {
        return Err(JobStepEligibilityError::NotCurrent);
    }
    Ok(JobStep {
        state: JobStepState::PreconditionsSatisfied,
        ..candidate.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_step_count_is_rejected() {
        assert_eq!(create_workflow(EndpointId::new(), 0), Err(EmptyWorkflow));
    }

    #[test]
    fn workflow_starts_pending_with_pending_steps_in_order() {
        let endpoint_id = EndpointId::new();
        let job = create_workflow(endpoint_id, 3).unwrap();

        assert_eq!(job.endpoint_id, endpoint_id);
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.steps.len(), 3);
        for (index, step) in job.steps.iter().enumerate() {
            assert_eq!(
                step.job_id, job.id,
                "every JobStep must correlate to its Job"
            );
            assert_eq!(step.order, index as i32);
            assert_eq!(step.state, JobStepState::Pending);
        }
    }

    #[test]
    fn single_step_workflow_is_accepted() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        assert_eq!(job.steps.len(), 1);
        assert_eq!(job.steps[0].order, 0);
    }

    #[test]
    fn job_and_every_jobstep_have_distinct_stable_identities() {
        let job = create_workflow(EndpointId::new(), 4).unwrap();
        let mut ids: Vec<Uuid> = job.steps.iter().map(|s| s.id.0).collect();
        ids.push(job.id.0);
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "Job identity and every JobStep identity must be distinct"
        );
    }

    #[test]
    fn two_workflows_never_share_identities() {
        let endpoint_id = EndpointId::new();
        let a = create_workflow(endpoint_id, 2).unwrap();
        let b = create_workflow(endpoint_id, 2).unwrap();

        assert_ne!(a.id, b.id);
        assert_ne!(a.steps[0].id, b.steps[0].id);
        assert_ne!(a.steps[1].id, b.steps[1].id);
    }

    fn intent() -> (InventoryRevisionId, TargetFingerprint) {
        (
            InventoryRevisionId(Uuid::new_v4()),
            TargetFingerprint::new("disk-a"),
        )
    }

    #[test]
    fn new_jobsteps_have_no_destructive_intent() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        assert_eq!(job.steps[0].destructive_intent, None);
    }

    #[test]
    fn attaching_an_intent_classifies_the_step_as_destructive_without_changing_state_or_identity() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let step = &job.steps[0];
        let (revision_id, fingerprint) = intent();

        let attached =
            authorize_destructive_intent(step, job.id, revision_id, fingerprint.clone()).unwrap();

        assert_eq!(attached.authorized_inventory_revision_id, revision_id);
        assert_eq!(attached.authorized_target_fingerprint, fingerprint);
        // authorize_destructive_intent returns the intent to attach; it does
        // not itself mutate `step` — identity/order/job correlation/state are
        // whatever the caller preserves when persisting alongside it.
        assert_eq!(step.id, job.steps[0].id);
        assert_eq!(step.job_id, job.id);
        assert_eq!(step.order, job.steps[0].order);
        assert_eq!(step.state, JobStepState::Pending);
    }

    #[test]
    fn wrong_job_correlation_is_rejected() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let other_job_id = JobId::new();
        let (revision_id, fingerprint) = intent();

        assert_eq!(
            authorize_destructive_intent(&job.steps[0], other_job_id, revision_id, fingerprint),
            Err(DestructiveIntentError::WrongJob)
        );
    }

    #[test]
    fn non_pending_step_is_not_eligible() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let mut step = job.steps[0].clone();
        step.state = JobStepState::PreconditionsSatisfied;
        let (revision_id, fingerprint) = intent();

        assert_eq!(
            authorize_destructive_intent(&step, job.id, revision_id, fingerprint),
            Err(DestructiveIntentError::NotEligible)
        );
    }

    #[test]
    fn an_existing_intent_cannot_be_silently_overwritten() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let mut step = job.steps[0].clone();
        let (first_revision, first_fingerprint) = intent();
        step.destructive_intent = Some(
            authorize_destructive_intent(&step, job.id, first_revision, first_fingerprint.clone())
                .unwrap(),
        );

        let (second_revision, second_fingerprint) = intent();
        let result =
            authorize_destructive_intent(&step, job.id, second_revision, second_fingerprint);

        assert_eq!(result, Err(DestructiveIntentError::AlreadyAuthorized));
        assert_eq!(
            step.destructive_intent,
            Some(DestructiveIntent {
                authorized_inventory_revision_id: first_revision,
                authorized_target_fingerprint: first_fingerprint,
            }),
            "the original snapshot must remain exactly as first authorized"
        );
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn admitting_a_pending_job_produces_running_state_and_one_job_started_event() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let outcome = admit_job(&job, now()).unwrap();

        assert_eq!(outcome.job.state, JobState::Running);
        assert_eq!(outcome.job.id, job.id);
        assert_eq!(outcome.job.endpoint_id, job.endpoint_id);
        match outcome.event {
            DomainEvent::JobStarted {
                job_id,
                endpoint_id,
                ..
            } => {
                assert_eq!(job_id, job.id);
                assert_eq!(endpoint_id, job.endpoint_id);
            }
            other => panic!("expected JobStarted, got {other:?}"),
        }
    }

    #[test]
    fn admitting_an_already_running_job_is_rejected() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let running = admit_job(&job, now()).unwrap().job;

        assert_eq!(
            admit_job(&running, now()),
            Err(JobAdmissionError::NotPending)
        );
    }

    #[test]
    fn admitting_a_terminal_job_is_rejected() {
        let mut job = create_workflow(EndpointId::new(), 1).unwrap();
        job.state = JobState::Succeeded;

        assert_eq!(admit_job(&job, now()), Err(JobAdmissionError::NotPending));
    }

    fn running_job(step_count: usize) -> Job {
        let job = create_workflow(EndpointId::new(), step_count).unwrap();
        admit_job(&job, now()).unwrap().job
    }

    #[test]
    fn the_first_pending_step_of_a_running_job_is_eligible() {
        let job = running_job(1);
        let step_id = job.steps[0].id;

        let advanced = satisfy_preliminary_preconditions(&job, step_id).unwrap();

        assert_eq!(advanced.state, JobStepState::PreconditionsSatisfied);
        assert_eq!(advanced.id, step_id);
    }

    #[test]
    fn a_later_pending_step_cannot_skip_an_earlier_unfinished_step() {
        let job = running_job(2);
        let second_step_id = job.steps[1].id;

        assert_eq!(
            satisfy_preliminary_preconditions(&job, second_step_id),
            Err(JobStepEligibilityError::NotCurrent)
        );
    }

    #[test]
    fn the_next_step_becomes_eligible_once_the_prior_step_is_succeeded() {
        let mut job = running_job(2);
        job.steps[0].state = JobStepState::Succeeded;
        let second_step_id = job.steps[1].id;

        let advanced = satisfy_preliminary_preconditions(&job, second_step_id).unwrap();

        assert_eq!(advanced.state, JobStepState::PreconditionsSatisfied);
    }

    #[test]
    fn a_non_running_job_cannot_advance_any_step() {
        let job = create_workflow(EndpointId::new(), 1).unwrap();
        let step_id = job.steps[0].id;

        assert_eq!(
            satisfy_preliminary_preconditions(&job, step_id),
            Err(JobStepEligibilityError::JobNotRunning)
        );
    }

    #[test]
    fn an_unknown_step_id_is_rejected() {
        let job = running_job(1);

        assert_eq!(
            satisfy_preliminary_preconditions(&job, JobStepId::new()),
            Err(JobStepEligibilityError::StepNotFound)
        );
    }

    #[test]
    fn a_non_pending_step_is_not_current() {
        let mut job = running_job(1);
        job.steps[0].state = JobStepState::PreconditionsSatisfied;
        let step_id = job.steps[0].id;

        assert_eq!(
            satisfy_preliminary_preconditions(&job, step_id),
            Err(JobStepEligibilityError::NotCurrent)
        );
    }
}
