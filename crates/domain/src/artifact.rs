//! Bamep Domain: Artifact lifecycle
//! (`docs/specifications/m0-data-plane-and-storage-contracts.md` "Artifact
//! lifecycle"; Issue #36).
//!
//! Cryptographic integrity ([`ArtifactState`]) and capture consistency
//! ([`CaptureConsistency`]) are independent facts on the same aggregate: no
//! transition function here reads or writes the other
//! (`m0-data-plane-and-storage-contracts.md` "Capture-consistency fact").
//! This module owns only the authoritative lifecycle transition — the heavy
//! byte-level digest computation that produces `digest_matches` for
//! [`complete_verification`] belongs to a future Worker (#39); no hashing or
//! I/O happens here.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::chunk_manifest::ChunkIndex;

/// Server-generated durable identity for one [`Artifact`], distinct from
/// [`crate::transfer::TransferId`], [`crate::job::JobId`],
/// [`crate::job::JobStepId`], [`crate::attempt::AttemptId`], and
/// [`crate::attempt::ActionId`] (Issue #36 "Stable identities").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId(pub Uuid);

impl ArtifactId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ArtifactId {
    fn default() -> Self {
        Self::new()
    }
}

/// The approved Artifact lifecycle
/// (`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle"):
/// `Incomplete -> PendingVerification -> Verified | Failed`, plus
/// `Incomplete -> Failed`. `Verified` and `Failed` are terminal — neither is
/// ever reopened or repaired into a different content identity in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactState {
    Incomplete,
    PendingVerification,
    Verified,
    Failed,
}

/// The closed capture-consistency vocabulary
/// (`m0-data-plane-and-storage-contracts.md` "Capture-consistency fact").
/// `Established` requires positive confirmation and is never a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureConsistency {
    NotApplicable,
    NotEstablished,
    Established,
}

/// One atomic integrity/completeness unit
/// (`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle": "An
/// Artifact is one atomic integrity/completeness unit"). `capture_consistency`
/// is tracked on the same aggregate but transitions independently of `state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: ArtifactId,
    pub state: ArtifactState,
    pub capture_consistency: CaptureConsistency,
}

impl Artifact {
    /// A freshly created Artifact: always `Incomplete`.
    /// `capture_consistency` starts `NotEstablished` — never `Established`
    /// by default (`m0-data-plane-and-storage-contracts.md`
    /// "Capture-consistency fact"). M1's Volume/Image capture always
    /// requires a capture-consistency fact (it is never `NotApplicable`),
    /// so `NotEstablished` is the correct starting point until a later
    /// mechanism (out of this Work Package's scope) positively confirms it.
    pub fn new_incomplete(id: ArtifactId) -> Self {
        Self {
            id,
            state: ArtifactState::Incomplete,
            capture_consistency: CaptureConsistency::NotEstablished,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.state, ArtifactState::Verified | ArtifactState::Failed)
    }
}

/// Rejections from the transition functions below. None represents a
/// partial mutation — a rejected call leaves the `Artifact` exactly as it
/// was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactTransitionError {
    #[error("artifact is not Incomplete")]
    NotIncomplete,
    #[error("artifact is not PendingVerification")]
    NotPendingVerification,
    #[error("manifest is not sealed")]
    ManifestNotSealed,
    #[error("manifest does not belong to this artifact")]
    ManifestMismatch,
    #[error("one or more expected chunks are not yet durably held/verified")]
    IncompleteChunks,
}

/// `Incomplete -> PendingVerification`
/// (`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle"): requires
/// `manifest` sealed and owned by `artifact`, and every one of its expected
/// chunk indices present in `held_chunk_indices`. `held_chunk_indices` must
/// reflect only durably verified chunk acceptance — never Worker-local
/// transient memory (`m0-data-plane-and-storage-contracts.md` "HTTPS
/// data-plane v1 contract" "Resume discovery"); this function performs no
/// I/O and trusts its caller to have supplied that durable fact under lock.
pub fn begin_verification(
    artifact: &Artifact,
    manifest: &crate::chunk_manifest::ChunkManifest,
    held_chunk_indices: &std::collections::BTreeSet<ChunkIndex>,
) -> Result<Artifact, ArtifactTransitionError> {
    if artifact.state != ArtifactState::Incomplete {
        return Err(ArtifactTransitionError::NotIncomplete);
    }
    if manifest.artifact_id != artifact.id {
        return Err(ArtifactTransitionError::ManifestMismatch);
    }
    if !manifest.sealed {
        return Err(ArtifactTransitionError::ManifestNotSealed);
    }
    let all_held = manifest
        .chunk_indices()
        .all(|index| held_chunk_indices.contains(&index));
    if !all_held {
        return Err(ArtifactTransitionError::IncompleteChunks);
    }
    Ok(Artifact {
        state: ArtifactState::PendingVerification,
        ..artifact.clone()
    })
}

