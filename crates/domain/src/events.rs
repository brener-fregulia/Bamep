use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::endpoint::EndpointId;

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
}

impl DomainEvent {
    pub fn event_id(&self) -> Uuid {
        match self {
            DomainEvent::EndpointPendingEnrollment { event_id, .. }
            | DomainEvent::EndpointEnrolled { event_id, .. }
            | DomainEvent::OperatorDecisionRecorded { event_id, .. }
            | DomainEvent::InventoryRevisionRecorded { event_id, .. } => *event_id,
        }
    }

    pub fn endpoint_id(&self) -> EndpointId {
        match self {
            DomainEvent::EndpointPendingEnrollment { endpoint_id, .. }
            | DomainEvent::EndpointEnrolled { endpoint_id, .. }
            | DomainEvent::OperatorDecisionRecorded { endpoint_id, .. }
            | DomainEvent::InventoryRevisionRecorded { endpoint_id, .. } => *endpoint_id,
        }
    }

    pub fn occurred_at(&self) -> DateTime<Utc> {
        match self {
            DomainEvent::EndpointPendingEnrollment { occurred_at, .. }
            | DomainEvent::EndpointEnrolled { occurred_at, .. }
            | DomainEvent::OperatorDecisionRecorded { occurred_at, .. }
            | DomainEvent::InventoryRevisionRecorded { occurred_at, .. } => *occurred_at,
        }
    }

    pub fn event_type(&self) -> &'static str {
        match self {
            DomainEvent::EndpointPendingEnrollment { .. } => "EndpointPendingEnrollment",
            DomainEvent::EndpointEnrolled { .. } => "EndpointEnrolled",
            DomainEvent::OperatorDecisionRecorded { .. } => "OperatorDecisionRecorded",
            DomainEvent::InventoryRevisionRecorded { .. } => "InventoryRevisionRecorded",
        }
    }
}

/// A durable, immutable audit record for safety-relevant activity
/// (`m0-persistence-observability-and-domain-events.md` "Auditability").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub audit_id: Uuid,
    pub endpoint_id: EndpointId,
    pub actor: Actor,
    pub occurred_at: DateTime<Utc>,
    pub detail: String,
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
