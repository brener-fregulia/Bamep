//! Bamep Domain: Job cancellation (Issue #27 "[WP] Execute Job cancellation
//! end to end"; `docs/specifications/m0-job-lifecycle-and-scheduling.md` "Job
//! lifecycle", "Attempt lifecycle").
//!
//! Two pure decisions live here:
//!
//! - [`request_cancellation`] — the operator/internal cancellation-request
//!   control path: `Running -> Cancelling` when an active/uncertain Attempt
//!   exists, or `Running -> Cancelled` immediately when none does. Scoped to
//!   an already-`Running`/`Cancelling`/terminal Job — a `Pending` Job is
//!   [`CancellationRequestError::NotEligible`] (this WP does not implement
//!   `Pending`-Job cancellation).
//! - [`apply_cancel_ack`] — the four `CancelAck` outcomes against the
//!   correlated Attempt/JobStep/Job, already locked and freshly read by the
//!   caller, mirroring `crate::action_evidence::apply_action_evidence`'s
//!   lock/decide/persist split.
//!
//! Both functions perform no I/O and construct no [`crate::events::AuditRecord`] —
//! the caller (`bamep_server::application::CancellationService`) decides what
//! to audit, mirroring `bamep_domain::final_dispatch`'s Domain/Application
//! split.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::attempt::{ActionId, Attempt, AttemptId, AttemptState};
use crate::events::DomainEvent;
use crate::job::{Job, JobState, JobStep, JobStepState};

// ---------------------------------------------------------------------
// Cancellation request
// ---------------------------------------------------------------------

/// Rejection from [`request_cancellation`]. Never represents a partial
/// mutation — a rejected call leaves `job` exactly as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CancellationRequestError {
    /// `job.state` is `Pending` — this WP is scoped to ACTIVE Job
    /// cancellation; a separate `Pending`-Job cancellation feature is not
    /// implemented here.
    #[error("job is not eligible for a cancellation request")]
    NotEligible,
}

/// Outcome of [`request_cancellation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationRequestOutcome {
    /// `Running -> Cancelling`: `active_attempt` was `Some`. The caller must
    /// atomically persist `job` plus the required operator cancellation
    /// audit, and — only after that transaction commits — attempt
    /// `CancelAction{action_id}` for `attempt_id`.
    EnteredCancelling {
        job: Job,
        attempt_id: AttemptId,
        action_id: ActionId,
    },
    /// `Running -> Cancelled` immediately: `active_attempt` was `None`. The
    /// caller must atomically persist `job`, `event`, and the required
    /// operator cancellation audit. No `CancelAction` is ever sent for this
    /// outcome.
    CompletedImmediately { job: Job, event: DomainEvent },
    /// `job.state` was already `Cancelling` — idempotent no-op: no new
    /// audit, no repeated `CancelAction`.
    AlreadyCancelling,
    /// `job.state` was already terminal (`Succeeded`/`Failed`/`Cancelled`) —
    /// no-op.
    AlreadyTerminal,
}

/// Decides an operator/internal cancellation request against `job`, given
/// `active_attempt` — the JobStep-current Attempt in `Dispatched`,
/// `InProgress`, or `AwaitingReconciliation`, if any, already resolved by the
/// caller under lock (`m0-job-lifecycle-and-scheduling.md` "Job lifecycle";
/// Issue #27 "Active Attempt selection"). Pure — performs no I/O and does not
/// itself verify that `active_attempt`, when `Some`, actually belongs to
/// `job`; that correlation is the caller's responsibility, exactly like
/// `crate::action_evidence::apply_action_evidence` trusts its caller's
/// `job_step`/`job` correlation.
pub fn request_cancellation(
    job: &Job,
    active_attempt: Option<&Attempt>,
    now: DateTime<Utc>,
) -> Result<CancellationRequestOutcome, CancellationRequestError> {
    match job.state {
        JobState::Running => match active_attempt {
            Some(attempt) => Ok(CancellationRequestOutcome::EnteredCancelling {
                job: Job {
                    state: JobState::Cancelling,
                    ..job.clone()
                },
                attempt_id: attempt.id,
                action_id: attempt.action_id,
            }),
            None => {
                let event = DomainEvent::JobCancelled {
                    event_id: Uuid::new_v4(),
                    job_id: job.id,
                    endpoint_id: job.endpoint_id,
                    occurred_at: now,
                };
                Ok(CancellationRequestOutcome::CompletedImmediately {
                    job: Job {
                        state: JobState::Cancelled,
                        ..job.clone()
                    },
                    event,
                })
            }
        },
        JobState::Cancelling => Ok(CancellationRequestOutcome::AlreadyCancelling),
        JobState::Succeeded | JobState::Failed | JobState::Cancelled => {
            Ok(CancellationRequestOutcome::AlreadyTerminal)
        }
        JobState::Pending => Err(CancellationRequestError::NotEligible),
    }
}

