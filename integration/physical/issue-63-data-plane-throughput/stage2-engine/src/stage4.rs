//! Issue #63 **Stage 4** — the 64 MiB serial-vs-prep-ahead micro-matrix plan +
//! its paired analysis + the Worker-PUT-decomposition aggregation. PURE.
//!
//! Stage 4 answers exactly two questions:
//!
//! * **Q1** — how much physical `bulk` throughput is recovered when local
//!   preparation of chunk N+1 overlaps the in-flight PUT/ACK of chunk N?
//! * **Q2** — inside the Worker-observed PUT boundary, where is the remaining
//!   time spent?
//!
//! The plan is a fixed 10-transfer micro-matrix at **64 MiB only**, 2048 MiB
//! extent (32 chunks, no partial final chunk):
//!
//! ```text
//! warm-ups (excluded):  S, P
//! measured cycle 1:     S, P
//! measured cycle 2:     P, S
//! measured cycle 3:     S, P
//! measured cycle 4:     P, S
//! ```
//!
//! => 2 warm-ups + 8 measured; each mode appears 4 measured times, and each
//! mode occupies each within-cycle slot exactly twice. `n = 4` per mode: this
//! module reports raw values, medians and paired P/S ratios and NEVER claims
//! statistical significance.
//!
//! Nothing here arms a physical transfer.

use serde::{Deserialize, Serialize};

use crate::analysis::{mad, median, quantile};
use crate::matrix::{expected_chunk_count, ArithmeticError, Phase, EXTENT_BYTES};
use crate::MIB;

/// The single Stage-4 chunk size.
pub const S4_CHUNK_SIZE_BYTES: u64 = 64 * MIB;
/// Measured cycles.
pub const S4_MEASURED_CYCLES: u8 = 4;
/// Transfers per measured cycle (one per mode).
pub const S4_SLOTS_PER_CYCLE: u8 = 2;
/// 2 warm-ups + 8 measured.
pub const S4_TOTAL_CASES: usize = 10;
/// Raw Artifact payload the Stage-4 matrix writes and preserves (10 x 2 GiB).
pub const S4_PAYLOAD_BYTES: u64 = S4_TOTAL_CASES as u64 * EXTENT_BYTES;

// ---- Issue #63 window_8 solution candidate (P vs W micro-plan) --------------

/// The hard-coded bounded PUT window of the `prep_ahead_window_8` candidate.
pub const W8_PUT_WINDOW: u64 = 8;
/// Measured window_8 cycles (each cycle = one P + one W transfer).
pub const W8_MEASURED_CYCLES: u8 = 2;
/// 1 warm-up (W) + 4 measured.
pub const W8_TOTAL_CASES: usize = 5;
/// Raw Artifact payload the window_8 plan writes and preserves (5 x 2 GiB).
pub const W8_PAYLOAD_BYTES: u64 = W8_TOTAL_CASES as u64 * EXTENT_BYTES;

/// Which single-pass streaming algorithm a case runs. The serde representation
/// is EXACTLY the `--mode` token the probe accepts (`wire()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum S4Mode {
    /// The already-proven Stage-3 serial path (`run_stream_pass`).
    #[serde(rename = "serial")]
    Serial,
    /// The throwaway depth-2 prep-ahead pipeline (`run_stream_pass_prep_ahead`).
    #[serde(rename = "prep_ahead_2")]
    PrepAhead2,
    /// The throwaway window_8 candidate: prep-ahead source pipeline + up to 8
    /// concurrent chunk PUTs, per-PUT Worker durability semantics UNCHANGED
    /// (`run_stream_pass_window8`).
    #[serde(rename = "prep_ahead_window_8")]
    PrepAheadWindow8,
    #[serde(rename = "prep_ahead_window_8_batch_8")]
    PrepAheadWindow8Batch8,
}

impl S4Mode {
    /// The exact `--mode` token the probe accepts.
    pub fn wire(self) -> &'static str {
        match self {
            S4Mode::Serial => "serial",
            S4Mode::PrepAhead2 => "prep_ahead_2",
            S4Mode::PrepAheadWindow8 => "prep_ahead_window_8",
            S4Mode::PrepAheadWindow8Batch8 => "prep_ahead_window_8_batch_8",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "serial" => Some(S4Mode::Serial),
            "prep_ahead_2" | "prep-ahead-2" => Some(S4Mode::PrepAhead2),
            "prep_ahead_window_8" | "prep-ahead-window-8" | "window_8" => {
                Some(S4Mode::PrepAheadWindow8)
            }
            "prep_ahead_window_8_batch_8" | "batch_8" => Some(S4Mode::PrepAheadWindow8Batch8),
            _ => None,
        }
    }
}

/// One planned Stage-4 transfer case. Every field is fixed by the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct S4Case {
    pub run_id: String,
    pub case_id: String,
    pub phase: Phase,
    pub mode: S4Mode,
    /// `None` for warm-up; `Some(1..=4)` for measured.
    pub cycle: Option<u8>,
    /// `None` for warm-up; `Some(1..=2)` for measured.
    pub slot: Option<u8>,
    pub chunk_size_bytes: u64,
    pub extent_bytes: u64,
    pub expected_chunk_count: u64,
}

/// The full deterministic Stage-4 plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S4Plan {
    pub run_id: String,
    pub cases: Vec<S4Case>,
}

