//! Measured-only aggregation + within-cycle paired-ratio analysis. PURE.
//!
//! The small statistical helpers ([`median`], [`quantile`], [`mad`]) are
//! re-derived here from the owner-reviewed Phase-A benchmark
//! (`integration/benchmarks/issue-63-data-plane-throughput/src/main.rs`), which
//! is a binary with NO library target and is FROZEN Phase-A evidence that must
//! not be modified. Same definitions, independent copy.
//!
//! Warm-up cases are excluded. Only cases that COMPLETED with `Artifact
//! Verified` contribute — no performance number is valid without verification.
//!
//! `n = 8` per measured size. This module reports raw values, medians, paired
//! ratios and robust spread; it NEVER claims statistical significance.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::matrix::CHUNK_SIZES_MIB;
use crate::result::CaseResult;
use crate::MIB;

/// Median of an f64 sample (even sizes: mean of the two middle values). Input
/// need not be sorted.
pub fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// Linear-interpolated quantile `p in [0, 1]` of a sample. Input need not be
/// sorted.
pub fn quantile(xs: &[f64], p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = p.clamp(0.0, 1.0) * (v.len() as f64 - 1.0);
    let (lo, hi) = (idx.floor() as usize, idx.ceil() as usize);
    v[lo] + (v[hi] - v[lo]) * (idx - lo as f64)
}

/// Median absolute deviation from the median.
pub fn mad(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let m = median(xs.to_vec());
    median(xs.iter().map(|x| (x - m).abs()).collect())
}

/// Which throughput series a paired ratio is computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Series {
    /// Boundary B — end-to-end verified transfer throughput.
    VerifiedTransfer,
    /// Boundary A — bulk stream throughput.
    BulkStream,
}

/// Per-chunk-size summary over the 8 measured cases (verified + bulk-stream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SizeSummary {
    pub chunk_size_mib: u64,
    pub n: usize,
    /// Raw boundary-B walls (ms), in cycle order.
    pub verified_walls_ms: Vec<f64>,
    /// Raw boundary-A walls (ms), in cycle order.
    pub bulk_stream_walls_ms: Vec<f64>,
    pub median_verified_mib_s: f64,
    pub median_bulk_stream_mib_s: f64,
    pub median_verified_mb_s: f64,
    pub min_verified_mib_s: f64,
    pub max_verified_mib_s: f64,
    pub min_bulk_stream_mib_s: f64,
    pub max_bulk_stream_mib_s: f64,
    pub median_seal_d2_ms: f64,
    /// Robust spread of the verified MiB/s sample (MAD).
    pub verified_mib_s_mad: f64,
}

/// One adjacent-size paired comparison (e.g. 8 -> 16 MiB) for one [`Series`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedRatio {
    pub series: Series,
    pub smaller_mib: u64,
    pub bigger_mib: u64,
    /// `throughput(bigger) / throughput(smaller)` per shared cycle (`> 1.0` =>
    /// the bigger chunk was faster that cycle).
    pub raw_ratios: Vec<f64>,
    pub median_ratio: f64,
    /// Cycles (out of `raw_ratios.len()`) where the bigger chunk was faster.
    pub cycles_favouring_bigger: usize,
    /// `[q1, q3]` of the ratio sample.
    pub iqr: (f64, f64),
    pub ratio_mad: f64,
}

/// The complete measured-only analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    pub sizes: Vec<SizeSummary>,
    pub paired: Vec<PairedRatio>,
    /// Cases that were dropped from the analysis because they did not complete
    /// with `Artifact Verified` (should be empty for a clean matrix).
    pub excluded_unverified: Vec<String>,
}

fn mib_s(extent_bytes: u64, wall_ms: f64) -> f64 {
    if wall_ms <= 0.0 {
        return 0.0;
    }
    (extent_bytes as f64 / MIB as f64) / (wall_ms / 1000.0)
}

