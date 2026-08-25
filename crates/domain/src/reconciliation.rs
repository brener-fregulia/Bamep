//! Bamep Domain: uncertain-execution reconciliation (Issue #28 "[WP]
//! Reconcile interrupted Attempts safely";
//! `docs/specifications/m0-job-lifecycle-and-scheduling.md` "Attempt
//! lifecycle", "Reconciliation").
//!
//! Three pure decisions live here, deliberately separate from
//! `crate::action_evidence` (which never owns `AwaitingReconciliation`/
//! `Indeterminate` — see that module's docs) and from `crate::cancellation`
//! (which owns only `CancelAck` evidence):
//!
//! - [`mark_awaiting_reconciliation`] — `Dispatched`/`InProgress ->
//!   AwaitingReconciliation` when delivery/execution becomes uncertain
//!   (connection loss, Server restart, acknowledgment timeout).
//! - [`apply_status_report`] — the closed `StatusReport.known_state`
//!   evidence vocabulary applied against an `AwaitingReconciliation` Attempt.
//!   One `Unknown` never produces `Indeterminate`. `Cancelled` evidence
//!   completes cancellation only when `job` is already `Cancelling` —
//!   mirroring `crate::cancellation::apply_cancel_ack`'s identical authority
//!   guard, an Agent-reported `Cancelled` must never itself initiate Job
//!   cancellation.
//! - [`close_indeterminate`] — the explicit reconciliation decision that
//!   closes an `AwaitingReconciliation` Attempt as `Indeterminate` when the
//!   authoritative outcome cannot be established. The Agent can never reach
//!   this decision on its own; only an explicit operator/internal control
//!   path invokes it (`bamep_server::application::ReconciliationService::close_indeterminate`).
//!
//! All three functions perform no I/O and construct no
//! [`crate::events::AuditRecord`] — the caller decides what to audit,
//! mirroring `crate::action_evidence`/`crate::cancellation`'s Domain/
//! Application split.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::attempt::{Attempt, AttemptState};
use crate::events::DomainEvent;
use crate::job::{Job, JobState, JobStep, JobStepFailureReason, JobStepState};

// ---------------------------------------------------------------------
// Entering AwaitingReconciliation
// ---------------------------------------------------------------------

