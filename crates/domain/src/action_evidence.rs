//! Bamep Domain: normal connected-session Agent action evidence transitions
//! (Issue #26 "[WP] Dispatch typed actions and complete Attempts";
//! `docs/specifications/m0-job-lifecycle-and-scheduling.md` "Attempt
//! lifecycle", "Duplicate and delayed evidence").
//!
//! This module decides only the four normal evidence transitions #26 owns —
//! `ActionAck{Accepted}`, `ActionAck{Rejected}`, `ActionResult{Succeeded}`,
//! `ActionResult{Failed}` — against already-locked, freshly read Attempt/
//! JobStep/Job state (`bamep_server::application::ActionEvidenceService`
//! resolves that state, under lock, before calling this module). It performs
//! no I/O, constructs no `AuditRecord` (the caller decides what to audit for
//! a terminal outcome, mirroring `bamep_domain::final_dispatch`'s
//! Domain/Application split), and never creates a new Attempt, JobStep, or
//! Job. `ActionProgress` is intentionally not represented here: it is
//! transient metadata that never reaches a Domain decision
//! (`m0-agent-protocol-contract.md` "ActionProgress fields").
//!
//! `AwaitingReconciliation`, `Cancelled`, and `Indeterminate` Attempt states
//! are outside this module's authority: evidence arriving while an Attempt is
//! in any of those states is [`ActionEvidenceOutcome::Conflict`], exactly
//! like evidence conflicting with an already-committed different terminal
//! outcome — #26 never mutates state outside its own owned transitions;
//! #27/#28 own those states.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::attempt::{Attempt, AttemptState};
use crate::events::DomainEvent;
use crate::job::{Job, JobState, JobStep, JobStepFailureReason, JobStepState};

/// The four normal evidence kinds #26 applies. `ActionProgress` is
/// deliberately absent — it never reaches a Domain decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionEvidence {
    AckAccepted,
    AckRejected,
    ResultSucceeded,
    ResultFailed,
}

/// Successful application of `evidence` against a freshly locked Attempt/
/// JobStep/Job: the resulting durable state, and whether this is a terminal
/// Attempt outcome that requires the caller to release the Attempt's
/// transient technical-resource reservation and commit a destructive
/// terminal-audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionEvidenceApplied {
    pub attempt: Attempt,
    pub job_step: JobStep,
    pub job: Job,
    pub events: Vec<DomainEvent>,
    /// `true` for `Rejected`/`Succeeded`/`Failed` — every case that reaches
    /// an authoritative Attempt terminal state and therefore requires the
    /// caller to release the reservation and persist the required terminal
    /// audit record. `false` only for `AckAccepted`'s `Dispatched ->
    /// InProgress` transition.
    pub terminal: bool,
}

/// The full result of [`apply_action_evidence`]: distinguishes an applied
/// transition from the two forms of non-mutation this contract requires
/// (`m0-job-lifecycle-and-scheduling.md` "Duplicate and delayed evidence").
/// This type crosses one evidence application at a time, never a hot
/// per-message path — `ActionProgress`, the actually high-frequency message,
/// never reaches this module at all (see module docs) — so the size
/// difference clippy flags is not worth the indirection here, mirroring
/// `bamep_domain::transitions::RedeemOutcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ActionEvidenceOutcome {
    /// A durable transition was decided; the caller must persist exactly
    /// this result.
    Applied(ActionEvidenceApplied),
    /// The evidence exactly matches already-committed authoritative state —
    /// a duplicate `Accepted` against `InProgress`, or duplicate terminal
    /// evidence matching the already-committed terminal outcome. No
    /// mutation, no event, no audit, no reservation release.
    NoOp,
    /// The evidence does not apply to the Attempt's current authoritative
    /// state and is not a matching duplicate — including conflicting
    /// terminal evidence after a *different* terminal outcome already
    /// committed, and evidence arriving while the Attempt is
    /// `AwaitingReconciliation`/`Cancelled`/`Indeterminate`, which #26 never
    /// owns. No mutation.
    Conflict,
}

