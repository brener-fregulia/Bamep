//! Bamep Domain: `ChunkManifest` identity/construction/sealing
//! (`docs/specifications/m0-data-plane-and-storage-contracts.md` "Chunk
//! manifest"; Issue #36).
//!
//! Digest values here are raw algorithm-output bytes. Wire base64url
//! encoding (`m0-data-plane-and-storage-contracts.md` "Chunk manifest": "the
//! raw digest-algorithm output ... encoded as canonical RFC 4648 base64url")
//! is a transport concern owned outside Domain; this module never claims a
//! wire-encoded string as its canonical internal identity.

use std::collections::BTreeMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactId;

/// M1's single interoperability digest algorithm
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005:
/// `"digest_algorithm": "sha256"`). A closed Domain enumeration, not a
/// generic pluggable crypto abstraction — this is an M1 interoperability
/// choice, not permanently fixed Bamep architecture
/// (`m0-data-plane-and-storage-contracts.md` "Out of scope").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DigestAlgorithm {
    Sha256,
}

impl DigestAlgorithm {
    /// Raw digest-output length in bytes for this algorithm (32 for
    /// SHA-256; `m0-data-plane-and-storage-contracts.md` "Chunk manifest").
    pub fn digest_len(self) -> usize {
        match self {
            DigestAlgorithm::Sha256 => 32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("digest has length {actual}, expected {expected} for this algorithm")]
pub struct InvalidDigestLength {
    pub expected: usize,
    pub actual: usize,
}

/// A raw digest-algorithm output value — never the wire base64url string
/// (module docs above).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest(Vec<u8>);

impl Digest {
    pub fn new(algorithm: DigestAlgorithm, bytes: Vec<u8>) -> Result<Self, InvalidDigestLength> {
        let expected = algorithm.digest_len();
        if bytes.len() != expected {
            return Err(InvalidDigestLength {
                expected,
                actual: bytes.len(),
            });
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// The canonical wire encoding of this digest: the raw digest-algorithm
    /// output as canonical RFC 4648 base64url without padding (43 ASCII
    /// characters for a 32-byte SHA-256 digest) —
    /// `m0-data-plane-and-storage-contracts.md` "Chunk manifest": "Every
    /// digest value on the wire ... is the raw digest-algorithm output ...
    /// encoded as canonical RFC 4648 base64url without padding". Used for the
    /// `expected_chunk_digest` the Worker UDS `AuthorizationDecision` carries
    /// (Issue #38).
    pub fn to_wire_value(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.0)
    }

    /// Strict inverse of [`Self::to_wire_value`], mirroring
    /// `bamep_domain::ProofId::parse_wire_value`'s discipline
    /// (`m0-data-plane-and-storage-contracts.md` "Chunk manifest": reject
    /// padding, the standard-base64 `+`/`/` alphabet, whitespace, wrong
    /// length, non-canonical trailing bits, or any value that does not
    /// round-trip byte-for-byte through the canonical encoder). Used to
    /// validate the `digest` a Worker `ChunkAcceptanceRequest` reports
    /// (Issue #39 Phase C1) — `bamepd` never recomputes the digest (it holds
    /// no bytes), it validates the reported integrity identity.
    pub fn parse_wire_value(
        algorithm: DigestAlgorithm,
        value: &str,
    ) -> Result<Self, DigestParseError> {
        let decoded = URL_SAFE_NO_PAD
            .decode(value.as_bytes())
            .map_err(|_| DigestParseError::InvalidEncoding)?;
        let digest = Self::new(algorithm, decoded).map_err(DigestParseError::Length)?;
        if digest.to_wire_value() != value {
            return Err(DigestParseError::NonCanonical);
        }
        Ok(digest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DigestParseError {
    #[error("digest wire value is not valid base64url-no-pad")]
    InvalidEncoding,
    #[error(transparent)]
    Length(#[from] InvalidDigestLength),
    #[error("digest wire value is not the canonical re-encoding of its own bytes")]
    NonCanonical,
}

/// A fixed, positive chunk size for one manifest, immutable once the
/// manifest exists (`m0-data-plane-and-storage-contracts.md` "Chunk
/// manifest"; M1 RF-005: "`chunk_size` ... positive ... fixed for this
/// Transfer's manifest"). This Domain invariant is `chunk_size > 0` — no
/// universal project-wide chunk-size value is encoded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkSize(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("chunk size must be positive")]
pub struct InvalidChunkSize;

impl ChunkSize {
    pub fn new(bytes: u32) -> Result<Self, InvalidChunkSize> {
        if bytes == 0 {
            return Err(InvalidChunkSize);
        }
        Ok(Self(bytes))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// A 0-based chunk index, assigned sequentially by the producing
/// participant (`m0-data-plane-and-storage-contracts.md` "Chunk manifest").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChunkIndex(pub u32);

/// One expected chunk identity: index, expected size, and expected digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedChunk {
    pub index: ChunkIndex,
    pub size: u32,
    pub digest: Digest,
}

/// One Artifact's chunk manifest (`m0-data-plane-and-storage-contracts.md`
/// "Chunk manifest"). Owns identity/construction/sealing only. Durable
/// held/verified per-chunk state is a distinct fact tracked alongside this
/// manifest at the persistence boundary (Issue #36 "Held / verified chunk
/// state"): an expected identity existing here is not the same fact as its
/// matching bytes having been durably accepted — see
/// [`crate::artifact::begin_verification`] and [`validate_verified_chunk`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkManifest {
    pub artifact_id: ArtifactId,
    pub digest_algorithm: DigestAlgorithm,
    pub chunk_size: ChunkSize,
    chunks: BTreeMap<u32, ExpectedChunk>,
    pub sealed: bool,
    pub chunk_count: Option<u32>,
    pub artifact_digest: Option<Digest>,
}

impl ChunkManifest {
    /// A fresh, empty, unsealed manifest for `artifact_id`
    /// (`m0-data-plane-and-storage-contracts.md` "Construction and
    /// sealing").
    pub fn new(
        artifact_id: ArtifactId,
        digest_algorithm: DigestAlgorithm,
        chunk_size: ChunkSize,
    ) -> Self {
        Self {
            artifact_id,
            digest_algorithm,
            chunk_size,
            chunks: BTreeMap::new(),
            sealed: false,
            chunk_count: None,
            artifact_digest: None,
        }
    }

    pub fn chunk_indices(&self) -> impl Iterator<Item = ChunkIndex> + '_ {
        self.chunks.keys().copied().map(ChunkIndex)
    }

    pub fn expected_chunk(&self, index: ChunkIndex) -> Option<&ExpectedChunk> {
        self.chunks.get(&index.0)
    }

    pub fn recorded_chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

/// The result of a legal [`ChunkManifest::record_expected_chunk`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkRecordOutcome {
    /// A genuinely new expected chunk identity was added to the unsealed
    /// manifest (`m0-data-plane-and-storage-contracts.md` "Capture
    /// continuation and transfer resume are distinct": "continuation may
    /// add a new chunk identity to an unsealed manifest").
    Added(ChunkManifest),
    /// The exact same `(index, size, digest)` was already recorded —
    /// idempotent, no mutation (resume/retransmission of an already-known
    /// identity).
    AlreadyRecorded,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChunkRecordError {
    #[error("chunk index {0:?} is already recorded with a different size or digest")]
    Conflict(ChunkIndex),
    #[error("manifest is sealed and this chunk index was never part of the sealed set")]
    NotContinuable,
    #[error(transparent)]
    InvalidDigestLength(#[from] InvalidDigestLength),
}

impl ChunkManifest {
    /// Records one expected chunk identity for `index`
    /// (`m0-data-plane-and-storage-contracts.md` "Chunk manifest",
    /// "Construction and sealing"). A recorded digest is never rewritten to
    /// accept different bytes: an already-recorded index with a different
    /// `size`/`digest` is rejected, never overwritten
    /// ([`ChunkRecordError::Conflict`]). Once sealed, only an index already
    /// part of the sealed set may still be recorded, and only with its
    /// exact already-recorded `size`/`digest` (idempotent resume
    /// resubmission) — a genuinely new index after sealing is
    /// [`ChunkRecordError::NotContinuable`], preserving "resume/
    /// retransmission must satisfy an already recorded chunk identity"
    /// (`m0-data-plane-and-storage-contracts.md` "Capture continuation and
    /// transfer resume are distinct").
    pub fn record_expected_chunk(
        &self,
        index: ChunkIndex,
        size: u32,
        digest_bytes: Vec<u8>,
    ) -> Result<ChunkRecordOutcome, ChunkRecordError> {
        let digest = Digest::new(self.digest_algorithm, digest_bytes)?;

        if let Some(existing) = self.chunks.get(&index.0) {
            return if existing.size == size && existing.digest == digest {
                Ok(ChunkRecordOutcome::AlreadyRecorded)
            } else {
                Err(ChunkRecordError::Conflict(index))
            };
        }

        if self.sealed {
            return Err(ChunkRecordError::NotContinuable);
        }

        let mut chunks = self.chunks.clone();
        chunks.insert(
            index.0,
            ExpectedChunk {
                index,
                size,
                digest,
            },
        );
        Ok(ChunkRecordOutcome::Added(ChunkManifest {
            chunks,
            ..self.clone()
        }))
    }
}

/// The result of a legal [`ChunkManifest::seal`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealOutcome {
    Sealed(ChunkManifest),
    /// Retried with the exact same already-sealed `chunk_count`/
    /// `artifact_digest` — idempotent, no mutation
    /// (`m0-data-plane-and-storage-contracts.md` "HTTPS data-plane v1
    /// contract": "A retry with the *same* already-sealed `chunk_count`/
    /// `artifact_digest` is idempotent success").
    AlreadySealed,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    #[error("declared chunk_count does not match the number of recorded expected chunks")]
    IncompleteChunkSet,
    #[error("recorded chunk indices are not the contiguous range 0..chunk_count")]
    NonContiguousIndices,
    #[error("manifest is already sealed with a different chunk_count or artifact_digest")]
    ConflictingReseal,
    #[error(transparent)]
    InvalidDigestLength(#[from] InvalidDigestLength),
}

impl ChunkManifest {
    /// Seals the manifest: `digest_algorithm`, `chunk_size`, `chunk_count`,
    /// the complete chunk-identity set, and `artifact_digest` become
    /// immutable (`m0-data-plane-and-storage-contracts.md` "Construction and
    /// sealing"). Sealing twice with identical facts is idempotent; a
    /// conflicting reseal is rejected.
    pub fn seal(
        &self,
        chunk_count: u32,
        artifact_digest_bytes: Vec<u8>,
    ) -> Result<SealOutcome, SealError> {
        let artifact_digest = Digest::new(self.digest_algorithm, artifact_digest_bytes)?;

        if self.sealed {
            return if self.chunk_count == Some(chunk_count)
                && self.artifact_digest.as_ref() == Some(&artifact_digest)
            {
                Ok(SealOutcome::AlreadySealed)
            } else {
                Err(SealError::ConflictingReseal)
            };
        }

        if self.chunks.len() != chunk_count as usize {
            return Err(SealError::IncompleteChunkSet);
        }
        let contiguous = (0..chunk_count).all(|i| self.chunks.contains_key(&i));
        if !contiguous {
            return Err(SealError::NonContiguousIndices);
        }

        Ok(SealOutcome::Sealed(ChunkManifest {
            sealed: true,
            chunk_count: Some(chunk_count),
            artifact_digest: Some(artifact_digest),
            ..self.clone()
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChunkAcceptError {
    #[error("chunk index {0:?} has no recorded expected identity")]
    UnknownChunkIndex(ChunkIndex),
    #[error("verified digest does not match the recorded expected identity for {0:?}")]
    DigestMismatch(ChunkIndex),
}

/// Validates that `verified_digest_bytes` matches the already-recorded
/// expected identity for `index` (`m0-data-plane-and-storage-contracts.md`
/// "Chunk transfer and resumability": "A received chunk is accepted only if
/// its digest matches the manifest"). This function does not itself compute
/// or verify byte-level digests — the caller has already independently
/// verified the bytes (a future Worker's responsibility, #39); it only
/// re-checks that result against the durable expected identity before the
/// caller may durably mark the chunk held. Invalid bytes never become valid
/// held state: a caller must not mark a chunk held when this returns `Err`.
pub fn validate_verified_chunk(
    manifest: &ChunkManifest,
    index: ChunkIndex,
    verified_digest_bytes: &[u8],
) -> Result<(), ChunkAcceptError> {
    let expected = manifest
        .expected_chunk(index)
        .ok_or(ChunkAcceptError::UnknownChunkIndex(index))?;
    if expected.digest.as_bytes() != verified_digest_bytes {
        return Err(ChunkAcceptError::DigestMismatch(index));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact_id() -> ArtifactId {
        ArtifactId::new()
    }

    fn manifest() -> ChunkManifest {
        ChunkManifest::new(
            artifact_id(),
            DigestAlgorithm::Sha256,
            ChunkSize::new(4096).unwrap(),
        )
    }

    fn digest32(byte: u8) -> Vec<u8> {
        vec![byte; 32]
    }

    #[test]
    fn zero_chunk_size_is_rejected() {
        assert_eq!(ChunkSize::new(0), Err(InvalidChunkSize));
    }

    #[test]
    fn positive_chunk_size_is_accepted() {
        assert_eq!(ChunkSize::new(1).unwrap().get(), 1);
    }

    #[test]
    fn digest_wrong_length_is_rejected() {
        assert_eq!(
            Digest::new(DigestAlgorithm::Sha256, vec![0u8; 31]),
            Err(InvalidDigestLength {
                expected: 32,
                actual: 31,
            })
        );
    }

    #[test]
    fn new_manifest_is_unsealed_and_empty() {
        let m = manifest();
        assert!(!m.sealed);
        assert_eq!(m.recorded_chunk_count(), 0);
        assert_eq!(m.chunk_count, None);
        assert_eq!(m.artifact_digest, None);
    }

    #[test]
    fn recording_a_new_chunk_on_unsealed_manifest_is_continuation() {
        let m = manifest();
        let outcome = m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap();
        match outcome {
            ChunkRecordOutcome::Added(m2) => {
                assert_eq!(m2.recorded_chunk_count(), 1);
                assert_eq!(m2.expected_chunk(ChunkIndex(0)).unwrap().size, 100);
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn recording_the_identical_chunk_twice_is_idempotent() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };

        let outcome = m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap();
        assert_eq!(outcome, ChunkRecordOutcome::AlreadyRecorded);
    }

    #[test]
    fn conflicting_digest_at_the_same_index_is_rejected_never_rewritten() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };

        // Source mutation producing different bytes at the same index must
        // never rewrite the expected identity.
        let err = m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(2))
            .unwrap_err();
        assert_eq!(err, ChunkRecordError::Conflict(ChunkIndex(0)));
        assert_eq!(
            m.expected_chunk(ChunkIndex(0)).unwrap().digest.as_bytes(),
            digest32(1).as_slice(),
            "the original expected digest must remain exactly as first recorded"
        );
    }

    #[test]
    fn conflicting_size_at_the_same_index_is_rejected() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };

        let err = m
            .record_expected_chunk(ChunkIndex(0), 200, digest32(1))
            .unwrap_err();
        assert_eq!(err, ChunkRecordError::Conflict(ChunkIndex(0)));
    }

    #[test]
    fn a_new_index_cannot_be_added_after_sealing() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };
        let sealed = match m.seal(1, digest32(9)).unwrap() {
            SealOutcome::Sealed(m) => m,
            _ => unreachable!(),
        };

        let err = sealed
            .record_expected_chunk(ChunkIndex(1), 50, digest32(3))
            .unwrap_err();
        assert_eq!(err, ChunkRecordError::NotContinuable);
    }

    #[test]
    fn an_already_sealed_index_may_be_idempotently_resubmitted() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };
        let sealed = match m.seal(1, digest32(9)).unwrap() {
            SealOutcome::Sealed(m) => m,
            _ => unreachable!(),
        };

        let outcome = sealed
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap();
        assert_eq!(outcome, ChunkRecordOutcome::AlreadyRecorded);
    }

    #[test]
    fn sealing_with_incomplete_chunk_set_is_rejected() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };

        assert_eq!(
            m.seal(2, digest32(9)).unwrap_err(),
            SealError::IncompleteChunkSet
        );
    }

    #[test]
    fn sealing_with_non_contiguous_indices_is_rejected() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };
        let m = match m
            .record_expected_chunk(ChunkIndex(2), 100, digest32(2))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };

        assert_eq!(
            m.seal(2, digest32(9)).unwrap_err(),
            SealError::NonContiguousIndices
        );
    }

    #[test]
    fn resealing_with_identical_facts_is_idempotent() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };
        let sealed = match m.seal(1, digest32(9)).unwrap() {
            SealOutcome::Sealed(m) => m,
            _ => unreachable!(),
        };

