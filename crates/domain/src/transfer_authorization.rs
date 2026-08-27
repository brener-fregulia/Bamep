//! Bamep Domain: sender-constrained transfer-authorization primitives
//! (`docs/specifications/m0-data-plane-and-storage-contracts.md` "Transfer
//! authorization"; Issue #38).
//!
//! Pure business logic only, mirroring this crate's existing invariant
//! (`AGENTS.md` "Architecture and dependencies"): no I/O, no PostgreSQL, no
//! process-local caching. Every value that requires a clock, CSPRNG, or
//! process-lifetime identity takes/produces it explicitly; the transient
//! capability store and replay cache are Runtime Services owned by
//! `bamep-server` (`m0-stack-and-boundaries-baseline.md`), not this module.
//!
//! This module implements exactly the M1 data-plane interoperability choice
//! materialized by Issue #35: Ed25519 proof keys, the fixed 137-byte
//! canonical proof transcript, and `SHA-256`-derived capability/thumbprint
//! identities. It is not a generic cryptography framework.

use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::artifact::ArtifactId;
use crate::attempt::AttemptId;
use crate::endpoint::EndpointId;
use crate::transfer::{TransferDirection, TransferId};

// ---------------------------------------------------------------------
// Ephemeral Ed25519 proof key (Agent-generated; only the public half and
// its thumbprint ever reach the Server)
// ---------------------------------------------------------------------

pub const PROOF_PUBLIC_KEY_BYTES: usize = 32;
pub const PROOF_PUBLIC_KEY_WIRE_LEN: usize = 43;

/// The Agent-generated ephemeral Ed25519 public key
/// (`m0-data-plane-and-storage-contracts.md` "Ephemeral proof key"): raw
/// 32-byte value, canonical RFC 4648 base64url without padding on the wire.
/// The private counterpart never reaches this type or crate — it is
/// Agent-local, non-durable state.
#[derive(Clone, Copy, Debug)]
pub struct ProofPublicKey {
    verifying_key: VerifyingKey,
}

#[derive(Debug, thiserror::Error)]
pub enum ProofPublicKeyError {
    #[error("proof public key wire value has the wrong length")]
    WireLength,
    #[error(
        "proof public key wire value contains characters outside the canonical \
         base64url-no-pad alphabet"
    )]
    InvalidCharacters,
    #[error("proof public key wire value failed base64url decoding")]
    InvalidEncoding,
    #[error("proof public key wire value did not decode to exactly 32 bytes")]
    InvalidDecodedLength,
    #[error("proof public key wire value is not the canonical re-encoding of its own bytes")]
    NonCanonical,
    #[error("bytes are not a valid Ed25519 public key")]
    InvalidKey(#[source] ed25519_dalek::SignatureError),
}

impl ProofPublicKey {
    /// Validates `bytes` as an Ed25519 public key according to the
    /// underlying implementation
    /// (`m0-data-plane-and-storage-contracts.md`: "Verification must reject
    /// non-canonical/problematic signature and public-key representations
    /// and weak-key cases where the verifying implementation supports doing
    /// so").
    pub fn from_bytes(bytes: [u8; PROOF_PUBLIC_KEY_BYTES]) -> Result<Self, ProofPublicKeyError> {
        let verifying_key =
            VerifyingKey::from_bytes(&bytes).map_err(ProofPublicKeyError::InvalidKey)?;
        Ok(Self { verifying_key })
    }

    pub fn as_bytes(&self) -> [u8; PROOF_PUBLIC_KEY_BYTES] {
        self.verifying_key.to_bytes()
    }

    pub(crate) fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    pub fn to_wire_value(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.as_bytes())
    }

    /// Strict parsing, mirroring `bamep_trusted_bootstrap::BootNonce`: exact
    /// 43-character length, canonical base64url alphabet only, and a
    /// canonical re-encode-and-compare check before accepting
    /// (`m0-data-plane-and-storage-contracts.md`: "reject padding, the
    /// standard-base64 `+`/`/` alphabet, whitespace, wrong length,
    /// non-canonical trailing bits, or any value that does not round-trip
    /// byte-for-byte through the canonical encoder").
    pub fn parse_wire_value(value: &str) -> Result<Self, ProofPublicKeyError> {
        if value.len() != PROOF_PUBLIC_KEY_WIRE_LEN {
            return Err(ProofPublicKeyError::WireLength);
        }
        if !value.bytes().all(is_canonical_base64url_char) {
            return Err(ProofPublicKeyError::InvalidCharacters);
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| ProofPublicKeyError::InvalidEncoding)?;
        let bytes: [u8; PROOF_PUBLIC_KEY_BYTES] = decoded
            .try_into()
            .map_err(|_| ProofPublicKeyError::InvalidDecodedLength)?;
        let candidate = Self::from_bytes(bytes)?;
        if candidate.to_wire_value() != value {
            return Err(ProofPublicKeyError::NonCanonical);
        }
        Ok(candidate)
    }
}

impl PartialEq for ProofPublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.verifying_key.as_bytes() == other.verifying_key.as_bytes()
    }
}

impl Eq for ProofPublicKey {}

fn is_canonical_base64url_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

// ---------------------------------------------------------------------
// Proof-key thumbprint
// ---------------------------------------------------------------------

pub const PROOF_KEY_THUMBPRINT_BYTES: usize = 32;