/// Decides `evidence`'s effect against `attempt`/`job_step`/`job`, all
/// already locked and freshly read by the caller
/// (`m0-job-lifecycle-and-scheduling.md` "Duplicate and delayed evidence";
/// Issue #26 "PostgreSQL evidence application"). `job_step` must be the
/// JobStep `attempt.job_step_id` identifies, already present in `job.steps` —
/// the caller is responsible for that correlation; this function trusts it
/// without re-deriving it from `job`.
pub fn apply_action_evidence(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    evidence: ActionEvidence,
    now: DateTime<Utc>,
) -> ActionEvidenceOutcome {
    match evidence {
        ActionEvidence::AckAccepted => match attempt.state {
            AttemptState::Dispatched => ActionEvidenceOutcome::Applied(ActionEvidenceApplied {
                attempt: Attempt {
                    state: AttemptState::InProgress,
                    ..*attempt
                },
                job_step: job_step.clone(),
                job: job.clone(),
                events: vec![],
                terminal: false,
            }),
            // Duplicate Accepted against already-InProgress is an idempotent
            // no-op; Accepted arriving after any other (terminal or
            // AwaitingReconciliation) state never regresses it — also a
            // no-op, never an error, since Accepted carries no destructive
            // consequence to reject.
            _ => ActionEvidenceOutcome::NoOp,
        },
        ActionEvidence::AckRejected => match attempt.state {
            AttemptState::Dispatched => ActionEvidenceOutcome::Applied(terminal_failure(
                job,
                job_step,
                attempt,
                AttemptState::Rejected,
                JobStepFailureReason::DispatchRejected,
                now,
            )),
            AttemptState::Rejected => ActionEvidenceOutcome::NoOp,
            _ => ActionEvidenceOutcome::Conflict,
        },
        ActionEvidence::ResultSucceeded => match attempt.state {
            // Direct Dispatched -> Succeeded is explicitly normative (a lost
            // or delayed Accepted Ack followed by authoritative terminal
            // evidence) — no synthetic InProgress step is synthesized first.
            AttemptState::Dispatched | AttemptState::InProgress => {
                ActionEvidenceOutcome::Applied(terminal_success(job, job_step, attempt, now))
            }
            AttemptState::Succeeded => ActionEvidenceOutcome::NoOp,
            _ => ActionEvidenceOutcome::Conflict,
        },
        ActionEvidence::ResultFailed => match attempt.state {
            AttemptState::Dispatched | AttemptState::InProgress => {
                ActionEvidenceOutcome::Applied(terminal_failure(
                    job,
                    job_step,
                    attempt,
                    AttemptState::Failed,
                    JobStepFailureReason::ExecutionFailed,
                    now,
                ))
            }
            AttemptState::Failed => ActionEvidenceOutcome::NoOp,
            _ => ActionEvidenceOutcome::Conflict,
        },
    }
}

