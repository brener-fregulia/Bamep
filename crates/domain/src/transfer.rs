//! Bamep Domain: Transfer identity/correlation
//! (`docs/specifications/m0-data-plane-and-storage-contracts.md`; Issue
//! #36).
//!
//! A [`Transfer`] is a durable logical correlation/authorization/resume
//! identity, not a state machine of its own. The current normative
//! lifecycle affected by a transfer is principally the owning
//! [`crate::artifact::Artifact`]'s
//! (`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle"). This
//! module deliberately introduces no `TransferState` enum: a durable
//! pre-dispatch `Transfer` existing before an owning `Attempt` is bound is
//! not automatically "transfer-authorized" merely because it exists — see
//! [`Transfer::is_attempt_bound`].

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::artifact::{Artifact, ArtifactId};
use crate::attempt::{Attempt, AttemptId};
use crate::chunk_manifest::{ChunkManifest, ChunkSize, DigestAlgorithm};
use crate::job::{JobId, JobStepId};
use crate::EndpointId;

/// Server-generated durable identity for one [`Transfer`], distinct from
/// [`crate::artifact::ArtifactId`], [`JobId`], [`JobStepId`], [`AttemptId`],
/// [`crate::attempt::ActionId`], HTTP request identity, and
/// proof/capability identity (Issue #36 "Stable identities").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransferId(pub Uuid);

impl TransferId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TransferId {
    fn default() -> Self {
        Self::new()
    }
}

/// M1's single proven direction
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005:
/// `"direction": "agent_to_server"`). The generic M0 data-plane contract
/// remains bidirectional; this Domain enum is extended, not replaced, when
/// a future milestone proves `ServerToAgent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferDirection {
    AgentToServer,
}

/// Opaque, strongly typed source-provenance identity for one Transfer's
/// concrete simulated capture source
/// (`m0-data-plane-and-storage-contracts.md` "Artifact provenance and
/// target identity": "Artifact source provenance must identify the concrete
/// capture source, not only the Endpoint"). Deliberately not a disk/
/// partition/filesystem inventory model — the minimum Simulator-safe
/// representation Issue #36 requires. Distinct from
/// [`crate::target_fingerprint::TargetFingerprint`] (a later destructive
/// *target* identity) and never compared against it: legitimate disk
/// replacement means source and target fingerprints may validly differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProvenance(String);

impl SourceProvenance {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The durable logical Transfer correlation
/// (`m0-data-plane-and-storage-contracts.md` "Durable versus transient
/// authorization state"). `attempt_id` is `None` before a later dispatch
/// boundary (#40) binds this Transfer to its owning Attempt — see
/// [`Transfer::is_attempt_bound`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub id: TransferId,
    pub endpoint_id: EndpointId,
    pub job_id: JobId,
    pub job_step_id: JobStepId,
    pub artifact_id: ArtifactId,
    pub direction: TransferDirection,
    pub digest_algorithm: DigestAlgorithm,
    pub chunk_size: ChunkSize,
    pub source_provenance: SourceProvenance,
    pub attempt_id: Option<AttemptId>,
}

impl Transfer {
    /// Whether this Transfer currently has an owning Attempt bound
    /// (`m0-data-plane-and-storage-contracts.md`; Issue #36 scope: "a
    /// Transfer without an owning Attempt is not eligible for transfer
    /// authorization"). This Work Package does not evaluate the owning
    /// Attempt's own non-terminal state — that composition belongs to a
    /// later authorization Work Package (#38).
    pub fn is_attempt_bound(&self) -> bool {
        self.attempt_id.is_some()
    }
}

/// The durable pre-dispatch state one [`create_transfer_context`] call
/// produces: the [`Transfer`], its owning [`Artifact`] (`Incomplete`), and
/// its empty/unsealed [`ChunkManifest`] (Issue #36 "Pre-dispatch creation").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferContext {
    pub transfer: Transfer,
    pub artifact: Artifact,
    pub manifest: ChunkManifest,
}

/// Constructs a fresh pre-dispatch `Transfer` + `Artifact` + empty
/// `ChunkManifest` for one Endpoint/JobStep workflow context (Issue #36
/// "Pre-dispatch creation"). Pure — performs no I/O and does not verify that
/// `endpoint_id`/`job_id`/`job_step_id` exist or are eligible; that
/// verification belongs to the Application/Adapter boundary that calls this
/// function before persisting its result, mirroring
/// [`crate::job::create_workflow`]. Never creates an Attempt or action
/// identity, never transitions any JobStep, and never evaluates the
/// destructive-operation gate.
pub fn create_transfer_context(
    endpoint_id: EndpointId,
    job_id: JobId,
    job_step_id: JobStepId,
    direction: TransferDirection,
    digest_algorithm: DigestAlgorithm,
    chunk_size: ChunkSize,
    source_provenance: SourceProvenance,
) -> TransferContext {
    let artifact_id = ArtifactId::new();
    let transfer = Transfer {
        id: TransferId::new(),
        endpoint_id,
        job_id,
        job_step_id,
        artifact_id,
        direction,
        digest_algorithm,
        chunk_size,
        source_provenance,
        attempt_id: None,
    };
    let artifact = Artifact::new_incomplete(artifact_id);
    let manifest = ChunkManifest::new(artifact_id, digest_algorithm, chunk_size);
    TransferContext {
        transfer,
        artifact,
        manifest,
    }
}