/// `SHA-256(raw 32-byte Ed25519 public-key value)`
/// (`m0-data-plane-and-storage-contracts.md` "Ephemeral proof key"
/// "Thumbprint"). Server-internal correlation state; not itself secret
/// (`m1-worker-data-plane-control-contract.md` "Security and logging":
/// "`proof_public_key` and its thumbprint are not themselves secret and may
/// appear in diagnostics").
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub struct ProofKeyThumbprint([u8; PROOF_KEY_THUMBPRINT_BYTES]);

impl ProofKeyThumbprint {
    pub fn from_public_key(key: &ProofPublicKey) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let mut bytes = [0u8; PROOF_KEY_THUMBPRINT_BYTES];
        bytes.copy_from_slice(hasher.finalize().as_slice());
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; PROOF_KEY_THUMBPRINT_BYTES] {
        &self.0
    }
}

// ---------------------------------------------------------------------
// proof_id — fresh, unpredictable, per-request anti-replay identity
// ---------------------------------------------------------------------

pub const PROOF_ID_BYTES: usize = 16;
pub const PROOF_ID_WIRE_LEN: usize = 22;

/// `proof_id` (`m0-data-plane-and-storage-contracts.md` "Freshness and
/// replay representation"): 16 bytes generated fresh per request from the
/// operating-system CSPRNG, canonical base64url-no-pad on the wire (22 ASCII
/// characters).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ProofId([u8; PROOF_ID_BYTES]);

#[derive(Debug, thiserror::Error)]
pub enum ProofIdError {
    #[error("proof_id wire value has the wrong length")]
    WireLength,
    #[error(
        "proof_id wire value contains characters outside the canonical base64url-no-pad alphabet"
    )]
    InvalidCharacters,
    #[error("proof_id wire value failed base64url decoding")]
    InvalidEncoding,
    #[error("proof_id wire value did not decode to exactly 16 bytes")]
    InvalidDecodedLength,
    #[error("proof_id wire value is not the canonical re-encoding of its own bytes")]
    NonCanonical,
}

impl ProofId {
    /// Fresh CSPRNG-generated `proof_id`. This is an Agent-side operation in
    /// production (the Agent constructs each per-request proof); represented
    /// here so Domain-level tests can exercise the exact wire/transcript
    /// shape without a second, test-only implementation.
    pub fn generate() -> Self {
        let mut bytes = [0u8; PROOF_ID_BYTES];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; PROOF_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; PROOF_ID_BYTES] {
        &self.0
    }

    pub fn to_wire_value(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn parse_wire_value(value: &str) -> Result<Self, ProofIdError> {
        if value.len() != PROOF_ID_WIRE_LEN {
            return Err(ProofIdError::WireLength);
        }
        if !value.bytes().all(is_canonical_base64url_char) {
            return Err(ProofIdError::InvalidCharacters);
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| ProofIdError::InvalidEncoding)?;
        let bytes: [u8; PROOF_ID_BYTES] = decoded
            .try_into()
            .map_err(|_| ProofIdError::InvalidDecodedLength)?;
        let candidate = Self(bytes);
        if candidate.to_wire_value() != value {
            return Err(ProofIdError::NonCanonical);
        }
        Ok(candidate)
    }
}

// ---------------------------------------------------------------------
// Per-request Ed25519 signature wire encoding
// ---------------------------------------------------------------------

pub const PROOF_SIGNATURE_BYTES: usize = 64;
pub const PROOF_SIGNATURE_WIRE_LEN: usize = 86;

/// The raw 64-byte Ed25519 signature, canonical RFC 4648 base64url-no-pad on
/// the wire (`m0-data-plane-and-storage-contracts.md` "Signature wire
/// encoding"), under the identical strict-parsing discipline as
/// [`ProofPublicKey`]/[`ProofId`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProofSignature([u8; PROOF_SIGNATURE_BYTES]);

#[derive(Debug, thiserror::Error)]
pub enum ProofSignatureError {
    #[error("proof signature wire value has the wrong length")]
    WireLength,
    #[error(
        "proof signature wire value contains characters outside the canonical \
         base64url-no-pad alphabet"
    )]
    InvalidCharacters,
    #[error("proof signature wire value failed base64url decoding")]
    InvalidEncoding,
    #[error("proof signature wire value did not decode to exactly 64 bytes")]
    InvalidDecodedLength,
    #[error("proof signature wire value is not the canonical re-encoding of its own bytes")]
    NonCanonical,
}

