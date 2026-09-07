//! The per-case result record schema (one NDJSON line per case) + a generated
//! run summary. PURE (serde only).
//!
//! Measurement boundaries (both are mandatory and separate):
//!
//!   A. BULK STREAM WALL — starts immediately before the first bounded source
//!      read that begins transferring the Artifact payload; ends after the
//!      final expected chunk is durably accepted.
//!
//!   B. END-TO-END VERIFIED TRANSFER WALL — starts immediately before resume
//!      discovery / transfer stream begins; includes resume, source read/hash,
//!      all PUT/ACK work, seal, and Worker D2 full-Artifact verification; ends
//!      when the Artifact is observed `Verified`.
//!
//! `resume_ms` and `seal_d2_ms` are also recorded separately so a slow D2 is
//! never hidden inside a "network throughput" number.

use serde::{Deserialize, Serialize};

/// Connection count is "expected / by construction" for the current serial,
/// fresh-connection `DataPlaneClient` (`N chunk PUTs + resume + seal = N + 2`),
/// kept distinct from any real socket counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionCount {
    pub resume_requests: u64,
    pub chunk_puts: u64,
    pub seal_requests: u64,
    /// `chunk_puts + resume_requests + seal_requests` for the serial client.
    pub expected_total: u64,
    /// An actual socket counter, when one is wired; `None` = not measured.
    pub observed_total: Option<u64>,
}

impl ConnectionCount {
    /// The by-construction count for `n` chunks over the current serial client.
    pub fn by_construction(n: u64) -> Self {
        Self {
            resume_requests: 1,
            chunk_puts: n,
            seal_requests: 1,
            expected_total: n + 2,
            observed_total: None,
        }
    }
}

/// Per-transfer stage timings (aggregate; NOT high-frequency telemetry).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StageTimings {
    pub read_ms: f64,
    pub chunk_sha_ms: f64,
    pub rolling_sha_ms: f64,
    pub proof_ms: f64,
    pub put_ack_ms: f64,
    pub resume_ms: f64,
    pub seal_d2_ms: f64,
}

/// One structured result record per case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseResult {
    pub run_id: String,
    pub case_id: String,
    pub phase: crate::matrix::Phase,
    pub cycle: Option<u8>,
    pub slot: Option<u8>,
    pub chunk_size_bytes: u64,
    pub extent_bytes: u64,
    pub chunk_count: u64,
    pub transfer_id: Option<String>,
    pub artifact_id: Option<String>,

    /// The `SafetyVerdict` rendered as a short token, e.g. `accept` or
    /// `reject:WrongDeviceLength`.
    pub source_safety_verdict: String,
    /// The clock/skew verdict token, e.g. `in_bound(skew_after_ms=-6)`.
    pub clock_skew_verdict: String,

    // ---- boundary A: bulk stream wall ----
    pub bulk_stream_wall_ms: f64,
    pub bulk_stream_mib_s: f64,
    pub bulk_stream_mb_s: f64,

    // ---- boundary B: end-to-end verified transfer wall ----
    pub verified_transfer_wall_ms: f64,
    pub verified_transfer_mib_s: f64,
    pub verified_transfer_mb_s: f64,

    // ---- separate sub-intervals ----
    pub resume_ms: f64,
    pub seal_d2_ms: f64,

    // ---- Agent/probe stage aggregates ----
    pub read_ms: f64,
    pub chunk_sha_ms: f64,
    pub rolling_sha_ms: f64,
    pub proof_ms: f64,
    pub put_ack_ms: f64,

    pub connection_count: ConnectionCount,

    /// `Verified` on success; never a performance number is valid without it.
    pub final_artifact_status: String,
    /// `completed` | `failed` | `contaminated`.
    pub case_status: String,
}

impl CaseResult {
    /// Serialize to one NDJSON line (no trailing newline). NEVER carries a
    /// secret value.
    pub fn to_ndjson_line(&self) -> String {
        serde_json::to_string(self).expect("CaseResult serializes")
    }

    pub fn is_completed_and_verified(&self) -> bool {
        self.case_status == "completed" && self.final_artifact_status == "Verified"
    }

    pub fn is_measured(&self) -> bool {
        self.phase == crate::matrix::Phase::Measured
    }

    /// Throughput = bytes / wall. Helper for constructing a record from raw
    /// walls (keeps the two rates internally consistent).
    pub fn rates(extent_bytes: u64, wall_ms: f64) -> (f64, f64) {
        if wall_ms <= 0.0 {
            return (0.0, 0.0);
        }
        let secs = wall_ms / 1000.0;
        let mib_s = (extent_bytes as f64 / crate::MIB as f64) / secs;
        let mb_s = (extent_bytes as f64 / 1_000_000.0) / secs;
        (mib_s, mb_s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Phase;

    fn sample() -> CaseResult {
        let (b_mib, b_mb) = CaseResult::rates(crate::matrix::EXTENT_BYTES, 60_000.0);
        let (v_mib, v_mb) = CaseResult::rates(crate::matrix::EXTENT_BYTES, 66_000.0);
        CaseResult {
            run_id: "i63s3-x".into(),
            case_id: "i63s3-x/c1/s1/08mib".into(),
            phase: Phase::Measured,
            cycle: Some(1),
            slot: Some(1),
            chunk_size_bytes: 8 * crate::MIB,
            extent_bytes: crate::matrix::EXTENT_BYTES,
            chunk_count: 256,
            transfer_id: Some("t-1".into()),
            artifact_id: Some("a-1".into()),
            source_safety_verdict: "accept".into(),
            clock_skew_verdict: "in_bound(skew_after_ms=-6)".into(),
            bulk_stream_wall_ms: 60_000.0,
            bulk_stream_mib_s: b_mib,
            bulk_stream_mb_s: b_mb,
            verified_transfer_wall_ms: 66_000.0,
            verified_transfer_mib_s: v_mib,
            verified_transfer_mb_s: v_mb,
            resume_ms: 3.0,
            seal_d2_ms: 5_800.0,
            read_ms: 12_000.0,
            chunk_sha_ms: 4_000.0,
            rolling_sha_ms: 4_000.0,
            proof_ms: 20.0,
            put_ack_ms: 40_000.0,
            connection_count: ConnectionCount::by_construction(256),
            final_artifact_status: "Verified".into(),
            case_status: "completed".into(),
        }
    }

    #[test]
    fn round_trips_through_ndjson() {
        let r = sample();
        let line = r.to_ndjson_line();
        assert!(!line.contains('\n'));
        let back: CaseResult = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn connection_count_is_n_plus_2_by_construction() {
        let c = ConnectionCount::by_construction(32);
        assert_eq!(c.expected_total, 34);
        assert_eq!(c.observed_total, None);
    }

    #[test]
    fn rates_are_consistent_and_zero_safe() {
        let (mib, mb) = CaseResult::rates(crate::matrix::EXTENT_BYTES, 0.0);
        assert_eq!((mib, mb), (0.0, 0.0));
        let (mib, _mb) = CaseResult::rates(crate::matrix::EXTENT_BYTES, 60_000.0);
        // 2048 MiB / 60 s ≈ 34.13 MiB/s
        assert!((mib - 2048.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn verified_requires_both_completed_status_and_verified_artifact() {
        let mut r = sample();
        assert!(r.is_completed_and_verified());
        r.final_artifact_status = "Failed".into();
        assert!(!r.is_completed_and_verified());
        r.final_artifact_status = "Verified".into();
        r.case_status = "contaminated".into();
        assert!(!r.is_completed_and_verified());
    }
}