        assert_eq!(
            sealed.seal(1, digest32(9)).unwrap(),
            SealOutcome::AlreadySealed
        );
    }

    #[test]
    fn resealing_with_conflicting_facts_is_rejected() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };
        let sealed = match m.seal(1, digest32(9)).unwrap() {
            SealOutcome::Sealed(m) => m,
            _ => unreachable!(),
        };

        assert_eq!(
            sealed.seal(1, digest32(8)).unwrap_err(),
            SealError::ConflictingReseal
        );
        // Sealed facts must remain exactly as first sealed.
        assert_eq!(
            sealed.artifact_digest.unwrap().as_bytes(),
            digest32(9).as_slice()
        );
    }

    #[test]
    fn sealed_manifest_immutability_is_structural_not_just_behavioral() {
        // `seal` never mutates in place — it returns a new value — so the
        // original unsealed manifest a caller still holds is unaffected.
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };
        let _sealed = match m.clone().seal(1, digest32(9)).unwrap() {
            SealOutcome::Sealed(m) => m,
            _ => unreachable!(),
        };
        assert!(
            !m.sealed,
            "the original manifest value must remain unsealed"
        );
    }

    #[test]
    fn digest_parse_wire_value_round_trips_and_rejects_non_canonical() {
        let digest = Digest::new(DigestAlgorithm::Sha256, digest32(7)).unwrap();
        let wire = digest.to_wire_value();
        assert_eq!(wire.len(), 43);
        assert_eq!(
            Digest::parse_wire_value(DigestAlgorithm::Sha256, &wire).unwrap(),
            digest
        );

        // padding
        assert_eq!(
            Digest::parse_wire_value(DigestAlgorithm::Sha256, &format!("{wire}=")),
            Err(DigestParseError::InvalidEncoding)
        );
        // standard-base64 alphabet (`+`/`/` are not canonical base64url)
        assert_eq!(
            Digest::parse_wire_value(DigestAlgorithm::Sha256, &format!("/{}", &wire[1..])),
            Err(DigestParseError::InvalidEncoding)
        );
        assert_eq!(
            Digest::parse_wire_value(DigestAlgorithm::Sha256, &format!("+{}", &wire[1..])),
            Err(DigestParseError::InvalidEncoding)
        );
        // wrong length (31 bytes)
        let short = URL_SAFE_NO_PAD.encode([0u8; 31]);
        assert!(matches!(
            Digest::parse_wire_value(DigestAlgorithm::Sha256, &short),
            Err(DigestParseError::Length(_))
        ));
        // non-canonical trailing bits: 32 zero bytes canonically end in "AAA";
        // "AAB" decodes to the same 32 bytes but is not the canonical form.
        let canonical_zero = URL_SAFE_NO_PAD.encode([0u8; 32]);
        let non_canonical = format!("{}B", &canonical_zero[..canonical_zero.len() - 1]);
        assert!(
            matches!(
                Digest::parse_wire_value(DigestAlgorithm::Sha256, &non_canonical),
                Err(DigestParseError::NonCanonical) | Err(DigestParseError::InvalidEncoding)
            ),
            "non-canonical trailing bits must be rejected"
        );
        // whitespace
        assert_eq!(
            Digest::parse_wire_value(DigestAlgorithm::Sha256, &format!(" {wire}")),
            Err(DigestParseError::InvalidEncoding)
        );
    }

    #[test]
    fn validate_verified_chunk_requires_matching_expected_digest() {
        let m = manifest();
        let m = match m
            .record_expected_chunk(ChunkIndex(0), 100, digest32(1))
            .unwrap()
        {
            ChunkRecordOutcome::Added(m) => m,
            _ => unreachable!(),
        };

        assert_eq!(
            validate_verified_chunk(&m, ChunkIndex(0), &digest32(1)),
            Ok(())
        );
        assert_eq!(
            validate_verified_chunk(&m, ChunkIndex(0), &digest32(2)),
            Err(ChunkAcceptError::DigestMismatch(ChunkIndex(0)))
        );
        assert_eq!(
            validate_verified_chunk(&m, ChunkIndex(5), &digest32(1)),
            Err(ChunkAcceptError::UnknownChunkIndex(ChunkIndex(5)))
        );
    }
}
