//! Transient issued-capability store (Runtime Service)
//! (`docs/specifications/m0-data-plane-and-storage-contracts.md` "Durable
//! versus transient authorization state": "Transient state includes issued
//! capabilities ... They are not persisted as reusable Endpoint
//! credentials"; Issue #38).
//!
//! Memory-only, exactly like [`super::presence::PresenceRegistry`] and
//! [`super::reservation_registry::AttemptReservationRegistry`] — a freshly
//! constructed store never reconstructs a previously issued capability. This
//! is one half of the process-restart invalidation mechanism
//! (`m0-data-plane-and-storage-contracts.md` "Server restart"): a capability
//! issued by a prior `bamepd` process simply does not exist in a freshly
//! started process's store. [`ProcessAuthorizationEpoch`] additionally
//! ensures no future persistence of this store could silently defeat that
//! invariant — every stored binding also carries the exact epoch this store
//! was constructed with, and `bamep_domain::capability_is_current` requires
//! it to match the *currently running* process's epoch, not merely "some
//! previously issued epoch".

use std::collections::HashMap;
use std::sync::Mutex;

use bamep_domain::{CapabilityBinding, CapabilityId, ProcessAuthorizationEpoch};
use chrono::{DateTime, Utc};

pub struct CapabilityStore {
    epoch: ProcessAuthorizationEpoch,
    capabilities: Mutex<HashMap<CapabilityId, CapabilityBinding>>,
}

impl CapabilityStore {
    /// Generates a fresh [`ProcessAuthorizationEpoch`] — call exactly once
    /// per `bamepd` process lifetime, at composition-root startup.
    pub fn new() -> Self {
        Self {
            epoch: ProcessAuthorizationEpoch::generate(),
            capabilities: Mutex::new(HashMap::new()),
        }
    }

    /// The authorization epoch every capability issued through this store
    /// instance carries. Capability verification must compare a presented
    /// capability's stored epoch against *this* value, obtained from the
    /// same store instance doing the lookup.
    pub fn epoch(&self) -> ProcessAuthorizationEpoch {
        self.epoch
    }

    /// Durably (for the process lifetime) records a freshly issued
    /// capability. Overwrites nothing meaningful in practice — `capability_id`
    /// is derived from a fresh CSPRNG-generated token
    /// (`bamep_domain::CapabilityToken::generate`), so a collision with an
    /// existing entry is not a realistic event this method needs to guard
    /// against.
    pub fn issue(&self, capability_id: CapabilityId, binding: CapabilityBinding) {
        self.capabilities
            .lock()
            .expect("capability store lock poisoned")
            .insert(capability_id, binding);
    }

    /// Looks up a previously issued capability by its derived identity.
    /// `None` covers both "never issued" and "issued by a different process
    /// lifetime" (this store simply never held it) uniformly — the caller
    /// must treat both as the same generic denial.
    pub fn lookup(&self, capability_id: &CapabilityId) -> Option<CapabilityBinding> {
        self.capabilities
            .lock()
            .expect("capability store lock poisoned")
            .get(capability_id)
            .copied()
    }

    /// Removes every capability whose `expires_at` is no longer in the
    /// future, bounding this store's memory over the process lifetime.
    /// Opportunistic — callers invoke this around issuance rather than on a
    /// dedicated background timer, matching this Work Package's minimum
    /// scope.
    pub fn evict_expired(&self, now: DateTime<Utc>) {
        self.capabilities
            .lock()
            .expect("capability store lock poisoned")
            .retain(|_, binding| binding.expires_at > now);
    }
}

impl Default for CapabilityStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamep_domain::{
        ArtifactId, AttemptId, EndpointId, ProofPublicKey, TransferDirection, TransferId,
    };

    fn sample_binding(now: DateTime<Utc>, epoch: ProcessAuthorizationEpoch) -> CapabilityBinding {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let public = ProofPublicKey::from_bytes(signing_key.verifying_key().to_bytes()).unwrap();
        CapabilityBinding {
            endpoint_id: EndpointId::new(),
            transfer_id: TransferId::new(),
            artifact_id: ArtifactId::new(),
            direction: TransferDirection::AgentToServer,
            attempt_id: AttemptId::new(),
            proof_public_key: public,
            expires_at: now + chrono::Duration::minutes(5),
            epoch,
        }
    }

    fn capability_id(seed: u8) -> CapabilityId {
        CapabilityId::from_token_bytes(&[seed; 8])
    }

    #[test]
    fn a_fresh_store_holds_nothing() {
        let store = CapabilityStore::new();
        assert!(store.lookup(&capability_id(1)).is_none());
    }

    #[test]
    fn issued_capabilities_are_found_by_their_exact_identity() {
        let store = CapabilityStore::new();
        let now = Utc::now();
        let binding = sample_binding(now, store.epoch());
        let id = capability_id(2);
        store.issue(id, binding);

        let found = store
            .lookup(&id)
            .expect("just-issued capability must be found");
        assert_eq!(found.transfer_id, binding.transfer_id);
        assert_eq!(found.epoch, store.epoch());
    }

    #[test]
    fn two_independently_constructed_stores_never_share_an_epoch() {
        let a = CapabilityStore::new();
        let b = CapabilityStore::new();
        assert_ne!(
            a.epoch(),
            b.epoch(),
            "each process-lifetime store must mint its own fresh epoch, mirroring a bamepd restart"
        );
    }

    #[test]
    fn evict_expired_removes_only_expired_entries() {
        let store = CapabilityStore::new();
        let now = Utc::now();
        let live_id = capability_id(3);
        let mut live = sample_binding(now, store.epoch());
        live.expires_at = now + chrono::Duration::minutes(5);
        store.issue(live_id, live);

        let expired_id = capability_id(4);
        let mut expired = sample_binding(now, store.epoch());
        expired.expires_at = now - chrono::Duration::seconds(1);
        store.issue(expired_id, expired);

        store.evict_expired(now);

        assert!(store.lookup(&live_id).is_some());
        assert!(store.lookup(&expired_id).is_none());
    }
}
