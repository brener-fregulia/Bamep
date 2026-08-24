//! Transient Attempt-scoped technical-resource arbiter (Runtime Service)
//! (`m0-job-lifecycle-and-scheduling.md` "Resource leases": "Other
//! constrained resources are Attempt-scoped, including network, storage
//! read/write, and CPU/Worker capacity"; Issue #32 "Minimum transient
//! Attempt-scoped technical-resource arbiter"). `bamep_server::application::FinalDispatchService`
//! (Issue #25) composes this arbiter, acquiring the reservation it needs
//! immediately before final dispatch revalidation and releasing it on any
//! non-success outcome.
//!
//! Deterministic in-memory capacity bookkeeping only — never persisted
//! (`m0-persistence-observability-and-domain-events.md` "Durable versus
//! transient state"). A freshly constructed arbiter never reconstructs a
//! previously granted reservation, exactly like [`super::presence::PresenceRegistry`]
//! never reconstructs presence from durable state after restart.
//!
//! Endpoint exclusivity is separate, durable, Job-scoped state
//! (`bamep_domain::job::admit_job`); this arbiter never represents it and
//! grants no reservation that could substitute for it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Generic technical-resource identity — network/storage/CPU-worker capacity
/// or any other M1 technical-resource class. Deliberately a plain owned
/// string rather than a closed enum: this Runtime Service does not hard-code
/// production resource topology (`m0-job-lifecycle-and-scheduling.md` "Out of
/// scope"; Issue #32 "Do not hard-code production topology").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceKind(pub String);