// ---------------------------------------------------------------------
// CancelAck evidence
// ---------------------------------------------------------------------

/// The four `CancelAck` outcomes #27 applies
/// (`m0-agent-protocol-contract.md` "Message types").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelAckEvidence {
    Cancelled,
    AlreadyCompleted,
    CannotCancel,
    Unknown,
}

/// Successful application of `evidence` against a freshly locked Attempt/
/// JobStep/Job: the resulting durable state, and whether this is a terminal
/// Attempt outcome that requires the caller to release the Attempt's
/// transient technical-resource reservation and commit a destructive
/// terminal-audit record — mirrors `crate::action_evidence::ActionEvidenceApplied`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelAckApplied {
    pub attempt: Attempt,
    pub job_step: JobStep,
    pub job: Job,
    pub events: Vec<DomainEvent>,
    pub terminal: bool,
}

/// The full result of [`apply_cancel_ack`]. Unlike
/// `crate::action_evidence::ActionEvidenceOutcome`, there is no `Conflict`
/// variant: `CancelAck` never overwrites an already-committed authoritative
/// outcome, but arriving after one is legitimate uncertainty-resolution
/// evidence, not a wire-level conflict — every non-`Applied` case is a safe
/// `NoOp`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum CancelAckOutcome {
    Applied(CancelAckApplied),
    NoOp,
}

/// Decides `evidence`'s effect against `attempt`/`job_step`/`job`, all
/// already locked and freshly read by the caller under the same Attempt ->
/// JobStep -> Job order `apply_action_evidence` uses
/// (`m0-job-lifecycle-and-scheduling.md` "Job lifecycle"; Issue #27 "Lock
/// order / concurrency"). `job_step` must be the JobStep `attempt.job_step_id`
/// identifies, already present in `job.steps`.
pub fn apply_cancel_ack(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    evidence: CancelAckEvidence,
    now: DateTime<Utc>,
) -> CancelAckOutcome {
    match evidence {
        CancelAckEvidence::Cancelled => match attempt.state {
            AttemptState::Dispatched
            | AttemptState::InProgress
            | AttemptState::AwaitingReconciliation => {
                CancelAckOutcome::Applied(cancel_terminal(job, job_step, attempt, now))
            }
            // Duplicate CancelAck{Cancelled} against an already-Cancelled
            // Attempt is idempotent; any other already-terminal state means
            // the Attempt resolved a different way first and must never be
            // overwritten with `Cancelled`.
            _ => CancelAckOutcome::NoOp,
        },
        CancelAckEvidence::AlreadyCompleted => match attempt.state {
            AttemptState::Dispatched | AttemptState::InProgress => {
                CancelAckOutcome::Applied(CancelAckApplied {
                    attempt: Attempt {
                        state: AttemptState::AwaitingReconciliation,
                        ..*attempt
                    },
                    job_step: job_step.clone(),
                    job: job.clone(),
                    events: vec![],
                    terminal: false,
                })
            }
            // Already uncertain — idempotent no-op.
            AttemptState::AwaitingReconciliation => CancelAckOutcome::NoOp,
            // The Attempt already reached its own authoritative terminal
            // result through unrelated evidence — preserve it exactly, never
            // overwrite with `Cancelled`. If the Job is still `Cancelling`
            // (meaning no other active/uncertain Attempt remains for this
            // linear workflow — the workflow admits only one active JobStep
            // at a time, and this terminal Attempt was it), complete Job
            // cancellation now instead of leaving it stranded `Cancelling`.
            _ if job.state == JobState::Cancelling => {
                CancelAckOutcome::Applied(complete_cancellation_now(job, job_step, attempt, now))
            }
            _ => CancelAckOutcome::NoOp,
        },
        // `CannotCancel` never mutates durable state: it does not establish
        // success/failure/cancellation, so the Server keeps waiting for the
        // actual `ActionResult`.
        CancelAckEvidence::CannotCancel => CancelAckOutcome::NoOp,
        CancelAckEvidence::Unknown => match attempt.state {
            AttemptState::Dispatched | AttemptState::InProgress => {
                CancelAckOutcome::Applied(CancelAckApplied {
                    attempt: Attempt {
                        state: AttemptState::AwaitingReconciliation,
                        ..*attempt
                    },
                    job_step: job_step.clone(),
                    job: job.clone(),
                    events: vec![],
                    terminal: false,
                })
            }
            // Repeated Unknown while already AwaitingReconciliation is
            // idempotent.
            AttemptState::AwaitingReconciliation => CancelAckOutcome::NoOp,
            // Already resolved terminally — Unknown never regresses it.
            _ => CancelAckOutcome::NoOp,
        },
    }
}