impl ProofSignature {
    pub fn from_bytes(bytes: [u8; PROOF_SIGNATURE_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; PROOF_SIGNATURE_BYTES] {
        &self.0
    }

    pub fn to_wire_value(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn parse_wire_value(value: &str) -> Result<Self, ProofSignatureError> {
        if value.len() != PROOF_SIGNATURE_WIRE_LEN {
            return Err(ProofSignatureError::WireLength);
        }
        if !value.bytes().all(is_canonical_base64url_char) {
            return Err(ProofSignatureError::InvalidCharacters);
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| ProofSignatureError::InvalidEncoding)?;
        let bytes: [u8; PROOF_SIGNATURE_BYTES] = decoded
            .try_into()
            .map_err(|_| ProofSignatureError::InvalidDecodedLength)?;
        let candidate = Self(bytes);
        if candidate.to_wire_value() != value {
            return Err(ProofSignatureError::NonCanonical);
        }
        Ok(candidate)
    }
}

// ---------------------------------------------------------------------
// Capability token / identity
// ---------------------------------------------------------------------

pub const CAPABILITY_TOKEN_SECRET_BYTES: usize = 32;
pub const CAPABILITY_ID_BYTES: usize = 32;

/// The opaque, Server-issued, sender-constrained capability
/// (`m0-data-plane-and-storage-contracts.md` "Capability opacity"): an
/// externally opaque UTF-8 string. This type's internal representation
/// (256 bits of CSPRNG entropy, base64url-encoded) is the implementation-time
/// serialization choice the Specification explicitly leaves open — never
/// derived from an ID, timestamp, hash of public state, or predictable
/// counter. `Debug` is redacted: this is bearer authorization secret
/// material.
#[derive(Clone, PartialEq, Eq)]
pub struct CapabilityToken(String);

impl CapabilityToken {
    /// Mints a fresh capability token from the operating-system CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = [0u8; CAPABILITY_TOKEN_SECRET_BYTES];
        OsRng.fill_bytes(&mut bytes);
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Wraps an already-known wire value — the Agent/Worker-side
    /// reconstruction path, which treats the token as an opaque forwarded
    /// string and never generates one itself.
    pub fn from_wire_value(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CapabilityToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CapabilityToken").field(&"REDACTED").finish()
    }
}

/// The derived, externally computable capability identity: exactly
/// `SHA-256(UTF-8 bytes of the exact token string as currently held)`
/// (`m0-data-plane-and-storage-contracts.md` "Capability opacity"). Used as
/// the lookup/binding key for issued capabilities and as `capability_id` in
/// the canonical proof transcript — never itself transmitted as a separate
/// wire field.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct CapabilityId([u8; CAPABILITY_ID_BYTES]);

impl CapabilityId {
    pub fn from_token(token: &CapabilityToken) -> Self {
        Self::from_token_bytes(token.as_str().as_bytes())
    }

    /// Computes the identity directly from raw UTF-8 token bytes — the exact
    /// operation both the Agent (signing a proof) and `bamepd` (verifying
    /// one, from the token bytes the Worker forwarded) perform independently
    /// from the same wire bytes.
    pub fn from_token_bytes(token_utf8: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(token_utf8);
        let mut bytes = [0u8; CAPABILITY_ID_BYTES];
        bytes.copy_from_slice(hasher.finalize().as_slice());
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; CAPABILITY_ID_BYTES] {
        &self.0
    }
}

// ---------------------------------------------------------------------
// Process-lifetime authorization epoch (Server-restart invalidation)
// ---------------------------------------------------------------------

/// A fresh, random value generated once per `bamepd` process lifetime and
/// bound into every capability this process issues
/// (`m0-data-plane-and-storage-contracts.md` "Server restart": "any
/// pre-restart capability whose replay-protection continuity cannot be
/// guaranteed is invalid ... The concrete invalidation mechanism (e.g.
/// epoch/fresh signing context) is implementation-time"). A capability bound
/// to a different epoch than the currently running process's epoch is
/// invalid, even if its own expiry has not yet passed — see
/// [`capability_is_current`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ProcessAuthorizationEpoch(Uuid);

impl ProcessAuthorizationEpoch {
    /// Generates a fresh epoch — call exactly once per `bamepd` process
    /// lifetime (composition-root startup), never per-request.
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
}

// ---------------------------------------------------------------------
// Authorization operation (closed vocabulary; transcript byte encoding)
// ---------------------------------------------------------------------

/// The closed per-request-proof operation vocabulary
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof"). Distinct
/// from `bamep_worker_protocol::AuthorizationOperation` — that crate carries
/// no Domain dependency and represents this as an opaque wire string;
/// `bamepd`'s Application layer converts between the two exactly once, at
/// the UDS boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationOperation {
    ChunkUpload,
    ResumeDiscovery,
    SealManifest,
}

impl AuthorizationOperation {
    /// The exact 1-byte transcript encoding
    /// (`m0-data-plane-and-storage-contracts.md` "Per-request proof": "1
    /// byte, closed enum: `1 = chunk_upload`, `2 = resume_discovery`, `3 =
    /// seal_manifest`").
    fn transcript_byte(self) -> u8 {
        match self {
            AuthorizationOperation::ChunkUpload => 1,
            AuthorizationOperation::ResumeDiscovery => 2,
            AuthorizationOperation::SealManifest => 3,
        }
    }