impl ResourceKind {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

/// One requested claim against one [`ResourceKind`].
#[derive(Debug, Clone)]
pub struct ResourceClaim {
    pub kind: ResourceKind,
    pub quantity: u64,
}

impl ResourceClaim {
    pub fn new(kind: ResourceKind, quantity: u64) -> Self {
        Self { kind, quantity }
    }
}

/// Opaque handle for one successful acquisition (Issue #32: "a narrow opaque
/// reservation/lease handle"). Carries no resource/quantity detail itself —
/// only [`TechnicalResourceArbiter::release`] can use it, and only to release
/// exactly the reservation it identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReservationId(u64);

/// Rejection from [`TechnicalResourceArbiter::acquire`]: at least one
/// requested claim did not fit under its configured capacity. Nothing is
/// reserved when this is returned — a failed acquisition consumes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("insufficient technical-resource capacity")]
pub struct InsufficientCapacity;

/// Deterministic transient technical-resource arbiter. Configured with fixed
/// capacities at construction. Acquisition is all-or-nothing across every
/// requested claim, contention is a deterministic explicit rejection (never a
/// queue/wait), and no fairness, priority, or production topology is
/// implemented — those remain explicitly out of scope for this M1 baseline.
pub struct TechnicalResourceArbiter {
    capacities: HashMap<ResourceKind, u64>,
    reserved: Mutex<HashMap<ResourceKind, u64>>,
    reservations: Mutex<HashMap<ReservationId, Vec<ResourceClaim>>>,
    next_id: AtomicU64,
}

impl TechnicalResourceArbiter {
    /// Builds an arbiter with fixed, deterministic capacities per
    /// [`ResourceKind`]. A resource kind never mentioned here has zero
    /// capacity — any claim against it always fails.
    pub fn new(capacities: impl IntoIterator<Item = (ResourceKind, u64)>) -> Self {
        Self {
            capacities: capacities.into_iter().collect(),
            reserved: Mutex::new(HashMap::new()),
            reservations: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Attempts to acquire every claim in `claims` atomically: it succeeds
    /// only when each requested resource kind's already-reserved quantity
    /// plus this request's quantity still fits its configured capacity, and
    /// in that case reserves every claim together. A caller that splits one
    /// logical claim into several entries of the same [`ResourceKind`] cannot
    /// bypass this check — same-kind claims within one request are merged
    /// before the capacity comparison. On success, returns an opaque
    /// [`ReservationId`] good for exactly one later [`Self::release`]. On
    /// failure — including an arithmetic overflow while aggregating
    /// same-kind claims or while combining a requested quantity with
    /// already-reserved capacity — no capacity is consumed. An overflow can
    /// never represent a legitimate grant (the true required total exceeds
    /// what `u64` — and therefore any representable capacity, even
    /// `u64::MAX` — can hold), so it is treated exactly like exceeding
    /// configured capacity rather than panicking, wrapping, or silently
    /// saturating into a false "fits" result.
    pub fn acquire(
        &self,
        claims: Vec<ResourceClaim>,
    ) -> Result<ReservationId, InsufficientCapacity> {
        let mut requested: HashMap<ResourceKind, u64> = HashMap::new();
        for claim in &claims {
            let entry = requested.entry(claim.kind.clone()).or_insert(0);
            *entry = entry
                .checked_add(claim.quantity)
                .ok_or(InsufficientCapacity)?;
        }

        let mut reserved = self.reserved.lock().expect("arbiter lock poisoned");

        // Validate every requested kind first, computing its new reserved
        // total without mutating anything yet.
        let mut new_totals: HashMap<ResourceKind, u64> = HashMap::with_capacity(requested.len());
        for (kind, quantity) in &requested {
            let capacity = self.capacities.get(kind).copied().unwrap_or(0);
            let already_reserved = reserved.get(kind).copied().unwrap_or(0);
            let new_total = already_reserved
                .checked_add(*quantity)
                .ok_or(InsufficientCapacity)?;
            if new_total > capacity {
                return Err(InsufficientCapacity);
            }
            new_totals.insert(kind.clone(), new_total);
        }

        // Every kind is already validated above — apply the precomputed
        // totals directly (a plain assignment, never further arithmetic, so
        // this step itself cannot overflow).
        for (kind, new_total) in new_totals {
            reserved.insert(kind, new_total);
        }
        drop(reserved);

        let id = ReservationId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.reservations
            .lock()
            .expect("arbiter lock poisoned")
            .insert(id, claims);
        Ok(id)
    }

    /// Releases exactly the reservation identified by `id`, restoring its
    /// claimed capacity. A no-op for an unknown or already-released `id` —
    /// releasing one reservation never affects another.
    pub fn release(&self, id: ReservationId) {
        let Some(claims) = self
            .reservations
            .lock()
            .expect("arbiter lock poisoned")
            .remove(&id)
        else {
            return;
        };
        let mut reserved = self.reserved.lock().expect("arbiter lock poisoned");
        for claim in claims {
            if let Some(current) = reserved.get_mut(&claim.kind) {
                *current = current.saturating_sub(claim.quantity);
                if *current == 0 {
                    reserved.remove(&claim.kind);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network() -> ResourceKind {
        ResourceKind::new("network")
    }

    fn storage() -> ResourceKind {
        ResourceKind::new("storage")
    }

    #[test]
    fn a_claim_within_capacity_is_granted() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);
        assert!(arbiter
            .acquire(vec![ResourceClaim::new(network(), 5)])
            .is_ok());
    }

    #[test]
    fn a_claim_exceeding_capacity_is_rejected_and_consumes_nothing() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);
        assert_eq!(
            arbiter.acquire(vec![ResourceClaim::new(network(), 11)]),
            Err(InsufficientCapacity)
        );
        // Full capacity must still be available for a subsequent request.
        assert!(arbiter
            .acquire(vec![ResourceClaim::new(network(), 10)])
            .is_ok());
    }

    #[test]
    fn a_second_competing_claim_is_rejected_once_capacity_is_exhausted() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);
        let first = arbiter.acquire(vec![ResourceClaim::new(network(), 10)]);
        assert!(first.is_ok());

        let second = arbiter.acquire(vec![ResourceClaim::new(network(), 1)]);
        assert_eq!(second, Err(InsufficientCapacity));
    }

    #[test]
    fn releasing_a_reservation_restores_capacity_for_a_later_claim() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);
        let first = arbiter
            .acquire(vec![ResourceClaim::new(network(), 10)])
            .unwrap();
        assert_eq!(
            arbiter.acquire(vec![ResourceClaim::new(network(), 1)]),
            Err(InsufficientCapacity)
        );

        arbiter.release(first);

