//! The exact Issue #63 Stage-3 physical throughput matrix — as PURE AUTHORITY.
//!
//! One deterministic representation. The order is NEVER derived ad hoc in shell.
//!
//! ```text
//! extent      : exactly 2,147,483,648 bytes = 2048 MiB   (every case)
//! chunk sizes : 8 / 16 / 32 / 64 MiB
//! warm-up     : one EXCLUDED transfer per size            (4 cases)
//! measured    : 8 balanced cycles x 4 slots               (32 cases)
//! total       : 36 transfer cases
//!
//! measured cycle order (a 4x4 Latin square repeated twice):
//!   cycle 1 & 5 :  8, 16, 32, 64
//!   cycle 2 & 6 : 64, 32, 16,  8
//!   cycle 3 & 7 : 16, 64,  8, 32
//!   cycle 4 & 8 : 32,  8, 64, 16
//! ```
//!
//! The Latin square is exactly the owner-reviewed balanced set already used by
//! the frozen Phase-A benchmark
//! (`integration/benchmarks/issue-63-data-plane-throughput/src/main.rs`,
//! `balanced_orderings` for `n == 4`): each size occupies each temporal slot
//! exactly once per 4-cycle block, and it also balances which-size-follows-which.
//!
//! EXACT CHUNK ARITHMETIC (asserted in pure logic here AND required at runtime
//! by [`verify_chunk_agreement`]):
//!
//! | chunk | chunks over 2048 MiB |
//! |------:|---------------------:|
//! |  8 MiB| 256 |
//! | 16 MiB| 128 |
//! | 32 MiB|  64 |
//! | 64 MiB|  32 |
//!
//! No partial final chunk for any size.

use serde::{Deserialize, Serialize};

use crate::MIB;

/// The bounded extent of every transfer case: exactly 2048 MiB.
pub const EXTENT_BYTES: u64 = 2048 * MIB; // 2_147_483_648

/// The four chunk sizes, in MiB, in ascending order.
pub const CHUNK_SIZES_MIB: [u64; 4] = [8, 16, 32, 64];

/// The number of measured cycles.
pub const MEASURED_CYCLES: u8 = 8;

/// The number of slots per measured cycle.
pub const SLOTS_PER_CYCLE: u8 = 4;

/// Total transfer cases the plan must produce: 4 warm-ups + 32 measured.
pub const TOTAL_CASES: usize = 36;

/// The 4x4 balanced Latin square (chunk sizes in MiB), one row per cycle
/// position; the plan repeats it for cycles 5..=8.
const LATIN_SQUARE_MIB: [[u64; 4]; 4] = [
    [8, 16, 32, 64],
    [64, 32, 16, 8],
    [16, 64, 8, 32],
    [32, 8, 64, 16],
];

/// Warm-up (excluded) or measured (analysed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Warmup,
    Measured,
}

/// One planned transfer case. Every field is fixed by the plan; nothing is
/// resolved at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    pub run_id: String,
    /// Unique, deterministic, stable across rebuilds for a given `run_id`.
    pub case_id: String,
    pub phase: Phase,
    /// `None` for warm-up; `Some(1..=8)` for measured.
    pub cycle: Option<u8>,
    /// `None` for warm-up; `Some(1..=4)` for measured.
    pub slot: Option<u8>,
    pub chunk_size_bytes: u64,
    pub extent_bytes: u64,
    pub expected_chunk_count: u64,
}

impl Case {
    /// The chunk size expressed in MiB (all four plan sizes are whole MiB).
    pub fn chunk_size_mib(&self) -> u64 {
        self.chunk_size_bytes / MIB
    }
}

/// Why the plan arithmetic is invalid. Any of these is a fail-closed condition:
/// the plan is never built, so no case is ever run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArithmeticError {
    ZeroExtent,
    ZeroChunkSize,
    /// `extent % chunk_size != 0` — a partial final chunk. Forbidden.
    PartialFinalChunk {
        extent_bytes: u64,
        chunk_size_bytes: u64,
        remainder_bytes: u64,
    },
}

/// The exact chunk count for `extent_bytes` at `chunk_size_bytes`, or a
/// fail-closed [`ArithmeticError`] if the division is not exact.
pub fn expected_chunk_count(
    extent_bytes: u64,
    chunk_size_bytes: u64,
) -> Result<u64, ArithmeticError> {
    if chunk_size_bytes == 0 {
        return Err(ArithmeticError::ZeroChunkSize);
    }
    if extent_bytes == 0 {
        return Err(ArithmeticError::ZeroExtent);
    }
    let remainder = extent_bytes % chunk_size_bytes;
    if remainder != 0 {
        return Err(ArithmeticError::PartialFinalChunk {
            extent_bytes,
            chunk_size_bytes,
            remainder_bytes: remainder,
        });
    }
    Ok(extent_bytes / chunk_size_bytes)
}