impl S4Plan {
    /// Build the canonical 10-case plan for `run_id`. Fails closed only if
    /// 2048 MiB is not an exact multiple of 64 MiB (it is — a guard).
    pub fn build(run_id: &str) -> Result<Self, ArithmeticError> {
        let expected_chunk_count = expected_chunk_count(EXTENT_BYTES, S4_CHUNK_SIZE_BYTES)?;
        let mut cases = Vec::with_capacity(S4_TOTAL_CASES);

        let mk = |case_id: String, phase, mode, cycle, slot| S4Case {
            run_id: run_id.to_string(),
            case_id,
            phase,
            mode,
            cycle,
            slot,
            chunk_size_bytes: S4_CHUNK_SIZE_BYTES,
            extent_bytes: EXTENT_BYTES,
            expected_chunk_count,
        };

        // 2 warm-ups: S then P, excluded from analysis.
        for mode in [S4Mode::Serial, S4Mode::PrepAhead2] {
            cases.push(mk(
                format!("{run_id}/warmup/{}", mode.wire()),
                Phase::Warmup,
                mode,
                None,
                None,
            ));
        }

        // 4 measured cycles, alternating slot order: (S,P),(P,S),(S,P),(P,S).
        for cycle in 1..=S4_MEASURED_CYCLES {
            let order = if cycle % 2 == 1 {
                [S4Mode::Serial, S4Mode::PrepAhead2]
            } else {
                [S4Mode::PrepAhead2, S4Mode::Serial]
            };
            for (slot_idx, &mode) in order.iter().enumerate() {
                let slot = slot_idx as u8 + 1;
                cases.push(mk(
                    format!("{run_id}/c{cycle}/s{slot}/{}", mode.wire()),
                    Phase::Measured,
                    mode,
                    Some(cycle),
                    Some(slot),
                ));
            }
        }

        Ok(Self {
            run_id: run_id.to_string(),
            cases,
        })
    }

    /// Build the canonical window_8 candidate plan for `run_id`:
    ///
    /// ```text
    /// warm-up (excluded):  W
    /// measured cycle 1:    P, W
    /// measured cycle 2:    W, P
    /// ```
    ///
    /// => 1 warm-up + 4 measured; `n = 2` per measured mode. P = the Stage-4
    /// `prep_ahead_2` reference (one PUT in flight); W = `prep_ahead_window_8`.
    /// Deliberately SMALL — a solution candidate, not another matrix.
    pub fn build_window8(run_id: &str) -> Result<Self, ArithmeticError> {
        let expected_chunk_count = expected_chunk_count(EXTENT_BYTES, S4_CHUNK_SIZE_BYTES)?;
        let mk = |case_id: String, phase, mode, cycle, slot| S4Case {
            run_id: run_id.to_string(),
            case_id,
            phase,
            mode,
            cycle,
            slot,
            chunk_size_bytes: S4_CHUNK_SIZE_BYTES,
            extent_bytes: EXTENT_BYTES,
            expected_chunk_count,
        };
        let mut cases = Vec::with_capacity(W8_TOTAL_CASES);
        cases.push(mk(
            format!("{run_id}/warmup/{}", S4Mode::PrepAheadWindow8.wire()),
            Phase::Warmup,
            S4Mode::PrepAheadWindow8,
            None,
            None,
        ));
        for cycle in 1..=W8_MEASURED_CYCLES {
            let order = if cycle % 2 == 1 {
                [S4Mode::PrepAhead2, S4Mode::PrepAheadWindow8]
            } else {
                [S4Mode::PrepAheadWindow8, S4Mode::PrepAhead2]
            };
            for (slot_idx, &mode) in order.iter().enumerate() {
                let slot = slot_idx as u8 + 1;
                cases.push(mk(
                    format!("{run_id}/c{cycle}/s{slot}/{}", mode.wire()),
                    Phase::Measured,
                    mode,
                    Some(cycle),
                    Some(slot),
                ));
            }
        }
        Ok(Self {
            run_id: run_id.to_string(),
            cases,
        })
    }

    pub fn build_batch8(run_id: &str) -> Result<Self, ArithmeticError> {
        let expected_chunk_count = expected_chunk_count(EXTENT_BYTES, S4_CHUNK_SIZE_BYTES)?;
        Ok(Self { run_id: run_id.into(), cases: (0..4).map(|i| S4Case {
            run_id: run_id.into(), case_id: format!("{run_id}/B{i}"),
            phase: if i == 0 { Phase::Warmup } else { Phase::Measured },
            mode: S4Mode::PrepAheadWindow8Batch8,
            cycle: if i == 0 { None } else { Some(i) }, slot: if i == 0 { None } else { Some(1) },
            chunk_size_bytes: S4_CHUNK_SIZE_BYTES, extent_bytes: EXTENT_BYTES, expected_chunk_count,
        }).collect() })
    }

    pub fn warmups(&self) -> impl Iterator<Item = &S4Case> {
        self.cases.iter().filter(|c| c.phase == Phase::Warmup)
    }
    pub fn measured(&self) -> impl Iterator<Item = &S4Case> {
        self.cases.iter().filter(|c| c.phase == Phase::Measured)
    }
}

// ---------------------------------------------------------------------------
// per-case result (self-contained; NOT the Stage-3 `CaseResult`)
// ---------------------------------------------------------------------------

/// One Stage-4 case result. The probe emits every field on its
/// `probe.case_result` line; the Stage-4 runner/coordinator parse it here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct S4CaseResult {
    pub run_id: String,
    pub case_id: String,
    pub mode: S4Mode,
    pub phase: Phase,
    pub cycle: Option<u8>,
    pub slot: Option<u8>,
    pub chunk_size_bytes: u64,
    pub extent_bytes: u64,
    pub chunk_count: u64,
    pub transfer_id: Option<String>,
    pub artifact_id: Option<String>,

    pub bulk_stream_wall_ms: f64,
    pub verified_transfer_wall_ms: f64,
    pub resume_ms: f64,
    pub seal_d2_ms: f64,
    pub read_ms: f64,
    pub chunk_sha_ms: f64,
    pub rolling_sha_ms: f64,
    pub proof_ms: f64,
    /// The AGGREGATE sum of individual PUT request waits. In prep-ahead this
    /// OVERLAPS local preparation and MUST NOT be compared to the wall as if it
    /// were exclusive of prep.
    pub put_ack_ms: f64,
    /// Proof the live payload-buffer bound held: `0` (serial), `2` (prep-ahead
    /// depth 2), or `<= 9` (window_8: <= 8 unacknowledged PUT payloads + <= 1
    /// producer/current chunk).
    pub prepared_buffer_peak: u64,
    pub device_read_count: u64,

    // ---- window_8 candidate fields (serde-defaulted so Stage-3/4 result
    // lines, which predate them, still parse; 0/false for non-window modes) ----
    /// The bounded PUT window (8 for `prep_ahead_window_8`; 0 otherwise).
    #[serde(default)]
    pub put_window: u64,
    #[serde(default)]
    pub put_started_count: u64,
    #[serde(default)]
    pub put_completed_count: u64,
    /// The maximum simultaneously-unacknowledged PUT count the window manager
    /// observed (an upper bound on true network concurrency; MUST be `<= 8`).
    #[serde(default)]
    pub peak_puts_in_flight: u64,
    /// `true` iff the probe verified every PUT was STARTED in strictly
    /// ascending chunk-index order.
    #[serde(default)]
    pub put_starts_ascending: bool,

    pub final_artifact_status: String,
    /// `completed` | `failed:*` | `contaminated`.
    pub case_status: String,
}