/// Build the measured-only analysis from every case result.
pub fn analyse(results: &[CaseResult]) -> Analysis {
    let mut excluded_unverified = Vec::new();

    // (chunk_size_mib, cycle) -> case, measured + verified only.
    let mut by_size_cycle: BTreeMap<(u64, u8), &CaseResult> = BTreeMap::new();
    for r in results.iter().filter(|r| r.is_measured()) {
        if !r.is_completed_and_verified() {
            excluded_unverified.push(r.case_id.clone());
            continue;
        }
        if let Some(cycle) = r.cycle {
            by_size_cycle.insert((r.chunk_size_bytes / MIB, cycle), r);
        }
    }

    let mut sizes = Vec::new();
    for &mib in &CHUNK_SIZES_MIB {
        let cases: Vec<&CaseResult> = (1..=8u8)
            .filter_map(|c| by_size_cycle.get(&(mib, c)).copied())
            .collect();
        if cases.is_empty() {
            continue;
        }
        let verified_walls_ms: Vec<f64> =
            cases.iter().map(|c| c.verified_transfer_wall_ms).collect();
        let bulk_stream_walls_ms: Vec<f64> =
            cases.iter().map(|c| c.bulk_stream_wall_ms).collect();
        let verified_mib: Vec<f64> = cases.iter().map(|c| c.verified_transfer_mib_s).collect();
        let bulk_mib: Vec<f64> = cases.iter().map(|c| c.bulk_stream_mib_s).collect();
        let verified_mb: Vec<f64> = cases.iter().map(|c| c.verified_transfer_mb_s).collect();

        sizes.push(SizeSummary {
            chunk_size_mib: mib,
            n: cases.len(),
            verified_walls_ms,
            bulk_stream_walls_ms,
            median_verified_mib_s: median(verified_mib.clone()),
            median_bulk_stream_mib_s: median(bulk_mib.clone()),
            median_verified_mb_s: median(verified_mb),
            min_verified_mib_s: verified_mib.iter().cloned().fold(f64::INFINITY, f64::min),
            max_verified_mib_s: verified_mib.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            min_bulk_stream_mib_s: bulk_mib.iter().cloned().fold(f64::INFINITY, f64::min),
            max_bulk_stream_mib_s: bulk_mib.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            median_seal_d2_ms: median(cases.iter().map(|c| c.seal_d2_ms).collect()),
            verified_mib_s_mad: mad(&verified_mib),
        });
    }

    // Paired ratios for 8->16, 16->32, 32->64, for both series.
    let mut paired = Vec::new();
    for w in CHUNK_SIZES_MIB.windows(2) {
        let (small, big) = (w[0], w[1]);
        for series in [Series::VerifiedTransfer, Series::BulkStream] {
            let mut raw = Vec::new();
            for cycle in 1..=8u8 {
                let (Some(s), Some(b)) = (
                    by_size_cycle.get(&(small, cycle)),
                    by_size_cycle.get(&(big, cycle)),
                ) else {
                    continue;
                };
                let (sw, bw) = match series {
                    Series::VerifiedTransfer => {
                        (s.verified_transfer_wall_ms, b.verified_transfer_wall_ms)
                    }
                    Series::BulkStream => (s.bulk_stream_wall_ms, b.bulk_stream_wall_ms),
                };
                // extent is identical, so throughput(big)/throughput(small)
                // == wall(small)/wall(big).
                let ext = s.extent_bytes;
                let (st, bt) = (mib_s(ext, sw), mib_s(ext, bw));
                if st > 0.0 {
                    raw.push(bt / st);
                }
            }
            if raw.is_empty() {
                continue;
            }
            let favouring = raw.iter().filter(|r| **r > 1.0).count();
            paired.push(PairedRatio {
                series,
                smaller_mib: small,
                bigger_mib: big,
                median_ratio: median(raw.clone()),
                cycles_favouring_bigger: favouring,
                iqr: (quantile(&raw, 0.25), quantile(&raw, 0.75)),
                ratio_mad: mad(&raw),
                raw_ratios: raw,
            });
        }
    }

    Analysis {
        sizes,
        paired,
        excluded_unverified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::{Phase, RunPlan, EXTENT_BYTES};
    use crate::result::{ConnectionCount, CaseResult};

    #[test]
    fn helpers_match_the_phase_a_definitions() {
        // From the frozen benchmark's own unit tests.
        assert_eq!(median(vec![30.0, 10.0, 20.0]), 20.0);
        assert_eq!(median(vec![204.0, 205.0]), 204.5);
        assert_eq!(median(vec![93.0, 93.0, 109.0, 109.0]), 101.0);
        assert!((quantile(&[1.0, 2.0, 3.0, 4.0], 0.5) - 2.5).abs() < 1e-9);
        assert_eq!(mad(&[1.0, 1.0, 1.0]), 0.0);
    }

    /// Build a measured `CaseResult` with a chosen boundary-B and boundary-A
    /// wall (ms); everything else nominal + Verified.
    fn measured(cycle: u8, slot: u8, mib: u64, verified_wall_ms: f64, bulk_wall_ms: f64) -> CaseResult {
        let (vmib, vmb) = CaseResult::rates(EXTENT_BYTES, verified_wall_ms);
        let (bmib, bmb) = CaseResult::rates(EXTENT_BYTES, bulk_wall_ms);
        let n = EXTENT_BYTES / (mib * MIB);
        CaseResult {
            run_id: "r".into(),
            case_id: format!("r/c{cycle}/s{slot}/{mib:02}mib"),
            phase: Phase::Measured,
            cycle: Some(cycle),
            slot: Some(slot),
            chunk_size_bytes: mib * MIB,
            extent_bytes: EXTENT_BYTES,
            chunk_count: n,
            transfer_id: Some("t".into()),
            artifact_id: Some("a".into()),
            source_safety_verdict: "accept".into(),
            clock_skew_verdict: "in_bound".into(),
            bulk_stream_wall_ms: bulk_wall_ms,
            bulk_stream_mib_s: bmib,
            bulk_stream_mb_s: bmb,
            verified_transfer_wall_ms: verified_wall_ms,
            verified_transfer_mib_s: vmib,
            verified_transfer_mb_s: vmb,
            resume_ms: 3.0,
            seal_d2_ms: 5_000.0,
            read_ms: 1.0,
            chunk_sha_ms: 1.0,
            rolling_sha_ms: 1.0,
            proof_ms: 1.0,
            put_ack_ms: 1.0,
            connection_count: ConnectionCount::by_construction(n),
            final_artifact_status: "Verified".into(),
            case_status: "completed".into(),
        }
    }

    /// A full 32-case measured set where bigger chunks are deterministically
    /// faster (smaller wall) by a fixed factor.
    fn full_measured_set() -> Vec<CaseResult> {
        let plan = RunPlan::build("r").unwrap();
        // Keep it consistent with the plan's own ordering.
        plan.measured()
            .map(|c| {
                let mib = c.chunk_size_bytes / MIB;
                // wall shrinks as size grows: 8->80s, 16->70s, 32->64s, 64->60s
                let base = match mib {
                    8 => 80_000.0,
                    16 => 70_000.0,
                    32 => 64_000.0,
                    64 => 60_000.0,
                    _ => unreachable!(),
                };
                measured(
                    c.cycle.unwrap(),
                    c.slot.unwrap(),
                    mib,
                    base + 6_000.0,
                    base,
                )
            })
            .collect()
    }

    #[test]
    fn analysis_summarises_each_measured_size_over_eight_cases() {
        let a = analyse(&full_measured_set());
        assert_eq!(a.sizes.len(), 4);
        assert!(a.excluded_unverified.is_empty());
        for s in &a.sizes {
            assert_eq!(s.n, 8);
            assert_eq!(s.verified_walls_ms.len(), 8);
            assert_eq!(s.bulk_stream_walls_ms.len(), 8);
            assert!(s.median_verified_mib_s > 0.0);
            assert!(s.max_verified_mib_s >= s.median_verified_mib_s);
            assert!(s.median_verified_mib_s >= s.min_verified_mib_s);
        }
        // 8 MiB has the slowest verified throughput, 64 MiB the fastest.
        let get = |mib: u64| a.sizes.iter().find(|s| s.chunk_size_mib == mib).unwrap();
        assert!(get(64).median_verified_mib_s > get(8).median_verified_mib_s);
    }

    #[test]
    fn paired_ratios_cover_both_series_and_three_adjacent_pairs() {
        let a = analyse(&full_measured_set());
        assert_eq!(a.paired.len(), 6, "3 pairs x 2 series");
        for p in &a.paired {
            assert_eq!(p.raw_ratios.len(), 8, "one ratio per shared cycle");
            assert!(p.median_ratio > 1.0, "bigger chunk faster in this fixture");
            assert_eq!(p.cycles_favouring_bigger, 8);
            assert!(p.iqr.0 <= p.iqr.1);
        }
        assert!(a
            .paired
            .iter()
            .any(|p| p.series == Series::VerifiedTransfer && p.smaller_mib == 8 && p.bigger_mib == 16));
        assert!(a
            .paired
            .iter()
            .any(|p| p.series == Series::BulkStream && p.smaller_mib == 32 && p.bigger_mib == 64));
    }

    #[test]
    fn unverified_measured_cases_are_excluded_not_counted() {
        let mut set = full_measured_set();
        set[0].final_artifact_status = "Failed".into();
        set[0].case_status = "failed".into();
        let a = analyse(&set);
        assert_eq!(a.excluded_unverified, vec![set[0].case_id.clone()]);
        // the size that lost a case now has n = 7
        let mib = set[0].chunk_size_bytes / MIB;
        assert_eq!(
            a.sizes.iter().find(|s| s.chunk_size_mib == mib).unwrap().n,
            7
        );
    }

    #[test]
    fn warmups_never_contribute() {
        let mut set = full_measured_set();
        let mut w = set[0].clone();
        w.phase = Phase::Warmup;
        w.cycle = None;
        w.slot = None;
        w.case_id = "r/warmup/08mib".into();
        set.push(w);
        let a = analyse(&set);
        assert_eq!(a.sizes.iter().map(|s| s.n).sum::<usize>(), 32);
    }
}
