//! Issue #63 Stage 2 — probe-side glue to the engine's physical source-safety
//! predicate. NEW for Issue #63 (no #61 equivalent).
//!
//! The predicate itself lives in `bamep_i63_stage2_engine::safety` and is pure /
//! unit-tested. This module only assembles its inputs from the probe's
//! read-only enumeration + resolver + 3-IOCTL device length, and is the
//! structural gate the probe `main` runs BEFORE creating the bulk `ChunkReader`.
//! A rejection means ZERO bulk source reads.

use bamep_i63_stage2_engine::safety::{
    evaluate_predicate, ResolvedSource, SafetyRequest, SafetyVerdict,
};

use crate::resolver::Resolved;
use crate::sources::{DeviceLength, LocalSource};

/// The outcome the probe acts on.
pub enum GateOutcome {
    /// Safe: `locator` may be opened for bulk reads; `device_length_bytes` is
    /// the agreed exact length.
    Accept {
        locator: String,
        device_length_bytes: u64,
    },
    /// Fail closed. `token` is a short reason for the evidence line; NO bulk
    /// read may occur.
    Reject { token: String },
}

/// Evaluate the Issue-63 physical source-safety predicate.
///
/// * `current_observation_id` — the epoch minted THIS process.
/// * `selected_agent_source_ids` — every id the operator's local-evidence
///   predicate selected (must be exactly one).
/// * `resolved` — the resolver's single-source result (or `None`).
/// * `resolved_source` — the enumerated `LocalSource` for the resolved locator
///   (model / serial / evidence).
/// * `device_length` — the read-only 3-IOCTL length agreement.
/// * `requested_extent_bytes` — the bounded matrix extent (2048 MiB).
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    current_observation_id: &str,
    selected_agent_source_ids: &[String],
    resolved: Option<&Resolved>,
    resolved_source: Option<&LocalSource>,
    device_length: Option<&DeviceLength>,
    requested_extent_bytes: u64,
) -> GateOutcome {
    let engine_resolved: Option<ResolvedSource> = match (resolved, resolved_source) {
        (Some(r), Some(src)) => Some(ResolvedSource {
            agent_source_id: r.agent_source_id.clone(),
            source_observation_id: current_observation_id.to_string(),
            local_locator: r.local_locator.clone(),
            model: src.product.clone(),
            serial: {
                let s = src.serial.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            },
            device_length_bytes: device_length.and_then(DeviceLength::authoritative),
        }),
        _ => None,
    };

    let req = SafetyRequest {
        current_observation_id,
        selected_agent_source_ids,
        resolved: engine_resolved.as_ref(),
        requested_extent_bytes,
    };

    match evaluate_predicate(&req) {
        SafetyVerdict::Accept {
            locator,
            device_length_bytes,
        } => GateOutcome::Accept {
            locator,
            device_length_bytes,
        },
        SafetyVerdict::Reject(reason) => GateOutcome::Reject {
            token: format!("{reason:?}")
                .split_whitespace()
                .next()
                .unwrap_or("Reject")
                .trim_end_matches('{')
                .to_string(),
        },
    }
}
