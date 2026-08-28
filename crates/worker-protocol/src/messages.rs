//! Worker IPC **Protocol v1** message shapes
//! (`docs/specifications/m1-worker-data-plane-control-contract.md`
//! "Minimum messages"). This module materializes the *complete* authoritative
//! v1 catalog:
//!
//! - handshake / errors: `WorkerHello`, `ServerHello`, `HandshakeRejected`,
//!   `ProtocolError`;
//! - chunk-upload authorization: `AuthorizationQuery` / `AuthorizationDecision`;
//! - verified-chunk durable acceptance: `ChunkAcceptanceRequest` /
//!   `ChunkAcceptanceDecision`;
//! - resume discovery + pagination: `ResumeDiscoveryQuery` /
//!   `ResumeDiscoveryPage` / `ResumeDiscoveryContinue`;
//! - seal first durable commit: `ManifestSealRequest` / `ManifestSealDecision`;
//! - full-Artifact verification: `ArtifactVerificationReport` /
//!   `ArtifactVerificationAck`.
//!
//! `protocol_version` stays `"1"`: the earlier partial #37/#38 rendering
//! (handshake + a single `AuthorizationQuery`/`AuthorizationDecision` pair
//! that also carried `operation`/`artifact_id`/`direction`) was an incomplete
//! in-progress form of this same first protocol, never a released baseline,
//! so completing it to the authoritative catalog is not a version increment
//! (`m1-worker-data-plane-control-contract.md` "Compatibility and unknown
//! fields" / "Freeze point for v1").
//!
//! The normative Markdown Specification remains authoritative over this crate
//! (ADR-0003); this crate is only its Rust wire representation and carries no
//! Domain/Server/PostgreSQL/HTTP/storage dependency.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::envelope::{is_uuid_v4, Envelope, ProtocolVersion};

/// Redaction placeholder used by the manual `Debug` impls for every secret or
/// non-diagnostic field (`m1-worker-data-plane-control-contract.md`
/// "Security and logging").
const REDACTED: &str = "REDACTED";

// ---------------------------------------------------------------------
// WorkerHello — Worker -> bamepd
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHelloBody {
    pub worker_protocol_version: ProtocolVersion,
    pub worker_instance_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHelloMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: WorkerHelloBody,
}

impl WorkerHelloMessage {
    /// `worker_instance_id` is the Worker process's own fresh-per-process-
    /// start UUID v4, stable across reconnects within that process lifetime
    /// (`m1-worker-data-plane-control-contract.md` "Handshake").
    pub fn new(worker_instance_id: Uuid) -> Self {
        Self {
            envelope: Envelope::new(),
            body: WorkerHelloBody {
                worker_protocol_version: ProtocolVersion::v1(),
                worker_instance_id,
            },
        }
    }

    /// Whether every normative field this received `WorkerHello` must carry
    /// is actually present and well-formed
    /// (`m1-worker-data-plane-control-contract.md` "Handshake"): envelope
    /// `protocol_version`/`message_id`, plus `worker_protocol_version` and
    /// `worker_instance_id` (UUID v4). `bamepd` must call this — and refuse
    /// `begin_generation` on failure — before trusting a received
    /// `WorkerHello`, never relying only on the peer's own constructor
    /// having produced valid values.
    pub fn is_valid(&self) -> bool {
        self.envelope.is_valid()
            && self.body.worker_protocol_version.is_v1()
            && is_uuid_v4(&self.body.worker_instance_id)
    }
}

// ---------------------------------------------------------------------
// ServerHello — bamepd -> Worker
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHelloBody {
    pub in_reply_to: Uuid,
    pub server_protocol_version: ProtocolVersion,
    pub compatible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHelloMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ServerHelloBody,
}

impl ServerHelloMessage {
    /// `compatible: true` — `bamepd` sends this only once it has already
    /// decided the peer's `worker_protocol_version` is supported; an
    /// unsupported version gets [`HandshakeRejectedMessage`] instead, never
    /// this message with `compatible: false`
    /// (`m1-worker-data-plane-control-contract.md` "Handshake": "`compatible`
    /// is `true` only when `bamepd` supports `worker_protocol_version`").
    pub fn new(in_reply_to: Uuid) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ServerHelloBody {
                in_reply_to,
                server_protocol_version: ProtocolVersion::v1(),
                compatible: true,
            },
        }
    }

    /// Whether every normative field this received `ServerHello` must carry
    /// is actually present, well-formed, and correlates to the `WorkerHello`
    /// this Worker sent (`m1-worker-data-plane-control-contract.md`
    /// "Handshake"): envelope `protocol_version`/`message_id`,
    /// `server_protocol_version == "1"`, `compatible == true`, and
    /// `in_reply_to` matching `sent_hello_id`. Worker must call this — and
    /// never enter `Ready` on failure — before trusting a received
    /// `ServerHello`.
    pub fn is_valid_reply_to(&self, sent_hello_id: Uuid) -> bool {
        self.envelope.is_valid()
            && self.body.server_protocol_version.is_v1()
            && self.body.compatible
            && self.body.in_reply_to == sent_hello_id
    }
}

// ---------------------------------------------------------------------
// HandshakeRejected — bamepd -> Worker
// ---------------------------------------------------------------------

/// Closed handshake-rejection reason vocabulary
/// (`m1-worker-data-plane-control-contract.md` "Handshake": "uses one closed
/// generic value (`\"incompatible_version\"`)").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeRejectionReason {
    #[serde(rename = "incompatible_version")]
    IncompatibleVersion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRejectedBody {
    pub in_reply_to: Uuid,
    pub reason: HandshakeRejectionReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRejectedMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: HandshakeRejectedBody,
}

impl HandshakeRejectedMessage {
    pub fn incompatible_version(in_reply_to: Uuid) -> Self {
        Self {
            envelope: Envelope::new(),
            body: HandshakeRejectedBody {
                in_reply_to,
                reason: HandshakeRejectionReason::IncompatibleVersion,
            },
        }
    }

    /// Whether every normative field this received `HandshakeRejected` must
    /// carry is actually present, well-formed, and correlates to the
    /// `WorkerHello` this Worker sent
    /// (`m1-worker-data-plane-control-contract.md` "Handshake"): envelope
    /// `protocol_version`/`message_id` plus `in_reply_to` matching
    /// `sent_hello_id`. `reason` is already a closed wire vocabulary
    /// enforced at decode time by `serde` (an unrecognized value fails
    /// deserialization), so no further check is needed here.
    pub fn is_valid_reply_to(&self, sent_hello_id: Uuid) -> bool {
        self.envelope.is_valid() && self.body.in_reply_to == sent_hello_id
    }
}

// ---------------------------------------------------------------------
// Shared wire vocabulary
// ---------------------------------------------------------------------

/// M1's single interoperability digest algorithm, as it appears on this
/// boundary (`m1-worker-data-plane-control-contract.md` "Chunk-upload
/// authorization": `digest_algorithm` "e.g. `\"sha256\"`"). A closed wire
/// enum: an unrecognized value fails deserialization rather than being
/// silently accepted, so the Worker fails closed on an algorithm it does not
/// implement rather than defaulting to `sha256`
/// (`m0-data-plane-and-storage-contracts.md` "Chunk manifest"). This crate
/// carries no Domain dependency, so it mirrors `bamep_domain::DigestAlgorithm`
/// as an independent wire type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireDigestAlgorithm {
    #[serde(rename = "sha256")]
    Sha256,
}

/// The authoritative committed Artifact integrity status
/// (`m1-worker-data-plane-control-contract.md` "Full-Artifact verification
/// result"). Closed wire enum; the Worker renders the value into the HTTP
/// seal response and never establishes it by assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireArtifactStatus {
    #[serde(rename = "Verified")]
    Verified,
    #[serde(rename = "Failed")]
    Failed,
}

// ---------------------------------------------------------------------
// AuthorizationQuery / AuthorizationDecision — chunk-upload authorization
// (`m1-worker-data-plane-control-contract.md` "Minimum messages" #1)
// ---------------------------------------------------------------------

/// `AuthorizationQuery{token, transfer_id, chunk_index, proof_id, issued_at,
/// signature}` — the pre-body authorization for
/// `PUT .../chunks/{chunk_index}` (`m1-worker-data-plane-control-contract.md`
/// "Chunk-upload authorization").
///
/// The operation is *implied* by the message type (`chunk_upload`) and never
/// carried as a field; `artifact_id` and `direction` are reconstructed by
/// `bamepd` from the capability binding and are **never** on this wire
/// (contract "Operations, HTTP mapping, and transcript inputs"). The signed
/// 137-byte proof transcript is unchanged — the Agent still signs
/// `artifact_id`/`direction` into it, and `bamepd` reconstructs them from the
/// capability binding it controls.
///
/// Every remaining field is opaque to this crate: `token`, `proof_id`, and
/// `signature` are forwarded verbatim exactly as the Worker received them
/// from the HTTPS request; `issued_at` is the Unix-epoch-millisecond integer
/// carried alongside the proof.
#[derive(Clone, Serialize, Deserialize)]
pub struct AuthorizationQueryBody {
    pub token: String,
    pub transfer_id: Uuid,
    pub chunk_index: u64,
    pub proof_id: String,
    pub issued_at: u64,
    pub signature: String,
}

impl fmt::Debug for AuthorizationQueryBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `token`, `proof_id`/`issued_at`/`signature` proof material MUST be
        // redacted (`m1-worker-data-plane-control-contract.md` "Security and
        // logging"). `transfer_id`/`chunk_index` are integrity/correlation
        // identities, not secrets, and remain visible for diagnostics.
        f.debug_struct("AuthorizationQueryBody")
            .field("token", &REDACTED)
            .field("transfer_id", &self.transfer_id)
            .field("chunk_index", &self.chunk_index)
            .field("proof_id", &REDACTED)
            .field("issued_at", &REDACTED)
            .field("signature", &REDACTED)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthorizationQueryMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: AuthorizationQueryBody,
}

impl fmt::Debug for AuthorizationQueryMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationQueryMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl AuthorizationQueryMessage {
    pub fn new(
        token: impl Into<String>,
        transfer_id: Uuid,
        chunk_index: u64,
        proof_id: impl Into<String>,
        issued_at: u64,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: AuthorizationQueryBody {
                token: token.into(),
                transfer_id,
                chunk_index,
                proof_id: proof_id.into(),
                issued_at,
                signature: signature.into(),
            },
        }
    }
}

/// `approved` | `denied` (`m1-worker-data-plane-control-contract.md`
/// "Chunk-upload authorization"). `denied` deliberately never carries a
/// reason field (non-enumerable denial).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorizationDecisionOutcome {
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "denied")]
    Denied,
}

