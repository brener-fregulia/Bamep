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
}

impl DomainEvent {
    pub fn event_id(&self) -> Uuid {
        match self {
            DomainEvent::EndpointPendingEnrollment { event_id, .. }
            | DomainEvent::EndpointEnrolled { event_id, .. }
            | DomainEvent::OperatorDecisionRecorded { event_id, .. }
            | DomainEvent::InventoryRevisionRecorded { event_id, .. }
            | DomainEvent::JobStarted { event_id, .. } => *event_id,
        }
    }

    pub fn endpoint_id(&self) -> EndpointId {
        match self {
            DomainEvent::EndpointPendingEnrollment { endpoint_id, .. }
            | DomainEvent::EndpointEnrolled { endpoint_id, .. }
            | DomainEvent::OperatorDecisionRecorded { endpoint_id, .. }
            | DomainEvent::InventoryRevisionRecorded { endpoint_id, .. }
            | DomainEvent::JobStarted { endpoint_id, .. } => *endpoint_id,
        }
    }

    pub fn occurred_at(&self) -> DateTime<Utc> {
        match self {
            DomainEvent::EndpointPendingEnrollment { occurred_at, .. }
            | DomainEvent::EndpointEnrolled { occurred_at, .. }
            | DomainEvent::OperatorDecisionRecorded { occurred_at, .. }
            | DomainEvent::InventoryRevisionRecorded { occurred_at, .. }
            | DomainEvent::JobStarted { occurred_at, .. } => *occurred_at,
        }
    }

    pub fn event_type(&self) -> &'static str {
        match self {
            DomainEvent::EndpointPendingEnrollment { .. } => "EndpointPendingEnrollment",
            DomainEvent::EndpointEnrolled { .. } => "EndpointEnrolled",
            DomainEvent::OperatorDecisionRecorded { .. } => "OperatorDecisionRecorded",
            DomainEvent::InventoryRevisionRecorded { .. } => "InventoryRevisionRecorded",
            DomainEvent::JobStarted { .. } => "JobStarted",
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