/// The full deterministic plan: exactly [`TOTAL_CASES`] cases in run order
/// (4 warm-ups ascending, then 8 measured cycles of 4 slots).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    pub run_id: String,
    pub cases: Vec<Case>,
}

impl RunPlan {
    /// Build the canonical 36-case plan for `run_id`. Fails closed if the fixed
    /// extent is not an exact multiple of any of the four chunk sizes (it is —
    /// this is a guard, not an expected path).
    pub fn build(run_id: &str) -> Result<Self, ArithmeticError> {
        let mut cases = Vec::with_capacity(TOTAL_CASES);

        // 4 warm-ups, one per size, ascending. Excluded from analysis.
        for &mib in &CHUNK_SIZES_MIB {
            let chunk_size_bytes = mib * MIB;
            let expected_chunk_count = expected_chunk_count(EXTENT_BYTES, chunk_size_bytes)?;
            cases.push(Case {
                run_id: run_id.to_string(),
                case_id: format!("{run_id}/warmup/{mib:02}mib"),
                phase: Phase::Warmup,
                cycle: None,
                slot: None,
                chunk_size_bytes,
                extent_bytes: EXTENT_BYTES,
                expected_chunk_count,
            });
        }

        // 8 measured cycles; cycles 5..=8 repeat the 4x4 Latin square.
        for cycle in 1..=MEASURED_CYCLES {
            let pattern = LATIN_SQUARE_MIB[((cycle - 1) % 4) as usize];
            for (slot_idx, &mib) in pattern.iter().enumerate() {
                let slot = slot_idx as u8 + 1;
                let chunk_size_bytes = mib * MIB;
                let expected_chunk_count =
                    expected_chunk_count(EXTENT_BYTES, chunk_size_bytes)?;
                cases.push(Case {
                    run_id: run_id.to_string(),
                    case_id: format!("{run_id}/c{cycle}/s{slot}/{mib:02}mib"),
                    phase: Phase::Measured,
                    cycle: Some(cycle),
                    slot: Some(slot),
                    chunk_size_bytes,
                    extent_bytes: EXTENT_BYTES,
                    expected_chunk_count,
                });
            }
        }

        Ok(Self {
            run_id: run_id.to_string(),
            cases,
        })
    }

    pub fn warmups(&self) -> impl Iterator<Item = &Case> {
        self.cases.iter().filter(|c| c.phase == Phase::Warmup)
    }

    pub fn measured(&self) -> impl Iterator<Item = &Case> {
        self.cases.iter().filter(|c| c.phase == Phase::Measured)
    }
}

// ---------------------------------------------------------------------------
// runtime chunk-size agreement gate
// ---------------------------------------------------------------------------

/// Every place a chunk size / extent is independently observed just before a
/// transfer. They MUST all agree, and the resulting count MUST match the plan,
/// or the transfer fails closed before any bulk bytes move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkAgreement {
    /// From the plan [`Case`].
    pub plan_extent_bytes: u64,
    pub plan_chunk_size_bytes: u64,
    pub plan_expected_chunk_count: u64,
    /// The Server `Transfer` row's `chunk_size` (via `create_transfer_context`).
    pub server_transfer_chunk_size: u64,
    /// The `ActionDispatch` / `AuthorizationDecision` chunk size the Agent is
    /// told to use.
    pub action_dispatch_chunk_size: u64,
    /// The chunk size the probe was actually launched with.
    pub probe_chunk_size: u64,
}

/// Why a [`ChunkAgreement`] check failed. Every variant is fail-closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkAgreementError {
    ArithmeticError(ArithmeticError),
    /// One of the observed chunk sizes disagrees with the plan's.
    ChunkSizeMismatch {
        source: &'static str,
        plan: u64,
        observed: u64,
    },
    /// The observed extent disagrees with the plan's.
    ExtentMismatch { plan: u64, observed: u64 },
    /// Extent/chunk-size divide exactly, but the count differs from the plan.
    ChunkCountMismatch { plan: u64, computed: u64 },
}

impl From<ArithmeticError> for ChunkAgreementError {
    fn from(e: ArithmeticError) -> Self {
        ChunkAgreementError::ArithmeticError(e)
    }
}

/// The single runtime gate for the Stage-3 physical matrix (fixed
/// [`EXTENT_BYTES`]). On `Ok` it returns the agreed chunk count, which every
/// downstream component (probe stream, seal `chunk_count`, D2 verify) must use.
/// On `Err` NOTHING is transferred.
pub fn verify_chunk_agreement(a: &ChunkAgreement) -> Result<u64, ChunkAgreementError> {
    verify_chunk_agreement_at_extent(EXTENT_BYTES, a)
}

