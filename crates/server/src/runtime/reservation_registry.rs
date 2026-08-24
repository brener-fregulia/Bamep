//! Transient Attempt reservation registry (Runtime Service)
//! (`m0-job-lifecycle-and-scheduling.md` "Resource leases"; Issue #26
//! "Attempt reservation registry"): the minimum transient ownership
//! structure `AttemptId -> ReservationId`, composing #25's held
//! [`ReservationId`](crate::runtime::resource_arbiter::ReservationId) so #26
//! knows exactly which reservation to release when an Attempt reaches an
//! authoritative terminal outcome.
//!
//! Memory-only — never persisted
//! (`m0-persistence-observability-and-domain-events.md` "Durable versus
//! transient state"). A freshly constructed registry never reconstructs a
//! previously registered mapping, exactly like
//! [`super::presence::PresenceRegistry`] and
//! [`super::resource_arbiter::TechnicalResourceArbiter`] never reconstruct
//! their own state after a Server restart — Server-restart recovery of lost
//! mappings belongs to #28.
//!
//! Deliberately not in `adapters` — this registry never touches WebSocket/
//! transport infrastructure at all.

use std::collections::HashMap;
use std::sync::Mutex;

use bamep_domain::AttemptId;

use super::resource_arbiter::ReservationId;

#[derive(Default)]
pub struct AttemptReservationRegistry {
    reservations: Mutex<HashMap<AttemptId, ReservationId>>,
}

impl AttemptReservationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `reservation_id` for `attempt_id`. Must happen before
    /// outbound dispatch becomes reachable for that Attempt
    /// (`m0-job-lifecycle-and-scheduling.md`; Issue #26: "register the
    /// mapping immediately after #25 returns the committed Attempt/
    /// reservation and before outbound dispatch becomes reachable").
    pub fn register(&self, attempt_id: AttemptId, reservation_id: ReservationId) {
        self.reservations
            .lock()
            .expect("attempt reservation registry lock poisoned")
            .insert(attempt_id, reservation_id);
    }

    /// Removes and returns the reservation registered for `attempt_id`,
    /// exactly once. An unknown or already-removed `attempt_id` is a safe
    /// no-op (`None`) — this is the atomicity boundary that prevents
    /// duplicate terminal evidence from releasing the same reservation
    /// twice: only the caller that receives `Some` may release it through
    /// [`super::resource_arbiter::TechnicalResourceArbiter`].
    pub fn take(&self, attempt_id: AttemptId) -> Option<ReservationId> {
        self.reservations
            .lock()
            .expect("attempt reservation registry lock poisoned")
            .remove(&attempt_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::resource_arbiter::{ResourceClaim, ResourceKind, TechnicalResourceArbiter};

    #[test]
    fn take_returns_the_registered_reservation_exactly_once() {
        let registry = AttemptReservationRegistry::new();
        let arbiter = TechnicalResourceArbiter::new([(ResourceKind::new("network"), 10)]);
        let attempt_id = AttemptId::new();
        let reservation = arbiter
            .acquire(vec![ResourceClaim::new(ResourceKind::new("network"), 10)])
            .unwrap();

        registry.register(attempt_id, reservation);
        assert_eq!(registry.take(attempt_id), Some(reservation));
        assert_eq!(
            registry.take(attempt_id),
            None,
            "a second take of the same attempt_id must be a no-op"
        );
    }

    #[test]
    fn take_on_an_unknown_attempt_id_is_a_safe_no_op() {
        let registry = AttemptReservationRegistry::new();
        assert_eq!(registry.take(AttemptId::new()), None);
    }

    #[test]
    fn different_attempts_remain_independent() {
        let registry = AttemptReservationRegistry::new();
        let arbiter = TechnicalResourceArbiter::new([(ResourceKind::new("network"), 10)]);
        let (a, b) = (AttemptId::new(), AttemptId::new());
        let ra = arbiter
            .acquire(vec![ResourceClaim::new(ResourceKind::new("network"), 5)])
            .unwrap();
        let rb = arbiter
            .acquire(vec![ResourceClaim::new(ResourceKind::new("network"), 5)])
            .unwrap();
        registry.register(a, ra);
        registry.register(b, rb);

        assert_eq!(registry.take(a), Some(ra));
        assert_eq!(
            registry.take(b),
            Some(rb),
            "removing A's mapping must not disturb B's"
        );
    }
}
