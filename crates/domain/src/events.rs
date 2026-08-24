use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::attempt::{ActionId, AttemptId};
use crate::endpoint::EndpointId;
use crate::job::{JobId, JobStepId};

/// Actor attribution for an operator decision
/// (`m0-persistence-observability-and-domain-events.md` "Auditability").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Actor {
    Operator { label: String },
    System,
}

/// A durable domain event, per `m0-persistence-observability-and-domain-events.md`
/// "Domain-event model". Only event types required by the implemented slice
/// are represented here; the catalog itself remains extensible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type")]
pub enum DomainEvent {
    EndpointPendingEnrollment {
        event_id: Uuid,
        endpoint_id: EndpointId,
        occurred_at: DateTime<Utc>,
    },
    EndpointEnrolled {
        event_id: Uuid,
        endpoint_id: EndpointId,
        occurred_at: DateTime<Utc>,
    },
    OperatorDecisionRecorded {
        event_id: Uuid,
        endpoint_id: EndpointId,
        decision: String,
        actor: Actor,
        occurred_at: DateTime<Utc>,
    },
    InventoryRevisionRecorded {
        event_id: Uuid,
        endpoint_id: EndpointId,
        inventory_revision_id: crate::InventoryRevisionId,
        occurred_at: DateTime<Utc>,
    },
    /// Emitted exactly once when a `Pending` Job is admitted into `Running`
    /// (`m0-persistence-observability-and-domain-events.md` "Domain events":
    /// "`JobStarted` | Job enters `Running`"; Issue #32). Carries both
    /// `job_id` and `endpoint_id` — the minimum correlation the current event
    /// contract requires for this transition.
    JobStarted {
        event_id: Uuid,
        job_id: JobId,
        endpoint_id: EndpointId,
        occurred_at: DateTime<Utc>,
    },
    /// Emitted exactly once when a `Job` enters `Succeeded`
    /// (`m0-persistence-observability-and-domain-events.md` "Required M1
    /// normal-terminal Job/JobStep events"; Issue #26).
    JobSucceeded {
        event_id: Uuid,
        job_id: JobId,
        endpoint_id: EndpointId,
        occurred_at: DateTime<Utc>,
    },
    /// Emitted exactly once when a `Job` enters `Failed`
    /// (`m0-persistence-observability-and-domain-events.md` "Required M1
    /// normal-terminal Job/JobStep events"; Issue #26).
    JobFailed {
        event_id: Uuid,
        job_id: JobId,
        endpoint_id: EndpointId,
        occurred_at: DateTime<Utc>,
    },
    /// Emitted exactly once when a `JobStep` enters `Failed`
    /// (`m0-persistence-observability-and-domain-events.md` "Required M1
    /// normal-terminal Job/JobStep events"; Issue #26). `job_step_id` is the
    /// narrow additional correlation this event requires beyond `job_id`.
    JobStepFailed {
        event_id: Uuid,
        job_id: JobId,
        job_step_id: JobStepId,
        endpoint_id: EndpointId,
        occurred_at: DateTime<Utc>,
    },
    /// Emitted exactly once when a `Job` enters `Cancelled`
    /// (`m0-persistence-observability-and-domain-events.md` "Domain events":
    /// "`JobCancelled` | Job reaches matching terminal state"; Issue #27).
    /// Neither `JobCancelling` nor per-Attempt/per-JobStep cancellation
    /// events are defined — this coarse-grained Job-terminal fact is the
    /// only one the persistence contract requires.
    JobCancelled {
        event_id: Uuid,
        job_id: JobId,
        endpoint_id: EndpointId,
        occurred_at: DateTime<Utc>,
    },
}

impl DomainEvent {
    pub fn event_id(&self) -> Uuid {
        match self {
            DomainEvent::EndpointPendingEnrollment { event_id, .. }
            | DomainEvent::EndpointEnrolled { event_id, .. }
            | DomainEvent::OperatorDecisionRecorded { event_id, .. }
            | DomainEvent::InventoryRevisionRecorded { event_id, .. }
            | DomainEvent::JobStarted { event_id, .. }
            | DomainEvent::JobSucceeded { event_id, .. }
            | DomainEvent::JobFailed { event_id, .. }
            | DomainEvent::JobStepFailed { event_id, .. }
            | DomainEvent::JobCancelled { event_id, .. } => *event_id,
        }
    }