/// `PendingVerification -> Verified | Failed`
/// (`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle"), decided
/// by an already independently computed full-Artifact digest match. This
/// function performs no hashing itself — see module docs.
pub fn complete_verification(
    artifact: &Artifact,
    digest_matches: bool,
) -> Result<Artifact, ArtifactTransitionError> {
    if artifact.state != ArtifactState::PendingVerification {
        return Err(ArtifactTransitionError::NotPendingVerification);
    }
    Ok(Artifact {
        state: if digest_matches {
            ArtifactState::Verified
        } else {
            ArtifactState::Failed
        },
        ..artifact.clone()
    })
}

/// `Incomplete -> Failed` (`m0-data-plane-and-storage-contracts.md`
/// "Artifact lifecycle"): a required chunk could not be reproduced/verified,
/// or capture/transfer was abandoned/cancelled. Rejects an already-terminal
/// Artifact — a failed/verified Artifact identity is never repaired in
/// place.
pub fn fail_incomplete(artifact: &Artifact) -> Result<Artifact, ArtifactTransitionError> {
    if artifact.state != ArtifactState::Incomplete {
        return Err(ArtifactTransitionError::NotIncomplete);
    }
    Ok(Artifact {
        state: ArtifactState::Failed,
        ..artifact.clone()
    })
}

