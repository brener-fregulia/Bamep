use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{DomainEvent, EndpointId};

/// Opaque parsed inventory snapshot. JSON object equality is semantic with
/// respect to object-key ordering through `serde_json::Map` equality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InventorySnapshot(pub Map<String, Value>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryRevisionId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InventoryRevision {
    pub id: InventoryRevisionId,
    pub endpoint_id: EndpointId,
    pub snapshot: InventorySnapshot,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InventoryRevisionChange {
    pub revision: InventoryRevision,
    pub event: DomainEvent,
}

/// Decides the inventory-on-change transition without I/O. An unchanged
/// parsed snapshot is a no-op, including a JSON object with different key
/// ordering.
pub fn record_inventory_on_change(
    endpoint_id: EndpointId,
    current: Option<&InventoryRevision>,
    observed: InventorySnapshot,
    now: DateTime<Utc>,
) -> Option<InventoryRevisionChange> {
    if current.is_some_and(|revision| revision.snapshot == observed) {
        return None;
    }

    let revision_id = InventoryRevisionId(Uuid::new_v4());
    Some(InventoryRevisionChange {
        revision: InventoryRevision {
            id: revision_id,
            endpoint_id,
            snapshot: observed,
            recorded_at: now,
        },
        event: DomainEvent::InventoryRevisionRecorded {
            event_id: Uuid::new_v4(),
            endpoint_id,
            inventory_revision_id: revision_id,
            occurred_at: now,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(value: Value) -> InventorySnapshot {
        InventorySnapshot(value.as_object().unwrap().clone())
    }

    #[test]
    fn object_key_order_is_not_an_inventory_change() {
        let endpoint_id = EndpointId::new();
        let now = Utc::now();
        let first = record_inventory_on_change(
            endpoint_id,
            None,
            snapshot(json!({"cpu": "sim", "memory": {"bytes": 8}})),
            now,
        )
        .unwrap();

        assert!(record_inventory_on_change(
            endpoint_id,
            Some(&first.revision),
            snapshot(json!({"memory": {"bytes": 8}, "cpu": "sim"})),
            now,
        )
        .is_none());
    }

    #[test]
    fn changed_snapshot_creates_a_fresh_revision_and_event() {
        let endpoint_id = EndpointId::new();
        let now = Utc::now();
        let first = record_inventory_on_change(
            endpoint_id,
            None,
            snapshot(json!({"memory_bytes": 8})),
            now,
        )
        .unwrap();
        let changed = record_inventory_on_change(
            endpoint_id,
            Some(&first.revision),
            snapshot(json!({"memory_bytes": 16})),
            now,
        )
        .unwrap();

        assert_ne!(changed.revision.id, first.revision.id);
        assert!(matches!(
            changed.event,
            DomainEvent::InventoryRevisionRecorded { .. }
        ));
    }
}
