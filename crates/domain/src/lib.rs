//! Bamep Domain: Endpoint identity lifecycle and runtime-credential chain.
//!
//! Pure business logic only — no I/O, no SQLite, no WebSocket, no clock or
//! RNG access hidden inside a transition (`AGENTS.md` "Architecture and
//! dependencies"; `docs/specifications/m0-stack-and-boundaries-baseline.md`
//! "Component responsibilities and boundaries"). Every state transition
//! takes `now` and any needed secrets as explicit parameters and returns new
//! state plus the domain events/audit record it requires, leaving
//! persistence entirely to the `server` crate's Adapters.

pub mod boot_context;
pub mod credential;
pub mod current_boot;
pub mod endpoint;
pub mod events;
pub mod hardware_confidence;
pub mod identity;
pub mod inventory;
pub mod job;
pub mod presented_credential;
pub mod target_fingerprint;
pub mod transitions;

pub use bamep_trusted_bootstrap::BootNonce;
pub use boot_context::{BootContext, BootContextResolveError};
pub use credential::{AuthOutcome, CredentialChain, CredentialDimension, DEFAULT_CREDENTIAL_TTL};
pub use current_boot::{CurrentBoot, TrustedBootstrapState};
pub use endpoint::{EndpointAggregate, EndpointId};
pub use events::{Actor, AuditRecord, DomainEvent, TransitionOutcome};
pub use hardware_confidence::HardwareConfidence;
pub use identity::{IdentityState, InvalidIdentityTransition};
pub use inventory::{
    record_inventory_on_change, InventoryRevision, InventoryRevisionChange, InventoryRevisionId,
    InventorySnapshot,
};
pub use job::{
    create_workflow, EmptyWorkflow, Job, JobId, JobState, JobStep, JobStepId, JobStepState,
};
pub use target_fingerprint::TargetFingerprint;
pub use transitions::{RedeemOutcome, TrustedBootstrapOutcome};
