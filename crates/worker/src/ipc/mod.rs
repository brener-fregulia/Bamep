//! The Worker-side half of the `bamepd` <-> Worker UDS control boundary
//! (Issue #37 handshake/reconnect slice; Issue #39 Phase E1 completes it into
//! the concurrent Worker Protocol v1 control client;
//! `docs/specifications/m1-worker-data-plane-control-contract.md`): Worker is
//! the reconnecting UDS client, `bamepd` is the UDS server (ADR-0018
//! "Server<->Worker IPC").

pub mod authority;
pub mod control;

pub use authority::{AuthorityPhase, AuthoritySnapshot, AuthorityTracker};
pub use control::{
    worker_control, AcceptanceTicket, ArtifactVerification, AuthorizeChunkInput, ChunkAcceptance,
    ChunkAuthorization, ChunkAuthorizationApproved, ControlDriver, ControlError, LocalGeneration,
    ManifestSeal, ManifestSealInput, ResumeAggregate, ResumeDiscovery, ResumeDiscoveryInput,
    SealSuccess, VerificationTicket, WorkerControlHandle,
};