/// Decides whether `attempt` must enter `AwaitingReconciliation` because its
/// delivery/execution outcome has become uncertain
/// (`m0-job-lifecycle-and-scheduling.md` "Attempt lifecycle": "`Dispatched |
/// InProgress -> AwaitingReconciliation` — connection loss or Server restart
/// leaves execution/delivery uncertain"). `Some` only for `Dispatched`/
/// `InProgress` — already-`AwaitingReconciliation` and every terminal state
/// are left exactly as they are (`None`), so a caller can safely skip
/// persistence entirely on `None` without re-deriving that idempotency rule
/// itself.
pub fn mark_awaiting_reconciliation(attempt: &Attempt) -> Option<Attempt> {
    match attempt.state {
        AttemptState::Dispatched | AttemptState::InProgress => Some(Attempt {
            state: AttemptState::AwaitingReconciliation,
            ..*attempt
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// StatusReport evidence
// ---------------------------------------------------------------------

/// The closed `StatusReport.known_state` evidence vocabulary
/// (`m0-agent-protocol-contract.md` "Agent-action state vocabulary").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusReportEvidence {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

/// Successful application of reconciliation evidence against a freshly
/// locked Attempt/JobStep/Job: the resulting durable state, and whether this
/// is a terminal Attempt outcome that requires the caller to release the
/// Attempt's transient technical-resource reservation and commit a
/// destructive terminal-audit record — mirrors
/// `crate::action_evidence::ActionEvidenceApplied`/
/// `crate::cancellation::CancelAckApplied`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationApplied {
    pub attempt: Attempt,
    pub job_step: JobStep,
    pub job: Job,
    pub events: Vec<DomainEvent>,
    pub terminal: bool,
}

/// The full result of [`apply_status_report`]. There is no `Conflict`
/// variant, mirroring `crate::cancellation::CancelAckOutcome`: evidence
/// arriving once the Attempt has already left `AwaitingReconciliation`
/// (resolved by other evidence, or already `Indeterminate`) is legitimate
/// late/duplicate uncertainty-resolution evidence, not a wire-level
/// conflict — it is always a safe `NoOp` that never regresses or reopens the
/// already-committed outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ReconciliationOutcome {
    Applied(ReconciliationApplied),
    NoOp,
}

/// Decides `evidence`'s effect against `attempt`/`job_step`/`job`, all
/// already locked and freshly read by the caller under the same Attempt ->
/// JobStep -> Job order `apply_action_evidence`/`apply_cancel_ack` use
/// (`m0-job-lifecycle-and-scheduling.md` "Reconciliation"). Only ever
/// mutates an Attempt currently `AwaitingReconciliation` — this module, not
/// `crate::action_evidence`, owns that state
/// (`crate::action_evidence` module docs). `job_step` must be the JobStep
/// `attempt.job_step_id` identifies, already present in `job.steps`.
pub fn apply_status_report(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    evidence: StatusReportEvidence,
    now: DateTime<Utc>,
) -> ReconciliationOutcome {
    if attempt.state != AttemptState::AwaitingReconciliation {
        // Never regress/reopen an Attempt this module does not currently
        // own — including a still-`Dispatched`/`InProgress` Attempt (no
        // uncertainty was ever declared for it) and every already-terminal
        // outcome, `Indeterminate` included.
        return ReconciliationOutcome::NoOp;
    }
    match evidence {
        // Agent-side `Accepted` and `Running` deliberately collapse into
        // Attempt `InProgress` (`m0-job-lifecycle-and-scheduling.md` "Agent
        // Protocol mapping").
        StatusReportEvidence::Accepted | StatusReportEvidence::Running => {
            ReconciliationOutcome::Applied(ReconciliationApplied {
                attempt: Attempt {
                    state: AttemptState::InProgress,
                    ..*attempt
                },
                job_step: job_step.clone(),
                job: job.clone(),
                events: vec![],
                terminal: false,
            })
        }
        StatusReportEvidence::Succeeded => {
            ReconciliationOutcome::Applied(terminal_success(job, job_step, attempt, now))
        }
        StatusReportEvidence::Failed => {
            ReconciliationOutcome::Applied(terminal_failure(job, job_step, attempt, now))
        }
        // Mirrors `crate::cancellation::apply_cancel_ack`'s identical
        // authority guard ("the Agent can never initiate Job cancellation"):
        // a `StatusReport{Cancelled}` must never itself create cancellation
        // intent. `CancelAction` is Server -> Agent only and is sent only
        // while `Cancelling` (Issue #27), so there is no legitimate way for
        // this evidence to be authoritative against a merely `Running` Job —
        // untrusted wire input the Server does not act on. The Attempt
        // remains `AwaitingReconciliation`, exactly like any other evidence
        // this module does not currently accept.
        StatusReportEvidence::Cancelled if job.state == JobState::Cancelling => {
            ReconciliationOutcome::Applied(terminal_cancelled(job, job_step, attempt, now))
        }
        StatusReportEvidence::Cancelled => ReconciliationOutcome::NoOp,
        // One StatusReport{Unknown} never automatically produces
        // Indeterminate (`m0-job-lifecycle-and-scheduling.md`: "One
        // StatusReport{Unknown} does not automatically produce
        // Indeterminate") — the Attempt simply remains AwaitingReconciliation.
        StatusReportEvidence::Unknown => ReconciliationOutcome::NoOp,
    }
}

/// `StatusReport{Succeeded}`: Attempt -> `Succeeded`, current JobStep ->
/// `Succeeded`, composing with a Job already `Cancelling` exactly like
/// `crate::action_evidence`'s identical composition — cancellation intent
/// already owns the workflow outcome, so the Job ends `Cancelled` (with
/// `JobCancelled`) rather than `Succeeded`, regardless of remaining
/// JobSteps.
fn terminal_success(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    now: DateTime<Utc>,
) -> ReconciliationApplied {
    let succeeded_step = JobStep {
        state: JobStepState::Succeeded,
        ..job_step.clone()
    };
    let updated_steps = replace_step(&job.steps, &succeeded_step);

    let mut events = Vec::new();
    let job_state = if job.state == JobState::Cancelling {
        events.push(DomainEvent::JobCancelled {
            event_id: Uuid::new_v4(),
            job_id: job.id,
            endpoint_id: job.endpoint_id,
            occurred_at: now,
        });
        JobState::Cancelled
    } else {
        let every_step_succeeded = updated_steps
            .iter()
            .all(|s| s.state == JobStepState::Succeeded);
        if every_step_succeeded {
            events.push(DomainEvent::JobSucceeded {
                event_id: Uuid::new_v4(),
                job_id: job.id,
                endpoint_id: job.endpoint_id,
                occurred_at: now,
            });
            JobState::Succeeded
        } else {
            JobState::Running
        }
    };

    ReconciliationApplied {
        attempt: Attempt {
            state: AttemptState::Succeeded,
            ..*attempt
        },
        job_step: succeeded_step,
        job: Job {
            state: job_state,
            steps: updated_steps,
            ..job.clone()
        },
        events,
        terminal: true,
    }
}

/// `StatusReport{Failed}`: Attempt -> `Failed`, current JobStep ->
/// `Failed{ExecutionFailed}` — the same failure reason `ActionResult{Failed}`
/// uses, since this is the identical execution-failure fact, only learned
/// through reconciliation instead of direct evidence. Composes with a Job
/// already `Cancelling` exactly like `crate::action_evidence::terminal_failure`.
fn terminal_failure(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    now: DateTime<Utc>,
) -> ReconciliationApplied {
    let failed_step = JobStep {
        state: JobStepState::Failed,
        failure_reason: Some(JobStepFailureReason::ExecutionFailed),
        ..job_step.clone()
    };
    let cancelling = job.state == JobState::Cancelling;
    let final_job = Job {
        state: if cancelling {
            JobState::Cancelled
        } else {
            JobState::Failed
        },
        steps: replace_step(&job.steps, &failed_step),
        ..job.clone()
    };
    let job_step_failed = DomainEvent::JobStepFailed {
        event_id: Uuid::new_v4(),
        job_id: job.id,
        job_step_id: failed_step.id,
        endpoint_id: job.endpoint_id,
        occurred_at: now,
    };
    let job_terminal_event = if cancelling {
        DomainEvent::JobCancelled {
            event_id: Uuid::new_v4(),
            job_id: job.id,
            endpoint_id: job.endpoint_id,
            occurred_at: now,
        }
    } else {
        DomainEvent::JobFailed {
            event_id: Uuid::new_v4(),
            job_id: job.id,
            endpoint_id: job.endpoint_id,
            occurred_at: now,
        }
    };
    ReconciliationApplied {
        attempt: Attempt {
            state: AttemptState::Failed,
            ..*attempt
        },
        job_step: failed_step,
        job: final_job,
        events: vec![job_step_failed, job_terminal_event],
        terminal: true,
    }
}

/// `StatusReport{Cancelled}` while `job` is already `Cancelling`: Attempt ->
/// `Cancelled`, current JobStep -> `Cancelled`, Job -> `Cancelled` with
/// exactly one `JobCancelled` event — mirrors `crate::cancellation::cancel_terminal`.
/// Callers must verify `job.state == JobState::Cancelling` first
/// ([`apply_status_report`]'s only call site already does); this function
/// itself does not re-check it.
fn terminal_cancelled(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    now: DateTime<Utc>,
) -> ReconciliationApplied {
    let cancelled_step = JobStep {
        state: JobStepState::Cancelled,
        ..job_step.clone()
    };
    let cancelled_job = Job {
        state: JobState::Cancelled,
        steps: replace_step(&job.steps, &cancelled_step),
        ..job.clone()
    };
    let event = DomainEvent::JobCancelled {
        event_id: Uuid::new_v4(),
        job_id: job.id,
        endpoint_id: job.endpoint_id,
        occurred_at: now,
    };
    ReconciliationApplied {
        attempt: Attempt {
            state: AttemptState::Cancelled,
            ..*attempt
        },
        job_step: cancelled_step,
        job: cancelled_job,
        events: vec![event],
        terminal: true,
    }
}

// ---------------------------------------------------------------------
// Explicit Indeterminate closure
// ---------------------------------------------------------------------

/// Outcome of [`close_indeterminate`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum CloseIndeterminateOutcome {
    Applied(ReconciliationApplied),
    /// `attempt.state` was already `Indeterminate` — idempotent no-op:
    /// repeated explicit closure must not duplicate the `AttemptIndeterminate`
    /// event/audit or re-derive the JobStep/Job consequence.
    AlreadyIndeterminate,
    /// `attempt.state` was neither `AwaitingReconciliation` nor
    /// `Indeterminate` — closure is valid only from the uncertain state
    /// (`m0-job-lifecycle-and-scheduling.md` "Attempt lifecycle":
    /// "`AwaitingReconciliation -> Indeterminate`").
    NotEligible,
}

/// Decides the explicit reconciliation decision that closes `attempt`
/// `Indeterminate` when its authoritative outcome cannot be established
/// (`m0-job-lifecycle-and-scheduling.md` "Reconciliation": "A destructive
/// Attempt resolved to `Indeterminate` requires an explicit recorded operator
/// decision"). Valid only from `AwaitingReconciliation`. The current JobStep
/// closes `Failed{ReconciliationIndeterminate}` and exactly one
/// `AttemptIndeterminate` event is always emitted alongside the required
/// `JobStepFailed`. Composes with a Job already `Cancelling` exactly like
/// every other terminal reconciliation outcome: cancellation intent already
/// owns the workflow outcome, so the Job ends `Cancelled` (with
/// `JobCancelled`, not `JobFailed`) while the real Attempt/JobStep
/// `Indeterminate`/`ReconciliationIndeterminate` outcome is preserved exactly,
/// never rewritten to a fabricated `Cancelled`.
pub fn close_indeterminate(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    now: DateTime<Utc>,
) -> CloseIndeterminateOutcome {
    match attempt.state {
        AttemptState::AwaitingReconciliation => {
            let failed_step = JobStep {
                state: JobStepState::Failed,
                failure_reason: Some(JobStepFailureReason::ReconciliationIndeterminate),
                ..job_step.clone()
            };
            let cancelling = job.state == JobState::Cancelling;
            let final_job = Job {
                state: if cancelling {
                    JobState::Cancelled
                } else {
                    JobState::Failed
                },
                steps: replace_step(&job.steps, &failed_step),
                ..job.clone()
            };
            let indeterminate_event = DomainEvent::AttemptIndeterminate {
                event_id: Uuid::new_v4(),
                job_id: job.id,
                job_step_id: failed_step.id,
                attempt_id: attempt.id,
                endpoint_id: job.endpoint_id,
                occurred_at: now,
            };
            let job_step_failed = DomainEvent::JobStepFailed {
                event_id: Uuid::new_v4(),
                job_id: job.id,
                job_step_id: failed_step.id,
                endpoint_id: job.endpoint_id,
                occurred_at: now,
            };
            let job_terminal_event = if cancelling {
                DomainEvent::JobCancelled {
                    event_id: Uuid::new_v4(),
                    job_id: job.id,
                    endpoint_id: job.endpoint_id,
                    occurred_at: now,
                }
            } else {
                DomainEvent::JobFailed {
                    event_id: Uuid::new_v4(),
                    job_id: job.id,
                    endpoint_id: job.endpoint_id,
                    occurred_at: now,
                }
            };
            CloseIndeterminateOutcome::Applied(ReconciliationApplied {
                attempt: Attempt {
                    state: AttemptState::Indeterminate,
                    ..*attempt
                },
                job_step: failed_step,
                job: final_job,
                events: vec![indeterminate_event, job_step_failed, job_terminal_event],
                terminal: true,
            })
        }
        AttemptState::Indeterminate => CloseIndeterminateOutcome::AlreadyIndeterminate,
        _ => CloseIndeterminateOutcome::NotEligible,
    }
}

fn replace_step(steps: &[JobStep], updated: &JobStep) -> Vec<JobStep> {
    steps
        .iter()
        .map(|s| {
            if s.id == updated.id {
                updated.clone()
            } else {
                s.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::{ActionId, AttemptId};
    use crate::job::{JobId, JobStepId};
    use crate::EndpointId;

    fn job_step(id: JobStepId, job_id: JobId, order: i32, state: JobStepState) -> JobStep {
        JobStep {
            id,
            job_id,
            order,
            state,
            destructive_intent: None,
            failure_reason: None,
        }
    }

    fn running_job(step_count: usize, dispatching_index: usize) -> (Job, Vec<JobStepId>) {
        let job_id = JobId::new();
        let endpoint_id = EndpointId::new();
        let step_ids: Vec<JobStepId> = (0..step_count).map(|_| JobStepId::new()).collect();
        let steps = step_ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let state = match i.cmp(&dispatching_index) {
                    std::cmp::Ordering::Less => JobStepState::Succeeded,
                    std::cmp::Ordering::Equal => JobStepState::Dispatching,
                    std::cmp::Ordering::Greater => JobStepState::Pending,
                };
                job_step(*id, job_id, i as i32, state)
            })
            .collect();
        (
            Job {
                id: job_id,
                endpoint_id,
                state: JobState::Running,
                steps,
            },
            step_ids,
        )
    }

    fn cancelling_job(step_count: usize, dispatching_index: usize) -> (Job, Vec<JobStepId>) {
        let (mut job, ids) = running_job(step_count, dispatching_index);
        job.state = JobState::Cancelling;
        (job, ids)
    }

    fn attempt_for(job_step_id: JobStepId, state: AttemptState) -> Attempt {
        Attempt {
            id: AttemptId::new(),
            job_step_id,
            action_id: ActionId::new(),
            state,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    // -- mark_awaiting_reconciliation --------------------------------------

    #[test]
    fn dispatched_and_in_progress_enter_awaiting_reconciliation() {
        for state in [AttemptState::Dispatched, AttemptState::InProgress] {
            let attempt = attempt_for(JobStepId::new(), state);
            let result = mark_awaiting_reconciliation(&attempt).unwrap();
            assert_eq!(result.state, AttemptState::AwaitingReconciliation);
            assert_eq!(result.id, attempt.id);
            assert_eq!(result.action_id, attempt.action_id);
        }
    }

    #[test]
    fn already_awaiting_reconciliation_is_a_no_op() {
        let attempt = attempt_for(JobStepId::new(), AttemptState::AwaitingReconciliation);
        assert_eq!(mark_awaiting_reconciliation(&attempt), None);
    }

    #[test]
    fn every_terminal_state_is_untouched() {
        for state in [
            AttemptState::Succeeded,
            AttemptState::Failed,
            AttemptState::Cancelled,
            AttemptState::Rejected,
            AttemptState::Indeterminate,
        ] {
            let attempt = attempt_for(JobStepId::new(), state);
            assert_eq!(
                mark_awaiting_reconciliation(&attempt),
                None,
                "state {state:?} must never be regressed to AwaitingReconciliation"
            );
        }
    }

    // -- apply_status_report ------------------------------------------------

    #[test]
    fn accepted_and_running_recover_in_progress() {
        for evidence in [
            StatusReportEvidence::Accepted,
            StatusReportEvidence::Running,
        ] {
            let (job, ids) = running_job(1, 0);
            let step = job.steps[0].clone();
            let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

            let outcome = apply_status_report(&job, &step, &attempt, evidence, now());
            let ReconciliationOutcome::Applied(applied) = outcome else {
                panic!("expected Applied for {evidence:?}")
            };
            assert_eq!(applied.attempt.state, AttemptState::InProgress);
            assert_eq!(applied.job.state, JobState::Running);
            assert!(applied.events.is_empty());
            assert!(!applied.terminal);
        }
    }

    #[test]
    fn succeeded_on_final_step_reaches_job_succeeded() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome = apply_status_report(
            &job,
            &step,
            &attempt,
            StatusReportEvidence::Succeeded,
            now(),
        );
        let ReconciliationOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Succeeded);
        assert_eq!(applied.job_step.state, JobStepState::Succeeded);
        assert_eq!(applied.job.state, JobState::Succeeded);
        assert!(applied.terminal);
        assert_eq!(applied.events.len(), 1);
        assert_eq!(applied.events[0].event_type(), "JobSucceeded");
    }

    #[test]
    fn failed_uses_execution_failed_reason() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome =
            apply_status_report(&job, &step, &attempt, StatusReportEvidence::Failed, now());
        let ReconciliationOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Failed);
        assert_eq!(applied.job_step.state, JobStepState::Failed);
        assert_eq!(
            applied.job_step.failure_reason,
            Some(JobStepFailureReason::ExecutionFailed)
        );
        assert_eq!(applied.job.state, JobState::Failed);
        assert!(applied.events.iter().any(|e| e.event_type() == "JobFailed"));
    }

    #[test]
    fn cancelled_while_cancelling_reaches_full_terminal_cancellation() {
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome = apply_status_report(
            &job,
            &step,
            &attempt,
            StatusReportEvidence::Cancelled,
            now(),
        );
        let ReconciliationOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Cancelled);
        assert_eq!(applied.job_step.state, JobStepState::Cancelled);
        assert_eq!(applied.job.state, JobState::Cancelled);
        assert_eq!(applied.events.len(), 1);
        assert_eq!(applied.events[0].event_type(), "JobCancelled");
    }

    #[test]
    fn duplicate_late_cancelled_after_terminal_commit_never_overwrites_it() {
        // Late/duplicate StatusReport{Cancelled} arriving after the Attempt
        // already committed Cancelled (even under a Cancelling Job) must
        // never re-apply or duplicate the JobCancelled event.
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::Cancelled);

        let outcome = apply_status_report(
            &job,
            &step,
            &attempt,
            StatusReportEvidence::Cancelled,
            now(),
        );
        assert_eq!(outcome, ReconciliationOutcome::NoOp);
    }

    #[test]
    fn cancelled_against_a_running_job_is_a_no_op_and_never_initiates_cancellation() {
        // The Agent can never initiate Job cancellation — mirrors
        // crate::cancellation's identical authority boundary. Even though an
        // Attempt is genuinely AwaitingReconciliation, an unsolicited
        // StatusReport{Cancelled} must not move a merely Running Job.
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome = apply_status_report(
            &job,
            &step,
            &attempt,
            StatusReportEvidence::Cancelled,
            now(),
        );
        assert_eq!(outcome, ReconciliationOutcome::NoOp);
    }

    #[test]
    fn unknown_never_produces_indeterminate_and_remains_uncertain() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome =
            apply_status_report(&job, &step, &attempt, StatusReportEvidence::Unknown, now());
        assert_eq!(outcome, ReconciliationOutcome::NoOp);
    }

    #[test]
    fn repeated_unknown_stays_idempotently_uncertain() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        for _ in 0..3 {
            let outcome =
                apply_status_report(&job, &step, &attempt, StatusReportEvidence::Unknown, now());
            assert_eq!(outcome, ReconciliationOutcome::NoOp);
        }
    }

    #[test]
    fn evidence_against_a_not_yet_uncertain_attempt_is_a_no_op() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        for state in [AttemptState::Dispatched, AttemptState::InProgress] {
            let attempt = attempt_for(ids[0], state);
            for evidence in [
                StatusReportEvidence::Accepted,
                StatusReportEvidence::Running,
                StatusReportEvidence::Succeeded,
                StatusReportEvidence::Failed,
                StatusReportEvidence::Cancelled,
                StatusReportEvidence::Unknown,
            ] {
                assert_eq!(
                    apply_status_report(&job, &step, &attempt, evidence, now()),
                    ReconciliationOutcome::NoOp,
                    "attempt state {state:?} evidence {evidence:?}"
                );
            }
        }
    }

    #[test]
    fn evidence_never_regresses_an_already_terminal_attempt() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        for state in [
            AttemptState::Succeeded,
            AttemptState::Failed,
            AttemptState::Cancelled,
            AttemptState::Rejected,
            AttemptState::Indeterminate,
        ] {
            let attempt = attempt_for(ids[0], state);
            for evidence in [
                StatusReportEvidence::Succeeded,
                StatusReportEvidence::Failed,
                StatusReportEvidence::Cancelled,
            ] {
                assert_eq!(
                    apply_status_report(&job, &step, &attempt, evidence, now()),
                    ReconciliationOutcome::NoOp,
                    "state {state:?} evidence {evidence:?}"
                );
            }
        }
    }

    #[test]
    fn succeeded_while_cancelling_still_ends_the_job_cancelled() {
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome = apply_status_report(
            &job,
            &step,
            &attempt,
            StatusReportEvidence::Succeeded,
            now(),
        );
        let ReconciliationOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Succeeded);
        assert_eq!(applied.job.state, JobState::Cancelled);
        assert_eq!(applied.events.len(), 1);
        assert_eq!(applied.events[0].event_type(), "JobCancelled");
    }

    #[test]
    fn failed_while_cancelling_still_ends_the_job_cancelled() {
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome =
            apply_status_report(&job, &step, &attempt, StatusReportEvidence::Failed, now());
        let ReconciliationOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Failed);
        assert_eq!(applied.job.state, JobState::Cancelled);
        assert!(
            applied
                .events
                .iter()
                .any(|e| e.event_type() == "JobCancelled"),
            "JobCancelled must be emitted, never JobFailed"
        );
        assert!(!applied.events.iter().any(|e| e.event_type() == "JobFailed"));
    }

    // -- close_indeterminate -------------------------------------------------

    #[test]
    fn close_indeterminate_from_awaiting_reconciliation_applies_full_consequence() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome = close_indeterminate(&job, &step, &attempt, now());
        let CloseIndeterminateOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Indeterminate);
        assert_eq!(applied.job_step.state, JobStepState::Failed);
        assert_eq!(
            applied.job_step.failure_reason,
            Some(JobStepFailureReason::ReconciliationIndeterminate)
        );
        assert_eq!(applied.job.state, JobState::Failed);
        assert!(applied.terminal);
        assert_eq!(applied.events.len(), 3);
        assert!(applied
            .events
            .iter()
            .any(|e| e.event_type() == "AttemptIndeterminate"));
        assert!(applied
            .events
            .iter()
            .any(|e| e.event_type() == "JobStepFailed"));
        assert!(applied.events.iter().any(|e| e.event_type() == "JobFailed"));
        let attempt_indeterminate_event = applied
            .events
            .iter()
            .find(|e| e.event_type() == "AttemptIndeterminate")
            .unwrap();
        assert_eq!(attempt_indeterminate_event.attempt_id(), Some(attempt.id));
        assert_eq!(
            attempt_indeterminate_event.job_step_id(),
            Some(applied.job_step.id)
        );
    }

    #[test]
    fn close_indeterminate_while_cancelling_completes_the_job_as_cancelled() {
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::AwaitingReconciliation);

        let outcome = close_indeterminate(&job, &step, &attempt, now());
        let CloseIndeterminateOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Indeterminate);
        assert_eq!(applied.job_step.state, JobStepState::Failed);
        assert_eq!(
            applied.job_step.failure_reason,
            Some(JobStepFailureReason::ReconciliationIndeterminate)
        );
        assert_eq!(
            applied.job.state,
            JobState::Cancelled,
            "cancellation intent owns the Job outcome"
        );
        assert!(applied
            .events
            .iter()
            .any(|e| e.event_type() == "JobCancelled"));
        assert!(!applied.events.iter().any(|e| e.event_type() == "JobFailed"));
    }

    #[test]
    fn repeated_close_indeterminate_is_idempotent() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], AttemptState::Indeterminate);

        assert_eq!(
            close_indeterminate(&job, &step, &attempt, now()),
            CloseIndeterminateOutcome::AlreadyIndeterminate
        );
    }

    #[test]
    fn close_indeterminate_is_not_eligible_from_any_other_state() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        for state in [
            AttemptState::Dispatched,
            AttemptState::InProgress,
            AttemptState::Succeeded,
            AttemptState::Failed,
            AttemptState::Cancelled,
            AttemptState::Rejected,
        ] {
            let attempt = attempt_for(ids[0], state);
            assert_eq!(
                close_indeterminate(&job, &step, &attempt, now()),
                CloseIndeterminateOutcome::NotEligible,
                "state {state:?}"
            );
        }
    }
}