    /// `chunk_index_present` is `1` only for `chunk_upload`
    /// (`m0-data-plane-and-storage-contracts.md` "Per-request proof": "`0`
    /// for `resume_discovery` and `seal_manifest`, which are transfer-scoped,
    /// not chunk-scoped").
    pub fn requires_chunk_index(self) -> bool {
        matches!(self, AuthorizationOperation::ChunkUpload)
    }
}

fn direction_transcript_byte(direction: TransferDirection) -> u8 {
    match direction {
        // `2` is reserved/unassigned in V1
        // (`m0-data-plane-and-storage-contracts.md` "Per-request proof").
        TransferDirection::AgentToServer => 1,
    }
}

// ---------------------------------------------------------------------
// Canonical proof transcript (Issue #35's materialized 137-byte shape)
// ---------------------------------------------------------------------

/// The exact 34-byte ASCII domain-separation string
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof").
const PROOF_TRANSCRIPT_DOMAIN: &[u8] = b"bamep.m1.data-plane-transfer.proof";
const PROOF_TRANSCRIPT_SCHEMA_VERSION: u16 = 1;

/// The exact fixed transcript length
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof": "The
/// signed payload is exactly 137 bytes").
pub const PROOF_TRANSCRIPT_LEN: usize = 137;

/// One field of the transcript this Domain reconstructs on the verifying
/// side (the operation/identity fields) and the Agent constructs on the
/// signing side — both parties MUST arrive at byte-identical transcripts
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof").
#[derive(Debug, Clone, Copy)]
pub struct ProofTranscriptFields {
    pub operation: AuthorizationOperation,
    pub transfer_id: TransferId,
    pub artifact_id: ArtifactId,
    pub direction: TransferDirection,
    pub chunk_index: Option<u64>,
    pub proof_id: ProofId,
    pub issued_at_millis: u64,
}

/// Builds the exact byte-for-byte canonical signed transcript
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof"):
///
/// ```text
/// u16be(34) || ASCII(domain) || u16be(1) || capability_id[32] ||
/// operation[1] || transfer_id[16] || artifact_id[16] || direction[1] ||
/// chunk_index_present[1] || chunk_index[8] || proof_id[16] || issued_at[8]
/// ```
///
/// `chunk_index` is encoded as exactly `0` when absent, per contract —
/// `chunk_index_present` alone carries whether it is meaningful.
pub fn build_proof_transcript(
    capability_id: &CapabilityId,
    fields: &ProofTranscriptFields,
) -> [u8; PROOF_TRANSCRIPT_LEN] {
    let mut buf = [0u8; PROOF_TRANSCRIPT_LEN];
    let mut offset = 0usize;

    buf[offset..offset + 2].copy_from_slice(&(PROOF_TRANSCRIPT_DOMAIN.len() as u16).to_be_bytes());
    offset += 2;
    buf[offset..offset + PROOF_TRANSCRIPT_DOMAIN.len()].copy_from_slice(PROOF_TRANSCRIPT_DOMAIN);
    offset += PROOF_TRANSCRIPT_DOMAIN.len();
    buf[offset..offset + 2].copy_from_slice(&PROOF_TRANSCRIPT_SCHEMA_VERSION.to_be_bytes());
    offset += 2;
    buf[offset..offset + CAPABILITY_ID_BYTES].copy_from_slice(capability_id.as_bytes());
    offset += CAPABILITY_ID_BYTES;
    buf[offset] = fields.operation.transcript_byte();
    offset += 1;
    buf[offset..offset + 16].copy_from_slice(fields.transfer_id.0.as_bytes());
    offset += 16;
    buf[offset..offset + 16].copy_from_slice(fields.artifact_id.0.as_bytes());
    offset += 16;
    buf[offset] = direction_transcript_byte(fields.direction);
    offset += 1;
    buf[offset] = u8::from(fields.chunk_index.is_some());
    offset += 1;
    buf[offset..offset + 8].copy_from_slice(&fields.chunk_index.unwrap_or(0).to_be_bytes());
    offset += 8;
    buf[offset..offset + PROOF_ID_BYTES].copy_from_slice(fields.proof_id.as_bytes());
    offset += PROOF_ID_BYTES;
    buf[offset..offset + 8].copy_from_slice(&fields.issued_at_millis.to_be_bytes());
    offset += 8;

    debug_assert_eq!(offset, PROOF_TRANSCRIPT_LEN);
    buf
}

/// Verifies `signature` (raw 64 bytes) over `transcript` under `public_key`,
/// using strict Ed25519 verification — no prehash, no context, rejecting
/// non-canonical/weak-key cases the underlying implementation can detect
/// (`m0-data-plane-and-storage-contracts.md` "Ephemeral proof key"
/// "Algorithm"), mirroring `bamep_trusted_bootstrap`'s identical discipline
/// for the site signing key.
pub fn verify_proof_signature(
    public_key: &ProofPublicKey,
    transcript: &[u8; PROOF_TRANSCRIPT_LEN],
    signature: &ProofSignature,
) -> bool {
    let signature = Signature::from_bytes(signature.as_bytes());
    public_key
        .verifying_key()
        .verify_strict(transcript, &signature)
        .is_ok()
}

// ---------------------------------------------------------------------
// Capability binding and current-state predicates
// ---------------------------------------------------------------------

/// Every semantic binding a durably issued capability carries
/// (`m0-data-plane-and-storage-contracts.md` "Capability bindings"). Held by
/// the Runtime capability store (`bamep_server::runtime`), never persisted to
/// PostgreSQL (`m0-data-plane-and-storage-contracts.md` "Durable versus
/// transient authorization state").
#[derive(Debug, Clone, Copy)]
pub struct CapabilityBinding {
    pub endpoint_id: EndpointId,
    pub transfer_id: TransferId,
    pub artifact_id: ArtifactId,
    pub direction: TransferDirection,
    pub attempt_id: AttemptId,
    /// Retained so a later per-request proof can be verified against the
    /// exact key this capability is sender-constrained to
    /// (`m0-data-plane-and-storage-contracts.md` "Ephemeral proof key": "the
    /// Server binds the granted capability to that key's thumbprint").
    /// [`ProofKeyThumbprint::from_public_key`] derives the normative
    /// thumbprint identity from this on demand — proof-of-possession itself
    /// is established by [`verify_proof_signature`] against this exact key,
    /// never by a separately presented thumbprint value (the per-request
    /// wire contract carries no `proof_public_key` field at all — see
    /// `m1-worker-data-plane-control-contract.md` "Authorization query /
    /// decision").
    pub proof_public_key: ProofPublicKey,
    pub expires_at: DateTime<Utc>,
    pub epoch: ProcessAuthorizationEpoch,
}

/// The presented request's own claimed operation scope, independent of any
/// stored capability — the counterpart [`capability_matches_request`]
/// compares `binding` against.
#[derive(Debug, Clone, Copy)]
pub struct RequestedOperation {
    pub operation: AuthorizationOperation,
    pub transfer_id: TransferId,
    pub artifact_id: ArtifactId,
    pub direction: TransferDirection,
    pub chunk_index: Option<u64>,
}

/// Internal-diagnostics-only denial cause (`m1-worker-data-plane-control-
/// contract.md` "Security and logging": never externally serialized — every
/// external denial surface remains the single generic non-enumerable value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationDenialReason {
    CapabilityNotFound,
    CapabilityExpired,
    CapabilityWrongEpoch,
    TransferMismatch,
    ArtifactMismatch,
    DirectionMismatch,
    OperationChunkIndexRuleViolation,
    SignatureInvalid,
    ProofTooStale,
    ProofTooFarInFuture,
    ProofReplayed,
    TransferNotAttemptBound,
    UnknownAttempt,
    AttemptNotEligible,
    WrongActionCorrelation,
    EndpointMismatch,
    CredentialNotActive,
}

