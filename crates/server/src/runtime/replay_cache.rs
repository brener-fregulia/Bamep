//! Transient `proof_id` replay cache (Runtime Service)
//! (`docs/specifications/m0-data-plane-and-storage-contracts.md` "Replay and
//! freshness"; Issue #38 acceptance criterion: "Replay-cache insertion/
//! checking is atomic enough that concurrent duplicate proof use cannot both
//! succeed").
//!
//! Memory-only, mirroring [`super::reservation_registry::AttemptReservationRegistry`]'s
//! `Entry`-based check-and-insert discipline: the check and the insert happen
//! under one lock acquisition, so two concurrent callers racing the exact
//! same `proof_id` can never both observe "not yet seen". `proof_id` is
//! generated fresh per request by the Agent from the OS CSPRNG
//! (`m0-data-plane-and-storage-contracts.md` "Freshness and replay
//! representation") — this cache is scoped globally by that value alone,
//! matching the Specification's own wording ("Each proof has a unique
//! unpredictable `proof_id`"; no compound scoping by capability is stated),
//! and a coincidental collision between two unrelated legitimate proofs is
//! cryptographically negligible (128 bits of entropy).
//!
//! Bounded: every `check_and_insert` call opportunistically evicts entries
//! older than `retention`, so this cache never grows for the lifetime of the
//! daemon — it holds only entries within the last `retention` duration,
//! which the caller sets to at least the accepted proof-freshness window
//! (`m0-data-plane-and-storage-contracts.md`: "Accepted `proof_id` values
//! remain in a bounded transient replay cache for at least the accepted
//! freshness window"). Loss of this cache's contents (e.g. a `bamepd`
//! restart) is exactly the process-restart invalidation the capability
//! store's [`bamep_domain::ProcessAuthorizationEpoch`] already covers
//! independently — this cache never attempts to survive a restart itself.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Mutex;

use bamep_domain::ProofId;
use chrono::{DateTime, Duration, Utc};

/// A `proof_id` that was already accepted and is therefore being replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("proof_id was already accepted and cannot be reused")]
pub struct ProofReplayed;

#[derive(Default)]
pub struct ReplayCache {
    seen: Mutex<HashMap<ProofId, DateTime<Utc>>>,
}

impl ReplayCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically checks whether `proof_id` was already accepted and, only
    /// if not, records it as accepted at `now` — one lock acquisition, one
    /// `HashMap` `Entry` decision, never a separate `contains`-then-`insert`
    /// pair (Issue #38 "Do NOT implement `contains()` ... under separate
    /// unlocked operations"). Opportunistically evicts every entry older
    /// than `retention` before deciding, so this cache's memory stays
    /// bounded by the retention window rather than the daemon's uptime.
    pub fn check_and_insert(
        &self,
        proof_id: ProofId,
        now: DateTime<Utc>,
        retention: Duration,
    ) -> Result<(), ProofReplayed> {
        let mut seen = self.seen.lock().expect("replay cache lock poisoned");
        seen.retain(|_, seen_at| now.signed_duration_since(*seen_at) <= retention);

        match seen.entry(proof_id) {
            Entry::Occupied(_) => Err(ProofReplayed),
            Entry::Vacant(slot) => {
                slot.insert(now);
                Ok(())
            }
        }
    }

    /// Current entry count — test/diagnostic use only, to observe bounded
    /// eviction behavior.
    pub fn len(&self) -> usize {
        self.seen.lock().expect("replay cache lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retention() -> Duration {
        Duration::milliseconds(bamep_domain::PROOF_FRESHNESS_PAST_WINDOW_MILLIS)
    }

    #[test]
    fn a_fresh_proof_id_is_accepted() {
        let cache = ReplayCache::new();
        assert_eq!(
            cache.check_and_insert(ProofId::generate(), Utc::now(), retention()),
            Ok(())
        );
    }

    #[test]
    fn the_exact_same_proof_id_is_rejected_on_reuse() {
        let cache = ReplayCache::new();
        let proof_id = ProofId::generate();
        let now = Utc::now();
        assert_eq!(cache.check_and_insert(proof_id, now, retention()), Ok(()));
        assert_eq!(
            cache.check_and_insert(proof_id, now, retention()),
            Err(ProofReplayed)
        );
    }

    #[test]
    fn different_proof_ids_remain_independent() {
        let cache = ReplayCache::new();
        let now = Utc::now();
        let a = ProofId::generate();
        let b = ProofId::generate();
        assert_eq!(cache.check_and_insert(a, now, retention()), Ok(()));
        assert_eq!(cache.check_and_insert(b, now, retention()), Ok(()));
    }

    #[test]
    fn entries_older_than_retention_are_evicted_and_therefore_reusable() {
        let cache = ReplayCache::new();
        let proof_id = ProofId::generate();
        let t0 = Utc::now();
        assert_eq!(cache.check_and_insert(proof_id, t0, retention()), Ok(()));

        // Still within the window: replay is still rejected.
        let t1 = t0 + retention() - Duration::milliseconds(1);
        assert_eq!(
            cache.check_and_insert(proof_id, t1, retention()),
            Err(ProofReplayed)
        );

        // Past the window: the old entry is evicted, so this is no longer
        // recognized as a replay by this cache (freshness itself is a
        // separate, independent check the caller performs beforehand).
        let t2 = t0 + retention() + Duration::milliseconds(1);
        assert_eq!(cache.check_and_insert(proof_id, t2, retention()), Ok(()));
    }

    #[test]
    fn eviction_keeps_memory_bounded_to_the_retention_window() {
        let cache = ReplayCache::new();
        let t0 = Utc::now();
        for _ in 0..50 {
            cache
                .check_and_insert(ProofId::generate(), t0, retention())
                .unwrap();
        }
        assert_eq!(cache.len(), 50);

        let later = t0 + retention() + Duration::milliseconds(1);
        cache
            .check_and_insert(ProofId::generate(), later, retention())
            .unwrap();
        assert_eq!(
            cache.len(),
            1,
            "every entry older than retention must be evicted on the next call"
        );
    }

    #[test]
    fn concurrent_duplicate_proof_use_lets_exactly_one_succeed() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(ReplayCache::new());
        let proof_id = ProofId::generate();
        let now = Utc::now();
        let barrier = Arc::new(std::sync::Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    cache.check_and_insert(proof_id, now, retention()).is_ok()
                })
            })
            .collect();

        let successes = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(
            successes, 1,
            "exactly one concurrent racer must observe acceptance of the same proof_id"
        );
    }
}
