//! Worker IPC v1 message shapes implemented by Issue #37
//! (`m1-worker-data-plane-control-contract.md` "Handshake", "Minimum
//! messages" #4 `ProtocolError`): `WorkerHello`, `ServerHello`,
//! `HandshakeRejected`, and `ProtocolError`. The business message catalog
//! (`AuthorizationQuery`/`AuthorizationDecision`, `ChunkAcceptanceRequest`/
//! `ChunkAcceptanceDecision`, `ArtifactVerificationReport`/
//! `ArtifactVerificationAck`) is out of scope for this crate today — see the
//! crate-level docs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::envelope::{Envelope, ProtocolVersion};

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
    ProtocolError(ProtocolErrorMessage),
}

impl WorkerProtocolMessage {
    pub fn envelope(&self) -> &Envelope {
        match self {
            WorkerProtocolMessage::WorkerHello(m) => &m.envelope,
            WorkerProtocolMessage::ServerHello(m) => &m.envelope,
            WorkerProtocolMessage::HandshakeRejected(m) => &m.envelope,
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
