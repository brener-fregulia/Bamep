//! The currently implemented Agent Protocol v1 message set
//! (`docs/specifications/m0-agent-protocol-contract.md` "Message types"):
//! `AuthRequest`, `SessionEstablished`, `AuthError` (Agent Protocol
//! handshake), `BootstrapEvidence` (trusted-bootstrap evidence report), and
//! `InventoryReport` (post-session opaque observed inventory snapshot).
//!
//! Every field beyond the common [`Envelope`] is opaque to this crate:
//! `credential`, `runtime_credential`, `bootstrap_assertion`, and
//! `boot_nonce` are carried as plain wire strings, never parsed or
//! interpreted here. Parsing `credential`/`runtime_credential` as a Domain
//! `PresentedCredential`, and verifying `bootstrap_assertion`, belong to the
//! Server's Domain/Application boundaries — this crate does not depend on
//! `bamep-domain`.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::envelope::{Envelope, MessageTimestamp, ProtocolId};

// ---------------------------------------------------------------------
// AuthRequest — Agent -> Server
// ---------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthRequestBody {
    /// Opaque presented credential wire value. Never parsed here — the
    /// Server Gateway (`agent_gateway.rs`) passes it, unmodified, to
    /// `EnrollmentService::redeem`.
    pub credential: String,
}

impl fmt::Debug for AuthRequestBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthRequestBody")
            .field("credential", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthRequestMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: AuthRequestBody,
}

impl fmt::Debug for AuthRequestMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthRequestMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl AuthRequestMessage {
    pub fn new(credential: impl Into<String>) -> Self {
        Self {
            envelope: Envelope::new(),
            body: AuthRequestBody {
                credential: credential.into(),
            },
        }
    }

    pub fn with_correlation_id(mut self, correlation_id: ProtocolId) -> Self {
        self.envelope = self.envelope.with_correlation_id(correlation_id);
        self
    }
}

// ---------------------------------------------------------------------
// SessionEstablished — Server -> Agent
// ---------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionEstablishedBody {
    pub session_id: ProtocolId,
    /// Opaque freshly issued runtime credential wire value (ADR-0012). The
    /// Server Gateway (`agent_gateway.rs`) converts the Domain
    /// `PresentedCredential` produced by `RedeemResult` into this value at
    /// the boundary.
    pub runtime_credential: String,
    pub credential_expires_at: MessageTimestamp,
}

impl fmt::Debug for SessionEstablishedBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionEstablishedBody")
            .field("session_id", &self.session_id)
            .field("runtime_credential", &"REDACTED")
            .field("credential_expires_at", &self.credential_expires_at)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionEstablishedMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: SessionEstablishedBody,
}

impl fmt::Debug for SessionEstablishedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionEstablishedMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl SessionEstablishedMessage {
    pub fn new(
        session_id: ProtocolId,
        runtime_credential: impl Into<String>,
        credential_expires_at: MessageTimestamp,
    ) -> Self {
        Self {
            envelope: Envelope::new(),
            body: SessionEstablishedBody {
                session_id,
                runtime_credential: runtime_credential.into(),
                credential_expires_at,
            },
        }
    }

    pub fn with_correlation_id(mut self, correlation_id: ProtocolId) -> Self {
        self.envelope = self.envelope.with_correlation_id(correlation_id);
        self
    }
}

// ---------------------------------------------------------------------
// AuthError — Server -> Agent
// ---------------------------------------------------------------------

/// `reason` is the smallest opaque textual representation compatible with
/// the approved contract: the normative Specification does not currently
/// define a richer closed `AuthError` reason taxonomy, and this checkpoint
/// does not invent one. Callers must not encode internal credential-failure
/// detail into it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthErrorBody {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthErrorMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: AuthErrorBody,
}

impl AuthErrorMessage {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            envelope: Envelope::new(),
            body: AuthErrorBody {
                reason: reason.into(),
            },
        }
    }

    pub fn with_correlation_id(mut self, correlation_id: ProtocolId) -> Self {
        self.envelope = self.envelope.with_correlation_id(correlation_id);
        self
    }
}

// ---------------------------------------------------------------------
// BootstrapEvidence — Agent -> Server
// ---------------------------------------------------------------------

