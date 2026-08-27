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
//! ## Retention model — proof-specific expiry, not fixed acceptance duration
//!
//! Each entry is keyed by the exact instant after which its proof can no
//! longer satisfy `bamep_domain::proof_is_fresh` for any `now` —
//! `issued_at + PROOF_FRESHNESS_PAST_WINDOW`, from
//! `bamep_domain::proof_replay_valid_until_millis`. An entry is evicted only
//! once `now` is strictly past that instant. A fixed
//! "retain N seconds from acceptance time" model was unsafe: a proof
//! accepted at `t0` with `issued_at = t0 + max_future_skew` stays
//! freshness-valid until `t0 + max_future_skew + past_window`, so a
//! fixed-from-`t0` eviction could drop the entry while a replay of the exact
//! same signed proof is still freshness-valid, letting it be approved twice.
//! Tying eviction to the proof's own signed freshness deadline closes that
//! gap by construction: every `proof_id` that can still pass `proof_is_fresh`
//! is still in the cache, and the accepted future skew is covered because the
//! deadline is measured from `issued_at`, not from acceptance. Moving the
//! clock backwards only makes `now <= deadline` true for longer, so it
//! conservatively retains entries longer and never makes a replay easier.
//!
//! ## Bounded capacity — fail closed at saturation
//!
//! Time-based expiry bounds how *long* an entry lives, not how *many* live at
//! once, so this cache additionally carries an explicit finite capacity
//! ([`DEFAULT_REPLAY_CACHE_CAPACITY`]). Expired entries are evicted first;
//! then, if inserting a genuinely new `proof_id` would exceed capacity, the
//! insertion fails closed ([`ReplayRejection::CapacitySaturated`]) rather
//! than evicting a still-live entry to make room — evicting a live entry
//! would reopen replay for whatever proof it protected. A `proof_id` already
//! present is still reported as a replay regardless of capacity, so
//! saturation can never let two duplicates through. The Application maps both
//! rejection variants to the single generic non-enumerable denial.
//!
//! Loss of this cache's contents (e.g. a `bamepd` restart) is exactly the
//! process-restart invalidation the capability store's
//! [`bamep_domain::ProcessAuthorizationEpoch`] already covers independently —
//! this cache never attempts to survive a restart itself.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Mutex;

use bamep_domain::ProofId;
use chrono::{DateTime, Utc};

/// The default bounded capacity of the replay cache — the maximum number of
/// live (not-yet-freshness-expired) `proof_id` entries held at once
/// (`m0-data-plane-and-storage-contracts.md` "Replay and freshness": "kept in
/// a bounded transient replay cache"; "replay-cache capacity remain[s]
/// implementation-time").
///
/// `2^16` is chosen as a defensible M1 value. An entry's actual worst-case
/// retention is not the 120 s past window alone (see "Retention model"
/// above): a proof accepted at the maximum accepted future skew (`proof
/// freshness future skew`, 30 s) stays freshness-valid, and therefore
/// cache-resident, for up to `120 s + 30 s = 150 s` from first acceptance. At
/// capacity that bounds sustained throughput at `65536 / 150 s ≈ 436`
/// accepted proofs per second — comfortably beyond any realistic aggregate
/// M1 chunk-request rate across the deterministic single-Endpoint vertical
/// (#19) and even the 20–24 concurrent Simulated Endpoints of the separate
/// scale exercise (#21), each of which mints one `proof_id` per HTTP round
/// trip over a LAN — while capping worst-case memory at a few MiB. Larger
/// deployments override it via [`ReplayCache::with_capacity`] from the
/// composition root; this constant is not a permanently fixed architectural
/// constant.
pub const DEFAULT_REPLAY_CACHE_CAPACITY: usize = 1 << 16;

/// Why a `check_and_insert` call refused to record a `proof_id`. Both
/// variants collapse to the single generic non-enumerable authorization
/// denial at every external boundary (`m0-data-plane-and-storage-contracts.md`
/// "Per-request verification": "All authorization failures return one generic
/// non-enumerable denial").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReplayRejection {
    /// This `proof_id` was already accepted and is still within its own
    /// signed freshness deadline — a replay.
    #[error("proof_id was already accepted and cannot be reused")]
    Replayed,
    /// The cache is at its bounded capacity and every entry it holds is
    /// still live, so this genuinely new `proof_id` cannot be recorded
    /// without reopening replay for an existing one — fail closed.
    #[error("replay cache is at capacity; authorization fails closed")]
    CapacitySaturated,
}