impl S4CaseResult {
    pub fn is_measured(&self) -> bool {
        self.phase == Phase::Measured
    }
    pub fn is_completed_and_verified(&self) -> bool {
        self.case_status == "completed" && self.final_artifact_status == "Verified"
    }
    /// `bulk` throughput, MiB/s.
    pub fn bulk_mib_s(&self) -> f64 {
        rate(self.extent_bytes, self.bulk_stream_wall_ms)
    }
    /// End-to-end verified throughput, MiB/s.
    pub fn verified_mib_s(&self) -> f64 {
        rate(self.extent_bytes, self.verified_transfer_wall_ms)
    }
    /// `bulk` throughput in DECIMAL MB/s (the product-target unit).
    pub fn bulk_mb_s(&self) -> f64 {
        if self.bulk_stream_wall_ms <= 0.0 {
            return 0.0;
        }
        (self.extent_bytes as f64 / 1_000_000.0) / (self.bulk_stream_wall_ms / 1000.0)
    }
    /// The serial-style component sum (prep + PUT/ACK) — a DERIVED diagnostic,
    /// not an authoritative protocol metric. In prep-ahead the real wall is
    /// smaller than this because prep now overlaps PUT/ACK.
    pub fn serial_style_component_sum_ms(&self) -> f64 {
        self.read_ms + self.chunk_sha_ms + self.rolling_sha_ms + self.proof_ms + self.put_ack_ms
    }
    /// `serial_style_component_sum - actual bulk wall` — labelled derived
    /// diagnostic (how much wall the overlap "saved" vs a fully serial view).
    pub fn overlap_saved_ms(&self) -> f64 {
        self.serial_style_component_sum_ms() - self.bulk_stream_wall_ms
    }
}

fn rate(extent_bytes: u64, wall_ms: f64) -> f64 {
    if wall_ms <= 0.0 {
        return 0.0;
    }
    (extent_bytes as f64 / MIB as f64) / (wall_ms / 1000.0)
}

// ---------------------------------------------------------------------------
// S vs P analysis (measured, verified-only)
// ---------------------------------------------------------------------------

/// Per-mode summary over the 4 measured cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct S4ModeSummary {
    pub mode: S4Mode,
    pub n: usize,
    /// Raw boundary-A walls (ms) in cycle order.
    pub bulk_walls_ms: Vec<f64>,
    /// Raw boundary-B walls (ms) in cycle order.
    pub verified_walls_ms: Vec<f64>,
    pub bulk_mib_s: Vec<f64>,
    pub verified_mib_s: Vec<f64>,
    pub median_bulk_mib_s: f64,
    pub median_verified_mib_s: f64,
    pub min_bulk_mib_s: f64,
    pub max_bulk_mib_s: f64,
    pub median_seal_d2_ms: f64,
    pub median_overlap_saved_ms: f64,
    pub bulk_mib_s_mad: f64,
}

/// Which throughput series a paired ratio is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum S4Series {
    BulkStream,
    VerifiedTransfer,
}

/// The within-cycle paired P/S ratios for one series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct S4PairedRatio {
    pub series: S4Series,
    /// `throughput(prep_ahead) / throughput(serial)` per shared measured cycle
    /// (`> 1.0` => prep-ahead was faster that cycle).
    pub raw_ratios: Vec<f64>,
    pub median_ratio: f64,
    pub cycles_favouring_prep_ahead: usize,
    pub iqr: (f64, f64),
    pub ratio_mad: f64,
}

/// The complete measured-only S-vs-P analysis. `n = 4`; NO significance claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct S4Analysis {
    pub serial: Option<S4ModeSummary>,
    pub prep_ahead: Option<S4ModeSummary>,
    pub paired: Vec<S4PairedRatio>,
    /// Measured cases dropped because they did not complete `Verified`.
    pub excluded_unverified: Vec<String>,
    pub significance_claim: &'static str,
}

fn mode_summary(mode: S4Mode, by_cycle: &[(u8, &S4CaseResult)]) -> Option<S4ModeSummary> {
    let mut cases: Vec<&S4CaseResult> = by_cycle
        .iter()
        .filter(|(_, r)| r.mode == mode)
        .map(|(_, r)| *r)
        .collect();
    if cases.is_empty() {
        return None;
    }
    cases.sort_by_key(|r| r.cycle.unwrap_or(0));
    let bulk_walls_ms: Vec<f64> = cases.iter().map(|c| c.bulk_stream_wall_ms).collect();
    let verified_walls_ms: Vec<f64> = cases.iter().map(|c| c.verified_transfer_wall_ms).collect();
    let bulk_mib_s: Vec<f64> = cases.iter().map(|c| c.bulk_mib_s()).collect();
    let verified_mib_s: Vec<f64> = cases.iter().map(|c| c.verified_mib_s()).collect();
    Some(S4ModeSummary {
        mode,
        n: cases.len(),
        median_bulk_mib_s: median(bulk_mib_s.clone()),
        median_verified_mib_s: median(verified_mib_s.clone()),
        min_bulk_mib_s: bulk_mib_s.iter().cloned().fold(f64::INFINITY, f64::min),
        max_bulk_mib_s: bulk_mib_s.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        median_seal_d2_ms: median(cases.iter().map(|c| c.seal_d2_ms).collect()),
        median_overlap_saved_ms: median(cases.iter().map(|c| c.overlap_saved_ms()).collect()),
        bulk_mib_s_mad: mad(&bulk_mib_s),
        bulk_walls_ms,
        verified_walls_ms,
        bulk_mib_s,
        verified_mib_s,
    })
}