/// `AuthorizationDecision{decision, digest_algorithm?, chunk_size?,
/// acceptance_handle?, expected_chunk_digest?}`
/// (`m1-worker-data-plane-control-contract.md` "Chunk-upload authorization").
///
/// On `approved`, `digest_algorithm`, `chunk_size`, and `acceptance_handle`
/// are all present and authoritative — the Worker MUST use `digest_algorithm`
/// and `chunk_size` from here, never a local constant. `expected_chunk_digest`
/// is additionally present only when `chunk_index` is already durable. On
/// `denied`, the message carries `decision` only — no reason, no
/// `digest_algorithm`/`chunk_size`/`acceptance_handle`/`expected_chunk_digest`.
#[derive(Clone, Serialize, Deserialize)]
pub struct AuthorizationDecisionBody {
    pub in_reply_to: Uuid,
    pub decision: AuthorizationDecisionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_algorithm: Option<WireDigestAlgorithm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u32>,
    /// Transient, generation-scoped; opaque to the Worker, echoed back on the
    /// matching `ChunkAcceptanceRequest`. Redacted from `Debug`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_handle: Option<String>,
    /// Present only when `decision: approved` and `chunk_index` is already
    /// durable — the already-recorded expected digest, canonical
    /// base64url-no-pad. An integrity identity, not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_chunk_digest: Option<String>,
}

impl fmt::Debug for AuthorizationDecisionBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationDecisionBody")
            .field("in_reply_to", &self.in_reply_to)
            .field("decision", &self.decision)
            .field("digest_algorithm", &self.digest_algorithm)
            .field("chunk_size", &self.chunk_size)
            .field(
                "acceptance_handle",
                &self.acceptance_handle.as_ref().map(|_| REDACTED),
            )
            .field("expected_chunk_digest", &self.expected_chunk_digest)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizationDecisionMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: AuthorizationDecisionBody,
}

impl AuthorizationDecisionMessage {
    pub fn approved(
        in_reply_to: Uuid,
        digest_algorithm: WireDigestAlgorithm,
        chunk_size: u32,
        acceptance_handle: impl Into<String>,
        expected_chunk_digest: Option<String>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: AuthorizationDecisionBody {
                in_reply_to,
                decision: AuthorizationDecisionOutcome::Approved,
                digest_algorithm: Some(digest_algorithm),
                chunk_size: Some(chunk_size),
                acceptance_handle: Some(acceptance_handle.into()),
                expected_chunk_digest,
            },
        }
    }

    /// `denied` never carries any field beyond `decision` — there is no way
    /// to construct a `denied` decision that also carries authoritative
    /// manifest facts, a handle, or a reason.
    pub fn denied(in_reply_to: Uuid) -> Self {
        Self {
            envelope: Envelope::new(),
            body: AuthorizationDecisionBody {
                in_reply_to,
                decision: AuthorizationDecisionOutcome::Denied,
                digest_algorithm: None,
                chunk_size: None,
                acceptance_handle: None,
                expected_chunk_digest: None,
            },
        }
    }

    pub fn is_reply_to(&self, request_message_id: Uuid) -> bool {
        self.envelope.is_valid() && self.body.in_reply_to == request_message_id
    }
}

// ---------------------------------------------------------------------
// ChunkAcceptanceRequest / ChunkAcceptanceDecision
// (`m1-worker-data-plane-control-contract.md` "Minimum messages" #2)
// ---------------------------------------------------------------------

/// `ChunkAcceptanceRequest{acceptance_handle, transfer_id, chunk_index,
/// digest, size}` — sent only after the Worker has itself received the body
/// and verified the bytes hash to `digest` with the `AuthorizationDecision`'s
/// `digest_algorithm`. `size` is the exact count of raw verified bytes the
/// Worker received and hashed, never a blindly trusted `Content-Length`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ChunkAcceptanceRequestBody {
    pub acceptance_handle: String,
    pub transfer_id: Uuid,
    pub chunk_index: u64,
    pub digest: String,
    pub size: u32,
}

impl fmt::Debug for ChunkAcceptanceRequestBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkAcceptanceRequestBody")
            .field("acceptance_handle", &REDACTED)
            .field("transfer_id", &self.transfer_id)
            .field("chunk_index", &self.chunk_index)
            .field("digest", &self.digest)
            .field("size", &self.size)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ChunkAcceptanceRequestMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ChunkAcceptanceRequestBody,
}

impl fmt::Debug for ChunkAcceptanceRequestMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkAcceptanceRequestMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl ChunkAcceptanceRequestMessage {
    pub fn new(
        acceptance_handle: impl Into<String>,
        transfer_id: Uuid,
        chunk_index: u64,
        digest: impl Into<String>,
        size: u32,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ChunkAcceptanceRequestBody {
                acceptance_handle: acceptance_handle.into(),
                transfer_id,
                chunk_index,
                digest: digest.into(),
                size,
            },
        }
    }
}

/// `committed` | `already_committed` | `rejected`
/// (`m1-worker-data-plane-control-contract.md` "Verified-chunk durable
/// acceptance").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkAcceptanceOutcome {
    #[serde(rename = "committed")]
    Committed,
    #[serde(rename = "already_committed")]
    AlreadyCommitted,
    #[serde(rename = "rejected")]
    Rejected,
}

/// Closed `rejected` reason vocabulary — each maps deterministically to one
/// HTTP `409` code. `DIGEST_MISMATCH`/`CHUNK_TOO_LARGE` are never
/// `ChunkAcceptanceDecision` outcomes (the Worker detects both locally).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkAcceptanceRejectionReason {
    #[serde(rename = "chunk_identity_conflict")]
    ChunkIdentityConflict,
    #[serde(rename = "transfer_not_continuable")]
    TransferNotContinuable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkAcceptanceDecisionBody {
    pub in_reply_to: Uuid,
    pub outcome: ChunkAcceptanceOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ChunkAcceptanceRejectionReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkAcceptanceDecisionMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ChunkAcceptanceDecisionBody,
}

impl ChunkAcceptanceDecisionMessage {
    pub fn committed(in_reply_to: Uuid) -> Self {
        Self::with_outcome(in_reply_to, ChunkAcceptanceOutcome::Committed, None)
    }

    pub fn already_committed(in_reply_to: Uuid) -> Self {
        Self::with_outcome(in_reply_to, ChunkAcceptanceOutcome::AlreadyCommitted, None)
    }

    pub fn rejected(in_reply_to: Uuid, reason: ChunkAcceptanceRejectionReason) -> Self {
        Self::with_outcome(in_reply_to, ChunkAcceptanceOutcome::Rejected, Some(reason))
    }

    fn with_outcome(
        in_reply_to: Uuid,
        outcome: ChunkAcceptanceOutcome,
        reason: Option<ChunkAcceptanceRejectionReason>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ChunkAcceptanceDecisionBody {
                in_reply_to,
                outcome,
                reason,
            },
        }
    }

    pub fn is_reply_to(&self, request_message_id: Uuid) -> bool {
        self.envelope.is_valid() && self.body.in_reply_to == request_message_id
    }
}

// ---------------------------------------------------------------------
// ResumeDiscoveryQuery / ResumeDiscoveryPage / ResumeDiscoveryContinue
// (`m1-worker-data-plane-control-contract.md` "Minimum messages" #3, #4)
// ---------------------------------------------------------------------

/// `ResumeDiscoveryQuery{token, transfer_id, proof_id, issued_at, signature}`
/// — the authorizing request for `GET .../chunks`. This message itself
/// authorizes `operation = resume_discovery`; there is no separate
/// `AuthorizationQuery` first.
#[derive(Clone, Serialize, Deserialize)]
pub struct ResumeDiscoveryQueryBody {
    pub token: String,
    pub transfer_id: Uuid,
    pub proof_id: String,
    pub issued_at: u64,
    pub signature: String,
}

impl fmt::Debug for ResumeDiscoveryQueryBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResumeDiscoveryQueryBody")
            .field("token", &REDACTED)
            .field("transfer_id", &self.transfer_id)
            .field("proof_id", &REDACTED)
            .field("issued_at", &REDACTED)
            .field("signature", &REDACTED)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ResumeDiscoveryQueryMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ResumeDiscoveryQueryBody,
}

impl fmt::Debug for ResumeDiscoveryQueryMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResumeDiscoveryQueryMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl ResumeDiscoveryQueryMessage {
    pub fn new(
        token: impl Into<String>,
        transfer_id: Uuid,
        proof_id: impl Into<String>,
        issued_at: u64,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ResumeDiscoveryQueryBody {
                token: token.into(),
                transfer_id,
                proof_id: proof_id.into(),
                issued_at,
                signature: signature.into(),
            },
        }
    }
}

/// One held-chunk entry in a [`ResumeDiscoveryPageBody`]: `≈ 60–70` UTF-8
/// bytes, so pages stay well within the 1 MiB frame limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldChunk {
    pub chunk_index: u64,
    pub digest: String,
}

/// `approved` | `denied` (`m1-worker-data-plane-control-contract.md`
/// "Resume-discovery authorization and first page"). `denied` carries
/// `decision` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeDiscoveryDecision {
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "denied")]
    Denied,
}

/// `ResumeDiscoveryPage` — reply to both `ResumeDiscoveryQuery` (first page)
/// and `ResumeDiscoveryContinue` (subsequent pages). Manifest-level fields
/// (`transfer_id`, `sealed`, `digest_algorithm`, `chunk_size`,
/// `expected_chunk_count`) appear on the first approved page only and the
/// Worker reuses them; continuation pages carry `held_chunks`/`resume_cursor`
/// only. `resume_cursor` is present iff at least one more held chunk remains.
/// `expected_chunk_count` is present only when `sealed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeDiscoveryPageBody {
    pub in_reply_to: Uuid,
    pub decision: ResumeDiscoveryDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_algorithm: Option<WireDigestAlgorithm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_chunk_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_chunks: Option<Vec<HeldChunk>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeDiscoveryPageMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ResumeDiscoveryPageBody,
}

impl ResumeDiscoveryPageMessage {
    /// `denied` — no durable facts on denial.
    pub fn denied(in_reply_to: Uuid) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ResumeDiscoveryPageBody {
                in_reply_to,
                decision: ResumeDiscoveryDecision::Denied,
                transfer_id: None,
                sealed: None,
                digest_algorithm: None,
                chunk_size: None,
                expected_chunk_count: None,
                held_chunks: None,
                resume_cursor: None,
            },
        }
    }

    /// The first approved page: carries the manifest-level fields plus this
    /// page's `held_chunks` slice and, iff more pages remain, `resume_cursor`.
    #[allow(clippy::too_many_arguments)]
    pub fn first_page(
        in_reply_to: Uuid,
        transfer_id: Uuid,
        sealed: bool,
        digest_algorithm: WireDigestAlgorithm,
        chunk_size: u32,
        expected_chunk_count: Option<u64>,
        held_chunks: Vec<HeldChunk>,
        resume_cursor: Option<String>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ResumeDiscoveryPageBody {
                in_reply_to,
                decision: ResumeDiscoveryDecision::Approved,
                transfer_id: Some(transfer_id),
                sealed: Some(sealed),
                digest_algorithm: Some(digest_algorithm),
                chunk_size: Some(chunk_size),
                expected_chunk_count,
                held_chunks: Some(held_chunks),
                resume_cursor,
            },
        }
    }

    /// A continuation page: `held_chunks` slice plus, iff still more remain,
    /// the next `resume_cursor`; no manifest-level fields.
    pub fn continuation_page(
        in_reply_to: Uuid,
        held_chunks: Vec<HeldChunk>,
        resume_cursor: Option<String>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ResumeDiscoveryPageBody {
                in_reply_to,
                decision: ResumeDiscoveryDecision::Approved,
                transfer_id: None,
                sealed: None,
                digest_algorithm: None,
                chunk_size: None,
                expected_chunk_count: None,
                held_chunks: Some(held_chunks),
                resume_cursor,
            },
        }
    }

    pub fn is_reply_to(&self, request_message_id: Uuid) -> bool {
        self.envelope.is_valid() && self.body.in_reply_to == request_message_id
    }
}

