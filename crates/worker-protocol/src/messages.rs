//! Worker IPC v1 message shapes implemented by Issue #37
//! (`m1-worker-data-plane-control-contract.md` "Handshake", "Minimum
//! messages" #4 `ProtocolError`): `WorkerHello`, `ServerHello`,
//! `HandshakeRejected`, and `ProtocolError`. The business message catalog
//! (`AuthorizationQuery`/`AuthorizationDecision`, `ChunkAcceptanceRequest`/
//! `ChunkAcceptanceDecision`, `ArtifactVerificationReport`/
//! `ArtifactVerificationAck`) is out of scope for this crate today — see the
//! crate-level docs.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::envelope::{is_uuid_v4, Envelope, ProtocolVersion};

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
// AuthorizationQuery / AuthorizationDecision (Issue #38)
// ---------------------------------------------------------------------

/// Closed `operation` vocabulary
/// (`m1-worker-data-plane-control-contract.md` "Authorization query /
/// decision"; `m0-data-plane-and-storage-contracts.md` "Per-request proof").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorizationOperation {
    #[serde(rename = "chunk_upload")]
    ChunkUpload,
    #[serde(rename = "resume_discovery")]
    ResumeDiscovery,
    #[serde(rename = "seal_manifest")]
    SealManifest,
}

/// Closed `direction` vocabulary for this boundary. Only `agent_to_server` is
/// assigned in V1 (`m0-data-plane-and-storage-contracts.md` "Per-request
/// proof": "value `2` is reserved for a future Server -> Agent milestone and
/// is unassigned/rejected in V1") — an unrecognized value fails deserialization
/// explicitly rather than being silently accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireTransferDirection {
    #[serde(rename = "agent_to_server")]
    AgentToServer,
}

/// `AuthorizationQuery{token, operation, transfer_id, artifact_id, direction,
/// chunk_index?, proof_id, issued_at, signature}`
/// (`m1-worker-data-plane-control-contract.md` "Authorization query /
/// decision"). Every field beyond `operation`/`transfer_id`/`artifact_id`/
/// `direction`/`chunk_index` is opaque to this crate — `token`, `proof_id`,
/// and `signature` are forwarded verbatim exactly as Worker received them
/// from the HTTPS request, and `issued_at` is the Unix-epoch-millisecond
/// integer already carried alongside the proof (`m0-data-plane-and-storage-
/// contracts.md` "Freshness and replay representation"); reconstructing the
/// canonical proof transcript and performing the authoritative decision
/// belongs to `bamepd`'s Domain/Application layers, never this crate.
#[derive(Clone, Serialize, Deserialize)]
pub struct AuthorizationQueryBody {
    pub token: String,
    pub operation: AuthorizationOperation,
    pub transfer_id: Uuid,
    pub artifact_id: Uuid,
    pub direction: WireTransferDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_index: Option<u64>,
    pub proof_id: String,
    pub issued_at: u64,
    pub signature: String,
}

impl fmt::Debug for AuthorizationQueryBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `token`, `proof_id`/`issued_at`/`signature` proof material MUST be
        // redacted from logs/debug output
        // (`m1-worker-data-plane-control-contract.md` "Security and
        // logging"). `operation`/`transfer_id`/`artifact_id`/`direction`/
        // `chunk_index` are not secret and remain visible for diagnostics.
        f.debug_struct("AuthorizationQueryBody")
            .field("token", &"REDACTED")
            .field("operation", &self.operation)
            .field("transfer_id", &self.transfer_id)
            .field("artifact_id", &self.artifact_id)
            .field("direction", &self.direction)
            .field("chunk_index", &self.chunk_index)
            .field("proof_id", &"REDACTED")
            .field("issued_at", &"REDACTED")
            .field("signature", &"REDACTED")
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

#[allow(clippy::too_many_arguments)]
impl AuthorizationQueryMessage {
    pub fn new(
        token: impl Into<String>,
        operation: AuthorizationOperation,
        transfer_id: Uuid,
        artifact_id: Uuid,
        direction: WireTransferDirection,
        chunk_index: Option<u64>,
        proof_id: impl Into<String>,
        issued_at: u64,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: AuthorizationQueryBody {
                token: token.into(),
                operation,
                transfer_id,
                artifact_id,
                direction,
                chunk_index,
                proof_id: proof_id.into(),
                issued_at,
                signature: signature.into(),
            },
        }
    }
}