/// Build the measured-only S-vs-P analysis from every Stage-4 case result.
pub fn analyse_s4(results: &[S4CaseResult]) -> S4Analysis {
    let mut excluded_unverified = Vec::new();
    let mut by_cycle: Vec<(u8, &S4CaseResult)> = Vec::new();
    for r in results.iter().filter(|r| r.is_measured()) {
        if !r.is_completed_and_verified() {
            excluded_unverified.push(r.case_id.clone());
            continue;
        }
        if let Some(cycle) = r.cycle {
            by_cycle.push((cycle, r));
        }
    }

    let serial = mode_summary(S4Mode::Serial, &by_cycle);
    let prep_ahead = mode_summary(S4Mode::PrepAhead2, &by_cycle);

    let mut paired = Vec::new();
    for series in [S4Series::BulkStream, S4Series::VerifiedTransfer] {
        let mut raw = Vec::new();
        for cycle in 1..=S4_MEASURED_CYCLES {
            let s = by_cycle
                .iter()
                .find(|(c, r)| *c == cycle && r.mode == S4Mode::Serial)
                .map(|(_, r)| *r);
            let p = by_cycle
                .iter()
                .find(|(c, r)| *c == cycle && r.mode == S4Mode::PrepAhead2)
                .map(|(_, r)| *r);
            let (Some(s), Some(p)) = (s, p) else { continue };
            let (st, pt) = match series {
                S4Series::BulkStream => (s.bulk_mib_s(), p.bulk_mib_s()),
                S4Series::VerifiedTransfer => (s.verified_mib_s(), p.verified_mib_s()),
            };
            if st > 0.0 {
                raw.push(pt / st);
            }
        }
        if raw.is_empty() {
            continue;
        }
        let favouring = raw.iter().filter(|r| **r > 1.0).count();
        paired.push(S4PairedRatio {
            series,
            median_ratio: median(raw.clone()),
            cycles_favouring_prep_ahead: favouring,
            iqr: (quantile(&raw, 0.25), quantile(&raw, 0.75)),
            ratio_mad: mad(&raw),
            raw_ratios: raw,
        });
    }

    S4Analysis {
        serial,
        prep_ahead,
        paired,
        excluded_unverified,
        significance_claim: "none (n=4 per mode; paired ratios only)",
    }
}

// ---------------------------------------------------------------------------
// window_8 P-vs-W analysis (measured, verified-only)
// ---------------------------------------------------------------------------

/// The within-cycle paired W/P ratios for one series (`> 1.0` => window_8 was
/// faster that cycle).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct W8PairedRatio {
    pub series: S4Series,
    pub raw_ratios: Vec<f64>,
    pub median_ratio: f64,
    pub cycles_favouring_window8: usize,
}

/// The complete measured-only P-vs-W analysis. `n = 2` per mode; NO
/// significance claim — this classifies a single solution candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct W8Analysis {
    pub prep_ahead: Option<S4ModeSummary>,
    pub window8: Option<S4ModeSummary>,
    /// Paired W/P ratios per shared cycle, both series.
    pub paired: Vec<W8PairedRatio>,
    pub put_window: u64,
    /// Median window_8 bulk throughput in DECIMAL MB/s (product-target unit).
    pub window8_median_bulk_mb_s: f64,
    /// Median window_8 bulk wall for the 2 GiB extent, ms.
    pub window8_median_bulk_wall_ms: f64,
    /// Max `peak_puts_in_flight` over the measured verified window_8 cases.
    pub max_peak_puts_in_flight: u64,
    /// `true` iff every measured verified window_8 case reported ascending PUT
    /// starts AND `peak_puts_in_flight` in `2..=8` AND `prepared_buffer_peak <= 9`.
    pub window_invariants_held: bool,
    pub excluded_unverified: Vec<String>,
    pub significance_claim: &'static str,
}

/// Build the measured-only P-vs-W analysis from every window_8-plan case result.
pub fn analyse_w8(results: &[S4CaseResult]) -> W8Analysis {
    let mut excluded_unverified = Vec::new();
    let mut by_cycle: Vec<(u8, &S4CaseResult)> = Vec::new();
    for r in results.iter().filter(|r| r.is_measured()) {
        if !r.is_completed_and_verified() {
            excluded_unverified.push(r.case_id.clone());
            continue;
        }
        if let Some(cycle) = r.cycle {
            by_cycle.push((cycle, r));
        }
    }

    let prep_ahead = mode_summary(S4Mode::PrepAhead2, &by_cycle);
    let window8 = mode_summary(S4Mode::PrepAheadWindow8, &by_cycle);

    let mut paired = Vec::new();
    for series in [S4Series::BulkStream, S4Series::VerifiedTransfer] {
        let mut raw = Vec::new();
        for cycle in 1..=W8_MEASURED_CYCLES {
            let p = by_cycle
                .iter()
                .find(|(c, r)| *c == cycle && r.mode == S4Mode::PrepAhead2)
                .map(|(_, r)| *r);
            let w = by_cycle
                .iter()
                .find(|(c, r)| *c == cycle && r.mode == S4Mode::PrepAheadWindow8)
                .map(|(_, r)| *r);
            let (Some(p), Some(w)) = (p, w) else { continue };
            let (pt, wt) = match series {
                S4Series::BulkStream => (p.bulk_mib_s(), w.bulk_mib_s()),
                S4Series::VerifiedTransfer => (p.verified_mib_s(), w.verified_mib_s()),
            };
            if pt > 0.0 {
                raw.push(wt / pt);
            }
        }
        if raw.is_empty() {
            continue;
        }
        paired.push(W8PairedRatio {
            series,
            median_ratio: median(raw.clone()),
            cycles_favouring_window8: raw.iter().filter(|r| **r > 1.0).count(),
            raw_ratios: raw,
        });
    }

    let w_cases: Vec<&S4CaseResult> = by_cycle
        .iter()
        .filter(|(_, r)| r.mode == S4Mode::PrepAheadWindow8)
        .map(|(_, r)| *r)
        .collect();
    let window_invariants_held = !w_cases.is_empty()
        && w_cases.iter().all(|r| {
            r.put_starts_ascending
                && r.put_window == W8_PUT_WINDOW
                && (2..=W8_PUT_WINDOW).contains(&r.peak_puts_in_flight)
                && r.prepared_buffer_peak <= W8_PUT_WINDOW + 1
                && r.put_started_count == r.chunk_count
                && r.put_completed_count == r.chunk_count
        });

    W8Analysis {
        prep_ahead,
        window8,
        paired,
        put_window: W8_PUT_WINDOW,
        window8_median_bulk_mb_s: median(w_cases.iter().map(|r| r.bulk_mb_s()).collect()),
        window8_median_bulk_wall_ms: median(
            w_cases.iter().map(|r| r.bulk_stream_wall_ms).collect(),
        ),
        max_peak_puts_in_flight: w_cases.iter().map(|r| r.peak_puts_in_flight).max().unwrap_or(0),
        window_invariants_held,
        excluded_unverified,
        significance_claim: "none (n=2 per mode; paired ratios only; solution-candidate check)",
    }
}