/// `ResumeDiscoveryContinue{resume_cursor}` — requests the next page, bound
/// to the one authorized `ResumeDiscoveryQuery` for the current connection
/// generation. Each cursor value is accepted once.
#[derive(Clone, Serialize, Deserialize)]
pub struct ResumeDiscoveryContinueBody {
    pub resume_cursor: String,
}

impl fmt::Debug for ResumeDiscoveryContinueBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResumeDiscoveryContinueBody")
            .field("resume_cursor", &REDACTED)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ResumeDiscoveryContinueMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ResumeDiscoveryContinueBody,
}

impl fmt::Debug for ResumeDiscoveryContinueMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResumeDiscoveryContinueMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl ResumeDiscoveryContinueMessage {
    pub fn new(resume_cursor: impl Into<String>) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ResumeDiscoveryContinueBody {
                resume_cursor: resume_cursor.into(),
            },
        }
    }
}

// ---------------------------------------------------------------------
// ManifestSealRequest / ManifestSealDecision
// (`m1-worker-data-plane-control-contract.md` "Minimum messages" #5)
// ---------------------------------------------------------------------

/// `ManifestSealRequest{token, transfer_id, proof_id, issued_at, signature,
/// chunk_count, artifact_digest}` — the authorizing request for
/// `POST .../seal`. This one message performs sender-constrained
/// authorization + current durable validation + the first durable seal
/// commit (`Incomplete -> PendingVerification`) atomically. There is no
/// separate `AuthorizationQuery` for seal. `chunk_count`/`artifact_digest`
/// are Agent-declared from the HTTP request body.
#[derive(Clone, Serialize, Deserialize)]
pub struct ManifestSealRequestBody {
    pub token: String,
    pub transfer_id: Uuid,
    pub proof_id: String,
    pub issued_at: u64,
    pub signature: String,
    pub chunk_count: u64,
    pub artifact_digest: String,
}

impl fmt::Debug for ManifestSealRequestBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManifestSealRequestBody")
            .field("token", &REDACTED)
            .field("transfer_id", &self.transfer_id)
            .field("proof_id", &REDACTED)
            .field("issued_at", &REDACTED)
            .field("signature", &REDACTED)
            .field("chunk_count", &self.chunk_count)
            .field("artifact_digest", &self.artifact_digest)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ManifestSealRequestMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ManifestSealRequestBody,
}

impl fmt::Debug for ManifestSealRequestMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManifestSealRequestMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl ManifestSealRequestMessage {
    pub fn new(
        token: impl Into<String>,
        transfer_id: Uuid,
        proof_id: impl Into<String>,
        issued_at: u64,
        signature: impl Into<String>,
        chunk_count: u64,
        artifact_digest: impl Into<String>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ManifestSealRequestBody {
                token: token.into(),
                transfer_id,
                proof_id: proof_id.into(),
                issued_at,
                signature: signature.into(),
                chunk_count,
                artifact_digest: artifact_digest.into(),
            },
        }
    }
}

/// `sealed` | `already_pending_verification` | `rejected` | `denied`
/// (`m1-worker-data-plane-control-contract.md` "Seal-manifest first durable
/// commit").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManifestSealOutcome {
    #[serde(rename = "sealed")]
    Sealed,
    #[serde(rename = "already_pending_verification")]
    AlreadyPendingVerification,
    #[serde(rename = "rejected")]
    Rejected,
    #[serde(rename = "denied")]
    Denied,
}

/// Closed `rejected` reason vocabulary for seal — each maps deterministically
/// to one HTTP `409`. A terminal owning Transfer/Artifact/Attempt is a
/// `denied`, never a `rejected` (contract: no `TRANSFER_NOT_CONTINUABLE` for
/// seal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManifestSealRejectionReason {
    #[serde(rename = "incomplete_manifest")]
    IncompleteManifest,
    #[serde(rename = "manifest_already_sealed")]
    ManifestAlreadySealed,
}

/// `ManifestSealDecision`. On `sealed`/`already_pending_verification`,
/// `verification_handle`, `artifact_id`, `digest_algorithm`, `chunk_size`,
/// `chunk_count`, and `expected_artifact_digest` are all present and are the
/// **authoritative durable sealed values** — the Worker verifies against
/// these, never against the values it sent in the HTTP body. `rejected`
/// carries `reason` only; `denied` carries `outcome` only.
#[derive(Clone, Serialize, Deserialize)]
pub struct ManifestSealDecisionBody {
    pub in_reply_to: Uuid,
    pub outcome: ManifestSealOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ManifestSealRejectionReason>,
    /// Transient, generation-scoped; opaque to the Worker. Redacted from
    /// `Debug`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_algorithm: Option<WireDigestAlgorithm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_artifact_digest: Option<String>,
}

impl fmt::Debug for ManifestSealDecisionBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManifestSealDecisionBody")
            .field("in_reply_to", &self.in_reply_to)
            .field("outcome", &self.outcome)
            .field("reason", &self.reason)
            .field(
                "verification_handle",
                &self.verification_handle.as_ref().map(|_| REDACTED),
            )
            .field("artifact_id", &self.artifact_id)
            .field("digest_algorithm", &self.digest_algorithm)
            .field("chunk_size", &self.chunk_size)
            .field("chunk_count", &self.chunk_count)
            .field("expected_artifact_digest", &self.expected_artifact_digest)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSealDecisionMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ManifestSealDecisionBody,
}

/// The authoritative durable values every committed (`sealed` /
/// `already_pending_verification`) `ManifestSealDecision` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedManifestFacts {
    pub verification_handle: String,
    pub artifact_id: Uuid,
    pub digest_algorithm: WireDigestAlgorithm,
    pub chunk_size: u32,
    pub chunk_count: u64,
    pub expected_artifact_digest: String,
}

impl ManifestSealDecisionMessage {
    pub fn denied(in_reply_to: Uuid) -> Self {
        Self::bare(in_reply_to, ManifestSealOutcome::Denied, None)
    }

    pub fn rejected(in_reply_to: Uuid, reason: ManifestSealRejectionReason) -> Self {
        Self::bare(in_reply_to, ManifestSealOutcome::Rejected, Some(reason))
    }

    pub fn sealed(in_reply_to: Uuid, facts: SealedManifestFacts) -> Self {
        Self::committed(in_reply_to, ManifestSealOutcome::Sealed, facts)
    }

    pub fn already_pending_verification(in_reply_to: Uuid, facts: SealedManifestFacts) -> Self {
        Self::committed(
            in_reply_to,
            ManifestSealOutcome::AlreadyPendingVerification,
            facts,
        )
    }

    fn bare(
        in_reply_to: Uuid,
        outcome: ManifestSealOutcome,
        reason: Option<ManifestSealRejectionReason>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ManifestSealDecisionBody {
                in_reply_to,
                outcome,
                reason,
                verification_handle: None,
                artifact_id: None,
                digest_algorithm: None,
                chunk_size: None,
                chunk_count: None,
                expected_artifact_digest: None,
            },
        }
    }

    fn committed(
        in_reply_to: Uuid,
        outcome: ManifestSealOutcome,
        facts: SealedManifestFacts,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ManifestSealDecisionBody {
                in_reply_to,
                outcome,
                reason: None,
                verification_handle: Some(facts.verification_handle),
                artifact_id: Some(facts.artifact_id),
                digest_algorithm: Some(facts.digest_algorithm),
                chunk_size: Some(facts.chunk_size),
                chunk_count: Some(facts.chunk_count),
                expected_artifact_digest: Some(facts.expected_artifact_digest),
            },
        }
    }

    pub fn is_reply_to(&self, request_message_id: Uuid) -> bool {
        self.envelope.is_valid() && self.body.in_reply_to == request_message_id
    }
}

// ---------------------------------------------------------------------
// ArtifactVerificationReport / ArtifactVerificationAck
// (`m1-worker-data-plane-control-contract.md` "Minimum messages" #6)
// ---------------------------------------------------------------------

/// `ArtifactVerificationReport{verification_handle, computed_artifact_digest}`
/// — sent once, after `bamepd` has already committed
/// `Incomplete -> PendingVerification` and the Worker has reconstructed the
/// full Artifact byte stream and computed its digest. The Worker reports only
/// the mechanical digest it computed — **no** `matches_expected` field.
#[derive(Clone, Serialize, Deserialize)]
pub struct ArtifactVerificationReportBody {
    pub verification_handle: String,
    pub computed_artifact_digest: String,
}

impl fmt::Debug for ArtifactVerificationReportBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactVerificationReportBody")
            .field("verification_handle", &REDACTED)
            .field("computed_artifact_digest", &self.computed_artifact_digest)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ArtifactVerificationReportMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ArtifactVerificationReportBody,
}

impl fmt::Debug for ArtifactVerificationReportMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactVerificationReportMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl ArtifactVerificationReportMessage {
    pub fn new(
        verification_handle: impl Into<String>,
        computed_artifact_digest: impl Into<String>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ArtifactVerificationReportBody {
                verification_handle: verification_handle.into(),
                computed_artifact_digest: computed_artifact_digest.into(),
            },
        }
    }
}

/// The single `outcome` value an `ArtifactVerificationAck` carries — `bamepd`
/// only sends this message after it has durably committed the
/// `PendingVerification -> Verified | Failed` transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactVerificationAckOutcome {
    #[serde(rename = "committed")]
    Committed,
}

/// `ArtifactVerificationAck{outcome: "committed", artifact_status: "Verified"
/// | "Failed"}`. `artifact_status` is the authoritative committed status the
/// Worker renders in the HTTP seal response; the Worker cannot establish
/// `Verified` by assertion — only `bamepd`'s own comparison against its
/// durable expected digest decides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactVerificationAckBody {
    pub in_reply_to: Uuid,
    pub outcome: ArtifactVerificationAckOutcome,
    pub artifact_status: WireArtifactStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactVerificationAckMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ArtifactVerificationAckBody,
}

