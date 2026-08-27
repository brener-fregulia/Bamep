//! Bamep Worker IPC v1 — shared Rust wire-model crate
//! (`docs/specifications/m1-worker-data-plane-control-contract.md`).
//!
//! This crate is the Rust representation of the local Unix Domain Socket
//! contract between `bamepd` (the UDS server) and the isolated Worker
//! process (the reconnecting UDS client), per ADR-0018. The normative
//! Markdown Specification remains the authoritative source of truth
//! (ADR-0003's contract-independence constraint) — this crate does not
//! redefine it, and must never be treated as authoritative over it.
//!
//! Scope: this crate implements the handshake/protocol-error message slice
//! `WorkerHello`/`ServerHello`/`HandshakeRejected`/`ProtocolError` (Issue
//! #37) plus `AuthorizationQuery`/`AuthorizationDecision` (Issue #38), plus
//! the common envelope, framing, and codec machinery. The remaining business
//! message catalog (`ChunkAcceptanceRequest`/`ChunkAcceptanceDecision`,
//! `ArtifactVerificationReport`/`ArtifactVerificationAck`) is deliberately
//! not represented here yet — #39 does not yet implement their business
//! behavior, and this crate does not add unused wire machinery ahead of the
//! Work Package that needs it.
//!
//! This crate carries no Domain, Server, PostgreSQL, HTTP framework, or
//! storage dependency: only `bamep-worker-protocol` itself, `serde`,
//! `serde_json`, `uuid`, `thiserror`, and `tokio`'s `io-util` feature (the
//! generic `AsyncRead`/`AsyncWrite` traits the length-prefixed framing in
//! [`framing`] is built on — no `net`, `rt`, or `process` feature, so this
//! crate itself opens no socket and spawns no task).

pub mod codec;
pub mod envelope;
pub mod framing;
pub mod messages;

pub use codec::{decode, encode, DecodeError, EncodeError};
pub use envelope::{is_uuid_v4, Envelope, ProtocolVersion, PROTOCOL_VERSION_V1};
pub use framing::{
    read_frame, receive, send, write_frame, FrameReadError, FrameWriteError, ReceiveError,
    SendError, MAX_FRAME_PAYLOAD_BYTES,
};
pub use messages::{
    AuthorizationDecisionBody, AuthorizationDecisionMessage, AuthorizationDecisionOutcome,
    AuthorizationOperation, AuthorizationQueryBody, AuthorizationQueryMessage,
    HandshakeRejectedBody, HandshakeRejectedMessage, HandshakeRejectionReason, ProtocolErrorBody,
    ProtocolErrorMessage, ServerHelloBody, ServerHelloMessage, WireTransferDirection,
    WorkerHelloBody, WorkerHelloMessage, WorkerProtocolMessage,
};
