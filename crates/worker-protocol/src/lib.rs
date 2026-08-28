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
//! Scope: this crate implements the **complete** Worker Protocol v1 message
//! catalog (`m1-worker-data-plane-control-contract.md` "Minimum messages"):
//! the handshake/error slice `WorkerHello`/`ServerHello`/`HandshakeRejected`/
//! `ProtocolError`, chunk-upload authorization `AuthorizationQuery`/
//! `AuthorizationDecision`, verified-chunk durable acceptance
//! `ChunkAcceptanceRequest`/`ChunkAcceptanceDecision`, resume discovery and
//! pagination `ResumeDiscoveryQuery`/`ResumeDiscoveryPage`/
//! `ResumeDiscoveryContinue`, seal first durable commit `ManifestSealRequest`/
//! `ManifestSealDecision`, and full-Artifact verification
//! `ArtifactVerificationReport`/`ArtifactVerificationAck` — plus the common
//! envelope, framing, and codec machinery. #39 completes the partial #37/#38
//! rendering to this catalog without a `protocol_version` increment
//! (`m1-worker-data-plane-control-contract.md` "Freeze point for v1").
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
    ArtifactVerificationAckBody, ArtifactVerificationAckMessage, ArtifactVerificationAckOutcome,
    ArtifactVerificationReportBody, ArtifactVerificationReportMessage, AuthorizationDecisionBody,
    AuthorizationDecisionMessage, AuthorizationDecisionOutcome, AuthorizationQueryBody,
    AuthorizationQueryMessage, ChunkAcceptanceDecisionBody, ChunkAcceptanceDecisionMessage,
    ChunkAcceptanceOutcome, ChunkAcceptanceRejectionReason, ChunkAcceptanceRequestBody,
    ChunkAcceptanceRequestMessage, HandshakeRejectedBody, HandshakeRejectedMessage,
    HandshakeRejectionReason, HeldChunk, ManifestSealDecisionBody, ManifestSealDecisionMessage,
    ManifestSealOutcome, ManifestSealRejectionReason, ManifestSealRequestBody,
    ManifestSealRequestMessage, ProtocolErrorBody, ProtocolErrorMessage,
    ResumeDiscoveryContinueBody, ResumeDiscoveryContinueMessage, ResumeDiscoveryDecision,
    ResumeDiscoveryPageBody, ResumeDiscoveryPageMessage, ResumeDiscoveryQueryBody,
    ResumeDiscoveryQueryMessage, SealedManifestFacts, ServerHelloBody, ServerHelloMessage,
    WireArtifactStatus, WireDigestAlgorithm, WorkerHelloBody, WorkerHelloMessage,
    WorkerProtocolMessage,
};