/// Rejections from [`bind_attempt`]. Neither represents a partial mutation —
/// a rejected call leaves `transfer` exactly as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransferBindingError {
    /// `attempt.job_step_id` does not match `transfer.job_step_id` — an
    /// Attempt must never come to own an unrelated Transfer through
    /// inconsistent correlation (Issue #36 "Transfer -> Attempt binding
    /// support").
    #[error("attempt does not belong to this transfer's job step")]
    WrongJobStep,
    /// `transfer` is already bound to a *different* Attempt.
    #[error("transfer is already bound to a different attempt")]
    ConflictingRebind,
}

/// Binds `transfer` to `attempt` exactly once
/// (`m0-data-plane-and-storage-contracts.md`; Issue #36 "Transfer -> Attempt
/// binding support": "zero times -> exactly one Attempt", "never rebound to
/// a different Attempt"). Idempotent when `transfer` is already bound to
/// this exact `attempt.id` (safe retry); rejects a genuinely different
/// Attempt without mutating anything. Never changes `TransferId`,
/// `ArtifactId`, or any other `Transfer` field. This function never creates
/// an Attempt itself — committing the owning Attempt belongs to a later
/// dispatch boundary (#40); this is only the narrow binding primitive that
/// boundary composes into its own atomic commitment.
pub fn bind_attempt(
    transfer: &Transfer,
    attempt: &Attempt,
) -> Result<Transfer, TransferBindingError> {
    if attempt.job_step_id != transfer.job_step_id {
        return Err(TransferBindingError::WrongJobStep);
    }
    match transfer.attempt_id {
        Some(existing) if existing != attempt.id => Err(TransferBindingError::ConflictingRebind),
        _ => Ok(Transfer {
            attempt_id: Some(attempt.id),
            ..transfer.clone()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::ActionId;

    fn ctx() -> TransferContext {
        create_transfer_context(
            EndpointId::new(),
            JobId::new(),
            JobStepId::new(),
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
    }

    fn attempt_for(job_step_id: JobStepId) -> Attempt {
        Attempt {
            id: AttemptId::new(),
            job_step_id,
            action_id: ActionId::new(),
            state: crate::attempt::AttemptState::Dispatched,
        }
    }

    #[test]
    fn fresh_context_has_incomplete_artifact_and_unbound_transfer() {
        let context = ctx();
        assert_eq!(
            context.artifact.state,
            crate::artifact::ArtifactState::Incomplete
        );
        assert_eq!(context.transfer.artifact_id, context.artifact.id);
        assert_eq!(context.manifest.artifact_id, context.artifact.id);
        assert!(!context.transfer.is_attempt_bound());
        assert_eq!(context.transfer.attempt_id, None);
    }

    #[test]
    fn two_contexts_never_share_identities() {
        let a = ctx();
        let b = ctx();
        assert_ne!(a.transfer.id, b.transfer.id);
        assert_ne!(a.artifact.id, b.artifact.id);
    }

    #[test]
    fn binding_an_unbound_transfer_succeeds() {
        let context = ctx();
        let attempt = attempt_for(context.transfer.job_step_id);

        let bound = bind_attempt(&context.transfer, &attempt).unwrap();
        assert_eq!(bound.attempt_id, Some(attempt.id));
        assert!(bound.is_attempt_bound());
        // Identity/correlation must remain exactly unchanged.
        assert_eq!(bound.id, context.transfer.id);
        assert_eq!(bound.artifact_id, context.transfer.artifact_id);
    }

    #[test]
    fn rebinding_the_same_attempt_is_idempotent() {
        let context = ctx();
        let attempt = attempt_for(context.transfer.job_step_id);
        let bound = bind_attempt(&context.transfer, &attempt).unwrap();

        let rebound = bind_attempt(&bound, &attempt).unwrap();
        assert_eq!(rebound.attempt_id, Some(attempt.id));
    }

    #[test]
    fn rebinding_a_different_attempt_is_rejected() {
        let context = ctx();
        let first = attempt_for(context.transfer.job_step_id);
        let bound = bind_attempt(&context.transfer, &first).unwrap();

        let second = attempt_for(context.transfer.job_step_id);
        let err = bind_attempt(&bound, &second).unwrap_err();
        assert_eq!(err, TransferBindingError::ConflictingRebind);
        // Must remain bound to the original attempt.
        assert_eq!(bound.attempt_id, Some(first.id));
    }

    #[test]
    fn binding_an_attempt_from_a_different_job_step_is_rejected() {
        let context = ctx();
        let foreign_attempt = attempt_for(JobStepId::new());

        let err = bind_attempt(&context.transfer, &foreign_attempt).unwrap_err();
        assert_eq!(err, TransferBindingError::WrongJobStep);
        assert!(!context.transfer.is_attempt_bound());
    }
}