/// Whether `binding` is still valid *as a capability instance*: bound to the
/// currently running process's authorization epoch, and not yet expired
/// (`m0-data-plane-and-storage-contracts.md` "Server restart"). Does not
/// evaluate proof-key/operation/scope matching — see
/// [`capability_matches_request`] — nor current durable Transfer/Attempt/
/// credential state, which the caller composes separately against the
/// PostgreSQL Adapter.
pub fn capability_is_current(
    binding: &CapabilityBinding,
    now: DateTime<Utc>,
    current_epoch: ProcessAuthorizationEpoch,
) -> Result<(), AuthorizationDenialReason> {
    if binding.epoch != current_epoch {
        return Err(AuthorizationDenialReason::CapabilityWrongEpoch);
    }
    if now >= binding.expires_at {
        return Err(AuthorizationDenialReason::CapabilityExpired);
    }
    Ok(())
}

/// Whether `binding` authorizes the requested operation/transfer/artifact/
/// direction/chunk-index scope (`m0-data-plane-and-storage-contracts.md`
/// "Capability bindings": "It authorizes only that tuple and is never a
/// generic data-plane credential"). Proof-key correctness is established
/// separately, by [`verify_proof_signature`] against `binding`'s own bound
/// key — there is no independently presented proof-key value to compare
/// against here (see [`CapabilityBinding::proof_public_key`]'s docs).
pub fn capability_matches_request(
    binding: &CapabilityBinding,
    requested: &RequestedOperation,
) -> Result<(), AuthorizationDenialReason> {
    if binding.transfer_id != requested.transfer_id {
        return Err(AuthorizationDenialReason::TransferMismatch);
    }
    if binding.artifact_id != requested.artifact_id {
        return Err(AuthorizationDenialReason::ArtifactMismatch);
    }
    if binding.direction != requested.direction {
        return Err(AuthorizationDenialReason::DirectionMismatch);
    }
    let chunk_index_rule_ok =
        requested.operation.requires_chunk_index() == requested.chunk_index.is_some();
    if !chunk_index_rule_ok {
        return Err(AuthorizationDenialReason::OperationChunkIndexRuleViolation);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Proof freshness
// ---------------------------------------------------------------------

/// Bounded proof freshness window
/// (`m0-data-plane-and-storage-contracts.md` "Replay and freshness": "Proofs
/// are accepted only inside a bounded freshness window ... Exact freshness
/// duration ... remain[s] implementation-time"). Two minutes tolerates
/// ordinary LAN/clock-skew conditions while remaining short relative to
/// capability TTL; chosen and documented here as the one explicit constant
/// this Work Package selects.
pub const PROOF_FRESHNESS_PAST_WINDOW_MILLIS: i64 = 120_000;
/// Bounded allowance for a proof whose `issued_at` is slightly ahead of this
/// process's own clock (ordinary NTP-scale skew) — never unbounded "must
/// equal now".
pub const PROOF_FRESHNESS_FUTURE_SKEW_MILLIS: i64 = 30_000;

/// Whether `issued_at_millis` falls inside the bounded freshness window
/// around `now`. Uses `i128` arithmetic so no realistic `u64` millisecond
/// timestamp can overflow the comparison.
pub fn proof_is_fresh(
    issued_at_millis: u64,
    now: DateTime<Utc>,
) -> Result<(), AuthorizationDenialReason> {
    let delta_millis = now.timestamp_millis() as i128 - issued_at_millis as i128;
    if delta_millis > PROOF_FRESHNESS_PAST_WINDOW_MILLIS as i128 {
        return Err(AuthorizationDenialReason::ProofTooStale);
    }
    if delta_millis < -(PROOF_FRESHNESS_FUTURE_SKEW_MILLIS as i128) {
        return Err(AuthorizationDenialReason::ProofTooFarInFuture);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Capability TTL
// ---------------------------------------------------------------------

/// Default capability lifetime
/// (`m0-data-plane-and-storage-contracts.md` "Transfer authorization":
/// "short-lived, transfer-scoped, sender-constrained capability"; "exact
/// numeric capability TTL ... remain[s] implementation-time"). Chosen short
/// relative to an expected multi-chunk transfer so renewal
/// (`m0-agent-protocol-contract.md` "Renewal and restart") is exercised in
/// practice, not merely specified.
pub const DEFAULT_CAPABILITY_TTL_MILLIS: i64 = 5 * 60 * 1000;

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair(seed: u8) -> (SigningKey, ProofPublicKey) {
        let signing_key = SigningKey::from_bytes(&[seed; 32]);
        let public = ProofPublicKey::from_bytes(signing_key.verifying_key().to_bytes()).unwrap();
        (signing_key, public)
    }

    fn sample_fields(proof_id: ProofId, issued_at_millis: u64) -> ProofTranscriptFields {
        ProofTranscriptFields {
            operation: AuthorizationOperation::ChunkUpload,
            transfer_id: TransferId(Uuid::new_v4()),
            artifact_id: ArtifactId(Uuid::new_v4()),
            direction: TransferDirection::AgentToServer,
            chunk_index: Some(7),
            proof_id,
            issued_at_millis,
        }
    }

    fn sign_and_build(
        signing_key: &SigningKey,
        capability_id: &CapabilityId,
        fields: &ProofTranscriptFields,
    ) -> ([u8; PROOF_TRANSCRIPT_LEN], ProofSignature) {
        let transcript = build_proof_transcript(capability_id, fields);
        let signature = signing_key.sign(&transcript);
        (transcript, ProofSignature::from_bytes(signature.to_bytes()))
    }

    // -- Proof-key wire encoding -----------------------------------------

    #[test]
    fn proof_public_key_wire_round_trips() {
        let (_signing_key, public) = keypair(1);
        let wire = public.to_wire_value();
        assert_eq!(wire.len(), PROOF_PUBLIC_KEY_WIRE_LEN);
        let parsed = ProofPublicKey::parse_wire_value(&wire).unwrap();
        assert_eq!(parsed, public);
    }

    #[test]
    fn proof_public_key_padding_is_rejected() {
        let (_signing_key, public) = keypair(1);
        let mut wire = public.to_wire_value();
        wire.push('=');
        assert!(matches!(
            ProofPublicKey::parse_wire_value(&wire),
            Err(ProofPublicKeyError::WireLength)
        ));
    }

    #[test]
    fn proof_id_wire_round_trips() {
        let id = ProofId::generate();
        let wire = id.to_wire_value();
        assert_eq!(wire.len(), PROOF_ID_WIRE_LEN);
        assert_eq!(ProofId::parse_wire_value(&wire).unwrap(), id);
    }

    #[test]
    fn proof_signature_wire_round_trips() {
        let signature = ProofSignature::from_bytes([42u8; PROOF_SIGNATURE_BYTES]);
        let wire = signature.to_wire_value();
        assert_eq!(wire.len(), PROOF_SIGNATURE_WIRE_LEN);
        assert_eq!(ProofSignature::parse_wire_value(&wire).unwrap(), signature);
    }

    #[test]
    fn proof_signature_rejects_standard_base64_alphabet() {
        let mut wire: Vec<u8> = vec![b'A'; PROOF_SIGNATURE_WIRE_LEN];
        wire[0] = b'+';
        let wire = String::from_utf8(wire).unwrap();
        assert!(matches!(
            ProofSignature::parse_wire_value(&wire),
            Err(ProofSignatureError::InvalidCharacters)
        ));
    }

    #[test]
    fn two_generated_proof_ids_are_never_equal() {
        assert_ne!(ProofId::generate(), ProofId::generate());
    }

    // -- Capability identity ----------------------------------------------

    #[test]
    fn capability_identity_is_sha256_of_exact_token_bytes() {
        let token = CapabilityToken::from_wire_value("exact-token-bytes");
        let expected = {
            let mut hasher = Sha256::new();
            hasher.update(b"exact-token-bytes");
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(hasher.finalize().as_slice());
            bytes
        };
        assert_eq!(CapabilityId::from_token(&token).as_bytes(), &expected);
    }

    #[test]
    fn two_generated_capability_tokens_are_never_equal() {
        assert_ne!(CapabilityToken::generate(), CapabilityToken::generate());
    }

    #[test]
    fn capability_token_debug_is_redacted() {
        let token = CapabilityToken::from_wire_value("super-secret-value");
        let debug = format!("{token:?}");
        assert!(!debug.contains("super-secret-value"));
        assert!(debug.contains("REDACTED"));
    }

    // -- Canonical transcript exact shape ----------------------------------

    #[test]
    fn transcript_is_exactly_137_bytes_with_the_documented_field_layout() {
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let fields = sample_fields(ProofId::generate(), 1_700_000_000_000);
        let transcript = build_proof_transcript(&capability_id, &fields);
        assert_eq!(transcript.len(), PROOF_TRANSCRIPT_LEN);

        assert_eq!(&transcript[0..2], &34u16.to_be_bytes());
        assert_eq!(&transcript[2..36], PROOF_TRANSCRIPT_DOMAIN);
        assert_eq!(&transcript[36..38], &1u16.to_be_bytes());
        assert_eq!(&transcript[38..70], capability_id.as_bytes());
        assert_eq!(transcript[70], 1); // chunk_upload
        assert_eq!(&transcript[71..87], fields.transfer_id.0.as_bytes());
        assert_eq!(&transcript[87..103], fields.artifact_id.0.as_bytes());
        assert_eq!(transcript[103], 1); // agent_to_server
        assert_eq!(transcript[104], 1); // chunk_index_present
        assert_eq!(&transcript[105..113], &7u64.to_be_bytes());
        assert_eq!(&transcript[113..129], fields.proof_id.as_bytes());
        assert_eq!(&transcript[129..137], &1_700_000_000_000u64.to_be_bytes());
    }

    #[test]
    fn chunk_index_absent_encodes_as_zero_with_present_flag_cleared() {
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let mut fields = sample_fields(ProofId::generate(), 1);
        fields.operation = AuthorizationOperation::ResumeDiscovery;
        fields.chunk_index = None;
        let transcript = build_proof_transcript(&capability_id, &fields);
        assert_eq!(transcript[70], 2); // resume_discovery
        assert_eq!(transcript[104], 0); // chunk_index_present
        assert_eq!(&transcript[105..113], &0u64.to_be_bytes());
    }

    // -- Signature verification ---------------------------------------------

    #[test]
    fn a_valid_ed25519_proof_verifies() {
        let (signing_key, public) = keypair(1);
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let fields = sample_fields(ProofId::generate(), 1_700_000_000_000);
        let (transcript, signature) = sign_and_build(&signing_key, &capability_id, &fields);
        assert!(verify_proof_signature(&public, &transcript, &signature));
    }

    #[test]
    fn a_proof_signed_by_the_wrong_private_key_is_rejected() {
        let (signing_key, _public) = keypair(1);
        let (_other_signing_key, other_public) = keypair(2);
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let fields = sample_fields(ProofId::generate(), 1_700_000_000_000);
        let (transcript, signature) = sign_and_build(&signing_key, &capability_id, &fields);
        assert!(!verify_proof_signature(
            &other_public,
            &transcript,
            &signature
        ));
    }

    #[test]
    fn a_modified_transcript_is_rejected() {
        let (signing_key, public) = keypair(1);
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let fields = sample_fields(ProofId::generate(), 1_700_000_000_000);
        let (mut transcript, signature) = sign_and_build(&signing_key, &capability_id, &fields);
        transcript[70] ^= 0xFF; // flip the operation byte
        assert!(!verify_proof_signature(&public, &transcript, &signature));
    }

    #[test]
    fn a_proof_built_for_a_different_transfer_produces_a_different_transcript() {
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let fields_a = sample_fields(ProofId::generate(), 1);
        let mut fields_b = fields_a;
        fields_b.transfer_id = TransferId(Uuid::new_v4());
        assert_ne!(
            build_proof_transcript(&capability_id, &fields_a),
            build_proof_transcript(&capability_id, &fields_b)
        );
    }

    #[test]
    fn a_proof_built_for_a_different_artifact_produces_a_different_transcript() {
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let fields_a = sample_fields(ProofId::generate(), 1);
        let mut fields_b = fields_a;
        fields_b.artifact_id = ArtifactId(Uuid::new_v4());
        assert_ne!(
            build_proof_transcript(&capability_id, &fields_a),
            build_proof_transcript(&capability_id, &fields_b)
        );
    }

    #[test]
    fn a_proof_built_for_a_different_operation_produces_a_different_transcript() {
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let mut fields_a = sample_fields(ProofId::generate(), 1);
        fields_a.chunk_index = None;
        let mut fields_b = fields_a;
        fields_b.operation = AuthorizationOperation::SealManifest;
        assert_ne!(
            build_proof_transcript(&capability_id, &fields_a),
            build_proof_transcript(&capability_id, &fields_b)
        );
    }

    #[test]
    fn a_proof_built_for_a_different_chunk_index_produces_a_different_transcript() {
        let capability_id = CapabilityId::from_token(&CapabilityToken::generate());
        let fields_a = sample_fields(ProofId::generate(), 1);
        let mut fields_b = fields_a;
        fields_b.chunk_index = Some(8);
        assert_ne!(
            build_proof_transcript(&capability_id, &fields_a),
            build_proof_transcript(&capability_id, &fields_b)
        );
    }

    #[test]
    fn a_proof_bound_to_a_different_capability_produces_a_different_transcript() {
        let capability_id_a = CapabilityId::from_token(&CapabilityToken::generate());
        let capability_id_b = CapabilityId::from_token(&CapabilityToken::generate());
        let fields = sample_fields(ProofId::generate(), 1);
        assert_ne!(
            build_proof_transcript(&capability_id_a, &fields),
            build_proof_transcript(&capability_id_b, &fields)
        );
    }

    // -- Capability binding predicates ---------------------------------------

    fn sample_binding(now: DateTime<Utc>, epoch: ProcessAuthorizationEpoch) -> CapabilityBinding {
        let (_signing_key, public) = keypair(9);
        CapabilityBinding {
            endpoint_id: EndpointId::new(),
            transfer_id: TransferId::new(),
            artifact_id: ArtifactId::new(),
            direction: TransferDirection::AgentToServer,
            attempt_id: AttemptId::new(),
            proof_public_key: public,
            expires_at: now + chrono::Duration::minutes(5),
            epoch,
        }
    }

    #[test]
    fn a_capability_from_the_current_epoch_and_unexpired_is_current() {
        let now = Utc::now();
        let epoch = ProcessAuthorizationEpoch::generate();
        let binding = sample_binding(now, epoch);
        assert_eq!(capability_is_current(&binding, now, epoch), Ok(()));
    }

    #[test]
    fn a_capability_from_an_old_process_epoch_is_rejected_even_though_unexpired() {
        let now = Utc::now();
        let old_epoch = ProcessAuthorizationEpoch::generate();
        let current_epoch = ProcessAuthorizationEpoch::generate();
        let binding = sample_binding(now, old_epoch);
        assert_eq!(
            capability_is_current(&binding, now, current_epoch),
            Err(AuthorizationDenialReason::CapabilityWrongEpoch)
        );
    }

    #[test]
    fn an_expired_capability_is_rejected_even_in_the_current_epoch() {
        let now = Utc::now();
        let epoch = ProcessAuthorizationEpoch::generate();
        let mut binding = sample_binding(now, epoch);
        binding.expires_at = now - chrono::Duration::seconds(1);
        assert_eq!(
            capability_is_current(&binding, now, epoch),
            Err(AuthorizationDenialReason::CapabilityExpired)
        );
    }

    #[test]
    fn matching_request_against_its_own_binding_succeeds() {
        let now = Utc::now();
        let epoch = ProcessAuthorizationEpoch::generate();
        let binding = sample_binding(now, epoch);
        let requested = RequestedOperation {
            operation: AuthorizationOperation::ChunkUpload,
            transfer_id: binding.transfer_id,
            artifact_id: binding.artifact_id,
            direction: binding.direction,
            chunk_index: Some(0),
        };
        assert_eq!(capability_matches_request(&binding, &requested), Ok(()));
    }

    /// The proof-key binding itself is proven by signature verification
    /// against the capability's own bound key, not by a separately presented
    /// thumbprint (`CapabilityBinding::proof_public_key`'s docs) — a proof
    /// signed by an unrelated private key fails at `verify_proof_signature`,
    /// covered by `a_proof_signed_by_the_wrong_private_key_is_rejected`
    /// above. This test only proves `capability_matches_request` does not
    /// itself depend on which key produced `binding`.
    #[test]
    fn capability_matching_is_independent_of_which_key_the_binding_carries() {
        let now = Utc::now();
        let epoch = ProcessAuthorizationEpoch::generate();
        let mut binding = sample_binding(now, epoch);
        let (_signing_key, other_public) = keypair(200);
        binding.proof_public_key = other_public;
        let requested = RequestedOperation {
            operation: AuthorizationOperation::ChunkUpload,
            transfer_id: binding.transfer_id,
            artifact_id: binding.artifact_id,
            direction: binding.direction,
            chunk_index: Some(0),
        };
        assert_eq!(capability_matches_request(&binding, &requested), Ok(()));
    }

    #[test]
    fn a_wrong_transfer_is_rejected() {
        let now = Utc::now();
        let epoch = ProcessAuthorizationEpoch::generate();
        let binding = sample_binding(now, epoch);
        let requested = RequestedOperation {
            operation: AuthorizationOperation::ChunkUpload,
            transfer_id: TransferId::new(),
            artifact_id: binding.artifact_id,
            direction: binding.direction,
            chunk_index: Some(0),
        };
        assert_eq!(
            capability_matches_request(&binding, &requested),
            Err(AuthorizationDenialReason::TransferMismatch)
        );
    }

    #[test]
    fn a_wrong_artifact_is_rejected() {
        let now = Utc::now();
        let epoch = ProcessAuthorizationEpoch::generate();
        let binding = sample_binding(now, epoch);
        let requested = RequestedOperation {
            operation: AuthorizationOperation::ChunkUpload,
            transfer_id: binding.transfer_id,
            artifact_id: ArtifactId::new(),
            direction: binding.direction,
            chunk_index: Some(0),
        };
        assert_eq!(
            capability_matches_request(&binding, &requested),
            Err(AuthorizationDenialReason::ArtifactMismatch)
        );
    }

    #[test]
    fn chunk_upload_without_a_chunk_index_is_rejected() {
        let now = Utc::now();
        let epoch = ProcessAuthorizationEpoch::generate();
        let binding = sample_binding(now, epoch);
        let requested = RequestedOperation {
            operation: AuthorizationOperation::ChunkUpload,
            transfer_id: binding.transfer_id,
            artifact_id: binding.artifact_id,
            direction: binding.direction,
            chunk_index: None,
        };
        assert_eq!(
            capability_matches_request(&binding, &requested),
            Err(AuthorizationDenialReason::OperationChunkIndexRuleViolation)
        );
    }

    #[test]
    fn seal_manifest_with_a_chunk_index_is_rejected() {
        let now = Utc::now();
        let epoch = ProcessAuthorizationEpoch::generate();
        let binding = sample_binding(now, epoch);
        let requested = RequestedOperation {
            operation: AuthorizationOperation::SealManifest,
            transfer_id: binding.transfer_id,
            artifact_id: binding.artifact_id,
            direction: binding.direction,
            chunk_index: Some(0),
        };
        assert_eq!(
            capability_matches_request(&binding, &requested),
            Err(AuthorizationDenialReason::OperationChunkIndexRuleViolation)
        );
    }

    // -- Freshness ------------------------------------------------------------

    #[test]
    fn a_proof_issued_right_now_is_fresh() {
        let now = Utc::now();
        assert_eq!(proof_is_fresh(now.timestamp_millis() as u64, now), Ok(()));
    }

    #[test]
    fn a_proof_just_inside_the_past_bound_is_fresh() {
        let now = Utc::now();
        let issued = now.timestamp_millis() - PROOF_FRESHNESS_PAST_WINDOW_MILLIS + 1;
        assert_eq!(proof_is_fresh(issued as u64, now), Ok(()));
    }

    #[test]
    fn a_proof_just_outside_the_past_bound_is_stale() {
        let now = Utc::now();
        let issued = now.timestamp_millis() - PROOF_FRESHNESS_PAST_WINDOW_MILLIS - 1;
        assert_eq!(
            proof_is_fresh(issued as u64, now),
            Err(AuthorizationDenialReason::ProofTooStale)
        );
    }

    #[test]
    fn a_proof_just_inside_the_future_skew_bound_is_fresh() {
        let now = Utc::now();
        let issued = now.timestamp_millis() + PROOF_FRESHNESS_FUTURE_SKEW_MILLIS - 1;
        assert_eq!(proof_is_fresh(issued as u64, now), Ok(()));
    }

    #[test]
    fn a_proof_just_outside_the_future_skew_bound_is_rejected() {
        let now = Utc::now();
        let issued = now.timestamp_millis() + PROOF_FRESHNESS_FUTURE_SKEW_MILLIS + 1;
        assert_eq!(
            proof_is_fresh(issued as u64, now),
            Err(AuthorizationDenialReason::ProofTooFarInFuture)
        );
    }
}
