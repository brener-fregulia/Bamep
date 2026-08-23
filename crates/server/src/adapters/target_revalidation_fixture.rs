//! Deterministic M1 fixture behind [`crate::ports::TargetRevalidationPort`]
//! (`docs/specifications/m0-simulator-contract-and-validation-strategy.md`
//! "Target-disk revalidation fidelity boundary"): a faked hardware/probing
//! boundary standing in for physical target-disk revalidation. Memory-only,
//! and independently configurable from `InventoryRevision` state so
//! Simulator/test scenarios can prove destructive-operation preconditions 4
//! and 5 fail independently — this fixture never reads inventory and never
//! parses inventory JSON. No physical disk probe, no production disk/
//! partition/storage-topology model, no new Agent Protocol message.

use std::collections::HashMap;
use std::sync::Mutex;

use bamep_domain::{EndpointId, TargetFingerprint};

use crate::ports::TargetRevalidationPort;

#[derive(Default)]
pub struct FixtureTargetRevalidation {
    current: Mutex<HashMap<EndpointId, TargetFingerprint>>,
}

impl FixtureTargetRevalidation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets (or replaces) the currently observed target-disk fingerprint for
    /// `endpoint_id`. Callers may move an Endpoint from one target to another
    /// — or configure a mismatch — at any time, entirely independently of any
    /// `InventoryRevision` state.
    pub fn set_current_target(&self, endpoint_id: EndpointId, fingerprint: TargetFingerprint) {
        self.current
            .lock()
            .expect("target revalidation fixture lock poisoned")
            .insert(endpoint_id, fingerprint);
    }

    /// Clears any configured current target-disk fingerprint for
    /// `endpoint_id`, so [`TargetRevalidationPort::current_target_fingerprint`]
    /// returns `None` for it again.
    pub fn clear_current_target(&self, endpoint_id: EndpointId) {
        self.current
            .lock()
            .expect("target revalidation fixture lock poisoned")
            .remove(&endpoint_id);
    }
}

impl TargetRevalidationPort for FixtureTargetRevalidation {
    fn current_target_fingerprint(&self, endpoint_id: EndpointId) -> Option<TargetFingerprint> {
        self.current
            .lock()
            .expect("target revalidation fixture lock poisoned")
            .get(&endpoint_id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_endpoint_has_no_current_target() {
        let fixture = FixtureTargetRevalidation::new();
        assert_eq!(fixture.current_target_fingerprint(EndpointId::new()), None);
    }

    #[test]
    fn configured_target_is_returned_deterministically() {
        let fixture = FixtureTargetRevalidation::new();
        let endpoint_id = EndpointId::new();
        fixture.set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));

        assert_eq!(
            fixture.current_target_fingerprint(endpoint_id),
            Some(TargetFingerprint::new("disk-a"))
        );
        assert_eq!(
            fixture.current_target_fingerprint(endpoint_id),
            Some(TargetFingerprint::new("disk-a")),
            "repeated reads must be deterministic"
        );
    }

    #[test]
    fn target_can_change_without_any_inventory_involvement() {
        let fixture = FixtureTargetRevalidation::new();
        let endpoint_id = EndpointId::new();
        fixture.set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));
        fixture.set_current_target(endpoint_id, TargetFingerprint::new("disk-b"));

        assert_eq!(
            fixture.current_target_fingerprint(endpoint_id),
            Some(TargetFingerprint::new("disk-b")),
            "the fixture's current target must be independently mutable"
        );
    }

    #[test]
    fn different_endpoints_are_isolated() {
        let fixture = FixtureTargetRevalidation::new();
        let a = EndpointId::new();
        let b = EndpointId::new();
        fixture.set_current_target(a, TargetFingerprint::new("disk-a"));

        assert_eq!(
            fixture.current_target_fingerprint(a),
            Some(TargetFingerprint::new("disk-a"))
        );
        assert_eq!(fixture.current_target_fingerprint(b), None);
    }

    #[test]
    fn clearing_a_configured_target_returns_it_to_absent() {
        let fixture = FixtureTargetRevalidation::new();
        let endpoint_id = EndpointId::new();
        fixture.set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));
        fixture.clear_current_target(endpoint_id);

        assert_eq!(fixture.current_target_fingerprint(endpoint_id), None);
    }
}