/// `AuthorizationDecision{decision, expected_chunk_digest?}`
/// (`m1-worker-data-plane-control-contract.md` "Authorization query /
/// decision"). `decision: "denied"` deliberately never carries a reason field
/// (non-enumerable denial, identical in spirit to
/// `m0-data-plane-and-storage-contracts.md`'s generic-denial requirement).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorizationDecisionOutcome {
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "denied")]
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizationDecisionBody {
    pub in_reply_to: Uuid,
    pub decision: AuthorizationDecisionOutcome,
    /// Present only when `decision: approved`, `operation: chunk_upload`,
    /// and `chunk_index` is already durable — the already-recorded expected
    /// digest for that `chunk_index`, canonical base64url-no-pad encoded.
    /// This crate treats it as an opaque wire string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_chunk_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizationDecisionMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: AuthorizationDecisionBody,
}

impl AuthorizationDecisionMessage {
    pub fn approved(in_reply_to: Uuid, expected_chunk_digest: Option<String>) -> Self {
        Self {
            envelope: Envelope::new(),
            body: AuthorizationDecisionBody {
                in_reply_to,
                decision: AuthorizationDecisionOutcome::Approved,
                expected_chunk_digest,
            },
        }
    }

    /// `denied` never carries `expected_chunk_digest`, mirroring the
    /// contract's non-enumerable-denial requirement — there is no way to
    /// construct a `denied` decision that also carries it.
    pub fn denied(in_reply_to: Uuid) -> Self {
        Self {
            envelope: Envelope::new(),
            body: AuthorizationDecisionBody {
                in_reply_to,
                decision: AuthorizationDecisionOutcome::Denied,
                expected_chunk_digest: None,
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
            WorkerProtocolMessage::ProtocolError(m) => &m.envelope,
        }
    }
}

impl From<WorkerHelloMessage> for WorkerProtocolMessage {
    fn from(value: WorkerHelloMessage) -> Self {
        WorkerProtocolMessage::WorkerHello(value)
    }
}

impl From<ServerHelloMessage> for WorkerProtocolMessage {
    fn from(value: ServerHelloMessage) -> Self {
        WorkerProtocolMessage::ServerHello(value)
    }
}

impl From<HandshakeRejectedMessage> for WorkerProtocolMessage {
    fn from(value: HandshakeRejectedMessage) -> Self {
        WorkerProtocolMessage::HandshakeRejected(value)
    }
}

impl From<ProtocolErrorMessage> for WorkerProtocolMessage {
    fn from(value: ProtocolErrorMessage) -> Self {
        WorkerProtocolMessage::ProtocolError(value)
    }
}

impl From<AuthorizationQueryMessage> for WorkerProtocolMessage {
    fn from(value: AuthorizationQueryMessage) -> Self {
        WorkerProtocolMessage::AuthorizationQuery(value)
    }
}

impl From<AuthorizationDecisionMessage> for WorkerProtocolMessage {
    fn from(value: AuthorizationDecisionMessage) -> Self {
        WorkerProtocolMessage::AuthorizationDecision(value)
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

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
    fn server_hello_with_wrong_envelope_protocol_version_is_invalid() {
        let sent_id = Uuid::new_v4();
        let mut response = ServerHelloMessage::new(sent_id);
        response.envelope.protocol_version = ProtocolVersion::new("2");
        assert!(!response.is_valid_reply_to(sent_id));
    }

    #[test]
    fn server_hello_with_non_v4_message_id_is_invalid() {
        let sent_id = Uuid::new_v4();
        let mut response = ServerHelloMessage::new(sent_id);
        response.envelope.message_id = Uuid::nil();
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
    fn server_hello_with_compatible_false_is_invalid() {
        let sent_id = Uuid::new_v4();
        let mut response = ServerHelloMessage::new(sent_id);
        response.body.compatible = false;
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
    fn handshake_rejected_with_wrong_envelope_protocol_version_is_invalid() {
        let sent_id = Uuid::new_v4();
        let mut response = HandshakeRejectedMessage::incompatible_version(sent_id);
        response.envelope.protocol_version = ProtocolVersion::new("2");
        assert!(!response.is_valid_reply_to(sent_id));
    }

    #[test]
    fn handshake_rejected_with_non_v4_message_id_is_invalid() {
        let sent_id = Uuid::new_v4();
        let mut response = HandshakeRejectedMessage::incompatible_version(sent_id);
        response.envelope.message_id = Uuid::nil();
        assert!(!response.is_valid_reply_to(sent_id));
    }

    #[test]
    fn authorization_query_round_trips_with_chunk_index() {
        let message = AuthorizationQueryMessage::new(
            "opaque-token",
            AuthorizationOperation::ChunkUpload,
            Uuid::new_v4(),
            Uuid::new_v4(),
            WireTransferDirection::AgentToServer,
            Some(3),
            "proof-id-value",
            1_700_000_000_000,
            "signature-value",
        );
        let wire =
            crate::codec::encode(&WorkerProtocolMessage::AuthorizationQuery(message.clone()))
                .expect("encode");
        let decoded = crate::codec::decode(&wire).expect("decode");
        let WorkerProtocolMessage::AuthorizationQuery(decoded) = decoded else {
            panic!("expected AuthorizationQuery");
        };
        assert_eq!(decoded.body.chunk_index, Some(3));
        assert_eq!(decoded.body.token, "opaque-token");
    }

    #[test]
    fn authorization_query_omits_chunk_index_when_absent_never_null() {
        let message = AuthorizationQueryMessage::new(
            "opaque-token",
            AuthorizationOperation::ResumeDiscovery,
            Uuid::new_v4(),
            Uuid::new_v4(),
            WireTransferDirection::AgentToServer,
            None,
            "proof-id-value",
            1_700_000_000_000,
            "signature-value",
        );
        let wire = crate::codec::encode(&WorkerProtocolMessage::AuthorizationQuery(message))
            .expect("encode");
        assert!(!wire.contains("chunk_index"));
    }

    #[test]
    fn authorization_query_debug_redacts_secret_fields() {
        let message = AuthorizationQueryMessage::new(
            "super-secret-token",
            AuthorizationOperation::ChunkUpload,
            Uuid::new_v4(),
            Uuid::new_v4(),
            WireTransferDirection::AgentToServer,
            Some(1),
            "secret-proof-id",
            1_700_000_000_000,
            "secret-signature",
        );
        let debug = format!("{message:?}");
        assert!(!debug.contains("super-secret-token"));
        assert!(!debug.contains("secret-proof-id"));
        assert!(!debug.contains("secret-signature"));
        assert!(debug.contains("REDACTED"));
        // Non-secret correlation fields remain visible for diagnostics.
        assert!(debug.contains("ChunkUpload"));
    }

    #[test]
    fn approved_decision_round_trips_with_expected_chunk_digest() {
        let request_id = Uuid::new_v4();
        let message =
            AuthorizationDecisionMessage::approved(request_id, Some("digest-value".to_string()));
        assert!(message.is_reply_to(request_id));
        let wire = crate::codec::encode(&WorkerProtocolMessage::AuthorizationDecision(
            message.clone(),
        ))
        .expect("encode");
        let decoded = crate::codec::decode(&wire).expect("decode");
        let WorkerProtocolMessage::AuthorizationDecision(decoded) = decoded else {
            panic!("expected AuthorizationDecision");
        };
        assert_eq!(
            decoded.body.decision,
            AuthorizationDecisionOutcome::Approved
        );
        assert_eq!(
            decoded.body.expected_chunk_digest,
            Some("digest-value".to_string())
        );
    }

    #[test]
    fn denied_decision_never_carries_expected_chunk_digest_on_the_wire() {
        let request_id = Uuid::new_v4();
        let message = AuthorizationDecisionMessage::denied(request_id);
        let wire = crate::codec::encode(&WorkerProtocolMessage::AuthorizationDecision(message))
            .expect("encode");
        assert!(!wire.contains("expected_chunk_digest"));
    }

    #[test]
    fn decision_reply_to_mismatch_is_detected() {
        let message = AuthorizationDecisionMessage::denied(Uuid::new_v4());
        assert!(!message.is_reply_to(Uuid::new_v4()));
    }

    #[test]
    fn unknown_operation_value_fails_decode() {
        let json = format!(
            r#"{{"type":"AuthorizationQuery","protocol_version":"1","message_id":"{}",
                "token":"t","operation":"not_a_real_operation","transfer_id":"{}",
                "artifact_id":"{}","direction":"agent_to_server","proof_id":"p",
                "issued_at":1,"signature":"s"}}"#,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        assert!(crate::codec::decode(&json).is_err());
    }

    #[test]
    fn unassigned_direction_value_fails_decode() {
        let json = format!(
            r#"{{"type":"AuthorizationQuery","protocol_version":"1","message_id":"{}",
                "token":"t","operation":"chunk_upload","transfer_id":"{}",
                "artifact_id":"{}","direction":"server_to_agent","chunk_index":0,
                "proof_id":"p","issued_at":1,"signature":"s"}}"#,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        assert!(crate::codec::decode(&json).is_err());
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
        assert!(crate::codec::decode(json).is_err());
    }
}