/// Sets `capture_consistency`, independently of `state`
/// (`m0-data-plane-and-storage-contracts.md` "Capture-consistency fact":
/// cryptographic integrity and capture consistency are independent facts).
/// Legal at any Artifact state — the concrete mechanism that establishes
/// this fact is outside this Specification and this Work Package; this
/// function only records the given value.
pub fn set_capture_consistency(artifact: &Artifact, value: CaptureConsistency) -> Artifact {
    Artifact {
        capture_consistency: value,
        ..artifact.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_manifest::{ChunkManifest, ChunkSize, DigestAlgorithm};
    use std::collections::BTreeSet;

    fn digest32(byte: u8) -> Vec<u8> {
        vec![byte; 32]
    }

    fn sealed_manifest(artifact_id: ArtifactId) -> ChunkManifest {
        let manifest = ChunkManifest::new(
            artifact_id,
            DigestAlgorithm::Sha256,
            ChunkSize::new(1024).unwrap(),
        );
        let manifest = match manifest
            .record_expected_chunk(ChunkIndex(0), 10, digest32(1))
            .unwrap()
        {
            crate::chunk_manifest::ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };
        match manifest.seal(1, digest32(9)).unwrap() {
            crate::chunk_manifest::SealOutcome::Sealed(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn fresh_artifact_is_incomplete_with_capture_consistency_not_established() {
        let artifact = Artifact::new_incomplete(ArtifactId::new());
        assert_eq!(artifact.state, ArtifactState::Incomplete);
        assert_eq!(
            artifact.capture_consistency,
            CaptureConsistency::NotEstablished
        );
        assert!(!artifact.is_terminal());
    }

    #[test]
    fn begin_verification_requires_sealed_manifest() {
        let artifact_id = ArtifactId::new();
        let artifact = Artifact::new_incomplete(artifact_id);
        let manifest = ChunkManifest::new(
            artifact_id,
            DigestAlgorithm::Sha256,
            ChunkSize::new(1024).unwrap(),
        );
        let held = BTreeSet::new();

        assert_eq!(
            begin_verification(&artifact, &manifest, &held),
            Err(ArtifactTransitionError::ManifestNotSealed)
        );
    }

    #[test]
    fn begin_verification_requires_every_expected_chunk_held() {
        let artifact_id = ArtifactId::new();
        let artifact = Artifact::new_incomplete(artifact_id);
        let manifest = sealed_manifest(artifact_id);
        let held = BTreeSet::new();

        assert_eq!(
            begin_verification(&artifact, &manifest, &held),
            Err(ArtifactTransitionError::IncompleteChunks)
        );
    }

    #[test]
    fn begin_verification_succeeds_once_every_chunk_is_held() {
        let artifact_id = ArtifactId::new();
        let artifact = Artifact::new_incomplete(artifact_id);
        let manifest = sealed_manifest(artifact_id);
        let mut held = BTreeSet::new();
        held.insert(ChunkIndex(0));

        let advanced = begin_verification(&artifact, &manifest, &held).unwrap();
        assert_eq!(advanced.state, ArtifactState::PendingVerification);
    }

    #[test]
    fn begin_verification_rejects_mismatched_manifest() {
        let artifact = Artifact::new_incomplete(ArtifactId::new());
        let other_manifest = sealed_manifest(ArtifactId::new());
        let held: BTreeSet<ChunkIndex> = [ChunkIndex(0)].into_iter().collect();

        assert_eq!(
            begin_verification(&artifact, &other_manifest, &held),
            Err(ArtifactTransitionError::ManifestMismatch)
        );
    }

    #[test]
    fn complete_verification_success_reaches_verified() {
        let artifact = Artifact {
            id: ArtifactId::new(),
            state: ArtifactState::PendingVerification,
            capture_consistency: CaptureConsistency::NotEstablished,
        };
        let done = complete_verification(&artifact, true).unwrap();
        assert_eq!(done.state, ArtifactState::Verified);
        assert!(done.is_terminal());
    }

    #[test]
    fn complete_verification_failure_reaches_failed() {
        let artifact = Artifact {
            id: ArtifactId::new(),
            state: ArtifactState::PendingVerification,
            capture_consistency: CaptureConsistency::NotEstablished,
        };
        let done = complete_verification(&artifact, false).unwrap();
        assert_eq!(done.state, ArtifactState::Failed);
        assert!(done.is_terminal());
    }

    #[test]
    fn complete_verification_rejects_non_pending_artifact() {
        let artifact = Artifact::new_incomplete(ArtifactId::new());
        assert_eq!(
            complete_verification(&artifact, true),
            Err(ArtifactTransitionError::NotPendingVerification)
        );
    }

    #[test]
    fn fail_incomplete_reaches_failed() {
        let artifact = Artifact::new_incomplete(ArtifactId::new());
        let failed = fail_incomplete(&artifact).unwrap();
        assert_eq!(failed.state, ArtifactState::Failed);
    }

    #[test]
    fn fail_incomplete_rejects_non_incomplete_artifact() {
        let artifact = Artifact {
            id: ArtifactId::new(),
            state: ArtifactState::PendingVerification,
            capture_consistency: CaptureConsistency::NotEstablished,
        };
        assert_eq!(
            fail_incomplete(&artifact),
            Err(ArtifactTransitionError::NotIncomplete)
        );
    }

    #[test]
    fn terminal_states_reject_every_further_transition() {
        let verified = Artifact {
            id: ArtifactId::new(),
            state: ArtifactState::Verified,
            capture_consistency: CaptureConsistency::NotEstablished,
        };
        assert_eq!(
            fail_incomplete(&verified),
            Err(ArtifactTransitionError::NotIncomplete)
        );
        assert_eq!(
            complete_verification(&verified, true),
            Err(ArtifactTransitionError::NotPendingVerification)
        );

        let failed = Artifact {
            id: ArtifactId::new(),
            state: ArtifactState::Failed,
            capture_consistency: CaptureConsistency::NotEstablished,
        };
        assert_eq!(
            fail_incomplete(&failed),
            Err(ArtifactTransitionError::NotIncomplete)
        );
        assert_eq!(
            complete_verification(&failed, true),
            Err(ArtifactTransitionError::NotPendingVerification)
        );
    }

    #[test]
    fn capture_consistency_is_independent_of_verified_state() {
        let artifact = Artifact {
            id: ArtifactId::new(),
            state: ArtifactState::Verified,
            capture_consistency: CaptureConsistency::NotEstablished,
        };
        // A Verified Artifact may validly remain NotEstablished — setting it
        // never touches `state`.
        let updated = set_capture_consistency(&artifact, CaptureConsistency::NotEstablished);
        assert_eq!(updated.state, ArtifactState::Verified);
        assert_eq!(
            updated.capture_consistency,
            CaptureConsistency::NotEstablished
        );

        let established = set_capture_consistency(&artifact, CaptureConsistency::Established);
        assert_eq!(established.state, ArtifactState::Verified);
        assert_eq!(
            established.capture_consistency,
            CaptureConsistency::Established
        );
    }
}