/// `CancelAck{Cancelled}` against a cancellation-relevant active/uncertain
/// Attempt: Attempt -> `Cancelled`, current JobStep -> `Cancelled`, Job ->
/// `Cancelled`, exactly one `JobCancelled` event. No `AttemptCancelled` or
/// `JobStepCancelled` event is invented.
fn cancel_terminal(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    now: DateTime<Utc>,
) -> CancelAckApplied {
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
    CancelAckApplied {
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

/// Completes Job cancellation for a Job still `Cancelling` whose sole
/// active/uncertain Attempt has already reached its own authoritative
/// terminal result through unrelated evidence: Job -> `Cancelled`, exactly
/// one `JobCancelled` event. Attempt/JobStep state is preserved exactly —
/// never overwritten with `Cancelled`.
fn complete_cancellation_now(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    now: DateTime<Utc>,
) -> CancelAckApplied {
    let event = DomainEvent::JobCancelled {
        event_id: Uuid::new_v4(),
        job_id: job.id,
        endpoint_id: job.endpoint_id,
        occurred_at: now,
    };
    CancelAckApplied {
        attempt: *attempt,
        job_step: job_step.clone(),
        job: Job {
            state: JobState::Cancelled,
            ..job.clone()
        },
        events: vec![event],
        terminal: true,
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

    // -- request_cancellation ------------------------------------------

    #[test]
    fn running_with_active_attempt_enters_cancelling() {
        let (job, ids) = running_job(1, 0);
        let attempt = attempt_for(ids[0], AttemptState::InProgress);

        let outcome = request_cancellation(&job, Some(&attempt), now()).unwrap();
        match outcome {
            CancellationRequestOutcome::EnteredCancelling {
                job: cancelling_job,
                attempt_id,
                action_id,
            } => {
                assert_eq!(cancelling_job.state, JobState::Cancelling);
                assert_eq!(attempt_id, attempt.id);
                assert_eq!(action_id, attempt.action_id);
                // Untouched JobSteps are preserved exactly.
                assert_eq!(cancelling_job.steps, job.steps);
            }
            other => panic!("expected EnteredCancelling, got {other:?}"),
        }
    }

    #[test]
    fn running_with_no_active_attempt_completes_immediately() {
        let job = crate::job::create_workflow(EndpointId::new(), 2).unwrap();
        let running = crate::job::admit_job(&job, now()).unwrap().job;

        let outcome = request_cancellation(&running, None, now()).unwrap();
        match outcome {
            CancellationRequestOutcome::CompletedImmediately { job, event } => {
                assert_eq!(job.state, JobState::Cancelled);
                // Untouched Pending steps are never fabricated into
                // Cancelled.
                assert!(job.steps.iter().all(|s| s.state == JobStepState::Pending));
                assert_eq!(event.event_type(), "JobCancelled");
                assert_eq!(event.job_id(), Some(job.id));
            }
            other => panic!("expected CompletedImmediately, got {other:?}"),
        }
    }

    #[test]
    fn already_cancelling_is_idempotent_no_op() {
        let (mut job, _) = running_job(1, 0);
        job.state = JobState::Cancelling;
        assert_eq!(
            request_cancellation(&job, None, now()).unwrap(),
            CancellationRequestOutcome::AlreadyCancelling
        );
    }

    #[test]
    fn terminal_job_states_are_no_ops() {
        for state in [JobState::Succeeded, JobState::Failed, JobState::Cancelled] {
            let (mut job, _) = running_job(1, 0);
            job.state = state;
            assert_eq!(
                request_cancellation(&job, None, now()).unwrap(),
                CancellationRequestOutcome::AlreadyTerminal
            );
        }
    }

    #[test]
    fn pending_job_is_not_eligible() {
        let job = crate::job::create_workflow(EndpointId::new(), 1).unwrap();
        assert_eq!(
            request_cancellation(&job, None, now()),
            Err(CancellationRequestError::NotEligible)
        );
    }

    // -- apply_cancel_ack -------------------------------------------------

    fn cancelling_job_with_dispatching_step(
        attempt_state: AttemptState,
    ) -> (Job, JobStep, Attempt) {
        let (mut job, ids) = running_job(1, 0);
        job.state = JobState::Cancelling;
        let step = job.steps[0].clone();
        let attempt = attempt_for(ids[0], attempt_state);
        (job, step, attempt)
    }

    #[test]
    fn cancelled_ack_from_dispatched_reaches_full_terminal_cancellation() {
        for state in [
            AttemptState::Dispatched,
            AttemptState::InProgress,
            AttemptState::AwaitingReconciliation,
        ] {
            let (job, step, attempt) = cancelling_job_with_dispatching_step(state);
            let outcome =
                apply_cancel_ack(&job, &step, &attempt, CancelAckEvidence::Cancelled, now());
            let CancelAckOutcome::Applied(applied) = outcome else {
                panic!("expected Applied from {state:?}")
            };
            assert_eq!(applied.attempt.state, AttemptState::Cancelled);
            assert_eq!(applied.job_step.state, JobStepState::Cancelled);
            assert_eq!(applied.job.state, JobState::Cancelled);
            assert!(applied.terminal);
            assert_eq!(applied.events.len(), 1);
            assert_eq!(applied.events[0].event_type(), "JobCancelled");
        }
    }

    #[test]
    fn duplicate_cancelled_ack_against_cancelled_is_a_no_op() {
        let (job, step, attempt) = cancelling_job_with_dispatching_step(AttemptState::Cancelled);
        let outcome = apply_cancel_ack(&job, &step, &attempt, CancelAckEvidence::Cancelled, now());
        assert_eq!(outcome, CancelAckOutcome::NoOp);
    }

    #[test]
    fn cancelled_ack_never_overwrites_a_different_already_terminal_outcome() {
        for state in [
            AttemptState::Succeeded,
            AttemptState::Failed,
            AttemptState::Rejected,
        ] {
            let (job, step, attempt) = cancelling_job_with_dispatching_step(state);
            let outcome =
                apply_cancel_ack(&job, &step, &attempt, CancelAckEvidence::Cancelled, now());
            assert_eq!(outcome, CancelAckOutcome::NoOp, "state {state:?}");
        }
    }

    #[test]
    fn already_completed_from_dispatched_or_in_progress_awaits_reconciliation() {
        for state in [AttemptState::Dispatched, AttemptState::InProgress] {
            let (job, step, attempt) = cancelling_job_with_dispatching_step(state);
            let outcome = apply_cancel_ack(
                &job,
                &step,
                &attempt,
                CancelAckEvidence::AlreadyCompleted,
                now(),
            );
            let CancelAckOutcome::Applied(applied) = outcome else {
                panic!("expected Applied from {state:?}")
            };
            assert_eq!(applied.attempt.state, AttemptState::AwaitingReconciliation);
            assert_eq!(applied.job_step.state, JobStepState::Dispatching);
            assert_eq!(applied.job.state, JobState::Cancelling);
            assert!(!applied.terminal);
            assert!(applied.events.is_empty());
        }
    }

    #[test]
    fn already_completed_repeated_against_awaiting_reconciliation_is_idempotent() {
        let (job, step, attempt) =
            cancelling_job_with_dispatching_step(AttemptState::AwaitingReconciliation);
        let outcome = apply_cancel_ack(
            &job,
            &step,
            &attempt,
            CancelAckEvidence::AlreadyCompleted,
            now(),
        );
        assert_eq!(outcome, CancelAckOutcome::NoOp);
    }

    #[test]
    fn already_completed_against_a_known_terminal_attempt_completes_pending_cancellation() {
        for state in [AttemptState::Succeeded, AttemptState::Failed] {
            let (job, step, attempt) = cancelling_job_with_dispatching_step(state);
            let outcome = apply_cancel_ack(
                &job,
                &step,
                &attempt,
                CancelAckEvidence::AlreadyCompleted,
                now(),
            );
            let CancelAckOutcome::Applied(applied) = outcome else {
                panic!("expected Applied from {state:?}")
            };
            // Attempt/JobStep preserved exactly, never overwritten.
            assert_eq!(applied.attempt.state, state);
            assert_eq!(applied.job_step, step);
            assert_eq!(applied.job.state, JobState::Cancelled);
            assert!(applied.terminal);
            assert_eq!(applied.events.len(), 1);
            assert_eq!(applied.events[0].event_type(), "JobCancelled");
        }
    }

    #[test]
    fn already_completed_against_a_terminal_attempt_when_job_no_longer_cancelling_is_a_no_op() {
        let (mut job, step, attempt) =
            cancelling_job_with_dispatching_step(AttemptState::Succeeded);
        job.state = JobState::Cancelled; // already resolved by other evidence
        let outcome = apply_cancel_ack(
            &job,
            &step,
            &attempt,
            CancelAckEvidence::AlreadyCompleted,
            now(),
        );
        assert_eq!(outcome, CancelAckOutcome::NoOp);
    }

    #[test]
    fn cannot_cancel_never_mutates_state_for_any_attempt_state() {
        for state in [
            AttemptState::Dispatched,
            AttemptState::InProgress,
            AttemptState::AwaitingReconciliation,
            AttemptState::Succeeded,
            AttemptState::Failed,
            AttemptState::Cancelled,
            AttemptState::Rejected,
        ] {
            let (job, step, attempt) = cancelling_job_with_dispatching_step(state);
            let outcome = apply_cancel_ack(
                &job,
                &step,
                &attempt,
                CancelAckEvidence::CannotCancel,
                now(),
            );
            assert_eq!(outcome, CancelAckOutcome::NoOp, "state {state:?}");
        }
    }

    #[test]
    fn unknown_from_dispatched_or_in_progress_awaits_reconciliation() {
        for state in [AttemptState::Dispatched, AttemptState::InProgress] {
            let (job, step, attempt) = cancelling_job_with_dispatching_step(state);
            let outcome =
                apply_cancel_ack(&job, &step, &attempt, CancelAckEvidence::Unknown, now());
            let CancelAckOutcome::Applied(applied) = outcome else {
                panic!("expected Applied from {state:?}")
            };
            assert_eq!(applied.attempt.state, AttemptState::AwaitingReconciliation);
            assert_eq!(applied.job.state, JobState::Cancelling);
            assert!(!applied.terminal);
        }
    }

    #[test]
    fn unknown_repeated_against_awaiting_reconciliation_is_idempotent() {
        let (job, step, attempt) =
            cancelling_job_with_dispatching_step(AttemptState::AwaitingReconciliation);
        let outcome = apply_cancel_ack(&job, &step, &attempt, CancelAckEvidence::Unknown, now());
        assert_eq!(outcome, CancelAckOutcome::NoOp);
    }

    #[test]
    fn unknown_never_regresses_an_already_terminal_attempt() {
        for state in [
            AttemptState::Succeeded,
            AttemptState::Failed,
            AttemptState::Cancelled,
            AttemptState::Rejected,
        ] {
            let (job, step, attempt) = cancelling_job_with_dispatching_step(state);
            let outcome =
                apply_cancel_ack(&job, &step, &attempt, CancelAckEvidence::Unknown, now());
            assert_eq!(outcome, CancelAckOutcome::NoOp, "state {state:?}");
        }
    }
}