/// Same gate, parameterised by the expected extent. The Stage-3 matrix uses
/// [`verify_chunk_agreement`] (extent pinned to [`EXTENT_BYTES`]); the Stage-2
/// host synthetic smoke uses a smaller equal extent and calls this directly.
pub fn verify_chunk_agreement_at_extent(
    expected_extent_bytes: u64,
    a: &ChunkAgreement,
) -> Result<u64, ChunkAgreementError> {
    if a.plan_extent_bytes != expected_extent_bytes {
        return Err(ChunkAgreementError::ExtentMismatch {
            plan: expected_extent_bytes,
            observed: a.plan_extent_bytes,
        });
    }
    for (source, observed) in [
        ("server_transfer", a.server_transfer_chunk_size),
        ("action_dispatch", a.action_dispatch_chunk_size),
        ("probe", a.probe_chunk_size),
    ] {
        if observed != a.plan_chunk_size_bytes {
            return Err(ChunkAgreementError::ChunkSizeMismatch {
                source,
                plan: a.plan_chunk_size_bytes,
                observed,
            });
        }
    }
    let computed = expected_chunk_count(a.plan_extent_bytes, a.plan_chunk_size_bytes)?;
    if computed != a.plan_expected_chunk_count {
        return Err(ChunkAgreementError::ChunkCountMismatch {
            plan: a.plan_expected_chunk_count,
            computed,
        });
    }
    Ok(computed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn plan() -> RunPlan {
        RunPlan::build("i63s3-20260907T120000").expect("canonical plan builds")
    }

    #[test]
    fn extent_is_exactly_2048_mib() {
        assert_eq!(EXTENT_BYTES, 2_147_483_648);
    }

    // ---- exact chunk arithmetic --------------------------------------------
    #[test]
    fn exact_chunk_counts_for_the_four_sizes() {
        assert_eq!(expected_chunk_count(EXTENT_BYTES, 8 * MIB).unwrap(), 256);
        assert_eq!(expected_chunk_count(EXTENT_BYTES, 16 * MIB).unwrap(), 128);
        assert_eq!(expected_chunk_count(EXTENT_BYTES, 32 * MIB).unwrap(), 64);
        assert_eq!(expected_chunk_count(EXTENT_BYTES, 64 * MIB).unwrap(), 32);
    }

    #[test]
    fn no_size_leaves_a_partial_final_chunk() {
        for &mib in &CHUNK_SIZES_MIB {
            assert_eq!(EXTENT_BYTES % (mib * MIB), 0, "{mib} MiB divides the extent");
        }
    }

    #[test]
    fn partial_final_chunk_is_fail_closed() {
        match expected_chunk_count(EXTENT_BYTES, 48 * MIB) {
            Err(ArithmeticError::PartialFinalChunk { remainder_bytes, .. }) => {
                assert_ne!(remainder_bytes, 0);
            }
            other => panic!("expected PartialFinalChunk, got {other:?}"),
        }
        assert_eq!(
            expected_chunk_count(0, 8 * MIB),
            Err(ArithmeticError::ZeroExtent)
        );
        assert_eq!(
            expected_chunk_count(EXTENT_BYTES, 0),
            Err(ArithmeticError::ZeroChunkSize)
        );
    }

    // ---- 36-case plan shape ----------------------------------------------
    #[test]
    fn plan_produces_exactly_36_cases_4_warmup_32_measured() {
        let p = plan();
        assert_eq!(p.cases.len(), 36);
        assert_eq!(p.warmups().count(), 4);
        assert_eq!(p.measured().count(), 32);
    }

    #[test]
    fn warmups_are_one_per_size_ascending_no_cycle_no_slot() {
        let p = plan();
        let sizes: Vec<u64> = p.warmups().map(|c| c.chunk_size_mib()).collect();
        assert_eq!(sizes, vec![8, 16, 32, 64]);
        for w in p.warmups() {
            assert_eq!(w.phase, Phase::Warmup);
            assert_eq!(w.cycle, None);
            assert_eq!(w.slot, None);
            assert_eq!(w.extent_bytes, EXTENT_BYTES);
        }
    }

    #[test]
    fn each_measured_size_appears_exactly_eight_times() {
        let p = plan();
        for &mib in &CHUNK_SIZES_MIB {
            let n = p
                .measured()
                .filter(|c| c.chunk_size_mib() == mib)
                .count();
            assert_eq!(n, 8, "{mib} MiB appears 8 times in the measured set");
        }
    }

    #[test]
    fn each_size_occupies_every_slot_exactly_twice() {
        let p = plan();
        for &mib in &CHUNK_SIZES_MIB {
            for slot in 1..=SLOTS_PER_CYCLE {
                let n = p
                    .measured()
                    .filter(|c| c.chunk_size_mib() == mib && c.slot == Some(slot))
                    .count();
                assert_eq!(n, 2, "{mib} MiB in slot {slot} exactly twice");
            }
        }
    }

    #[test]
    fn measured_order_is_the_latin_square_repeated_twice() {
        let p = plan();
        let expect_block = [
            [8u64, 16, 32, 64],
            [64, 32, 16, 8],
            [16, 64, 8, 32],
            [32, 8, 64, 16],
        ];
        for cycle in 1..=MEASURED_CYCLES {
            let row: Vec<u64> = p
                .measured()
                .filter(|c| c.cycle == Some(cycle))
                .map(|c| c.chunk_size_mib())
                .collect();
            assert_eq!(
                row,
                expect_block[((cycle - 1) % 4) as usize].to_vec(),
                "cycle {cycle} order"
            );
        }
    }

    #[test]
    fn case_ids_are_unique_and_deterministic() {
        let p1 = plan();
        let p2 = RunPlan::build("i63s3-20260907T120000").unwrap();
        assert_eq!(p1, p2, "same run_id -> byte-identical plan");
        let ids: BTreeSet<&str> = p1.cases.iter().map(|c| c.case_id.as_str()).collect();
        assert_eq!(ids.len(), 36, "all 36 case_ids unique");
    }

    #[test]
    fn no_duplicated_measured_cycle_slot_pair_and_no_omitted_size() {
        let p = plan();
        let mut seen: BTreeSet<(u8, u8)> = BTreeSet::new();
        for c in p.measured() {
            let key = (c.cycle.unwrap(), c.slot.unwrap());
            assert!(seen.insert(key), "duplicate measured (cycle,slot) {key:?}");
        }
        assert_eq!(seen.len(), 32);
        // every measured cycle contains all four sizes
        for cycle in 1..=MEASURED_CYCLES {
            let sizes: BTreeSet<u64> = p
                .measured()
                .filter(|c| c.cycle == Some(cycle))
                .map(|c| c.chunk_size_mib())
                .collect();
            assert_eq!(
                sizes,
                BTreeSet::from([8, 16, 32, 64]),
                "cycle {cycle} has no omitted size"
            );
        }
    }

    #[test]
    fn every_case_carries_its_exact_expected_chunk_count() {
        let p = plan();
        for c in &p.cases {
            let want = match c.chunk_size_mib() {
                8 => 256,
                16 => 128,
                32 => 64,
                64 => 32,
                other => panic!("unexpected size {other}"),
            };
            assert_eq!(c.expected_chunk_count, want, "{}", c.case_id);
        }
    }

    // ---- runtime chunk-size agreement gate -------------------------------
    fn agreeing(mib: u64) -> ChunkAgreement {
        let cs = mib * MIB;
        ChunkAgreement {
            plan_extent_bytes: EXTENT_BYTES,
            plan_chunk_size_bytes: cs,
            plan_expected_chunk_count: EXTENT_BYTES / cs,
            server_transfer_chunk_size: cs,
            action_dispatch_chunk_size: cs,
            probe_chunk_size: cs,
        }
    }

    #[test]
    fn chunk_agreement_accepts_a_fully_consistent_case() {
        for &mib in &CHUNK_SIZES_MIB {
            assert_eq!(verify_chunk_agreement(&agreeing(mib)).unwrap(), EXTENT_BYTES / (mib * MIB));
        }
    }

    #[test]
    fn chunk_agreement_rejects_any_single_disagreeing_source() {
        let mut a = agreeing(32);
        a.server_transfer_chunk_size = 16 * MIB;
        assert!(matches!(
            verify_chunk_agreement(&a),
            Err(ChunkAgreementError::ChunkSizeMismatch { source: "server_transfer", .. })
        ));

        let mut a = agreeing(32);
        a.action_dispatch_chunk_size = 64 * MIB;
        assert!(matches!(
            verify_chunk_agreement(&a),
            Err(ChunkAgreementError::ChunkSizeMismatch { source: "action_dispatch", .. })
        ));

        let mut a = agreeing(32);
        a.probe_chunk_size = 8 * MIB;
        assert!(matches!(
            verify_chunk_agreement(&a),
            Err(ChunkAgreementError::ChunkSizeMismatch { source: "probe", .. })
        ));
    }

    #[test]
    fn chunk_agreement_rejects_wrong_extent_and_wrong_count() {
        let mut a = agreeing(32);
        a.plan_extent_bytes = EXTENT_BYTES - MIB;
        assert!(matches!(
            verify_chunk_agreement(&a),
            Err(ChunkAgreementError::ExtentMismatch { .. })
        ));

        let mut a = agreeing(32);
        a.plan_expected_chunk_count = 63; // real is 64
        assert!(matches!(
            verify_chunk_agreement(&a),
            Err(ChunkAgreementError::ChunkCountMismatch { plan: 63, computed: 64 })
        ));
    }
}
