//! Bamep Agent Protocol v1 — shared Rust wire-model crate
//! (`docs/specifications/m0-agent-protocol-contract.md`).
//!
//! This crate is the Rust representation of the Agent Protocol v1 wire
//! contract shared between the future Server Agent Control Gateway and the
//! future Agent/Simulator. The normative Markdown Specification remains the
//! authoritative source of truth (ADR-0003's contract-independence
//! constraint) — this crate does not redefine it, and must never be treated
//! as authoritative over it.
//!
//! Scope: this checkpoint implements only the WP1 handshake/evidence message
//! slice required by Issue #17 — `AuthRequest`, `SessionEstablished`,
//! `AuthError`, `BootstrapEvidence`, `InventoryReport` — plus the common envelope. It carries no
//! transport (no WebSocket/TLS), no Domain dependency, and no Server
//! dependency: only `bamep-agent-protocol` itself, `serde`, `serde_json`,
//! `chrono`, `uuid`, and `thiserror`.

pub mod codec;
pub mod envelope;
pub mod messages;

pub use codec::{decode, encode, DecodeError, EncodeError};
pub use envelope::{Envelope, MessageTimestamp, ProtocolId, ProtocolIdError, ProtocolVersion};
pub use messages::{
    AgentProtocolMessage, AuthErrorBody, AuthErrorMessage, AuthRequestBody, AuthRequestMessage,
    BootstrapEvidenceBody, BootstrapEvidenceMessage, InventoryReportBody, InventoryReportMessage,
    LocalBootTrust, ProtocolErrorBody, ProtocolErrorMessage, SessionEstablishedBody,
    SessionEstablishedMessage,
};
