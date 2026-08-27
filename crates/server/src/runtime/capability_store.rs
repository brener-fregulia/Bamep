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

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Mutex;

use bamep_domain::{CapabilityBinding, CapabilityId, ProcessAuthorizationEpoch};
use chrono::{DateTime, Utc};

/// The default bounded capacity of the issued-capability store — the maximum
/// number of live (not-yet-expired) capabilities held at once
/// (`m0-data-plane-and-storage-contracts.md` "Out of scope": exact TTL and
/// cache sizing "remain implementation-time").
///
/// `4096` comfortably covers M1: capabilities are 5-minute-TTL,
/// transfer-scoped, and one-per-active-transfer plus a bounded number of
/// renewals; the deterministic single-Endpoint vertical (#19) and even the
/// 20–24 concurrent Simulated Endpoints of the separate scale exercise (#21)
/// stay far below it. Overridable via [`CapabilityStore::with_capacity`] from
/// the composition root; not a permanently fixed architectural constant.
pub const DEFAULT_CAPABILITY_STORE_CAPACITY: usize = 4096;

/// Why [`CapabilityStore::issue`] refused to record a freshly minted
/// capability. Both variants collapse to the single generic non-enumerable
/// authorization denial at the Agent boundary
/// (`m0-data-plane-and-storage-contracts.md`: denials are non-enumerable) —
/// a caller must not surface either cause as a distinct externally
/// observable reason. Kept as two internal variants (Issue #38 final
/// correction §9) so the two causes are never confused with each other
/// internally: capacity saturation is an expected, load-dependent condition,
/// while an `IdCollision` would mean a fresh CSPRNG-derived `CapabilityId`
/// collided with a still-live binding — cryptographically negligible, but
/// never silently overwritten if it ever happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityIssueError {
    #[error("issued-capability store is at capacity; issuance fails closed")]
    Saturated,
    #[error("capability_id collides with a still-live binding; issuance fails closed")]
    IdCollision,
}

pub struct CapabilityStore {
    epoch: ProcessAuthorizationEpoch,
    capacity: usize,
    capabilities: Mutex<HashMap<CapabilityId, CapabilityBinding>>,
}

impl CapabilityStore {
    /// Generates a fresh [`ProcessAuthorizationEpoch`] — call exactly once
    /// per `bamepd` process lifetime, at composition-root startup. Uses
    /// [`DEFAULT_CAPABILITY_STORE_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPABILITY_STORE_CAPACITY)
    }

    /// A store with an explicit finite capacity (tests use a small value to
    /// exercise saturation); `capacity` is clamped to at least 1.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            epoch: ProcessAuthorizationEpoch::generate(),
            capacity: capacity.max(1),
            capabilities: Mutex::new(HashMap::new()),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The authorization epoch every capability issued through this store
    /// instance carries. Capability verification must compare a presented
    /// capability's stored epoch against *this* value, obtained from the
    /// same store instance doing the lookup.
    pub fn epoch(&self) -> ProcessAuthorizationEpoch {
        self.epoch
    }

    /// Durably (for the process lifetime) records a freshly issued
    /// capability, failing closed with [`CapabilityIssueError::Saturated`]
    /// when the store is already at [`capacity`](Self::capacity) and this
    /// `capability_id` is not already present — a still-live capability is
    /// never evicted to make room, since that would silently and
    /// unpredictably change authorization semantics for whatever transfer it
    /// authorized. Callers should [`evict_expired`](Self::evict_expired)
    /// first so only genuinely live capabilities count toward the bound.
    ///
    /// `capability_id` is derived from a fresh CSPRNG-generated token
    /// (`bamep_domain::CapabilityToken::generate`), so a collision with an
    /// existing entry is not a realistic event; the `Occupied` arm exists for
    /// total coverage and fails closed with
    /// [`CapabilityIssueError::IdCollision`] instead — a live binding is
    /// never overwritten with different newly-issued authorization material
    /// (Issue #38 final correction §9).
    pub fn issue(
        &self,
        capability_id: CapabilityId,
        binding: CapabilityBinding,
    ) -> Result<(), CapabilityIssueError> {
        let mut capabilities = self
            .capabilities
            .lock()
            .expect("capability store lock poisoned");
        let at_capacity = capabilities.len() >= self.capacity;
        match capabilities.entry(capability_id) {
            Entry::Occupied(_) => Err(CapabilityIssueError::IdCollision),
            Entry::Vacant(slot) => {
                if at_capacity {
                    return Err(CapabilityIssueError::Saturated);
                }
                slot.insert(binding);
                Ok(())
            }
        }
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
        store.issue(id, binding).unwrap();

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
        store.issue(live_id, live).unwrap();

        let expired_id = capability_id(4);
        let mut expired = sample_binding(now, store.epoch());
        expired.expires_at = now - chrono::Duration::seconds(1);
        store.issue(expired_id, expired).unwrap();

        store.evict_expired(now);

        assert!(store.lookup(&live_id).is_some());
        assert!(store.lookup(&expired_id).is_none());
    }

    #[test]
    fn issuance_fails_closed_at_capacity_without_evicting_a_live_capability() {
        let store = CapabilityStore::with_capacity(2);
        let now = Utc::now();
        let a = capability_id(10);
        let b = capability_id(11);
        store.issue(a, sample_binding(now, store.epoch())).unwrap();
        store.issue(b, sample_binding(now, store.epoch())).unwrap();

        assert_eq!(
            store.issue(capability_id(12), sample_binding(now, store.epoch())),
            Err(CapabilityIssueError::Saturated),
            "a genuinely new capability must fail closed at capacity"
        );
        // Neither live capability was evicted to make room.
        assert!(store.lookup(&a).is_some());
        assert!(store.lookup(&b).is_some());
    }

    #[test]
    fn a_capability_id_collision_never_overwrites_the_live_binding() {
        let store = CapabilityStore::new();
        let now = Utc::now();
        let id = capability_id(30);
        let original = sample_binding(now, store.epoch());
        store.issue(id, original).unwrap();

        // A deliberately repeated `CapabilityId` presenting a different
        // binding must fail closed rather than silently replace the live
        // one.
        let mut colliding = sample_binding(now, store.epoch());
        colliding.transfer_id = TransferId::new();
        assert_eq!(
            store.issue(id, colliding),
            Err(CapabilityIssueError::IdCollision),
        );

        let found = store
            .lookup(&id)
            .expect("the original live binding must remain");
        assert_eq!(
            found.transfer_id, original.transfer_id,
            "the original binding must be completely unchanged"
        );
    }

    #[test]
    fn expired_capabilities_are_reclaimed_before_capacity_is_evaluated() {
        let store = CapabilityStore::with_capacity(2);
        let now = Utc::now();
        let mut expired = sample_binding(now, store.epoch());
        expired.expires_at = now - chrono::Duration::seconds(1);
        store.issue(capability_id(20), expired).unwrap();
        store
            .issue(capability_id(21), sample_binding(now, store.epoch()))
            .unwrap();

        // At capacity, but one entry is expired: evicting first frees a slot.
        store.evict_expired(now);
        assert_eq!(
            store.issue(capability_id(22), sample_binding(now, store.epoch())),
            Ok(())
        );
    }
}