/// Shared terminal-failure shape for `ActionAck{Rejected}` and
/// `ActionResult{Failed}`: Attempt -> `attempt_state`, current JobStep ->
/// `Failed{reason}`, `JobStepFailed` always emitted. When `job` is not
/// `Cancelling`, owning Job -> `Failed` with `JobFailed`. When `job` is
/// already `Cancelling` (Issue #27 "Normal action evidence while Job is
/// Cancelling"), cancellation intent already owns the workflow outcome: Job
/// -> `Cancelled` with `JobCancelled` instead — `JobFailed` is never emitted
/// for a Job that reaches its authoritative terminal state `Cancelled`.
fn terminal_failure(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    attempt_state: AttemptState,
    reason: JobStepFailureReason,
    now: DateTime<Utc>,
) -> ActionEvidenceApplied {
    let failed_step = JobStep {
        state: JobStepState::Failed,
        failure_reason: Some(reason),
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
    ActionEvidenceApplied {
        attempt: Attempt {
            state: attempt_state,
            ..*attempt
        },
        job_step: failed_step,
        job: final_job,
        events: vec![job_step_failed, job_terminal_event],
        terminal: true,
    }
}

/// `ActionResult{Succeeded}`: Attempt -> `Succeeded`, current JobStep ->
/// `Succeeded`. When `job` is not `Cancelling`: if every ordered JobStep is
/// now `Succeeded`, Job -> `Succeeded` with `JobSucceeded`; otherwise Job
/// remains `Running` and retains Job-scoped Endpoint exclusivity. #26 never
/// automatically schedules/dispatches a later JobStep merely because this one
/// succeeded. When `job` is already `Cancelling` (Issue #27 "Normal action
/// evidence while Job is Cancelling"), the execution result is preserved on
/// the Attempt/JobStep, but cancellation intent already owns the workflow
/// outcome: Job -> `Cancelled` with `JobCancelled` regardless of remaining
/// JobSteps — no further JobStep is ever scheduled and `JobSucceeded` is
/// never emitted for a Job that reaches `Cancelled`.
fn terminal_success(
    job: &Job,
    job_step: &JobStep,
    attempt: &Attempt,
    now: DateTime<Utc>,
) -> ActionEvidenceApplied {
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

    ActionEvidenceApplied {
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

    fn dispatched_attempt(job_step_id: JobStepId) -> Attempt {
        Attempt {
            id: AttemptId::new(),
            job_step_id,
            action_id: ActionId::new(),
            state: AttemptState::Dispatched,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn accepted_from_dispatched_moves_attempt_to_in_progress_without_terminal_effect() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckAccepted, now());

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::InProgress);
        assert_eq!(applied.job_step.state, JobStepState::Dispatching);
        assert_eq!(applied.job.state, JobState::Running);
        assert!(applied.events.is_empty());
        assert!(!applied.terminal);
    }

    #[test]
    fn duplicate_accepted_against_in_progress_is_a_no_op() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::InProgress,
            ..dispatched_attempt(ids[0])
        };

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckAccepted, now());
        assert_eq!(outcome, ActionEvidenceOutcome::NoOp);
    }

    #[test]
    fn accepted_after_terminal_never_regresses_and_is_a_no_op() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::Succeeded,
            ..dispatched_attempt(ids[0])
        };

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckAccepted, now());
        assert_eq!(outcome, ActionEvidenceOutcome::NoOp);
    }

    #[test]
    fn rejected_from_dispatched_fails_step_and_job_with_dispatch_rejected_reason() {
        let (job, ids) = running_job(2, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckRejected, now());

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Rejected);
        assert_eq!(applied.job_step.state, JobStepState::Failed);
        assert_eq!(
            applied.job_step.failure_reason,
            Some(JobStepFailureReason::DispatchRejected)
        );
        assert_eq!(applied.job.state, JobState::Failed);
        assert!(applied.terminal);
        assert_eq!(applied.events.len(), 2);
        assert!(applied
            .events
            .iter()
            .any(|e| e.event_type() == "JobStepFailed"));
        assert!(applied.events.iter().any(|e| e.event_type() == "JobFailed"));
        // The step's own event carries this exact JobStep's id.
        let step_event_matches = applied.events.iter().any(|e| {
            matches!(e, DomainEvent::JobStepFailed { job_step_id, .. } if *job_step_id == applied.job_step.id)
        });
        assert!(step_event_matches);
    }

    #[test]
    fn duplicate_rejected_against_rejected_is_a_no_op() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::Rejected,
            ..dispatched_attempt(ids[0])
        };

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckRejected, now());
        assert_eq!(outcome, ActionEvidenceOutcome::NoOp);
    }

    #[test]
    fn rejected_against_in_progress_is_a_conflict_not_a_mutation() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::InProgress,
            ..dispatched_attempt(ids[0])
        };

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckRejected, now());
        assert_eq!(outcome, ActionEvidenceOutcome::Conflict);
    }

    #[test]
    fn succeeded_direct_from_dispatched_never_synthesizes_in_progress() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome = apply_action_evidence(
            &job,
            &step,
            &attempt,
            ActionEvidence::ResultSucceeded,
            now(),
        );

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Succeeded);
    }

    #[test]
    fn succeeded_from_in_progress_on_final_step_reaches_job_succeeded() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::InProgress,
            ..dispatched_attempt(ids[0])
        };

        let outcome = apply_action_evidence(
            &job,
            &step,
            &attempt,
            ActionEvidence::ResultSucceeded,
            now(),
        );

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.job_step.state, JobStepState::Succeeded);
        assert_eq!(applied.job.state, JobState::Succeeded);
        assert!(applied.terminal);
        assert_eq!(applied.events.len(), 1);
        assert_eq!(applied.events[0].event_type(), "JobSucceeded");
    }

    #[test]
    fn succeeded_on_a_non_final_step_leaves_job_running_with_no_event() {
        let (job, ids) = running_job(2, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome = apply_action_evidence(
            &job,
            &step,
            &attempt,
            ActionEvidence::ResultSucceeded,
            now(),
        );

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.job_step.state, JobStepState::Succeeded);
        assert_eq!(applied.job.state, JobState::Running);
        assert!(applied.events.is_empty());
        // The second (not-yet-dispatched) step is untouched.
        assert_eq!(applied.job.steps[1].state, JobStepState::Pending);
    }

    #[test]
    fn duplicate_succeeded_against_succeeded_is_a_no_op() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::Succeeded,
            ..dispatched_attempt(ids[0])
        };

        let outcome = apply_action_evidence(
            &job,
            &step,
            &attempt,
            ActionEvidence::ResultSucceeded,
            now(),
        );
        assert_eq!(outcome, ActionEvidenceOutcome::NoOp);
    }

    #[test]
    fn conflicting_succeeded_against_already_failed_never_overwrites() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::Failed,
            ..dispatched_attempt(ids[0])
        };

        let outcome = apply_action_evidence(
            &job,
            &step,
            &attempt,
            ActionEvidence::ResultSucceeded,
            now(),
        );
        assert_eq!(outcome, ActionEvidenceOutcome::Conflict);
    }

    #[test]
    fn failed_from_dispatched_and_in_progress_both_fail_step_and_job_with_execution_failed_reason()
    {
        for state in [AttemptState::Dispatched, AttemptState::InProgress] {
            let (job, ids) = running_job(1, 0);
            let step = job.steps[0].clone();
            let attempt = Attempt {
                state,
                ..dispatched_attempt(ids[0])
            };

            let outcome =
                apply_action_evidence(&job, &step, &attempt, ActionEvidence::ResultFailed, now());

            let ActionEvidenceOutcome::Applied(applied) = outcome else {
                panic!("expected Applied from {state:?}")
            };
            assert_eq!(applied.attempt.state, AttemptState::Failed);
            assert_eq!(applied.job_step.state, JobStepState::Failed);
            assert_eq!(
                applied.job_step.failure_reason,
                Some(JobStepFailureReason::ExecutionFailed)
            );
            assert_eq!(applied.job.state, JobState::Failed);
            assert!(applied.terminal);
        }
    }

    #[test]
    fn duplicate_failed_against_failed_is_a_no_op() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::Failed,
            ..dispatched_attempt(ids[0])
        };

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::ResultFailed, now());
        assert_eq!(outcome, ActionEvidenceOutcome::NoOp);
    }

    // -- Issue #27: normal evidence while Job is Cancelling ---------------

    fn cancelling_job(step_count: usize, dispatching_index: usize) -> (Job, Vec<JobStepId>) {
        let (mut job, ids) = running_job(step_count, dispatching_index);
        job.state = JobState::Cancelling;
        (job, ids)
    }

    #[test]
    fn accepted_while_cancelling_moves_to_in_progress_without_cancelling_the_cancellation() {
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckAccepted, now());

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::InProgress);
        assert_eq!(applied.job.state, JobState::Cancelling);
        assert_eq!(applied.job_step.state, JobStepState::Dispatching);
        assert!(applied.events.is_empty());
        assert!(!applied.terminal, "no terminal event/audit/release");
    }

    #[test]
    fn rejected_while_cancelling_preserves_rejected_semantics_but_job_ends_cancelled() {
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckRejected, now());

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Rejected);
        assert_eq!(applied.job_step.state, JobStepState::Failed);
        assert_eq!(
            applied.job_step.failure_reason,
            Some(JobStepFailureReason::DispatchRejected)
        );
        assert_eq!(
            applied.job.state,
            JobState::Cancelled,
            "never JobState::Failed"
        );
        assert!(applied.terminal);
        assert_eq!(applied.events.len(), 2);
        assert!(applied
            .events
            .iter()
            .any(|e| e.event_type() == "JobStepFailed"));
        assert!(
            applied
                .events
                .iter()
                .any(|e| e.event_type() == "JobCancelled"),
            "JobCancelled must be emitted, never JobFailed"
        );
        assert!(!applied.events.iter().any(|e| e.event_type() == "JobFailed"));
    }

    #[test]
    fn succeeded_while_cancelling_preserves_success_semantics_but_job_ends_cancelled() {
        // Even the FINAL step succeeding must never reach JobState::Succeeded
        // or emit JobSucceeded while cancellation intent is authoritative.
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome = apply_action_evidence(
            &job,
            &step,
            &attempt,
            ActionEvidence::ResultSucceeded,
            now(),
        );

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Succeeded);
        assert_eq!(applied.job_step.state, JobStepState::Succeeded);
        assert_eq!(applied.job.state, JobState::Cancelled);
        assert!(applied.terminal);
        assert_eq!(applied.events.len(), 1);
        assert_eq!(applied.events[0].event_type(), "JobCancelled");
    }

    #[test]
    fn succeeded_on_a_non_final_step_while_cancelling_still_ends_the_job_cancelled() {
        let (job, ids) = cancelling_job(2, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome = apply_action_evidence(
            &job,
            &step,
            &attempt,
            ActionEvidence::ResultSucceeded,
            now(),
        );

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.job.state, JobState::Cancelled);
        assert_eq!(applied.events.len(), 1);
        assert_eq!(applied.events[0].event_type(), "JobCancelled");
        // No later JobStep is scheduled/advanced.
        assert_eq!(applied.job.steps[1].state, JobStepState::Pending);
    }

    #[test]
    fn failed_while_cancelling_preserves_failed_semantics_but_job_ends_cancelled() {
        let (job, ids) = cancelling_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = dispatched_attempt(ids[0]);

        let outcome =
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::ResultFailed, now());

        let ActionEvidenceOutcome::Applied(applied) = outcome else {
            panic!("expected Applied")
        };
        assert_eq!(applied.attempt.state, AttemptState::Failed);
        assert_eq!(applied.job_step.state, JobStepState::Failed);
        assert_eq!(
            applied.job_step.failure_reason,
            Some(JobStepFailureReason::ExecutionFailed)
        );
        assert_eq!(applied.job.state, JobState::Cancelled);
        assert!(applied.terminal);
        assert!(applied
            .events
            .iter()
            .any(|e| e.event_type() == "JobCancelled"),);
        assert!(!applied.events.iter().any(|e| e.event_type() == "JobFailed"));
    }

    #[test]
    fn evidence_against_awaiting_reconciliation_is_never_mutated_by_issue_26() {
        let (job, ids) = running_job(1, 0);
        let step = job.steps[0].clone();
        let attempt = Attempt {
            state: AttemptState::AwaitingReconciliation,
            ..dispatched_attempt(ids[0])
        };

        for evidence in [
            ActionEvidence::AckRejected,
            ActionEvidence::ResultSucceeded,
            ActionEvidence::ResultFailed,
        ] {
            assert_eq!(
                apply_action_evidence(&job, &step, &attempt, evidence, now()),
                ActionEvidenceOutcome::Conflict
            );
        }
        // Accepted is deliberately a no-op rather than a conflict — it never
        // regresses/reopens state and carries no destructive consequence.
        assert_eq!(
            apply_action_evidence(&job, &step, &attempt, ActionEvidence::AckAccepted, now()),
            ActionEvidenceOutcome::NoOp
        );
    }
}
