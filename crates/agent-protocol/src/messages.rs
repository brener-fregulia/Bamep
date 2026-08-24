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

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::envelope::{Envelope, MessageTimestamp, Percent, ProtocolId};

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
// ActionDispatch — Server -> Agent
// ---------------------------------------------------------------------

/// `ActionDispatch{action_id, action_type, action_version, parameters,
/// retry_of?}` (`m0-agent-protocol-contract.md` "Message types", "Action
/// field contract"). `action_type`/`parameters` schemas are owned by the
/// Specification introducing the concrete `action_type` — this crate treats
/// both as opaque. `retry_of`, when present, is a UUID v4 `action_id`
/// referencing the action being retried; Issue #26 never sets it (it creates
/// no retry) but the field is represented so the generic wire shape is
/// already complete for a later retry-issuing Work Package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDispatchBody {
    pub action_id: ProtocolId,
    pub action_type: String,
    pub action_version: String,
    pub parameters: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<ProtocolId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDispatchMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ActionDispatchBody,
}

impl ActionDispatchMessage {
    /// A fresh envelope whose `correlation_id` is always `action_id`
    /// (`m0-agent-protocol-contract.md` "Message envelope": "Every
    /// action-scoped message MUST have `correlation_id` equal to its
    /// relevant `action_id`") — there is no way to construct this message
    /// without that correlation already set.
    pub fn new(
        action_id: ProtocolId,
        action_type: impl Into<String>,
        action_version: impl Into<String>,
        parameters: Map<String, Value>,
    ) -> Self {
        Self {
            envelope: Envelope::new().with_correlation_id(action_id),
            body: ActionDispatchBody {
                action_id,
                action_type: action_type.into(),
                action_version: action_version.into(),
                parameters,
                retry_of: None,
            },
        }
    }

    /// `retry_of` must differ from this message's own `action_id`
    /// (`m0-agent-protocol-contract.md` "Action field contract") — callers
    /// constructing a genuine retry are responsible for that distinctness;
    /// this crate does not itself own retry policy.
    pub fn with_retry_of(mut self, retry_of: ProtocolId) -> Self {
        self.body.retry_of = Some(retry_of);
        self
    }
}

// ---------------------------------------------------------------------
// ActionAck — Agent -> Server
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionAckOutcome {
    Accepted,
    Rejected,
}

/// Rejects a wire-invalid `ActionAck`/`ActionAckError` combination
/// (`m0-agent-protocol-contract.md` "ActionAck diagnostic shape"): a derived
/// `Deserialize` alone would accept several invalid forms — `Accepted`
/// carrying `error`, `Rejected` carrying no `error`, or an `error` with an
/// empty `code`/`message` — that our own constructors never produce but
/// untrusted wire input is not guaranteed to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ActionAckContractError {
    #[error("ActionAck.error.code must be non-empty")]
    EmptyErrorCode,
    #[error("ActionAck.error.message, when present, must be non-empty")]
    EmptyErrorMessage,
    #[error("ActionAck{{outcome: Accepted}} must never carry error")]
    AcceptedWithError,
    #[error("ActionAck{{outcome: Rejected}} must always carry error")]
    RejectedWithoutError,
}

/// `ActionAck.error` diagnostic shape (`m0-agent-protocol-contract.md`
/// "ActionAck diagnostic shape"): present only for `outcome: Rejected`.
/// `code` is a non-empty stable diagnostic string owned by the Specification
/// that owns the concrete `action_type`; this crate does not interpret it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionAckError {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ActionAckError {
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: None,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
}