// ---------------------------------------------------------------------------
// Worker PUT decomposition (Q2)
// ---------------------------------------------------------------------------

/// One Worker per-PUT timing record, as emitted (env-gated) by
/// `crates/worker/src/data_plane/i63_timing.rs`. Nanoseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerPutRecord {
    pub ts_ms: u64,
    pub transfer_id: String,
    pub chunk_index: u64,
    pub outcome: String,
    pub verified_size: i64,
    pub handler_total_ns: u64,
    pub authorize_ns: u64,
    pub stage_call_ns: u64,
    pub commit_chunk_ns: u64,
    pub overlapping: WorkerOverlapping,
    pub begin_stage_ns: u64,
    pub write_sum_ns: u64,
    pub digest_ns: u64,
    pub finalize_ns: u64,
}

/// The intervals that OVERLAP BY DESIGN — never summed as exclusive phases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerOverlapping {
    /// The async request-body pump loop wall.
    pub body_pump_ns: u64,
    /// The `spawn_blocking` staging-worker wall (envelops begin_stage + write +
    /// digest + finalize; concurrent with `body_pump_ns`).
    pub staging_worker_ns: u64,
}

/// Robust stats for one Worker sub-interval (ms).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalStats {
    pub name: &'static str,
    pub n: usize,
    pub median_ms: f64,
    pub p10_ms: f64,
    pub p90_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    /// `true` for `body_pump_ms` / `staging_worker_ms` — do NOT sum with the
    /// exclusive intervals.
    pub overlapping: bool,
}

/// Worker decomposition for one population of PUT records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerDecomp {
    pub n_records: usize,
    pub intervals: Vec<IntervalStats>,
    pub note: &'static str,
}

fn interval_stats(name: &'static str, overlapping: bool, samples_ns: &[u64]) -> IntervalStats {
    let ms: Vec<f64> = samples_ns.iter().map(|n| *n as f64 / 1_000_000.0).collect();
    IntervalStats {
        name,
        n: ms.len(),
        median_ms: median(ms.clone()),
        p10_ms: quantile(&ms, 0.10),
        p90_ms: quantile(&ms, 0.90),
        min_ms: ms.iter().cloned().fold(f64::INFINITY, f64::min),
        max_ms: ms.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        overlapping,
    }
}