/// Closed M0 `local_boot_trust` vocabulary
/// (`m0-agent-protocol-contract.md` "Message types"): exactly one value.
/// A local failure to establish trusted bootstrap prevents the Agent from
/// sending `BootstrapEvidence` at all, so no other value exists in M0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalBootTrust {
    Established,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BootstrapEvidenceBody {
    /// Boot-context correlator; opaque here, semantically owned by the
    /// trusted-bootstrap contract.
    pub boot_nonce: String,
    /// Opaque nonce-bound signed bootstrap assertion. Its algorithm and
    /// internal serialization are implementation-time and out of scope for
    /// this checkpoint.
    pub bootstrap_assertion: String,
    pub local_boot_trust: LocalBootTrust,
}

impl fmt::Debug for BootstrapEvidenceBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootstrapEvidenceBody")
            .field("boot_nonce", &self.boot_nonce)
            .field("bootstrap_assertion", &"REDACTED")
            .field("local_boot_trust", &self.local_boot_trust)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BootstrapEvidenceMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: BootstrapEvidenceBody,
}

// ---------------------------------------------------------------------
// ProtocolError — bidirectional post-session protocol violation
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolErrorMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ProtocolErrorBody,
}

impl ProtocolErrorMessage {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            envelope: Envelope::new(),
            body: ProtocolErrorBody {
                code: code.into(),
                message: message.into(),
            },
        }
    }

    pub fn with_correlation_id(mut self, correlation_id: ProtocolId) -> Self {
        self.envelope = self.envelope.with_correlation_id(correlation_id);
        self
    }
}

impl fmt::Debug for BootstrapEvidenceMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootstrapEvidenceMessage")
            .field("envelope", &self.envelope)
            .field("body", &self.body)
            .finish()
    }
}

impl BootstrapEvidenceMessage {
    pub fn new(boot_nonce: impl Into<String>, bootstrap_assertion: impl Into<String>) -> Self {
        Self {
            envelope: Envelope::new(),
            body: BootstrapEvidenceBody {
                boot_nonce: boot_nonce.into(),
                bootstrap_assertion: bootstrap_assertion.into(),
                local_boot_trust: LocalBootTrust::Established,
            },
        }
    }

    pub fn with_correlation_id(mut self, correlation_id: ProtocolId) -> Self {
        self.envelope = self.envelope.with_correlation_id(correlation_id);
        self
    }
}

// ---------------------------------------------------------------------
// InventoryReport — Agent -> Server
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InventoryReportBody {
    /// Opaque structured inventory. Agent Protocol constrains this to a JSON
    /// object but does not interpret its fields.
    pub inventory: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryReportMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: InventoryReportBody,
}

impl InventoryReportMessage {
    pub fn new(inventory: Map<String, Value>) -> Self {
        Self {
            envelope: Envelope::new(),
            body: InventoryReportBody { inventory },
        }
    }
}

// ---------------------------------------------------------------------
// Top-level message union
// ---------------------------------------------------------------------

/// The currently implemented Agent Protocol v1 messages, internally tagged
/// on the wire `"type"` field with exactly the normative message names.
///
/// An unrecognized `"type"` value fails deserialization explicitly (a
/// `serde` "unknown variant" error) rather than silently falling back to any
/// variant — satisfying "Unknown top-level message type must fail parsing as
/// an unknown message type." Generating the corresponding `AuthError` /
/// `ProtocolError` response belongs to the Server's handshake/session
/// handler (`agent_gateway.rs`), not this crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentProtocolMessage {
    AuthRequest(AuthRequestMessage),
    SessionEstablished(SessionEstablishedMessage),
    AuthError(AuthErrorMessage),
    BootstrapEvidence(BootstrapEvidenceMessage),
    InventoryReport(InventoryReportMessage),
    ProtocolError(ProtocolErrorMessage),
}

impl AgentProtocolMessage {
    pub fn envelope(&self) -> &Envelope {
        match self {
            AgentProtocolMessage::AuthRequest(m) => &m.envelope,
            AgentProtocolMessage::SessionEstablished(m) => &m.envelope,
            AgentProtocolMessage::AuthError(m) => &m.envelope,
            AgentProtocolMessage::BootstrapEvidence(m) => &m.envelope,
            AgentProtocolMessage::InventoryReport(m) => &m.envelope,
            AgentProtocolMessage::ProtocolError(m) => &m.envelope,
        }
    }
}