impl<'de> Deserialize<'de> for ActionAckError {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            code: String,
            #[serde(default)]
            message: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.code.is_empty() {
            return Err(D::Error::custom(ActionAckContractError::EmptyErrorCode));
        }
        if raw.message.as_deref() == Some("") {
            return Err(D::Error::custom(ActionAckContractError::EmptyErrorMessage));
        }
        Ok(Self {
            code: raw.code,
            message: raw.message,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionAckBody {
    pub action_id: ProtocolId,
    pub outcome: ActionAckOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ActionAckError>,
}

impl<'de> Deserialize<'de> for ActionAckBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            action_id: ProtocolId,
            outcome: ActionAckOutcome,
            #[serde(default)]
            error: Option<ActionAckError>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match (raw.outcome, &raw.error) {
            (ActionAckOutcome::Accepted, Some(_)) => {
                return Err(D::Error::custom(ActionAckContractError::AcceptedWithError))
            }
            (ActionAckOutcome::Rejected, None) => {
                return Err(D::Error::custom(
                    ActionAckContractError::RejectedWithoutError,
                ))
            }
            (ActionAckOutcome::Accepted, None) | (ActionAckOutcome::Rejected, Some(_)) => {}
        }
        Ok(Self {
            action_id: raw.action_id,
            outcome: raw.outcome,
            error: raw.error,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionAckMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ActionAckBody,
}

impl ActionAckMessage {
    /// `outcome: Accepted` — `error` is always absent, never `null`
    /// (`m0-agent-protocol-contract.md` "ActionAck diagnostic shape":
    /// "absent when `outcome: Accepted`").
    pub fn accepted(action_id: ProtocolId) -> Self {
        Self {
            envelope: Envelope::new().with_correlation_id(action_id),
            body: ActionAckBody {
                action_id,
                outcome: ActionAckOutcome::Accepted,
                error: None,
            },
        }
    }

    /// `outcome: Rejected` — `error` is always present.
    pub fn rejected(action_id: ProtocolId, error: ActionAckError) -> Self {
        Self {
            envelope: Envelope::new().with_correlation_id(action_id),
            body: ActionAckBody {
                action_id,
                outcome: ActionAckOutcome::Rejected,
                error: Some(error),
            },
        }
    }

    /// Re-emits this already-constructed Ack under a fresh `message_id`
    /// (`m0-agent-protocol-contract.md` "Message envelope": "`message_id` is
    /// a fresh UUID v4 for every message transmission, including when
    /// retained semantic evidence ... is resent"). `correlation_id`
    /// (`action_id`) and every body field are preserved exactly.
    pub fn with_fresh_message_id(mut self) -> Self {
        self.envelope.message_id = ProtocolId::generate();
        self.envelope.timestamp = MessageTimestamp::now();
        self
    }
}

// ---------------------------------------------------------------------
// ActionProgress — Agent -> Server
// ---------------------------------------------------------------------

/// Rejects an `ActionProgress` with every field absent
/// (`m0-agent-protocol-contract.md` "ActionProgress fields": "at least one of
/// `percent`, `bytes_processed`, `eta` must be present").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("ActionProgress requires at least one of percent, bytes_processed, eta")]
pub struct EmptyActionProgress;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionProgressBody {
    pub action_id: ProtocolId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<Percent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_processed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta: Option<MessageTimestamp>,
}

impl<'de> Deserialize<'de> for ActionProgressBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            action_id: ProtocolId,
            #[serde(default)]
            percent: Option<Percent>,
            #[serde(default)]
            bytes_processed: Option<u64>,
            #[serde(default)]
            eta: Option<MessageTimestamp>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.percent.is_none() && raw.bytes_processed.is_none() && raw.eta.is_none() {
            return Err(D::Error::custom(EmptyActionProgress));
        }
        Ok(Self {
            action_id: raw.action_id,
            percent: raw.percent,
            bytes_processed: raw.bytes_processed,
            eta: raw.eta,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionProgressMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ActionProgressBody,
}

impl ActionProgressMessage {
    /// Generic constructor over the full field set the Specification allows
    /// — rejects the all-absent case explicitly rather than constructing a
    /// message with no reportable evidence at all.
    pub fn new(
        action_id: ProtocolId,
        percent: Option<Percent>,
        bytes_processed: Option<u64>,
        eta: Option<MessageTimestamp>,
    ) -> Result<Self, EmptyActionProgress> {
        if percent.is_none() && bytes_processed.is_none() && eta.is_none() {
            return Err(EmptyActionProgress);
        }
        Ok(Self {
            envelope: Envelope::new().with_correlation_id(action_id),
            body: ActionProgressBody {
                action_id,
                percent,
                bytes_processed,
                eta,
            },
        })
    }

    /// The M1 `bamep.m1.simulated-execution` action reports `percent` only
    /// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-004).
    pub fn percent(action_id: ProtocolId, percent: Percent) -> Self {
        Self::new(action_id, Some(percent), None, None)
            .expect("percent is always present, so this can never be empty")
    }

    /// Re-emits this Progress under a fresh `message_id`, exactly like
    /// [`ActionAckMessage::with_fresh_message_id`].
    pub fn with_fresh_message_id(mut self) -> Self {
        self.envelope.message_id = ProtocolId::generate();
        self.envelope.timestamp = MessageTimestamp::now();
        self
    }
}

// ---------------------------------------------------------------------
// ActionResult — Agent -> Server
// ---------------------------------------------------------------------

/// `ActionResult.outcome` uses only the terminal execution values
/// (`m0-agent-protocol-contract.md` "Agent-action state vocabulary").
/// `CancelAction`/`CancelAck` are owned by Issue #27; Issue #26 handles only
/// `Succeeded`/`Failed` normal execution, but `Cancelled` is structurally
/// required here because it is already part of the closed generic
/// `ActionResult.outcome` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionResultOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResultBody {
    pub action_id: ProtocolId,
    pub outcome: ActionResultOutcome,
    /// `detail` schema is owned by the Specification that owns the concrete
    /// `action_type`; this crate treats it as opaque.
    pub detail: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResultMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: ActionResultBody,
}

impl ActionResultMessage {
    pub fn new(
        action_id: ProtocolId,
        outcome: ActionResultOutcome,
        detail: Map<String, Value>,
    ) -> Self {
        Self {
            envelope: Envelope::new().with_correlation_id(action_id),
            body: ActionResultBody {
                action_id,
                outcome,
                detail,
            },
        }
    }

    /// Re-emits this already-committed Result under a fresh `message_id`,
    /// exactly like [`ActionAckMessage::with_fresh_message_id`].
    pub fn with_fresh_message_id(mut self) -> Self {
        self.envelope.message_id = ProtocolId::generate();
        self.envelope.timestamp = MessageTimestamp::now();
        self
    }
}

// ---------------------------------------------------------------------
// CancelAction — Server -> Agent
// ---------------------------------------------------------------------

/// `CancelAction{action_id}` (`m0-agent-protocol-contract.md` "Message
/// types"). `action_id` is always the exact existing action identity being
/// cancelled — Issue #27 never generates a replacement action identity for a
/// cancellation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelActionBody {
    pub action_id: ProtocolId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelActionMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: CancelActionBody,
}

impl CancelActionMessage {
    /// A fresh envelope whose `correlation_id` is always `action_id`, exactly
    /// like [`ActionDispatchMessage::new`].
    pub fn new(action_id: ProtocolId) -> Self {
        Self {
            envelope: Envelope::new().with_correlation_id(action_id),
            body: CancelActionBody { action_id },
        }
    }
}

// ---------------------------------------------------------------------
// CancelAck — Agent -> Server
// ---------------------------------------------------------------------

/// `CancelAck.outcome` (`m0-agent-protocol-contract.md` "Message types"):
/// `CannotCancel` means the Agent knows the action but cannot stop it;
/// `Unknown` means no authoritative local state exists — never "not
/// executed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelAckOutcome {
    Cancelled,
    AlreadyCompleted,
    CannotCancel,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelAckBody {
    pub action_id: ProtocolId,
    pub outcome: CancelAckOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelAckMessage {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub body: CancelAckBody,
}

impl CancelAckMessage {
    pub fn new(action_id: ProtocolId, outcome: CancelAckOutcome) -> Self {
        Self {
            envelope: Envelope::new().with_correlation_id(action_id),
            body: CancelAckBody { action_id, outcome },
        }
    }

    /// Re-emits this already-constructed Ack under a fresh `message_id`,
    /// exactly like [`ActionAckMessage::with_fresh_message_id`] — used for
    /// Agent-side idempotency when a known already-cancelled `action_id`
    /// receives another `CancelAction`.
    pub fn with_fresh_message_id(mut self) -> Self {
        self.envelope.message_id = ProtocolId::generate();
        self.envelope.timestamp = MessageTimestamp::now();
        self
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
    ActionDispatch(ActionDispatchMessage),
    ActionAck(ActionAckMessage),
    ActionProgress(ActionProgressMessage),
    ActionResult(ActionResultMessage),
    CancelAction(CancelActionMessage),
    CancelAck(CancelAckMessage),
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
            AgentProtocolMessage::ActionDispatch(m) => &m.envelope,
            AgentProtocolMessage::ActionAck(m) => &m.envelope,
            AgentProtocolMessage::ActionProgress(m) => &m.envelope,
            AgentProtocolMessage::ActionResult(m) => &m.envelope,
            AgentProtocolMessage::CancelAction(m) => &m.envelope,
            AgentProtocolMessage::CancelAck(m) => &m.envelope,
            AgentProtocolMessage::ProtocolError(m) => &m.envelope,
        }
    }
}