impl ArtifactVerificationAckMessage {
    pub fn committed(in_reply_to: Uuid, artifact_status: WireArtifactStatus) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ArtifactVerificationAckBody {
                in_reply_to,
                outcome: ArtifactVerificationAckOutcome::Committed,
                artifact_status,
            },
        }
    }

    pub fn is_reply_to(&self, request_message_id: Uuid) -> bool {
        self.envelope.is_valid() && self.body.in_reply_to == request_message_id
    }
}

// ---------------------------------------------------------------------
// ProtocolError — bidirectional
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolErrorBody {
    pub code: String,
    /// Never secret material (`m1-worker-data-plane-control-contract.md`
    /// "Security and logging"). Omitted, never `null`, when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolErrorMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ProtocolErrorBody,
}

impl ProtocolErrorMessage {
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ProtocolErrorBody {
                code: code.into(),
                message: None,
                in_reply_to: None,
            },
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.body.message = Some(message.into());
        self
    }

    pub fn with_in_reply_to(mut self, in_reply_to: Uuid) -> Self {
        self.body.in_reply_to = Some(in_reply_to);
        self
    }
}

// ---------------------------------------------------------------------
// Semantic shape validation
// ---------------------------------------------------------------------
//
// `serde` alone accepts any combination of the `Option<T>` conditional
// fields, so a *received* JSON object can deserialize into a known message
// type whose `outcome`/`decision` combination the authoritative Specification
// says cannot exist (for example `AuthorizationDecision{decision: "denied",
// chunk_size: 4096}`). The constructors above never *emit* such a shape, but
// `codec::decode` must not *admit* one either. After `serde` parses a known
// message, [`WorkerProtocolMessage::validate_shape`] checks that the decoded
// body is a legal instance of its own declared outcome/decision — using only
// fields the message already carries, so genuinely unknown forward-compatible
// fields are still ignored (`m1-worker-data-plane-control-contract.md`
// "Compatibility and unknown fields").
//
// This is a distinct concern from envelope validity, `in_reply_to`
// correlation, and connection-generation ownership — each of those remains
// its own check.

/// Why a successfully deserialized, known Worker Protocol v1 message is not a
/// legal instance of its own declared outcome/decision shape.
///
/// `message_type` and `detail` are fixed diagnostic strings — never derived
/// from received content — so surfacing this error cannot leak arbitrary
/// wire data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message_type} is not a legal instance of its declared shape: {detail}")]
pub struct InvalidMessageShape {
    pub message_type: &'static str,
    pub detail: &'static str,
}

fn shape(message_type: &'static str, detail: &'static str) -> InvalidMessageShape {
    InvalidMessageShape {
        message_type,
        detail,
    }
}