    pub fn endpoint_id(&self) -> EndpointId {
        match self {
            DomainEvent::EndpointPendingEnrollment { endpoint_id, .. }
            | DomainEvent::EndpointEnrolled { endpoint_id, .. }
            | DomainEvent::OperatorDecisionRecorded { endpoint_id, .. }
            | DomainEvent::InventoryRevisionRecorded { endpoint_id, .. }
            | DomainEvent::JobStarted { endpoint_id, .. }
            | DomainEvent::JobSucceeded { endpoint_id, .. }
            | DomainEvent::JobFailed { endpoint_id, .. }
            | DomainEvent::JobStepFailed { endpoint_id, .. }
            | DomainEvent::JobCancelled { endpoint_id, .. } => *endpoint_id,
        }
    }

    pub fn occurred_at(&self) -> DateTime<Utc> {
        match self {
            DomainEvent::EndpointPendingEnrollment { occurred_at, .. }
            | DomainEvent::EndpointEnrolled { occurred_at, .. }
            | DomainEvent::OperatorDecisionRecorded { occurred_at, .. }
            | DomainEvent::InventoryRevisionRecorded { occurred_at, .. }
            | DomainEvent::JobStarted { occurred_at, .. }
            | DomainEvent::JobSucceeded { occurred_at, .. }
            | DomainEvent::JobFailed { occurred_at, .. }
            | DomainEvent::JobStepFailed { occurred_at, .. }
            | DomainEvent::JobCancelled { occurred_at, .. } => *occurred_at,
        }
    }

    /// The owning `JobId`, for the Job-scoped event types (Issue #26/#27).
    /// Endpoint-only event types have no Job correlation.
    pub fn job_id(&self) -> Option<JobId> {
        match self {
            DomainEvent::JobStarted { job_id, .. }
            | DomainEvent::JobSucceeded { job_id, .. }
            | DomainEvent::JobFailed { job_id, .. }
            | DomainEvent::JobStepFailed { job_id, .. }
            | DomainEvent::JobCancelled { job_id, .. } => Some(*job_id),
            _ => None,
        }
    }

    /// The owning `JobStepId`, for the JobStep-scoped event types (Issue
    /// #26). Every other event type has no JobStep correlation.
    pub fn job_step_id(&self) -> Option<crate::job::JobStepId> {
        match self {
            DomainEvent::JobStepFailed { job_step_id, .. } => Some(*job_step_id),
            _ => None,
        }
    }

    pub fn event_type(&self) -> &'static str {
        match self {
            DomainEvent::EndpointPendingEnrollment { .. } => "EndpointPendingEnrollment",
            DomainEvent::EndpointEnrolled { .. } => "EndpointEnrolled",
            DomainEvent::OperatorDecisionRecorded { .. } => "OperatorDecisionRecorded",
            DomainEvent::InventoryRevisionRecorded { .. } => "InventoryRevisionRecorded",
            DomainEvent::JobStarted { .. } => "JobStarted",
            DomainEvent::JobSucceeded { .. } => "JobSucceeded",
            DomainEvent::JobFailed { .. } => "JobFailed",
            DomainEvent::JobStepFailed { .. } => "JobStepFailed",
            DomainEvent::JobCancelled { .. } => "JobCancelled",
        }
    }
}

/// A durable, immutable audit record for safety-relevant activity
/// (`m0-persistence-observability-and-domain-events.md` "Auditability").
///
/// `job_id`/`job_step_id`/`attempt_id`/`action_id` are narrow optional
/// correlation fields added by Issue #25 for the destructive-dispatch
/// commitment audit record, which "must carry applicable correlation
/// structurally" rather than hidden solely inside `detail`. Every audit
/// record predating #25 (enrollment approval, etc.) leaves all four `None` —
/// this extension preserves that existing behavior unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub audit_id: Uuid,
    pub endpoint_id: EndpointId,
    pub actor: Actor,
    pub occurred_at: DateTime<Utc>,
    pub detail: String,
    pub job_id: Option<JobId>,
    pub job_step_id: Option<JobStepId>,
    pub attempt_id: Option<AttemptId>,
    pub action_id: Option<ActionId>,
}

/// The full result of one durable domain transition: the new aggregate
/// state, any domain events, and any audit record it requires — always
/// persisted together in one atomic transaction (ADR-0007 "Transactional
/// consistency between domain state, domain events, and audit records").
#[derive(Debug, Clone)]
pub struct TransitionOutcome {
    pub endpoint: crate::endpoint::EndpointAggregate,
    pub events: Vec<DomainEvent>,
    pub audit: Option<AuditRecord>,
}