/// Aggregate a population of Worker PUT records into per-interval medians/ranges.
/// Overlapping intervals stay explicitly identified.
pub fn analyse_worker_decomp(records: &[WorkerPutRecord]) -> WorkerDecomp {
    let pick = |f: fn(&WorkerPutRecord) -> u64| records.iter().map(f).collect::<Vec<_>>();
    let intervals = vec![
        interval_stats("handler_total_ms", false, &pick(|r| r.handler_total_ns)),
        interval_stats("authorize_ms", false, &pick(|r| r.authorize_ns)),
        interval_stats("stage_call_ms", false, &pick(|r| r.stage_call_ns)),
        interval_stats("body_pump_ms", true, &pick(|r| r.overlapping.body_pump_ns)),
        interval_stats(
            "staging_worker_ms",
            true,
            &pick(|r| r.overlapping.staging_worker_ns),
        ),
        interval_stats("begin_stage_ms", false, &pick(|r| r.begin_stage_ns)),
        interval_stats("write_sum_ms", false, &pick(|r| r.write_sum_ns)),
        interval_stats("digest_ms", false, &pick(|r| r.digest_ns)),
        interval_stats("finalize_ms", false, &pick(|r| r.finalize_ns)),
        interval_stats("commit_chunk_ms", false, &pick(|r| r.commit_chunk_ns)),
    ];
    WorkerDecomp {
        n_records: records.len(),
        intervals,
        note: "body_pump_ms & staging_worker_ms OVERLAP; begin_stage/write_sum/digest/finalize are \
               sequential *within* staging_worker_ms; write_sum folds in the incremental Worker SHA",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> S4Plan {
        S4Plan::build("i63s4-20260907T200000").unwrap()
    }

    #[test]
    fn batch8_plan_has_only_four_fixed_candidate_cases() {
        let p = S4Plan::build_batch8("i63b8-test").unwrap();
        assert_eq!(p.cases.len(), 4);
        assert_eq!(p.warmups().count(), 1);
        for (i,c) in p.cases.iter().enumerate() {
            assert_eq!(c.case_id, format!("i63b8-test/B{i}"));
            assert_eq!(c.mode.wire(), "prep_ahead_window_8_batch_8");
            assert_eq!((c.chunk_size_bytes,c.extent_bytes,c.expected_chunk_count), (64*MIB,EXTENT_BYTES,32));
        }
    }

    #[test]
    fn plan_is_10_cases_2_warmup_8_measured_all_64_mib_32_chunks() {
        let p = plan();
        assert_eq!(p.cases.len(), 10);
        assert_eq!(p.warmups().count(), 2);
        assert_eq!(p.measured().count(), 8);
        for c in &p.cases {
            assert_eq!(c.chunk_size_bytes, 64 * MIB);
            assert_eq!(c.extent_bytes, EXTENT_BYTES);
            assert_eq!(c.expected_chunk_count, 32);
        }
    }

    #[test]
    fn warmups_are_s_then_p_no_cycle_no_slot() {
        let p = plan();
        let w: Vec<S4Mode> = p.warmups().map(|c| c.mode).collect();
        assert_eq!(w, vec![S4Mode::Serial, S4Mode::PrepAhead2]);
        for c in p.warmups() {
            assert_eq!(c.cycle, None);
            assert_eq!(c.slot, None);
        }
    }

    #[test]
    fn measured_cycle_order_is_sp_ps_sp_ps() {
        let p = plan();
        let rows: Vec<Vec<S4Mode>> = (1..=4)
            .map(|cy| {
                p.measured()
                    .filter(|c| c.cycle == Some(cy))
                    .map(|c| c.mode)
                    .collect()
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                vec![S4Mode::Serial, S4Mode::PrepAhead2],
                vec![S4Mode::PrepAhead2, S4Mode::Serial],
                vec![S4Mode::Serial, S4Mode::PrepAhead2],
                vec![S4Mode::PrepAhead2, S4Mode::Serial],
            ]
        );
    }

    #[test]
    fn each_mode_appears_4_measured_times_and_twice_per_slot() {
        let p = plan();
        for mode in [S4Mode::Serial, S4Mode::PrepAhead2] {
            assert_eq!(p.measured().filter(|c| c.mode == mode).count(), 4);
            for slot in 1..=2 {
                assert_eq!(
                    p.measured()
                        .filter(|c| c.mode == mode && c.slot == Some(slot))
                        .count(),
                    2
                );
            }
        }
    }

    #[test]
    fn case_ids_unique_and_deterministic() {
        let p1 = plan();
        let p2 = S4Plan::build("i63s4-20260907T200000").unwrap();
        assert_eq!(p1, p2);
        let ids: std::collections::BTreeSet<&str> =
            p1.cases.iter().map(|c| c.case_id.as_str()).collect();
        assert_eq!(ids.len(), 10);
    }

    #[test]
    fn s4mode_wire_round_trips_and_matches_probe_tokens() {
        assert_eq!(S4Mode::Serial.wire(), "serial");
        assert_eq!(S4Mode::PrepAhead2.wire(), "prep_ahead_2");
        assert_eq!(S4Mode::PrepAheadWindow8.wire(), "prep_ahead_window_8");
        assert_eq!(S4Mode::parse("serial"), Some(S4Mode::Serial));
        assert_eq!(S4Mode::parse("prep_ahead_2"), Some(S4Mode::PrepAhead2));
        assert_eq!(S4Mode::parse("prep-ahead-2"), Some(S4Mode::PrepAhead2));
        assert_eq!(
            S4Mode::parse("prep_ahead_window_8"),
            Some(S4Mode::PrepAheadWindow8)
        );
        assert_eq!(S4Mode::parse("window_8"), Some(S4Mode::PrepAheadWindow8));
        assert_eq!(S4Mode::parse("nonsense"), None);
    }

    // ---- window_8 candidate plan + analysis --------------------------------

    #[test]
    fn window8_plan_is_5_cases_w_warmup_then_pw_wp() {
        let p = S4Plan::build_window8("i63w8-t").unwrap();
        assert_eq!(p.cases.len(), W8_TOTAL_CASES);
        assert_eq!(p.warmups().count(), 1);
        assert_eq!(p.measured().count(), 4);
        let w: Vec<S4Mode> = p.warmups().map(|c| c.mode).collect();
        assert_eq!(w, vec![S4Mode::PrepAheadWindow8]);
        let rows: Vec<Vec<S4Mode>> = (1..=2)
            .map(|cy| {
                p.measured()
                    .filter(|c| c.cycle == Some(cy))
                    .map(|c| c.mode)
                    .collect()
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                vec![S4Mode::PrepAhead2, S4Mode::PrepAheadWindow8],
                vec![S4Mode::PrepAheadWindow8, S4Mode::PrepAhead2],
            ]
        );
        for c in &p.cases {
            assert_eq!(c.chunk_size_bytes, 64 * MIB);
            assert_eq!(c.expected_chunk_count, 32);
        }
        let ids: std::collections::BTreeSet<&str> =
            p.cases.iter().map(|c| c.case_id.as_str()).collect();
        assert_eq!(ids.len(), 5);
    }

    fn w8_full_set() -> Vec<S4CaseResult> {
        let mut v = Vec::new();
        let mut w = result(0, 0, S4Mode::PrepAheadWindow8, 22_000.0, 28_000.0);
        w.phase = Phase::Warmup;
        w.cycle = None;
        w.slot = None;
        w.case_id = "r/warmup/prep_ahead_window_8".into();
        v.push(w);
        for cycle in 1..=2u8 {
            v.push(result(cycle, 1, S4Mode::PrepAhead2, 66_000.0, 72_000.0));
            v.push(result(cycle, 2, S4Mode::PrepAheadWindow8, 21_000.0, 27_000.0));
        }
        v
    }

    #[test]
    fn w8_analysis_pairs_w_over_p_and_reports_target_units() {
        let a = analyse_w8(&w8_full_set());
        assert!(a.excluded_unverified.is_empty());
        let p = a.prep_ahead.unwrap();
        let w = a.window8.unwrap();
        assert_eq!((p.n, w.n), (2, 2));
        assert!(w.median_bulk_mib_s > p.median_bulk_mib_s);
        assert_eq!(a.paired.len(), 2);
        for pr in &a.paired {
            assert_eq!(pr.raw_ratios.len(), 2);
            assert!(pr.median_ratio > 2.0);
            assert_eq!(pr.cycles_favouring_window8, 2);
        }
        // 2 GiB / 21 s ~= 102.26 MB/s decimal.
        assert!((a.window8_median_bulk_mb_s - 2_147_483_648.0 / 1e6 / 21.0).abs() < 0.5);
        assert_eq!(a.window8_median_bulk_wall_ms, 21_000.0);
        assert_eq!(a.put_window, 8);
        assert_eq!(a.max_peak_puts_in_flight, 8);
        assert!(a.window_invariants_held);
    }

    #[test]
    fn w8_analysis_flags_broken_window_invariants_and_unverified_cases() {
        let mut set = w8_full_set();
        // a window case that never reached >1 in flight breaks the invariant
        set[2].peak_puts_in_flight = 1;
        let a = analyse_w8(&set);
        assert!(!a.window_invariants_held);

        let mut set2 = w8_full_set();
        set2[4].final_artifact_status = "Failed".into();
        set2[4].case_status = "failed:artifact_verification".into();
        let a2 = analyse_w8(&set2);
        assert_eq!(a2.excluded_unverified, vec![set2[4].case_id.clone()]);
        // the cycle missing its W case contributes no ratio
        assert!(a2.paired.iter().all(|pr| pr.raw_ratios.len() == 1));
    }

    #[test]
    fn s4_case_result_without_window_fields_defaults_them() {
        // A Stage-4-era line (no window fields) must still parse.
        let line = r#"{"run_id":"i63s4-x","case_id":"i63s4-x/c2/s1/prep_ahead_2","mode":"prep_ahead_2","phase":"measured","cycle":2,"slot":1,"chunk_size_bytes":67108864,"extent_bytes":2147483648,"chunk_count":32,"transfer_id":"t","artifact_id":"a","bulk_stream_wall_ms":40000.0,"verified_transfer_wall_ms":46000.0,"resume_ms":3.0,"seal_d2_ms":5000.0,"read_ms":5700.0,"chunk_sha_ms":5400.0,"rolling_sha_ms":6100.0,"proof_ms":6.0,"put_ack_ms":33000.0,"prepared_buffer_peak":2,"device_read_count":32,"final_artifact_status":"Verified","case_status":"completed"}"#;
        let r: S4CaseResult = serde_json::from_str(line).unwrap();
        assert_eq!(r.put_window, 0);
        assert_eq!(r.peak_puts_in_flight, 0);
        assert!(!r.put_starts_ascending);
    }

    #[test]
    fn s4_case_result_parses_the_window8_runner_shape() {
        // EXACTLY the object the runner's `build_s4_case_result` emits for a
        // window_8 case (cross-crate contract).
        let line = r#"{"run_id":"i63w8-x","case_id":"i63w8-x/c1/s2/prep_ahead_window_8","mode":"prep_ahead_window_8","phase":"measured","cycle":1,"slot":2,"chunk_size_bytes":67108864,"extent_bytes":2147483648,"chunk_count":32,"transfer_id":"t","artifact_id":"a","bulk_stream_wall_ms":21000.0,"verified_transfer_wall_ms":27000.0,"resume_ms":3.0,"seal_d2_ms":5000.0,"read_ms":5700.0,"chunk_sha_ms":5400.0,"rolling_sha_ms":6100.0,"proof_ms":6.0,"put_ack_ms":90000.0,"prepared_buffer_peak":9,"device_read_count":32,"put_window":8,"put_started_count":32,"put_completed_count":32,"peak_puts_in_flight":8,"put_starts_ascending":true,"final_artifact_status":"Verified","case_status":"completed"}"#;
        let r: S4CaseResult = serde_json::from_str(line).unwrap();
        assert_eq!(r.mode, S4Mode::PrepAheadWindow8);
        assert_eq!(r.put_window, 8);
        assert_eq!(r.peak_puts_in_flight, 8);
        assert!(r.put_starts_ascending);
        assert!(r.is_completed_and_verified());
    }

    fn result(cycle: u8, slot: u8, mode: S4Mode, bulk_ms: f64, verified_ms: f64) -> S4CaseResult {
        S4CaseResult {
            run_id: "r".into(),
            case_id: format!("r/c{cycle}/s{slot}/{}", mode.wire()),
            mode,
            phase: Phase::Measured,
            cycle: Some(cycle),
            slot: Some(slot),
            chunk_size_bytes: S4_CHUNK_SIZE_BYTES,
            extent_bytes: EXTENT_BYTES,
            chunk_count: 32,
            transfer_id: Some("t".into()),
            artifact_id: Some("a".into()),
            bulk_stream_wall_ms: bulk_ms,
            verified_transfer_wall_ms: verified_ms,
            resume_ms: 3.0,
            seal_d2_ms: 5_000.0,
            read_ms: 5_700.0,
            chunk_sha_ms: 5_400.0,
            rolling_sha_ms: 6_100.0,
            proof_ms: 6.0,
            put_ack_ms: 33_000.0,
            prepared_buffer_peak: match mode {
                S4Mode::PrepAhead2 => 2,
                S4Mode::PrepAheadWindow8 | S4Mode::PrepAheadWindow8Batch8 => 9,
                S4Mode::Serial => 0,
            },
            device_read_count: 32,
            put_window: if mode == S4Mode::PrepAheadWindow8 { 8 } else { 0 },
            put_started_count: if mode == S4Mode::PrepAheadWindow8 { 32 } else { 0 },
            put_completed_count: if mode == S4Mode::PrepAheadWindow8 { 32 } else { 0 },
            peak_puts_in_flight: if mode == S4Mode::PrepAheadWindow8 { 8 } else { 0 },
            put_starts_ascending: mode == S4Mode::PrepAheadWindow8,
            final_artifact_status: "Verified".into(),
            case_status: "completed".into(),
        }
    }

    /// P faster than S every cycle (smaller wall) by a fixed factor.
    fn full_set() -> Vec<S4CaseResult> {
        let mut v = Vec::new();
        // warm-ups (excluded)
        for mode in [S4Mode::Serial, S4Mode::PrepAhead2] {
            let mut w = result(0, 0, mode, 52_000.0, 58_000.0);
            w.phase = Phase::Warmup;
            w.cycle = None;
            w.slot = None;
            w.case_id = format!("r/warmup/{}", mode.wire());
            v.push(w);
        }
        for cycle in 1..=4u8 {
            v.push(result(cycle, 1, S4Mode::Serial, 52_000.0, 58_000.0));
            v.push(result(cycle, 2, S4Mode::PrepAhead2, 40_000.0, 46_000.0));
        }
        v
    }

    #[test]
    fn analysis_summarises_each_mode_over_four_measured_cases() {
        let a = analyse_s4(&full_set());
        assert!(a.excluded_unverified.is_empty());
        let s = a.serial.unwrap();
        let p = a.prep_ahead.unwrap();
        assert_eq!(s.n, 4);
        assert_eq!(p.n, 4);
        assert!(p.median_bulk_mib_s > s.median_bulk_mib_s);
        assert_eq!(s.bulk_walls_ms.len(), 4);
        // overlap_saved for prep-ahead is positive (component sum > wall).
        assert!(p.median_overlap_saved_ms > 0.0);
    }

    #[test]
    fn paired_ratios_cover_both_series_favouring_prep_ahead() {
        let a = analyse_s4(&full_set());
        assert_eq!(a.paired.len(), 2);
        for pr in &a.paired {
            assert_eq!(pr.raw_ratios.len(), 4, "one ratio per shared cycle");
            assert!(pr.median_ratio > 1.0);
            assert_eq!(pr.cycles_favouring_prep_ahead, 4);
            assert!(pr.iqr.0 <= pr.iqr.1);
        }
        assert_eq!(a.significance_claim, "none (n=4 per mode; paired ratios only)");
    }

    #[test]
    fn unverified_measured_case_is_excluded() {
        let mut set = full_set();
        set[3].final_artifact_status = "Failed".into();
        set[3].case_status = "failed:artifact_verification".into();
        let a = analyse_s4(&set);
        assert_eq!(a.excluded_unverified, vec![set[3].case_id.clone()]);
    }

    #[test]
    fn warmups_never_contribute_to_the_analysis() {
        let a = analyse_s4(&full_set());
        assert_eq!(a.serial.unwrap().n, 4);
        assert_eq!(a.prep_ahead.unwrap().n, 4);
    }

    fn wrec(chunk_index: u64, staging_ns: u64, finalize_ns: u64, commit_ns: u64) -> WorkerPutRecord {
        WorkerPutRecord {
            ts_ms: 1_788_000_000_000 + chunk_index,
            transfer_id: "11111111-1111-1111-1111-111111111111".into(),
            chunk_index,
            outcome: "accepted".into(),
            verified_size: 67_108_864,
            handler_total_ns: staging_ns + commit_ns + 10_000,
            authorize_ns: 5_000,
            stage_call_ns: staging_ns + 2_000,
            commit_chunk_ns: commit_ns,
            overlapping: WorkerOverlapping {
                body_pump_ns: staging_ns - 1_000,
                staging_worker_ns: staging_ns,
            },
            begin_stage_ns: 1_000,
            write_sum_ns: staging_ns - finalize_ns - 2_000,
            digest_ns: 500,
            finalize_ns,
        }
    }

    #[test]
    fn worker_decomp_reports_every_interval_with_overlap_labels() {
        let recs: Vec<WorkerPutRecord> = (0..32)
            .map(|i| wrec(i, 900_000_000, 300_000_000, 40_000_000))
            .collect();
        let d = analyse_worker_decomp(&recs);
        assert_eq!(d.n_records, 32);
        let by = |name: &str| d.intervals.iter().find(|x| x.name == name).unwrap();
        assert!(by("body_pump_ms").overlapping);
        assert!(by("staging_worker_ms").overlapping);
        assert!(!by("finalize_ms").overlapping);
        assert!(!by("commit_chunk_ms").overlapping);
        assert!((by("staging_worker_ms").median_ms - 900.0).abs() < 1.0);
        assert!((by("finalize_ms").median_ms - 300.0).abs() < 1.0);
        assert!((by("commit_chunk_ms").median_ms - 40.0).abs() < 1.0);
    }

    #[test]
    fn worker_record_round_trips_through_json_including_nested_overlapping() {
        let r = wrec(7, 800_000_000, 250_000_000, 35_000_000);
        let line = serde_json::to_string(&r).unwrap();
        assert!(line.contains(r#""overlapping":{"#));
        let back: WorkerPutRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn s4_case_result_parses_the_winpe_runner_build_s4_case_result_shape() {
        // EXACTLY the object `winpe-runner/src/matrix.rs::build_s4_case_result`
        // emits for `case_completed` (cross-crate contract; the runner cannot
        // depend on this crate).
        let line = r#"{"run_id":"i63s4-x","case_id":"i63s4-x/c2/s1/prep_ahead_2","mode":"prep_ahead_2","phase":"measured","cycle":2,"slot":1,"chunk_size_bytes":67108864,"extent_bytes":2147483648,"chunk_count":32,"transfer_id":"t","artifact_id":"a","bulk_stream_wall_ms":40000.0,"verified_transfer_wall_ms":46000.0,"resume_ms":3.0,"seal_d2_ms":5000.0,"read_ms":5700.0,"chunk_sha_ms":5400.0,"rolling_sha_ms":6100.0,"proof_ms":6.0,"put_ack_ms":33000.0,"prepared_buffer_peak":2,"device_read_count":32,"final_artifact_status":"Verified","case_status":"completed"}"#;
        let r: S4CaseResult = serde_json::from_str(line).unwrap();
        assert_eq!(r.mode, S4Mode::PrepAhead2);
        assert_eq!(r.cycle, Some(2));
        assert_eq!(r.prepared_buffer_peak, 2);
        assert!(r.is_completed_and_verified());
        // integer-valued walls (serde_json emits `40000` not `40000.0` from
        // the runner's `json!` macro) still parse as f64:
        let intish = line.replace("40000.0", "40000").replace("46000.0", "46000");
        let r2: S4CaseResult = serde_json::from_str(&intish).unwrap();
        assert_eq!(r2.bulk_stream_wall_ms, 40000.0);
    }

    #[test]
    fn worker_record_parses_the_hand_built_worker_hook_line_shape() {
        // Exactly the shape `i63_timing::PutTimer::finish` emits.
        let line = r#"{"ts_ms":1788825000000,"transfer_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","chunk_index":5,"outcome":"accepted","verified_size":67108864,"handler_total_ns":1050000000,"authorize_ns":4200,"stage_call_ns":902000000,"commit_chunk_ns":41000000,"overlapping":{"body_pump_ns":880000000,"staging_worker_ns":900000000},"begin_stage_ns":900,"write_sum_ns":600000000,"digest_ns":450,"finalize_ns":295000000}"#;
        let r: WorkerPutRecord = serde_json::from_str(line).unwrap();
        assert_eq!(r.chunk_index, 5);
        assert_eq!(r.outcome, "accepted");
        assert_eq!(r.overlapping.staging_worker_ns, 900_000_000);
        assert_eq!(r.finalize_ns, 295_000_000);
    }
}