impl AuthorizationDecisionBody {
    /// `approved` requires `digest_algorithm` + `chunk_size` +
    /// `acceptance_handle` (`expected_chunk_digest` stays optional);
    /// `denied` requires every one of those four fields absent, and there is
    /// no `reason` field (`m1-worker-data-plane-control-contract.md`
    /// "Chunk-upload authorization").
    fn validate_shape(&self) -> Result<(), InvalidMessageShape> {
        match self.decision {
            AuthorizationDecisionOutcome::Approved => {
                if self.digest_algorithm.is_none()
                    || self.chunk_size.is_none()
                    || self.acceptance_handle.is_none()
                {
                    return Err(shape(
                        "AuthorizationDecision",
                        "an approved decision must carry digest_algorithm, chunk_size, and \
                         acceptance_handle",
                    ));
                }
            }
            AuthorizationDecisionOutcome::Denied => {
                if self.digest_algorithm.is_some()
                    || self.chunk_size.is_some()
                    || self.acceptance_handle.is_some()
                    || self.expected_chunk_digest.is_some()
                {
                    return Err(shape(
                        "AuthorizationDecision",
                        "a denied decision must carry nothing beyond `decision`",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl ChunkAcceptanceDecisionBody {
    /// `committed`/`already_committed` require `reason` absent; `rejected`
    /// requires `reason` present (`m1-worker-data-plane-control-contract.md`
    /// "Verified-chunk durable acceptance").
    fn validate_shape(&self) -> Result<(), InvalidMessageShape> {
        match self.outcome {
            ChunkAcceptanceOutcome::Committed | ChunkAcceptanceOutcome::AlreadyCommitted => {
                if self.reason.is_some() {
                    return Err(shape(
                        "ChunkAcceptanceDecision",
                        "a committed/already_committed outcome must not carry a reason",
                    ));
                }
            }
            ChunkAcceptanceOutcome::Rejected => {
                if self.reason.is_none() {
                    return Err(shape(
                        "ChunkAcceptanceDecision",
                        "a rejected outcome must carry a reason",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl ResumeDiscoveryPageBody {
    /// The generic invariant `codec::decode` enforces on every
    /// `ResumeDiscoveryPage`, independent of whether the consumer expects a
    /// first page or a continuation page:
    ///
    /// - `denied` carries none of the durable/state fields;
    /// - `approved` carries `held_chunks`, and `expected_chunk_count` is
    ///   present iff `sealed == true` (so a continuation page, which omits
    ///   `sealed`, must also omit `expected_chunk_count`).
    ///
    /// First-page-vs-continuation-specific requirements are enforced by the
    /// explicit [`ResumeDiscoveryPageMessage::approved_first_page`] /
    /// [`ResumeDiscoveryPageMessage::approved_continuation_page`] validators
    /// the Phase C/E consumers call, not here
    /// (`m1-worker-data-plane-control-contract.md` "Resume-discovery
    /// authorization and first page" / "Resume-discovery pagination").
    fn validate_shape(&self) -> Result<(), InvalidMessageShape> {
        match self.decision {
            ResumeDiscoveryDecision::Denied => {
                if self.transfer_id.is_some()
                    || self.sealed.is_some()
                    || self.digest_algorithm.is_some()
                    || self.chunk_size.is_some()
                    || self.expected_chunk_count.is_some()
                    || self.held_chunks.is_some()
                    || self.resume_cursor.is_some()
                {
                    return Err(shape(
                        "ResumeDiscoveryPage",
                        "a denied resume-discovery page must carry no durable/state field",
                    ));
                }
            }
            ResumeDiscoveryDecision::Approved => {
                if self.held_chunks.is_none() {
                    return Err(shape(
                        "ResumeDiscoveryPage",
                        "an approved resume-discovery page must carry held_chunks",
                    ));
                }
                if self.expected_chunk_count.is_some() != (self.sealed == Some(true)) {
                    return Err(shape(
                        "ResumeDiscoveryPage",
                        "expected_chunk_count must be present iff sealed == true",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The validated fields of an **approved first** `ResumeDiscoveryPage`
/// (`m1-worker-data-plane-control-contract.md` "Resume-discovery
/// authorization and first page"). Every manifest-level field is guaranteed
/// present; `expected_chunk_count` is `Some` iff `sealed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeFirstPage {
    pub transfer_id: Uuid,
    pub sealed: bool,
    pub digest_algorithm: WireDigestAlgorithm,
    pub chunk_size: u32,
    pub expected_chunk_count: Option<u64>,
    pub held_chunks: Vec<HeldChunk>,
    pub resume_cursor: Option<String>,
}

/// The validated fields of an **approved continuation** `ResumeDiscoveryPage`
/// (`m1-worker-data-plane-control-contract.md` "Resume-discovery
/// pagination"): `held_chunks` present, every manifest-level field absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeContinuationPage {
    pub held_chunks: Vec<HeldChunk>,
    pub resume_cursor: Option<String>,
}

impl ResumeDiscoveryPageMessage {
    /// Interprets this page as an **approved first page**, enforcing that
    /// `decision == approved` and that `transfer_id`, `sealed`,
    /// `digest_algorithm`, `chunk_size`, and `held_chunks` are all present,
    /// with `expected_chunk_count` present iff `sealed == true`. A Phase C/E
    /// consumer expecting the first page calls this so it cannot accidentally
    /// accept a denied page or a continuation-shaped page.
    pub fn approved_first_page(&self) -> Result<ResumeFirstPage, InvalidMessageShape> {
        let b = &self.body;
        if b.decision != ResumeDiscoveryDecision::Approved {
            return Err(shape(
                "ResumeDiscoveryPage",
                "expected an approved first page, got a denied page",
            ));
        }
        let (
            Some(transfer_id),
            Some(sealed),
            Some(digest_algorithm),
            Some(chunk_size),
            Some(held_chunks),
        ) = (
            b.transfer_id,
            b.sealed,
            b.digest_algorithm,
            b.chunk_size,
            b.held_chunks.clone(),
        )
        else {
            return Err(shape(
                "ResumeDiscoveryPage",
                "an approved first page must carry transfer_id, sealed, digest_algorithm, \
                 chunk_size, and held_chunks",
            ));
        };
        if b.expected_chunk_count.is_some() != sealed {
            return Err(shape(
                "ResumeDiscoveryPage",
                "expected_chunk_count must be present iff sealed == true",
            ));
        }
        Ok(ResumeFirstPage {
            transfer_id,
            sealed,
            digest_algorithm,
            chunk_size,
            expected_chunk_count: b.expected_chunk_count,
            held_chunks,
            resume_cursor: b.resume_cursor.clone(),
        })
    }

    /// Interprets this page as an **approved continuation page**, enforcing
    /// that `decision == approved`, `held_chunks` is present, and no
    /// manifest-level field (`transfer_id`, `sealed`, `digest_algorithm`,
    /// `chunk_size`, `expected_chunk_count`) is carried.
    pub fn approved_continuation_page(
        &self,
    ) -> Result<ResumeContinuationPage, InvalidMessageShape> {
        let b = &self.body;
        if b.decision != ResumeDiscoveryDecision::Approved {
            return Err(shape(
                "ResumeDiscoveryPage",
                "expected an approved continuation page, got a denied page",
            ));
        }
        let Some(held_chunks) = b.held_chunks.clone() else {
            return Err(shape(
                "ResumeDiscoveryPage",
                "an approved continuation page must carry held_chunks",
            ));
        };
        if b.transfer_id.is_some()
            || b.sealed.is_some()
            || b.digest_algorithm.is_some()
            || b.chunk_size.is_some()
            || b.expected_chunk_count.is_some()
        {
            return Err(shape(
                "ResumeDiscoveryPage",
                "a continuation page must not carry any manifest-level field",
            ));
        }
        Ok(ResumeContinuationPage {
            held_chunks,
            resume_cursor: b.resume_cursor.clone(),
        })
    }
}

impl ManifestSealDecisionBody {
    /// `sealed`/`already_pending_verification` require `reason` absent and the
    /// complete sealed-manifest fact bundle present (`verification_handle`,
    /// `artifact_id`, `digest_algorithm`, `chunk_size`, `chunk_count`,
    /// `expected_artifact_digest`). `rejected` requires `reason` present and
    /// no success field. `denied` requires `reason` absent and no success
    /// field (`m1-worker-data-plane-control-contract.md` "Seal-manifest first
    /// durable commit").
    fn validate_shape(&self) -> Result<(), InvalidMessageShape> {
        let any_success_field = self.verification_handle.is_some()
            || self.artifact_id.is_some()
            || self.digest_algorithm.is_some()
            || self.chunk_size.is_some()
            || self.chunk_count.is_some()
            || self.expected_artifact_digest.is_some();
        let all_success_fields = self.verification_handle.is_some()
            && self.artifact_id.is_some()
            && self.digest_algorithm.is_some()
            && self.chunk_size.is_some()
            && self.chunk_count.is_some()
            && self.expected_artifact_digest.is_some();
        match self.outcome {
            ManifestSealOutcome::Sealed | ManifestSealOutcome::AlreadyPendingVerification => {
                if self.reason.is_some() {
                    return Err(shape(
                        "ManifestSealDecision",
                        "a sealed/already_pending_verification outcome must not carry a reason",
                    ));
                }
                if !all_success_fields {
                    return Err(shape(
                        "ManifestSealDecision",
                        "a sealed/already_pending_verification outcome must carry the complete \
                         sealed-manifest fact bundle (verification_handle, artifact_id, \
                         digest_algorithm, chunk_size, chunk_count, expected_artifact_digest)",
                    ));
                }
            }
            ManifestSealOutcome::Rejected => {
                if self.reason.is_none() {
                    return Err(shape(
                        "ManifestSealDecision",
                        "a rejected outcome must carry a reason",
                    ));
                }
                if any_success_field {
                    return Err(shape(
                        "ManifestSealDecision",
                        "a rejected outcome must not carry any sealed-manifest fact",
                    ));
                }
            }
            ManifestSealOutcome::Denied => {
                if self.reason.is_some() {
                    return Err(shape(
                        "ManifestSealDecision",
                        "a denied outcome must not carry a reason",
                    ));
                }
                if any_success_field {
                    return Err(shape(
                        "ManifestSealDecision",
                        "a denied outcome must not carry any sealed-manifest fact",
                    ));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Top-level closed message union
// ---------------------------------------------------------------------

/// Every Worker IPC v1 message this crate represents, dispatched on the wire
/// `"type"` field with exactly the normative message names.
///
/// An unrecognized `"type"` value fails deserialization explicitly (a
/// `serde` "unknown variant" error) rather than silently falling back to any
/// variant — satisfying "Unknown top-level `type`: rejected with
/// `ProtocolError`". Generating the `ProtocolError` response itself belongs
/// to the `bamepd`/Worker handshake handler, not this crate. Unknown fields
/// inside an otherwise valid, known message type are ignored by the
/// underlying struct deserializers (no `deny_unknown_fields` anywhere in
/// this crate), per the Specification's forward-compatibility requirement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkerProtocolMessage {
    WorkerHello(WorkerHelloMessage),
    ServerHello(ServerHelloMessage),
    HandshakeRejected(HandshakeRejectedMessage),
    AuthorizationQuery(AuthorizationQueryMessage),
    AuthorizationDecision(AuthorizationDecisionMessage),
    ChunkAcceptanceRequest(ChunkAcceptanceRequestMessage),
    ChunkAcceptanceDecision(ChunkAcceptanceDecisionMessage),
    ResumeDiscoveryQuery(ResumeDiscoveryQueryMessage),
    ResumeDiscoveryPage(ResumeDiscoveryPageMessage),
    ResumeDiscoveryContinue(ResumeDiscoveryContinueMessage),
    ManifestSealRequest(ManifestSealRequestMessage),
    ManifestSealDecision(ManifestSealDecisionMessage),
    ArtifactVerificationReport(ArtifactVerificationReportMessage),
    ArtifactVerificationAck(ArtifactVerificationAckMessage),
    ProtocolError(ProtocolErrorMessage),
}

impl WorkerProtocolMessage {
    pub fn envelope(&self) -> &Envelope {
        match self {
            WorkerProtocolMessage::WorkerHello(m) => &m.envelope,
            WorkerProtocolMessage::ServerHello(m) => &m.envelope,
            WorkerProtocolMessage::HandshakeRejected(m) => &m.envelope,
            WorkerProtocolMessage::AuthorizationQuery(m) => &m.envelope,
            WorkerProtocolMessage::AuthorizationDecision(m) => &m.envelope,
            WorkerProtocolMessage::ChunkAcceptanceRequest(m) => &m.envelope,
            WorkerProtocolMessage::ChunkAcceptanceDecision(m) => &m.envelope,
            WorkerProtocolMessage::ResumeDiscoveryQuery(m) => &m.envelope,
            WorkerProtocolMessage::ResumeDiscoveryPage(m) => &m.envelope,
            WorkerProtocolMessage::ResumeDiscoveryContinue(m) => &m.envelope,
            WorkerProtocolMessage::ManifestSealRequest(m) => &m.envelope,
            WorkerProtocolMessage::ManifestSealDecision(m) => &m.envelope,
            WorkerProtocolMessage::ArtifactVerificationReport(m) => &m.envelope,
            WorkerProtocolMessage::ArtifactVerificationAck(m) => &m.envelope,
            WorkerProtocolMessage::ProtocolError(m) => &m.envelope,
        }
    }

    /// Validates that this decoded message is a legal instance of its own
    /// declared outcome/decision shape — the invariants the authoritative
    /// Specification expresses over the message's own conditional fields
    /// (approved-vs-denied, committed-vs-rejected, sealed-vs-denied, …).
    /// `codec::decode` calls this after `serde` parsing so a contract-invalid
    /// combination of *known* fields fails closed, while genuinely unknown
    /// forward-compatible fields are still ignored. Message types with no
    /// conditional shape (the handshake messages, request messages whose
    /// fields are all mandatory, `ProtocolError`, `ArtifactVerificationAck`)
    /// are always shape-valid once `serde` has accepted them.
    pub fn validate_shape(&self) -> Result<(), InvalidMessageShape> {
        match self {
            WorkerProtocolMessage::AuthorizationDecision(m) => m.body.validate_shape(),
            WorkerProtocolMessage::ChunkAcceptanceDecision(m) => m.body.validate_shape(),
            WorkerProtocolMessage::ResumeDiscoveryPage(m) => m.body.validate_shape(),
            WorkerProtocolMessage::ManifestSealDecision(m) => m.body.validate_shape(),
            _ => Ok(()),
        }
    }
}

macro_rules! impl_from_message {
    ($($ty:ident => $variant:ident),+ $(,)?) => {
        $(
            impl From<$ty> for WorkerProtocolMessage {
                fn from(value: $ty) -> Self {
                    WorkerProtocolMessage::$variant(value)
                }
            }
        )+
    };
}

impl_from_message! {
    WorkerHelloMessage => WorkerHello,
    ServerHelloMessage => ServerHello,
    HandshakeRejectedMessage => HandshakeRejected,
    AuthorizationQueryMessage => AuthorizationQuery,
    AuthorizationDecisionMessage => AuthorizationDecision,
    ChunkAcceptanceRequestMessage => ChunkAcceptanceRequest,
    ChunkAcceptanceDecisionMessage => ChunkAcceptanceDecision,
    ResumeDiscoveryQueryMessage => ResumeDiscoveryQuery,
    ResumeDiscoveryPageMessage => ResumeDiscoveryPage,
    ResumeDiscoveryContinueMessage => ResumeDiscoveryContinue,
    ManifestSealRequestMessage => ManifestSealRequest,
    ManifestSealDecisionMessage => ManifestSealDecision,
    ArtifactVerificationReportMessage => ArtifactVerificationReport,
    ArtifactVerificationAckMessage => ArtifactVerificationAck,
    ProtocolErrorMessage => ProtocolError,
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use crate::codec;

    fn round_trip(message: WorkerProtocolMessage) -> WorkerProtocolMessage {
        let wire = codec::encode(&message).expect("encode");
        codec::decode(&wire).expect("decode")
    }

    // -- Handshake regression (unchanged from #37) ------------------------

    #[test]
    fn a_freshly_constructed_worker_hello_is_valid() {
        assert!(WorkerHelloMessage::new(Uuid::new_v4()).is_valid());
    }

    #[test]
    fn worker_hello_with_wrong_envelope_protocol_version_is_invalid() {
        let mut hello = WorkerHelloMessage::new(Uuid::new_v4());
        hello.envelope.protocol_version = ProtocolVersion::new("2");
        assert!(!hello.is_valid());
    }

    #[test]
    fn worker_hello_with_non_v4_message_id_is_invalid() {
        let mut hello = WorkerHelloMessage::new(Uuid::new_v4());
        hello.envelope.message_id = Uuid::nil();
        assert!(!hello.is_valid());
    }

    #[test]
    fn worker_hello_with_wrong_worker_protocol_version_is_invalid() {
        let mut hello = WorkerHelloMessage::new(Uuid::new_v4());
        hello.body.worker_protocol_version = ProtocolVersion::new("2");
        assert!(!hello.is_valid());
    }

    #[test]
    fn worker_hello_with_non_v4_worker_instance_id_is_invalid() {
        let mut hello = WorkerHelloMessage::new(Uuid::new_v4());
        hello.body.worker_instance_id = Uuid::nil();
        assert!(!hello.is_valid());
    }

    #[test]
    fn a_freshly_constructed_server_hello_correlates_and_is_valid() {
        let sent_id = Uuid::new_v4();
        assert!(ServerHelloMessage::new(sent_id).is_valid_reply_to(sent_id));
    }

    #[test]
    fn server_hello_with_uncorrelated_in_reply_to_is_invalid() {
        let sent_id = Uuid::new_v4();
        let response = ServerHelloMessage::new(Uuid::new_v4());
        assert!(!response.is_valid_reply_to(sent_id));
    }

    #[test]
    fn server_hello_with_compatible_false_is_invalid() {
        let sent_id = Uuid::new_v4();
        let mut response = ServerHelloMessage::new(sent_id);
        response.body.compatible = false;
        assert!(!response.is_valid_reply_to(sent_id));
    }

    #[test]
    fn server_hello_with_wrong_server_protocol_version_is_invalid() {
        let sent_id = Uuid::new_v4();
        let mut response = ServerHelloMessage::new(sent_id);
        response.body.server_protocol_version = ProtocolVersion::new("2");
        assert!(!response.is_valid_reply_to(sent_id));
    }

    #[test]
    fn a_freshly_constructed_handshake_rejected_correlates_and_is_valid() {
        let sent_id = Uuid::new_v4();
        assert!(HandshakeRejectedMessage::incompatible_version(sent_id).is_valid_reply_to(sent_id));
    }

    #[test]
    fn handshake_rejected_with_uncorrelated_in_reply_to_is_invalid() {
        let sent_id = Uuid::new_v4();
        let response = HandshakeRejectedMessage::incompatible_version(Uuid::new_v4());
        assert!(!response.is_valid_reply_to(sent_id));
    }

    #[test]
    fn malformed_handshake_rejected_reason_fails_at_decode_time() {
        let json = r#"{
            "type":"HandshakeRejected",
            "protocol_version":"1",
            "message_id":"9d1c9a3e-5f3e-4a3b-8b0a-6a9b5f3a2b11",
            "in_reply_to":"9d1c9a3e-5f3e-4a3b-8b0a-6a9b5f3a2b11",
            "reason":"not_a_real_reason"
        }"#;
        assert!(codec::decode(json).is_err());
    }

    #[test]
    fn every_message_carries_protocol_version_one() {
        for message in super::super::messages::tests_support::one_of_each_variant() {
            let wire = codec::encode(&message).expect("encode");
            assert!(
                wire.contains("\"protocol_version\":\"1\""),
                "message {message:?} must serialize protocol_version \"1\""
            );
            assert_eq!(message.envelope().protocol_version.as_str(), "1");
        }
    }

    // -- AuthorizationQuery / AuthorizationDecision ----------------------

    #[test]
    fn authorization_query_v1_shape_carries_no_operation_artifact_or_direction() {
        let message = AuthorizationQueryMessage::new(
            "opaque-token",
            Uuid::new_v4(),
            3,
            "proof-id-value",
            1_700_000_000_000,
            "signature-value",
        );
        let wire = codec::encode(&WorkerProtocolMessage::from(message)).expect("encode");
        assert!(!wire.contains("operation"));
        assert!(!wire.contains("artifact_id"));
        assert!(!wire.contains("direction"));
        assert!(wire.contains("\"chunk_index\":3"));
    }

    #[test]
    fn authorization_query_round_trips() {
        let message = AuthorizationQueryMessage::new(
            "opaque-token",
            Uuid::new_v4(),
            7,
            "proof-id-value",
            1_700_000_000_000,
            "signature-value",
        );
        let WorkerProtocolMessage::AuthorizationQuery(decoded) =
            round_trip(WorkerProtocolMessage::from(message))
        else {
            panic!("expected AuthorizationQuery");
        };
        assert_eq!(decoded.body.chunk_index, 7);
        assert_eq!(decoded.body.token, "opaque-token");
    }

    #[test]
    fn authorization_query_debug_redacts_secret_fields() {
        let message = AuthorizationQueryMessage::new(
            "super-secret-token",
            Uuid::new_v4(),
            1,
            "secret-proof-id",
            1_700_000_000_000,
            "secret-signature",
        );
        let debug = format!("{message:?}");
        assert!(!debug.contains("super-secret-token"));
        assert!(!debug.contains("secret-proof-id"));
        assert!(!debug.contains("secret-signature"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn approved_decision_carries_authoritative_manifest_facts_and_a_handle() {
        let request_id = Uuid::new_v4();
        let message = AuthorizationDecisionMessage::approved(
            request_id,
            WireDigestAlgorithm::Sha256,
            4 * 1024 * 1024,
            "acc-handle",
            Some("expected-digest".to_string()),
        );
        assert!(message.is_reply_to(request_id));
        let WorkerProtocolMessage::AuthorizationDecision(decoded) =
            round_trip(WorkerProtocolMessage::from(message))
        else {
            panic!("expected AuthorizationDecision");
        };
        assert_eq!(
            decoded.body.decision,
            AuthorizationDecisionOutcome::Approved
        );
        assert_eq!(
            decoded.body.digest_algorithm,
            Some(WireDigestAlgorithm::Sha256)
        );
        assert_eq!(decoded.body.chunk_size, Some(4 * 1024 * 1024));
        assert_eq!(
            decoded.body.acceptance_handle.as_deref(),
            Some("acc-handle")
        );
        assert_eq!(
            decoded.body.expected_chunk_digest.as_deref(),
            Some("expected-digest")
        );
    }

    #[test]
    fn denied_decision_carries_only_the_decision_on_the_wire() {
        let request_id = Uuid::new_v4();
        let message = AuthorizationDecisionMessage::denied(request_id);
        let wire = codec::encode(&WorkerProtocolMessage::from(message)).expect("encode");
        assert!(!wire.contains("digest_algorithm"));
        assert!(!wire.contains("chunk_size"));
        assert!(!wire.contains("acceptance_handle"));
        assert!(!wire.contains("expected_chunk_digest"));
        assert!(!wire.contains("reason"));
        assert!(!wire.contains("null"));
    }

    #[test]
    fn decision_debug_redacts_the_acceptance_handle() {
        let message = AuthorizationDecisionMessage::approved(
            Uuid::new_v4(),
            WireDigestAlgorithm::Sha256,
            1024,
            "sensitive-handle-value",
            None,
        );
        assert!(!format!("{message:?}").contains("sensitive-handle-value"));
    }

    #[test]
    fn unknown_digest_algorithm_fails_decode() {
        let json = format!(
            r#"{{"type":"AuthorizationDecision","protocol_version":"1","message_id":"{}",
                "in_reply_to":"{}","decision":"approved","digest_algorithm":"md5",
                "chunk_size":1024,"acceptance_handle":"h"}}"#,
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        assert!(codec::decode(&json).is_err());
    }

    // -- ChunkAcceptanceRequest / ChunkAcceptanceDecision ---------------

    #[test]
    fn chunk_acceptance_request_round_trips_and_redacts_its_handle() {
        let message = ChunkAcceptanceRequestMessage::new(
            "handle-secret",
            Uuid::new_v4(),
            4,
            "digestval",
            1024,
        );
        assert!(!format!("{message:?}").contains("handle-secret"));
        let WorkerProtocolMessage::ChunkAcceptanceRequest(decoded) =
            round_trip(WorkerProtocolMessage::from(message))
        else {
            panic!("expected ChunkAcceptanceRequest");
        };
        assert_eq!(decoded.body.chunk_index, 4);
        assert_eq!(decoded.body.size, 1024);
        assert_eq!(decoded.body.digest, "digestval");
    }

    #[test]
    fn chunk_acceptance_decision_outcomes_round_trip() {
        let id = Uuid::new_v4();
        for (message, expected) in [
            (
                ChunkAcceptanceDecisionMessage::committed(id),
                (ChunkAcceptanceOutcome::Committed, None),
            ),
            (
                ChunkAcceptanceDecisionMessage::already_committed(id),
                (ChunkAcceptanceOutcome::AlreadyCommitted, None),
            ),
            (
                ChunkAcceptanceDecisionMessage::rejected(
                    id,
                    ChunkAcceptanceRejectionReason::ChunkIdentityConflict,
                ),
                (
                    ChunkAcceptanceOutcome::Rejected,
                    Some(ChunkAcceptanceRejectionReason::ChunkIdentityConflict),
                ),
            ),
            (
                ChunkAcceptanceDecisionMessage::rejected(
                    id,
                    ChunkAcceptanceRejectionReason::TransferNotContinuable,
                ),
                (
                    ChunkAcceptanceOutcome::Rejected,
                    Some(ChunkAcceptanceRejectionReason::TransferNotContinuable),
                ),
            ),
        ] {
            assert!(message.is_reply_to(id));
            let WorkerProtocolMessage::ChunkAcceptanceDecision(decoded) =
                round_trip(WorkerProtocolMessage::from(message))
            else {
                panic!("expected ChunkAcceptanceDecision");
            };
            assert_eq!((decoded.body.outcome, decoded.body.reason), expected);
        }
    }

    #[test]
    fn non_rejected_chunk_acceptance_decision_omits_reason_never_null() {
        let wire = codec::encode(&WorkerProtocolMessage::from(
            ChunkAcceptanceDecisionMessage::committed(Uuid::new_v4()),
        ))
        .expect("encode");
        assert!(!wire.contains("reason"));
        assert!(!wire.contains("null"));
    }

    #[test]
    fn unknown_chunk_acceptance_reason_fails_decode() {
        let json = format!(
            r#"{{"type":"ChunkAcceptanceDecision","protocol_version":"1","message_id":"{}",
                "in_reply_to":"{}","outcome":"rejected","reason":"not_a_reason"}}"#,
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        assert!(codec::decode(&json).is_err());
    }

    // -- ResumeDiscovery ------------------------------------------------

    #[test]
    fn resume_discovery_query_round_trips_and_redacts_secrets() {
        let message = ResumeDiscoveryQueryMessage::new(
            "tok-secret",
            Uuid::new_v4(),
            "pid-secret",
            42,
            "sig-secret",
        );
        let debug = format!("{message:?}");
        for secret in ["tok-secret", "pid-secret", "sig-secret"] {
            assert!(!debug.contains(secret));
        }
        let WorkerProtocolMessage::ResumeDiscoveryQuery(decoded) =
            round_trip(WorkerProtocolMessage::from(message))
        else {
            panic!("expected ResumeDiscoveryQuery");
        };
        assert_eq!(decoded.body.issued_at, 42);
    }

    #[test]
    fn resume_discovery_first_page_carries_manifest_fields() {
        let request_id = Uuid::new_v4();
        let transfer_id = Uuid::new_v4();
        let held = vec![
            HeldChunk {
                chunk_index: 0,
                digest: "d0".to_string(),
            },
            HeldChunk {
                chunk_index: 1,
                digest: "d1".to_string(),
            },
        ];
        let message = ResumeDiscoveryPageMessage::first_page(
            request_id,
            transfer_id,
            true,
            WireDigestAlgorithm::Sha256,
            4096,
            Some(10),
            held.clone(),
            Some("cursor-1".to_string()),
        );
        assert!(message.is_reply_to(request_id));
        let WorkerProtocolMessage::ResumeDiscoveryPage(decoded) =
            round_trip(WorkerProtocolMessage::from(message))
        else {
            panic!("expected ResumeDiscoveryPage");
        };
        assert_eq!(decoded.body.decision, ResumeDiscoveryDecision::Approved);
        assert_eq!(decoded.body.transfer_id, Some(transfer_id));
        assert_eq!(decoded.body.sealed, Some(true));
        assert_eq!(decoded.body.expected_chunk_count, Some(10));
        assert_eq!(decoded.body.held_chunks, Some(held));
        assert_eq!(decoded.body.resume_cursor.as_deref(), Some("cursor-1"));
    }

    #[test]
    fn resume_discovery_continuation_page_omits_manifest_fields() {
        let wire = codec::encode(&WorkerProtocolMessage::from(
            ResumeDiscoveryPageMessage::continuation_page(
                Uuid::new_v4(),
                vec![HeldChunk {
                    chunk_index: 5,
                    digest: "d5".to_string(),
                }],
                None,
            ),
        ))
        .expect("encode");
        assert!(!wire.contains("transfer_id"));
        assert!(!wire.contains("digest_algorithm"));
        assert!(!wire.contains("expected_chunk_count"));
        assert!(!wire.contains("resume_cursor"));
        assert!(!wire.contains("sealed"));
    }

    #[test]
    fn denied_resume_discovery_page_carries_only_the_decision() {
        let wire = codec::encode(&WorkerProtocolMessage::from(
            ResumeDiscoveryPageMessage::denied(Uuid::new_v4()),
        ))
        .expect("encode");
        assert!(!wire.contains("held_chunks"));
        assert!(!wire.contains("resume_cursor"));
        assert!(!wire.contains("digest_algorithm"));
        assert!(!wire.contains("null"));
    }

    #[test]
    fn resume_discovery_continue_round_trips_and_redacts_cursor() {
        let message = ResumeDiscoveryContinueMessage::new("cursor-secret");
        assert!(!format!("{message:?}").contains("cursor-secret"));
        let WorkerProtocolMessage::ResumeDiscoveryContinue(decoded) =
            round_trip(WorkerProtocolMessage::from(message))
        else {
            panic!("expected ResumeDiscoveryContinue");
        };
        assert_eq!(decoded.body.resume_cursor, "cursor-secret");
    }

    // -- ManifestSeal --------------------------------------------------

    #[test]
    fn manifest_seal_request_round_trips_and_redacts_secrets() {
        let message = ManifestSealRequestMessage::new(
            "tok",
            Uuid::new_v4(),
            "pid",
            9,
            "sig",
            12,
            "artdigest",
        );
        let debug = format!("{message:?}");
        for secret in ["\"tok\"", "\"pid\"", "\"sig\""] {
            assert!(!debug.contains(secret), "debug leaked {secret}: {debug}");
        }
        let WorkerProtocolMessage::ManifestSealRequest(decoded) =
            round_trip(WorkerProtocolMessage::from(message))
        else {
            panic!("expected ManifestSealRequest");
        };
        assert_eq!(decoded.body.chunk_count, 12);
        assert_eq!(decoded.body.artifact_digest, "artdigest");
    }

    fn sealed_facts() -> SealedManifestFacts {
        SealedManifestFacts {
            verification_handle: "vh-secret".to_string(),
            artifact_id: Uuid::new_v4(),
            digest_algorithm: WireDigestAlgorithm::Sha256,
            chunk_size: 4096,
            chunk_count: 3,
            expected_artifact_digest: "exp-art-digest".to_string(),
        }
    }

    #[test]
    fn sealed_and_already_pending_decisions_carry_authoritative_values() {
        let request_id = Uuid::new_v4();
        let facts = sealed_facts();
        for message in [
            ManifestSealDecisionMessage::sealed(request_id, facts.clone()),
            ManifestSealDecisionMessage::already_pending_verification(request_id, facts.clone()),
        ] {
            assert!(message.is_reply_to(request_id));
            assert!(!format!("{message:?}").contains("vh-secret"));
            let WorkerProtocolMessage::ManifestSealDecision(decoded) =
                round_trip(WorkerProtocolMessage::from(message))
            else {
                panic!("expected ManifestSealDecision");
            };
            assert_eq!(
                decoded.body.verification_handle.as_deref(),
                Some("vh-secret")
            );
            assert_eq!(decoded.body.artifact_id, Some(facts.artifact_id));
            assert_eq!(decoded.body.chunk_count, Some(3));
            assert_eq!(
                decoded.body.expected_artifact_digest.as_deref(),
                Some("exp-art-digest")
            );
        }
    }

    #[test]
    fn rejected_and_denied_seal_decisions_carry_no_authoritative_values() {
        for wire in [
            codec::encode(&WorkerProtocolMessage::from(
                ManifestSealDecisionMessage::rejected(
                    Uuid::new_v4(),
                    ManifestSealRejectionReason::IncompleteManifest,
                ),
            ))
            .expect("encode"),
            codec::encode(&WorkerProtocolMessage::from(
                ManifestSealDecisionMessage::denied(Uuid::new_v4()),
            ))
            .expect("encode"),
        ] {
            assert!(!wire.contains("verification_handle"));
            assert!(!wire.contains("artifact_id"));
            assert!(!wire.contains("expected_artifact_digest"));
            assert!(!wire.contains("null"));
        }
    }

    #[test]
    fn denied_seal_decision_omits_reason() {
        let wire = codec::encode(&WorkerProtocolMessage::from(
            ManifestSealDecisionMessage::denied(Uuid::new_v4()),
        ))
        .expect("encode");
        assert!(!wire.contains("reason"));
    }

    // -- ArtifactVerification ----------------------------------------

    #[test]
    fn artifact_verification_report_carries_no_verdict_field() {
        let message = ArtifactVerificationReportMessage::new("vh-secret", "computed-digest");
        assert!(!format!("{message:?}").contains("vh-secret"));
        let wire = codec::encode(&WorkerProtocolMessage::from(message)).expect("encode");
        assert!(!wire.contains("matches_expected"));
        assert!(wire.contains("computed_artifact_digest"));
    }

    #[test]
    fn artifact_verification_ack_round_trips_both_statuses() {
        let id = Uuid::new_v4();
        for status in [WireArtifactStatus::Verified, WireArtifactStatus::Failed] {
            let message = ArtifactVerificationAckMessage::committed(id, status);
            assert!(message.is_reply_to(id));
            let WorkerProtocolMessage::ArtifactVerificationAck(decoded) =
                round_trip(WorkerProtocolMessage::from(message))
            else {
                panic!("expected ArtifactVerificationAck");
            };
            assert_eq!(decoded.body.artifact_status, status);
            assert_eq!(
                decoded.body.outcome,
                ArtifactVerificationAckOutcome::Committed
            );
        }
    }

    #[test]
    fn unknown_artifact_status_fails_decode() {
        let json = format!(
            r#"{{"type":"ArtifactVerificationAck","protocol_version":"1","message_id":"{}",
                "in_reply_to":"{}","outcome":"committed","artifact_status":"Partial"}}"#,
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        assert!(codec::decode(&json).is_err());
    }

    // -- Forward compatibility --------------------------------------

    #[test]
    fn unknown_fields_inside_a_known_business_message_are_ignored() {
        let json = format!(
            r#"{{"type":"ChunkAcceptanceRequest","protocol_version":"1","message_id":"{}",
                "acceptance_handle":"h","transfer_id":"{}","chunk_index":0,"digest":"d",
                "size":1,"a_future_optional_field":123}}"#,
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        assert!(codec::decode(&json).is_ok());
    }
}

/// Negative tests for `codec::decode`'s semantic-shape validation: a JSON
/// object that `serde` parses into a known message type but whose
/// outcome/decision + field combination the authoritative Specification says
/// cannot exist must decode as [`codec::DecodeError::InvalidShape`], never as
/// a valid message (`m1-worker-data-plane-control-contract.md` — the
/// per-message "present only when …" rules).
#[cfg(test)]
mod shape_validation_tests {
    use super::*;
    use crate::codec::{self, DecodeError};

    fn env() -> String {
        format!(
            r#""protocol_version":"1","message_id":"{}","in_reply_to":"{}""#,
            Uuid::new_v4(),
            Uuid::new_v4()
        )
    }

    #[track_caller]
    fn assert_invalid_shape(json: &str) {
        match codec::decode(json) {
            Err(DecodeError::InvalidShape(_)) => {}
            other => panic!("expected DecodeError::InvalidShape, got {other:?}\njson: {json}"),
        }
    }

    #[track_caller]
    fn assert_decodes(json: &str) {
        codec::decode(json)
            .unwrap_or_else(|e| panic!("expected a valid decode, got {e:?}\njson: {json}"));
    }

    // -- AuthorizationDecision -----------------------------------------

    #[test]
    fn denied_authorization_decision_with_manifest_facts_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"AuthorizationDecision",{},"decision":"denied","digest_algorithm":"sha256","chunk_size":4096,"acceptance_handle":"acc_x"}}"#,
            env()
        ));
    }

    #[test]
    fn denied_authorization_decision_with_only_expected_chunk_digest_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"AuthorizationDecision",{},"decision":"denied","expected_chunk_digest":"d"}}"#,
            env()
        ));
    }

    #[test]
    fn approved_authorization_decision_missing_digest_algorithm_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"AuthorizationDecision",{},"decision":"approved","chunk_size":4096,"acceptance_handle":"acc_x"}}"#,
            env()
        ));
    }

    #[test]
    fn approved_authorization_decision_missing_chunk_size_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"AuthorizationDecision",{},"decision":"approved","digest_algorithm":"sha256","acceptance_handle":"acc_x"}}"#,
            env()
        ));
    }

    #[test]
    fn approved_authorization_decision_missing_acceptance_handle_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"AuthorizationDecision",{},"decision":"approved","digest_algorithm":"sha256","chunk_size":4096}}"#,
            env()
        ));
    }

    #[test]
    fn well_formed_approved_and_denied_authorization_decisions_decode() {
        assert_decodes(&format!(
            r#"{{"type":"AuthorizationDecision",{},"decision":"approved","digest_algorithm":"sha256","chunk_size":4096,"acceptance_handle":"acc_x"}}"#,
            env()
        ));
        assert_decodes(&format!(
            r#"{{"type":"AuthorizationDecision",{},"decision":"denied"}}"#,
            env()
        ));
    }

    // -- ChunkAcceptanceDecision --------------------------------------

    #[test]
    fn rejected_chunk_acceptance_without_reason_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ChunkAcceptanceDecision",{},"outcome":"rejected"}}"#,
            env()
        ));
    }

    #[test]
    fn committed_chunk_acceptance_with_reason_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ChunkAcceptanceDecision",{},"outcome":"committed","reason":"chunk_identity_conflict"}}"#,
            env()
        ));
    }

    #[test]
    fn already_committed_chunk_acceptance_with_reason_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ChunkAcceptanceDecision",{},"outcome":"already_committed","reason":"transfer_not_continuable"}}"#,
            env()
        ));
    }

    #[test]
    fn unknown_chunk_acceptance_reason_still_fails_as_malformed_not_invalid_shape() {
        let json = format!(
            r#"{{"type":"ChunkAcceptanceDecision",{},"outcome":"rejected","reason":"not_a_reason"}}"#,
            env()
        );
        match codec::decode(&json) {
            Err(DecodeError::Malformed(_)) => {}
            other => panic!("expected Malformed for an unknown enum value, got {other:?}"),
        }
    }

    // -- ResumeDiscoveryPage: generic decode-time validation ----------

    #[test]
    fn denied_resume_page_with_held_chunks_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ResumeDiscoveryPage",{},"decision":"denied","held_chunks":[{{"chunk_index":0,"digest":"d"}}]}}"#,
            env()
        ));
    }

    #[test]
    fn denied_resume_page_with_manifest_metadata_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ResumeDiscoveryPage",{},"decision":"denied","digest_algorithm":"sha256","chunk_size":4096}}"#,
            env()
        ));
    }

    #[test]
    fn approved_resume_page_without_held_chunks_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ResumeDiscoveryPage",{},"decision":"approved","transfer_id":"{}","sealed":false,"digest_algorithm":"sha256","chunk_size":4096}}"#,
            env(),
            Uuid::new_v4()
        ));
    }

    #[test]
    fn approved_resume_page_with_expected_count_but_not_sealed_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ResumeDiscoveryPage",{},"decision":"approved","transfer_id":"{}","sealed":false,"digest_algorithm":"sha256","chunk_size":4096,"expected_chunk_count":3,"held_chunks":[]}}"#,
            env(),
            Uuid::new_v4()
        ));
    }

    #[test]
    fn approved_resume_page_sealed_but_missing_expected_count_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ResumeDiscoveryPage",{},"decision":"approved","transfer_id":"{}","sealed":true,"digest_algorithm":"sha256","chunk_size":4096,"held_chunks":[]}}"#,
            env(),
            Uuid::new_v4()
        ));
    }

    #[test]
    fn well_formed_resume_pages_decode() {
        // denied
        assert_decodes(&format!(
            r#"{{"type":"ResumeDiscoveryPage",{},"decision":"denied"}}"#,
            env()
        ));
        // approved first page (sealed)
        assert_decodes(&format!(
            r#"{{"type":"ResumeDiscoveryPage",{},"decision":"approved","transfer_id":"{}","sealed":true,"digest_algorithm":"sha256","chunk_size":4096,"expected_chunk_count":2,"held_chunks":[{{"chunk_index":0,"digest":"d"}}]}}"#,
            env(),
            Uuid::new_v4()
        ));
        // approved continuation page
        assert_decodes(&format!(
            r#"{{"type":"ResumeDiscoveryPage",{},"decision":"approved","held_chunks":[{{"chunk_index":5,"digest":"d"}}]}}"#,
            env()
        ));
    }

    // -- ResumeDiscoveryPage: explicit first/continuation validators ---

    fn first_page_message() -> ResumeDiscoveryPageMessage {
        ResumeDiscoveryPageMessage::first_page(
            Uuid::new_v4(),
            Uuid::new_v4(),
            true,
            WireDigestAlgorithm::Sha256,
            4096,
            Some(3),
            vec![HeldChunk {
                chunk_index: 0,
                digest: "d".to_string(),
            }],
            Some("cur".to_string()),
        )
    }

    #[test]
    fn first_page_validator_accepts_a_real_first_page_and_rejects_a_denied_one() {
        let page = first_page_message();
        let view = page.approved_first_page().expect("valid first page");
        assert!(view.sealed);
        assert_eq!(view.expected_chunk_count, Some(3));

        let denied = ResumeDiscoveryPageMessage::denied(Uuid::new_v4());
        assert!(denied.approved_first_page().is_err());
    }

    #[test]
    fn first_page_validator_rejects_missing_required_manifest_fields() {
        let mut page = first_page_message();
        page.body.digest_algorithm = None;
        assert!(page.approved_first_page().is_err());

        let mut page = first_page_message();
        page.body.transfer_id = None;
        assert!(page.approved_first_page().is_err());
    }

    #[test]
    fn first_page_validator_enforces_expected_chunk_count_iff_sealed() {
        let mut page = first_page_message();
        page.body.sealed = Some(false); // still has expected_chunk_count -> invalid
        assert!(page.approved_first_page().is_err());

        let mut page = first_page_message();
        page.body.expected_chunk_count = None; // sealed true but no count -> invalid
        assert!(page.approved_first_page().is_err());
    }

    #[test]
    fn continuation_validator_requires_held_chunks_and_rejects_manifest_fields() {
        let ok = ResumeDiscoveryPageMessage::continuation_page(
            Uuid::new_v4(),
            vec![HeldChunk {
                chunk_index: 9,
                digest: "d".to_string(),
            }],
            None,
        );
        assert!(ok.approved_continuation_page().is_ok());

        // a first page must not pass the continuation validator (it carries
        // manifest-level fields)
        assert!(first_page_message().approved_continuation_page().is_err());

        // approved but no held_chunks at all
        let mut missing =
            ResumeDiscoveryPageMessage::continuation_page(Uuid::new_v4(), vec![], None);
        missing.body.held_chunks = None;
        assert!(missing.approved_continuation_page().is_err());
    }

    // -- ManifestSealDecision ----------------------------------------

    fn seal_success_fields(uuid: Uuid) -> String {
        format!(
            r#""verification_handle":"vh","artifact_id":"{uuid}","digest_algorithm":"sha256","chunk_size":4096,"chunk_count":3,"expected_artifact_digest":"ead""#
        )
    }

    #[test]
    fn denied_seal_decision_with_success_fields_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"denied",{}}}"#,
            env(),
            seal_success_fields(Uuid::new_v4())
        ));
    }

    #[test]
    fn denied_seal_decision_with_a_reason_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"denied","reason":"incomplete_manifest"}}"#,
            env()
        ));
    }

    #[test]
    fn rejected_seal_decision_without_reason_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"rejected"}}"#,
            env()
        ));
    }

    #[test]
    fn rejected_seal_decision_with_success_fields_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"rejected","reason":"incomplete_manifest","chunk_count":3}}"#,
            env()
        ));
    }

    #[test]
    fn sealed_decision_missing_each_required_success_dimension_is_rejected() {
        let all = [
            "verification_handle",
            "artifact_id",
            "digest_algorithm",
            "chunk_size",
            "chunk_count",
            "expected_artifact_digest",
        ];
        for omitted in all {
            let uuid = Uuid::new_v4();
            let fields: Vec<String> = all
                .iter()
                .filter(|f| **f != omitted)
                .map(|f| match *f {
                    "verification_handle" => r#""verification_handle":"vh""#.to_string(),
                    "artifact_id" => format!(r#""artifact_id":"{uuid}""#),
                    "digest_algorithm" => r#""digest_algorithm":"sha256""#.to_string(),
                    "chunk_size" => r#""chunk_size":4096"#.to_string(),
                    "chunk_count" => r#""chunk_count":3"#.to_string(),
                    "expected_artifact_digest" => r#""expected_artifact_digest":"ead""#.to_string(),
                    _ => unreachable!(),
                })
                .collect();
            assert_invalid_shape(&format!(
                r#"{{"type":"ManifestSealDecision",{},"outcome":"sealed",{}}}"#,
                env(),
                fields.join(",")
            ));
        }
    }

    #[test]
    fn sealed_decision_with_a_reason_is_rejected() {
        assert_invalid_shape(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"sealed","reason":"incomplete_manifest",{}}}"#,
            env(),
            seal_success_fields(Uuid::new_v4())
        ));
    }

    #[test]
    fn already_pending_verification_obeys_the_same_success_field_requirements() {
        // complete bundle -> ok
        assert_decodes(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"already_pending_verification",{}}}"#,
            env(),
            seal_success_fields(Uuid::new_v4())
        ));
        // missing one dimension -> rejected
        assert_invalid_shape(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"already_pending_verification","verification_handle":"vh","digest_algorithm":"sha256","chunk_size":4096,"chunk_count":3,"expected_artifact_digest":"ead"}}"#,
            env()
        ));
    }

    #[test]
    fn well_formed_seal_decisions_decode() {
        assert_decodes(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"sealed",{}}}"#,
            env(),
            seal_success_fields(Uuid::new_v4())
        ));
        assert_decodes(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"rejected","reason":"manifest_already_sealed"}}"#,
            env()
        ));
        assert_decodes(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"denied"}}"#,
            env()
        ));
    }

    #[test]
    fn unknown_seal_rejection_reason_still_fails_decode() {
        let json = format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"rejected","reason":"not_a_reason"}}"#,
            env()
        );
        assert!(codec::decode(&json).is_err());
    }

    // -- Cross-cutting guarantees preserved -------------------------

    #[test]
    fn unknown_optional_future_field_on_a_conditionally_shaped_message_is_still_ignored() {
        assert_decodes(&format!(
            r#"{{"type":"AuthorizationDecision",{},"decision":"denied","some_future_optional":"x"}}"#,
            env()
        ));
        assert_decodes(&format!(
            r#"{{"type":"ManifestSealDecision",{},"outcome":"denied","some_future_optional":42}}"#,
            env()
        ));
    }

    #[test]
    fn every_constructor_emitted_message_passes_shape_validation() {
        for message in super::tests_support::one_of_each_variant() {
            message.validate_shape().unwrap_or_else(|e| {
                panic!("constructor emitted an invalid shape: {e} for {message:?}")
            });
            // and a full encode -> decode round trip must not reject it
            let wire = codec::encode(&message).expect("encode");
            codec::decode(&wire).expect("valid constructor output must decode");
        }
    }

    #[test]
    fn protocol_version_stays_one_through_every_shape_validated_message() {
        for message in super::tests_support::one_of_each_variant() {
            assert_eq!(message.envelope().protocol_version.as_str(), "1");
        }
    }
}