        assert!(arbiter
            .acquire(vec![ResourceClaim::new(network(), 10)])
            .is_ok());
    }

    #[test]
    fn releasing_one_reservation_does_not_release_another() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);
        let a = arbiter
            .acquire(vec![ResourceClaim::new(network(), 5)])
            .unwrap();
        let b = arbiter
            .acquire(vec![ResourceClaim::new(network(), 5)])
            .unwrap();
        assert_ne!(a, b);

        arbiter.release(a);

        // B's 5 units remain reserved: only 5 more must be acquirable, not 10.
        assert_eq!(
            arbiter.acquire(vec![ResourceClaim::new(network(), 6)]),
            Err(InsufficientCapacity)
        );
        assert!(arbiter
            .acquire(vec![ResourceClaim::new(network(), 5)])
            .is_ok());
    }

    #[test]
    fn releasing_an_unknown_reservation_is_a_safe_no_op() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);
        let real = arbiter
            .acquire(vec![ResourceClaim::new(network(), 10)])
            .unwrap();

        // A reservation id this arbiter never issued.
        arbiter.release(ReservationId(9999));

        // The real reservation's capacity must be untouched.
        assert_eq!(
            arbiter.acquire(vec![ResourceClaim::new(network(), 1)]),
            Err(InsufficientCapacity)
        );
        arbiter.release(real);
        assert!(arbiter
            .acquire(vec![ResourceClaim::new(network(), 10)])
            .is_ok());
    }

    #[test]
    fn a_failed_multi_resource_acquisition_is_all_or_nothing() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10), (storage(), 10)]);

        let result = arbiter.acquire(vec![
            ResourceClaim::new(network(), 5),
            ResourceClaim::new(storage(), 11),
        ]);
        assert_eq!(result, Err(InsufficientCapacity));

        // Neither claim may have been partially reserved: both resources
        // must still admit their full capacity afterward.
        assert!(arbiter
            .acquire(vec![
                ResourceClaim::new(network(), 10),
                ResourceClaim::new(storage(), 10)
            ])
            .is_ok());
    }

    #[test]
    fn an_unconfigured_resource_kind_has_zero_capacity() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);
        assert_eq!(
            arbiter.acquire(vec![ResourceClaim::new(storage(), 1)]),
            Err(InsufficientCapacity)
        );
    }

    #[test]
    fn multiple_claims_for_the_same_kind_aggregate_correctly() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);

        // Two claims of 4 each for the same kind must aggregate to 8 before
        // the capacity check, not be treated as two independent claims each
        // checked against the full capacity.
        assert!(arbiter
            .acquire(vec![
                ResourceClaim::new(network(), 4),
                ResourceClaim::new(network(), 4),
            ])
            .is_ok());

        // 8 is already reserved; only 2 more units must fit.
        assert_eq!(
            arbiter.acquire(vec![ResourceClaim::new(network(), 3)]),
            Err(InsufficientCapacity)
        );
        assert!(arbiter
            .acquire(vec![ResourceClaim::new(network(), 2)])
            .is_ok());
    }

    #[test]
    fn overflowing_same_kind_requested_quantities_are_rejected_without_consuming_capacity() {
        let arbiter = TechnicalResourceArbiter::new([(network(), u64::MAX)]);

        // Aggregating these two claims for the same kind overflows u64.
        let result = arbiter.acquire(vec![
            ResourceClaim::new(network(), u64::MAX),
            ResourceClaim::new(network(), 1),
        ]);
        assert_eq!(result, Err(InsufficientCapacity));

        // Nothing was consumed: the full capacity must still be acquirable.
        assert!(arbiter
            .acquire(vec![ResourceClaim::new(network(), u64::MAX)])
            .is_ok());
    }

    #[test]
    fn already_reserved_plus_requested_overflow_is_rejected_even_at_u64_max_capacity() {
        let arbiter = TechnicalResourceArbiter::new([(network(), u64::MAX)]);
        let first = arbiter
            .acquire(vec![ResourceClaim::new(network(), u64::MAX - 5)])
            .unwrap();

        // already_reserved (MAX - 5) + requested (10) overflows u64. A
        // saturating comparison against a `u64::MAX` capacity would
        // incorrectly appear to "fit"; this must still be rejected.
        let result = arbiter.acquire(vec![ResourceClaim::new(network(), 10)]);
        assert_eq!(result, Err(InsufficientCapacity));

        // The original reservation must be untouched by the rejected attempt.
        arbiter.release(first);
        assert!(arbiter
            .acquire(vec![ResourceClaim::new(network(), u64::MAX)])
            .is_ok());
    }

    #[test]
    fn a_fresh_arbiter_after_restart_reconstructs_no_reservations() {
        let arbiter = TechnicalResourceArbiter::new([(network(), 10)]);
        let _ = arbiter
            .acquire(vec![ResourceClaim::new(network(), 10)])
            .unwrap();

        // A brand-new instance — standing in for a Server restart — must
        // start with full capacity, never inheriting the prior instance's
        // reservations (they were never persisted).
        let restarted = TechnicalResourceArbiter::new([(network(), 10)]);
        assert!(restarted
            .acquire(vec![ResourceClaim::new(network(), 10)])
            .is_ok());
    }
}
