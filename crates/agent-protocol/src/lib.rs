//! Bamep Agent Protocol v1 — shared Rust wire-model crate
//! (`docs/specifications/m0-agent-protocol-contract.md`).
//!
//! This crate is the Rust representation of the Agent Protocol v1 wire
//! contract shared between the Server Agent Control Gateway
//! (`crates/server/src/adapters/agent_gateway.rs`) and the Agent/Simulator
//! (`crates/simulator`). The normative Markdown Specification remains the
//! authoritative source of truth (ADR-0003's contract-independence
//! constraint) — this crate does not redefine it, and must never be treated
//! as authoritative over it.
//!
//! Scope: this crate currently implements the handshake message slice
//! (`AuthRequest`, `SessionEstablished`, `AuthError`, `BootstrapEvidence` —
//! Issue #17) plus the post-session `InventoryReport` message (Issue #18),
//! plus the common envelope. It carries no transport (no WebSocket/TLS), no
//! Domain dependency, and no Server dependency: only `bamep-agent-protocol`
//! itself, `serde`, `serde_json`, `chrono`, `uuid`, and `thiserror`.

pub mod codec;
pub mod envelope;
pub mod messages;

pub use codec::{decode, encode, DecodeError, EncodeError};
pub use envelope::{
    Envelope, MessageTimestamp, Percent, PercentOutOfRange, ProtocolId, ProtocolIdError,
    ProtocolVersion,
};
pub use messages::{
    ActionAckBody, ActionAckContractError, ActionAckError, ActionAckMessage, ActionAckOutcome,
    ActionDispatchBody, ActionDispatchMessage, ActionProgressBody, ActionProgressMessage,
    ActionResultBody, ActionResultMessage, ActionResultOutcome, AgentProtocolMessage,
    AuthErrorBody, AuthErrorMessage, AuthRequestBody, AuthRequestMessage, BootstrapEvidenceBody,
    BootstrapEvidenceMessage, CancelAckBody, CancelAckMessage, CancelAckOutcome, CancelActionBody,
    CancelActionMessage, EmptyActionProgress, InventoryReportBody, InventoryReportMessage,
    LocalBootTrust, ProtocolErrorBody, ProtocolErrorMessage, SessionEstablishedBody,
    SessionEstablishedMessage,
};