/// Test-support constructors shared between this crate's `codec` and
/// `messages` test modules — one representative value of every
/// [`WorkerProtocolMessage`] variant, so a variant added without wiring it
/// through `codec::KNOWN_MESSAGE_TYPES` (or the `envelope()`/`From` arms) is
/// caught by an existing test rather than silently misclassified.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    pub(crate) fn one_of_each_variant() -> Vec<WorkerProtocolMessage> {
        let id = Uuid::new_v4();
        let facts = SealedManifestFacts {
            verification_handle: "vh".to_string(),
            artifact_id: Uuid::new_v4(),
            digest_algorithm: WireDigestAlgorithm::Sha256,
            chunk_size: 4096,
            chunk_count: 2,
            expected_artifact_digest: "exp".to_string(),
        };
        vec![
            WorkerHelloMessage::new(Uuid::new_v4()).into(),
            ServerHelloMessage::new(id).into(),
            HandshakeRejectedMessage::incompatible_version(id).into(),
            AuthorizationQueryMessage::new("t", Uuid::new_v4(), 0, "p", 1, "s").into(),
            AuthorizationDecisionMessage::approved(
                id,
                WireDigestAlgorithm::Sha256,
                4096,
                "h",
                None,
            )
            .into(),
            ChunkAcceptanceRequestMessage::new("h", Uuid::new_v4(), 0, "d", 1).into(),
            ChunkAcceptanceDecisionMessage::committed(id).into(),
            ResumeDiscoveryQueryMessage::new("t", Uuid::new_v4(), "p", 1, "s").into(),
            ResumeDiscoveryPageMessage::denied(id).into(),
            ResumeDiscoveryContinueMessage::new("c").into(),
            ManifestSealRequestMessage::new("t", Uuid::new_v4(), "p", 1, "s", 2, "ad").into(),
            ManifestSealDecisionMessage::sealed(id, facts).into(),
            ArtifactVerificationReportMessage::new("vh", "cd").into(),
            ArtifactVerificationAckMessage::committed(id, WireArtifactStatus::Verified).into(),
            ProtocolErrorMessage::new("some_code").into(),
        ]
    }
}