pub struct ReplayCache {
    capacity: usize,
    /// `proof_id -> replay_valid_until` (the instant past which the proof can
    /// no longer be freshness-valid; see module docs).
    seen: Mutex<HashMap<ProofId, DateTime<Utc>>>,
}

impl ReplayCache {
    /// A cache with the [`DEFAULT_REPLAY_CACHE_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_REPLAY_CACHE_CAPACITY)
    }

    /// A cache with an explicit finite capacity. Tests use a small value to
    /// exercise saturation without allocating the production maximum;
    /// `capacity` is clamped to at least 1.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            seen: Mutex::new(HashMap::new()),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Atomically, under one lock acquisition:
    ///
    /// 1. evicts every entry whose proof can no longer be freshness-valid
    ///    (`now` strictly past its `replay_valid_until`);
    /// 2. rejects `proof_id` as [`ReplayRejection::Replayed`] if it is still
    ///    present (a live replay) — this check wins even at saturation, so
    ///    two concurrent duplicates can never both succeed;
    /// 3. rejects a genuinely new `proof_id` as
    ///    [`ReplayRejection::CapacitySaturated`] if recording it would exceed
    ///    `capacity` — a still-live entry is never evicted to make room;
    /// 4. otherwise records `proof_id` with `replay_valid_until` and returns
    ///    `Ok`.
    ///
    /// `replay_valid_until` comes from
    /// `bamep_domain::proof_replay_valid_until_millis(issued_at)` — see module
    /// docs. Never a separate `contains`-then-`insert` pair under two lock
    /// acquisitions (Issue #38 "Do NOT implement `contains()` ... under
    /// separate unlocked operations").
    pub fn check_and_insert(
        &self,
        proof_id: ProofId,
        now: DateTime<Utc>,
        replay_valid_until: DateTime<Utc>,
    ) -> Result<(), ReplayRejection> {
        let mut seen = self.seen.lock().expect("replay cache lock poisoned");
        seen.retain(|_, valid_until| now <= *valid_until);

        let at_capacity = seen.len() >= self.capacity;
        match seen.entry(proof_id) {
            Entry::Occupied(_) => Err(ReplayRejection::Replayed),
            Entry::Vacant(slot) => {
                if at_capacity {
                    return Err(ReplayRejection::CapacitySaturated);
                }
                slot.insert(replay_valid_until);
                Ok(())
            }
        }
    }

    /// Current entry count — test/diagnostic use only, to observe bounded
    /// eviction/saturation behavior.
    pub fn len(&self) -> usize {
        self.seen.lock().expect("replay cache lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ReplayCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamep_domain::{proof_replay_valid_until_millis, PROOF_FRESHNESS_FUTURE_SKEW_MILLIS};
    use chrono::Duration;

    fn valid_until(issued_at_millis: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(
            proof_replay_valid_until_millis(issued_at_millis as u64) as i64
        )
        .unwrap()
    }

    #[test]
    fn a_fresh_proof_id_is_accepted() {
        let cache = ReplayCache::new();
        let now = Utc::now();
        assert_eq!(
            cache.check_and_insert(
                ProofId::generate(),
                now,
                now + Duration::milliseconds(120_000)
            ),
            Ok(())
        );
    }

    #[test]
    fn the_exact_same_proof_id_is_rejected_on_reuse() {
        let cache = ReplayCache::new();
        let proof_id = ProofId::generate();
        let now = Utc::now();
        let vu = now + Duration::milliseconds(120_000);
        assert_eq!(cache.check_and_insert(proof_id, now, vu), Ok(()));
        assert_eq!(
            cache.check_and_insert(proof_id, now, vu),
            Err(ReplayRejection::Replayed)
        );
    }

    #[test]
    fn different_proof_ids_remain_independent() {
        let cache = ReplayCache::new();
        let now = Utc::now();
        let vu = now + Duration::milliseconds(120_000);
        assert_eq!(cache.check_and_insert(ProofId::generate(), now, vu), Ok(()));
        assert_eq!(cache.check_and_insert(ProofId::generate(), now, vu), Ok(()));
    }

    /// The BLOCKER regression (Issue #38 correction §3–§4): a proof issued at
    /// the maximum accepted future skew, first accepted at `t0`, replayed at
    /// `t0 + past_window + epsilon`. The replay MUST still be rejected while
    /// the proof itself remains freshness-valid — the old
    /// "retention from acceptance time" model evicted the entry too early
    /// here and allowed the identical signed proof to be approved twice.
    #[test]
    fn a_future_skew_proof_stays_replay_protected_while_it_is_still_fresh() {
        let cache = ReplayCache::new();
        let proof_id = ProofId::generate();

        let t0 = 1_700_000_000_000i64;
        let issued_at = t0 + PROOF_FRESHNESS_FUTURE_SKEW_MILLIS;
        let vu = valid_until(issued_at);

        let first_acceptance = DateTime::from_timestamp_millis(t0).unwrap();
        assert_eq!(
            cache.check_and_insert(proof_id, first_acceptance, vu),
            Ok(())
        );

        // t0 + past_window + epsilon: past a fixed-from-acceptance retention
        // of `past_window`, but the proof (issued at t0 + skew) is still
        // freshness-valid, so the replay entry must survive.
        let replay_at = DateTime::from_timestamp_millis(t0 + 120_000 + 1).unwrap();
        assert_eq!(
            cache.check_and_insert(proof_id, replay_at, vu),
            Err(ReplayRejection::Replayed),
            "a replay must still fail while the proof can still pass proof_is_fresh"
        );

        // Strictly past the proof's own freshness deadline the entry may
        // finally be evicted — freshness itself now independently rejects it.
        let after_deadline = DateTime::from_timestamp_millis((vu.timestamp_millis()) + 1).unwrap();
        assert_eq!(cache.check_and_insert(proof_id, after_deadline, vu), Ok(()));
    }

    #[test]
    fn moving_the_clock_backwards_only_retains_entries_longer() {
        let cache = ReplayCache::new();
        let proof_id = ProofId::generate();
        let issued_at = 1_700_000_000_000i64;
        let vu = valid_until(issued_at);

        let now = DateTime::from_timestamp_millis(issued_at + 1_000).unwrap();
        assert_eq!(cache.check_and_insert(proof_id, now, vu), Ok(()));

        let earlier = DateTime::from_timestamp_millis(issued_at - 50_000).unwrap();
        assert_eq!(
            cache.check_and_insert(proof_id, earlier, vu),
            Err(ReplayRejection::Replayed)
        );
    }

    #[test]
    fn capacity_saturation_fails_closed_without_evicting_live_entries() {
        let cache = ReplayCache::with_capacity(3);
        let issued_at = 1_700_000_000_000i64;
        let vu = valid_until(issued_at);
        let now = DateTime::from_timestamp_millis(issued_at + 1_000).unwrap();

        let live: Vec<ProofId> = (0..3).map(|_| ProofId::generate()).collect();
        for id in &live {
            assert_eq!(cache.check_and_insert(*id, now, vu), Ok(()));
        }
        assert_eq!(cache.len(), 3);

        // N+1 distinct live proof: fail closed, never evict a live entry.
        assert_eq!(
            cache.check_and_insert(ProofId::generate(), now, vu),
            Err(ReplayRejection::CapacitySaturated)
        );
        assert_eq!(cache.len(), 3);

        // Existing replays still rejected even at saturation.
        assert_eq!(
            cache.check_and_insert(live[0], now, vu),
            Err(ReplayRejection::Replayed)
        );

        // Once the live set can no longer be fresh, capacity frees up again.
        let after = DateTime::from_timestamp_millis(vu.timestamp_millis() + 1).unwrap();
        assert_eq!(
            cache.check_and_insert(
                ProofId::generate(),
                after,
                valid_until(after.timestamp_millis())
            ),
            Ok(())
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn concurrent_duplicate_proof_use_lets_exactly_one_succeed() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(ReplayCache::new());
        let proof_id = ProofId::generate();
        let now = Utc::now();
        let vu = now + Duration::milliseconds(120_000);
        let barrier = Arc::new(std::sync::Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    cache.check_and_insert(proof_id, now, vu).is_ok()
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

    #[test]
    fn concurrent_racers_cannot_exceed_capacity() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(ReplayCache::with_capacity(4));
        let now = Utc::now();
        let vu = now + Duration::milliseconds(120_000);
        let barrier = Arc::new(std::sync::Barrier::new(16));

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    cache.check_and_insert(ProofId::generate(), now, vu).is_ok()
                })
            })
            .collect();

        let successes = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(successes, 4, "never more live entries than the capacity");
        assert_eq!(cache.len(), 4);
    }
}
