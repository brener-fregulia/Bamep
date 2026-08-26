//! Bamep Domain: Endpoint identity lifecycle and runtime-credential chain.
//!
//! Pure business logic only — no I/O, no SQLite, no WebSocket, no clock or
//! RNG access hidden inside a transition (`AGENTS.md` "Architecture and
//! dependencies"; `docs/specifications/m0-stack-and-boundaries-baseline.md`
//! "Component responsibilities and boundaries"). Every state transition
//! takes `now` and any needed secrets as explicit parameters and returns new
//! state plus the domain events/audit record it requires, leaving
//! persistence entirely to the `server` crate's Adapters.

pub mod action_evidence;
pub mod artifact;
pub mod attempt;
pub mod boot_context;
pub mod cancellation;
pub mod chunk_manifest;
pub mod credential;
pub mod current_boot;
pub mod endpoint;
pub mod events;
pub mod final_dispatch;
pub mod hardware_confidence;
pub mod identity;
pub mod inventory;
pub mod job;
pub mod presented_credential;
pub mod reconciliation;
pub mod target_fingerprint;
pub mod transfer;
pub mod transitions;

pub use action_evidence::{
    apply_action_evidence, ActionEvidence, ActionEvidenceApplied, ActionEvidenceOutcome,
};
pub use artifact::{
    begin_verification, complete_verification, fail_incomplete, set_capture_consistency, Artifact,
    ArtifactId, ArtifactState, ArtifactTransitionError, CaptureConsistency,
};
pub use attempt::{ActionId, Attempt, AttemptId, AttemptState};
pub use bamep_trusted_bootstrap::BootNonce;
pub use boot_context::{BootContext, BootContextResolveError};
pub use cancellation::{
    apply_cancel_ack, request_cancellation, CancelAckApplied, CancelAckEvidence, CancelAckOutcome,
    CancellationRequestError, CancellationRequestOutcome,
};
pub use chunk_manifest::{
    validate_verified_chunk, ChunkAcceptError, ChunkIndex, ChunkManifest, ChunkRecordError,
    ChunkRecordOutcome, ChunkSize, Digest, DigestAlgorithm, ExpectedChunk, InvalidChunkSize,
    InvalidDigestLength, SealError, SealOutcome,
};
pub use credential::{AuthOutcome, CredentialChain, CredentialDimension, DEFAULT_CREDENTIAL_TTL};
pub use current_boot::{CurrentBoot, TrustedBootstrapState};
pub use endpoint::{EndpointAggregate, EndpointId};
pub use events::{Actor, AuditRecord, DomainEvent, TransitionOutcome};
pub use final_dispatch::{
    evaluate_final_destructive_dispatch, FinalDispatchDenial, FinalDispatchInputs,
    FinalDispatchOutcome, FinalDispatchRejection,
};
pub use hardware_confidence::HardwareConfidence;
pub use identity::{IdentityState, InvalidIdentityTransition};
pub use inventory::{
    record_inventory_on_change, InventoryRevision, InventoryRevisionChange, InventoryRevisionId,
    InventorySnapshot,
};
pub use job::{
    admit_job, authorize_destructive_intent, create_workflow, satisfy_preliminary_preconditions,
    DestructiveIntent, DestructiveIntentError, EmptyWorkflow, Job, JobAdmissionError,
    JobAdmissionOutcome, JobId, JobState, JobStep, JobStepEligibilityError, JobStepFailureReason,
    JobStepId, JobStepState,
};
pub use reconciliation::{
    apply_status_report, close_indeterminate, mark_awaiting_reconciliation,
    CloseIndeterminateOutcome, ReconciliationApplied, ReconciliationOutcome, StatusReportEvidence,
};
pub use target_fingerprint::TargetFingerprint;
pub use transfer::{
    bind_attempt, create_transfer_context, SourceProvenance, Transfer, TransferBindingError,
    TransferContext, TransferDirection, TransferId,
};
pub use transitions::{RedeemOutcome, TrustedBootstrapOutcome};
