//! Bamep Simulator — Agent-side real Agent Protocol v1 WSS transport client
//! (`docs/specifications/m0-simulator-contract-and-validation-strategy.md`
//! "Simulator fidelity boundary": a Simulated Endpoint's Agent participant
//! must use the real WSS transport end-to-end, never an in-process fake).
//!
//! Scope: the transport checkpoint (Issue #17 WP1) proved the transport
//! boundary — pinned exact-leaf-certificate TLS 1.3 Server-identity
//! verification strictly before the WebSocket Upgrade, and real Agent
//! Protocol JSON-over-WebSocket carriage. A later transport checkpoint added
//! the Simulator-side handshake helper ([`handshake::authenticate`]) that
//! sends `AuthRequest` and validates the `SessionEstablished`/`AuthError`
//! response over an already-established WSS connection. This checkpoint
//! adds local trusted-bootstrap establishment
//! ([`trusted_bootstrap::establish_trusted_bootstrap`]) and the
//! establish-then-connect composition helper
//! ([`trusted_bootstrap::connect_after_trusted_bootstrap`]) and sends the
//! retained assertion as post-authentication `BootstrapEvidence`. Issue #18
//! added the post-session inventory-reporting helper
//! ([`handshake::send_inventory_report`]).
//!
//! Production dependency direction: `bamep-simulator` depends on
//! `bamep-agent-protocol` for the wire model and on
//! `bamep-trusted-bootstrap` for the trusted-bootstrap contract primitives.
//! It does not depend on `bamep-domain` or `bamep-server`.

pub mod action;
pub mod data_plane;
pub mod handshake;
pub mod transfer_action;
pub mod transfer_authorization;
pub mod transfer_source;
pub mod transport;
pub mod trusted_bootstrap;
pub mod verifier;

pub use action::{
    CancelBehavior, ScenarioOutcome, SimulatedActionAgent, M1_ACTION_TYPE, M1_ACTION_VERSION,
};
pub use bamep_trusted_bootstrap::ServerCertFingerprint;
pub use data_plane::{
    DataPlaneClient, DataPlaneClientError, DataPlaneTransportError, HeldChunk as ResumeHeldChunk,
    PutChunkOutcome, ResumeManifest, ResumeOutcome, SealArtifactStatus, SealOutcome,
    DEFAULT_REQUEST_TIMEOUT as DATA_PLANE_REQUEST_TIMEOUT_DEFAULT,
};
pub use handshake::{
    authenticate, send_bootstrap_evidence, send_inventory_report, SimulatorHandshakeError,
    SimulatorHandshakeOutcome,
};
pub use transfer_action::{
    parse_transfer_dispatch_parameters, AcceptedTransfer, DataPlaneTransferAgent,
    DataPlaneTransferParams, SuspendReason, SuspendedTransfer, TransferActionResult,
    TransferDigestAlgorithm, TransferDispatchRejection, TransferDispatchResponse, TransferProgress,
    TransferRunError, TransferRunOptions, TransferRunOutcome, M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
    M1_DATA_PLANE_TRANSFER_ACTION_VERSION,
};
pub use transfer_authorization::{
    build_proof_transcript, AgentProofKey, AgentTransferAuthorization, ProofError, ProofId,
    TransferDirection as DataPlaneTransferDirection, TransferOperation, TransferProof,
    PROOF_TRANSCRIPT_LEN as DATA_PLANE_PROOF_TRANSCRIPT_LEN,
};
pub use transfer_source::{InMemoryTransferSource, TransferSource};
pub use transport::{connect_pinned_wss, SimulatorTransportError};
pub use trusted_bootstrap::{
    connect_after_trusted_bootstrap, establish_trusted_bootstrap,
    ConnectAfterTrustedBootstrapError, EstablishedTrustedBootstrap, LocalBootstrapError,
    SimulatedBootstrapMaterial, SimulatedPairedTrust, TrustedBootstrapConnection,
    TrustedBootstrapFixtureError, TrustedBootstrapFixtureIssuer,
};
pub use verifier::{pinned_tls13_client_config, PinnedServerCertVerifier};
