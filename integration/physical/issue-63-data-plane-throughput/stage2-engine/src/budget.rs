//! Stage-3 disk-budget preflight — PURE.
//!
//! The physical Stage-3 matrix intentionally PRESERVES every Artifact until
//! owner review. Payload alone is `36 x 2 GiB = 72 GiB`. Add a safe margin for
//! filesystem metadata, staging, DB/runtime, logs, and uncertainty:
//!
//!   default Stage-3 gate: require `>= 90 GiB` available under the selected
//!   Worker storage root.
//!
//! This module implements/tests only the calculation. It NEVER deletes #61
//! evidence or any other data, and NEVER shrinks the matrix. Insufficient space
//! ⇒ FAIL CLOSED before the matrix starts.

use crate::matrix::{EXTENT_BYTES, TOTAL_CASES};
use crate::GIB;

/// The raw Artifact payload the Stage-3 matrix writes and keeps: every one of
/// the 36 cases (4 warm-ups + 32 measured) is a full 2048 MiB Artifact.
pub const PAYLOAD_BYTES: u64 = TOTAL_CASES as u64 * EXTENT_BYTES; // 72 GiB

/// The default free-space gate under the selected Worker storage root.
pub const DEFAULT_MIN_FREE_BYTES: u64 = 90 * GIB;

/// Inputs to the preflight. `observed_free_bytes` comes from a `statvfs`/`df`
/// read the CALLER performs; this module never touches the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetInputs {
    pub case_count: u64,
    pub extent_bytes: u64,
    pub required_min_free_bytes: u64,
    pub observed_free_bytes: u64,
}

impl BudgetInputs {
    /// The canonical Stage-3 inputs (36 cases, 2048 MiB each, 90 GiB gate) with
    /// a caller-provided free-space observation.
    pub fn stage3(observed_free_bytes: u64) -> Self {
        Self {
            case_count: TOTAL_CASES as u64,
            extent_bytes: EXTENT_BYTES,
            required_min_free_bytes: DEFAULT_MIN_FREE_BYTES,
            observed_free_bytes,
        }
    }

    pub fn payload_bytes(&self) -> u64 {
        self.case_count.saturating_mul(self.extent_bytes)
    }
}

/// The preflight verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetVerdict {
    Ok {
        payload_bytes: u64,
        /// `observed_free - required_min_free` (always `>= 0` here).
        headroom_over_gate_bytes: u64,
    },
    /// FAIL CLOSED — do not start the matrix.
    Insufficient {
        payload_bytes: u64,
        required_min_free_bytes: u64,
        observed_free_bytes: u64,
        /// `required_min_free - observed_free`.
        shortfall_bytes: u64,
    },
}

impl BudgetVerdict {
    pub fn is_ok(&self) -> bool {
        matches!(self, BudgetVerdict::Ok { .. })
    }
}

/// Evaluate the preflight. Also fails closed if the gate is somehow set below
/// the raw payload (a misconfiguration must never let the matrix start).
pub fn evaluate_budget(inputs: &BudgetInputs) -> BudgetVerdict {
    let payload = inputs.payload_bytes();
    let effective_gate = inputs.required_min_free_bytes.max(payload);

    if inputs.observed_free_bytes >= effective_gate {
        BudgetVerdict::Ok {
            payload_bytes: payload,
            headroom_over_gate_bytes: inputs.observed_free_bytes - effective_gate,
        }
    } else {
        BudgetVerdict::Insufficient {
            payload_bytes: payload,
            required_min_free_bytes: effective_gate,
            observed_free_bytes: inputs.observed_free_bytes,
            shortfall_bytes: effective_gate - inputs.observed_free_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_exactly_72_gib() {
        assert_eq!(PAYLOAD_BYTES, 72 * GIB);
        assert_eq!(PAYLOAD_BYTES, 77_309_411_328);
    }

    #[test]
    fn default_gate_is_90_gib() {
        assert_eq!(DEFAULT_MIN_FREE_BYTES, 96_636_764_160);
    }

    #[test]
    fn exactly_90_gib_free_is_ok_89_gib_is_fail_closed() {
        assert!(evaluate_budget(&BudgetInputs::stage3(90 * GIB)).is_ok());
        match evaluate_budget(&BudgetInputs::stage3(89 * GIB)) {
            BudgetVerdict::Insufficient {
                payload_bytes,
                shortfall_bytes,
                ..
            } => {
                assert_eq!(payload_bytes, 72 * GIB);
                assert_eq!(shortfall_bytes, GIB);
            }
            other => panic!("expected Insufficient, got {other:?}"),
        }
    }

    #[test]
    fn headroom_is_reported_over_the_gate() {
        match evaluate_budget(&BudgetInputs::stage3(120 * GIB)) {
            BudgetVerdict::Ok {
                payload_bytes,
                headroom_over_gate_bytes,
            } => {
                assert_eq!(payload_bytes, 72 * GIB);
                assert_eq!(headroom_over_gate_bytes, 30 * GIB);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn a_gate_below_the_raw_payload_is_clamped_up_to_the_payload() {
        // Misconfigured 40 GiB gate; 60 GiB free would "pass" the bad gate but
        // cannot hold the 72 GiB payload -> still fail closed.
        let inputs = BudgetInputs {
            required_min_free_bytes: 40 * GIB,
            observed_free_bytes: 60 * GIB,
            ..BudgetInputs::stage3(60 * GIB)
        };
        match evaluate_budget(&inputs) {
            BudgetVerdict::Insufficient {
                required_min_free_bytes,
                ..
            } => assert_eq!(required_min_free_bytes, PAYLOAD_BYTES),
            other => panic!("expected Insufficient, got {other:?}"),
        }
    }

    #[test]
    fn does_not_shrink_the_matrix_or_change_case_count() {
        let inputs = BudgetInputs::stage3(10 * GIB);
        assert_eq!(inputs.case_count, 36);
        assert_eq!(inputs.payload_bytes(), 72 * GIB);
        assert!(!evaluate_budget(&inputs).is_ok());
    }
}
