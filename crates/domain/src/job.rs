//! Bamep Domain: Job/JobStep workflow model
//! (`docs/specifications/m0-job-lifecycle-and-scheduling.md` "Domain model").
//!
//! Issue #24 stops at durable `Pending` workflow creation
//! (`docs/decisions/0006-job-jobstep-attempt-state-model-and-scheduling.md`):
//! only the identities, `Job -> Endpoint`/`JobStep -> Job` correlations,
//! explicit linear order, and initial `Pending` states required by that
//! boundary are represented here. The complete lifecycle vocabularies already
//! reflect the Specification's later states so #25-#28 do not need a Domain
//! type change, but this module performs no I/O and [`create_workflow`]
//! constructs no state beyond `Pending`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

/// One ordered linear stage of its owning [`Job`]. `order` is the explicit
/// stable linear position required by the accepted linear-workflow baseline
/// (`m0-job-lifecycle-and-scheduling.md`: "The baseline workflow is linear;
/// branching/parallel JobSteps are outside this contract"). No action/type/
/// parameter payload is represented yet — Issue #24 owns only workflow
/// identity/correlation/order/state; an action-specific contract will extend
/// this still-pre-baseline shape when it is introduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStep {
    pub id: JobStepId,
    pub job_id: JobId,
    pub order: i32,
    pub state: JobStepState,
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
        })
        .collect();
    Ok(Job {
        id: job_id,
        endpoint_id,
        state: JobState::Pending,
        steps,
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
}
