//! Application layer: orchestrates Domain transitions/constructions against
//! the `EndpointRepository`/`CredentialRedemptionRepository`/`JobRepository`
//! Ports. Owns no business rules of its own — every decision about whether a
//! transition or construction is legal, and what it produces, comes from
//! `bamep_domain`. This layer's job is sequencing (fetch, decide, one atomic
//! commit) and translating Domain outcomes into results the Runtime Services
//! (Agent Control Gateway, operator-approval harness, workflow-creation
//! harness) can act on.

use std::sync::Arc;

use bamep_agent_protocol::{
    ActionDispatchMessage, BootstrapEvidenceMessage, CancelActionMessage, InventoryReportMessage,
    ProtocolId,
};
use bamep_domain::credential::{CredentialDimension, CredentialHash};
use bamep_domain::presented_credential::{CredentialKind, PresentedCredential};
use bamep_domain::{
    build_proof_transcript, capability_is_current, capability_matches_request,
    evaluate_final_destructive_dispatch, evaluate_transfer_dispatch, proof_is_fresh, transitions,
    verify_proof_signature, ActionEvidence, ActionEvidenceOutcome, ActionId, Actor, ArtifactId,
    Attempt, AttemptState, AuditRecord, AuthorizationOperation, BootContext, BootNonce,
    CancelAckEvidence, CancellationRequestOutcome, CapabilityBinding, CapabilityId,
    CapabilityToken, DestructiveIntent, DigestAlgorithm, EmptyWorkflow, EndpointId,
    FinalDispatchInputs, FinalDispatchOutcome, FinalDispatchRejection, IdentityState,
    InvalidIdentityTransition, InventoryRevision, InventorySnapshot, Job, JobId, JobStepId,
    ProofId, ProofPublicKey, ProofSignature, ProofTranscriptFields, RequestedOperation, Transfer,
    TransferDirection, TransferDispatchInputs, TransferDispatchRejection, TransferId,
    TrustedBootstrapState, DEFAULT_CAPABILITY_TTL_MILLIS, DEFAULT_CREDENTIAL_TTL,
};
use bamep_trusted_bootstrap::{AcceptedSiteKeys, BootstrapAssertion, ServerCertFingerprint};
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::ports::{
    ActionEvidenceCommit, ActionEvidenceLockedFacts, AdmitJobDecision, AdmitJobError,
    AgentDispatchError, AgentDispatchPort, ApplyActionEvidenceDecision,
    ApplyActionEvidenceDecisionOutcome, ApplyActionEvidenceError, ApplyActionEvidenceResult,
    ApplyCancelAckDecision, ApplyCancelAckDecisionOutcome, ApplyCancelAckResult,
    AuthorizeDestructiveIntentDecision, AuthorizeDestructiveIntentError, BootContextRepository,
    CancelAckCommit, CancellationRequestDecided, CommitDestructiveDispatchError,
    CommitTransferDispatchError, CreateWorkflowError, CredentialRedemptionRepository,
    EndpointRepository, EndpointUpdateError, FinalDispatchCommit, FinalDispatchDecision,
    FinalDispatchLockedFacts, InventoryRepository, JobRepository, RedemptionDecision,
    RedemptionTarget, RepositoryError, RequestCancellationDecision, RequestCancellationError,
    RequestCancellationLockedFacts, RequestCancellationResult, SatisfyStepPreconditionsDecision,
    SatisfyStepPreconditionsError, TargetRevalidationPort, TransferAuthorizationRepository,
    TransferDispatchDecision, TransferDispatchLockedFacts, TransferRepository,
};
use crate::runtime::capability_store::CapabilityStore;
use crate::runtime::presence::PresenceRegistry;
use crate::runtime::replay_cache::ReplayCache;
use crate::runtime::reservation_registry::{AttemptReservationRegistry, RegistrationOutcome};
use crate::runtime::resource_arbiter::{
    InsufficientCapacity, ReservationId, ResourceClaim, TechnicalResourceArbiter,
};

/// The single M1 Simulator-only concrete typed action
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-004;
/// `m0-agent-protocol-contract.md` "concrete `action_type` definitions
/// belong to the Specifications that introduce those operations"). The v1
/// `parameters` schema is closed and empty.
pub const M1_SIMULATED_EXECUTION_ACTION_TYPE: &str = "bamep.m1.simulated-execution";
pub const M1_SIMULATED_EXECUTION_ACTION_VERSION: &str = "1";

/// The M1 Agent -> Server data-plane transfer concrete typed action
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005),
/// distinct from [`M1_SIMULATED_EXECUTION_ACTION_TYPE`]. Its v1 `parameters`
/// schema is `{transfer_id, artifact_id, direction, digest_algorithm,
/// chunk_size}`, built by [`transfer_action_parameters`] from authoritative
/// durable Transfer state only — never a caller-supplied replacement.
pub const M1_DATA_PLANE_TRANSFER_ACTION_TYPE: &str = "bamep.m1.data-plane-transfer";
pub const M1_DATA_PLANE_TRANSFER_ACTION_VERSION: &str = "1";

/// Builds the exact RF-005 `bamep.m1.data-plane-transfer` v1 `parameters`
/// object from durable `transfer` (Issue #40 "Action parameter
/// reconstruction"). Every value is reconstructed from the authoritative
/// `Transfer` the durable dispatch commitment already bound — `transfer_id`
/// and `artifact_id` are never regenerated, and `direction`/
/// `digest_algorithm` are converted through an exhaustive `match` so a
/// future additional Domain variant cannot silently fall through to the
/// wrong wire string.
fn transfer_action_parameters(transfer: &Transfer) -> serde_json::Map<String, serde_json::Value> {
    let direction = match transfer.direction {
        TransferDirection::AgentToServer => "agent_to_server",
    };
    let digest_algorithm = match transfer.digest_algorithm {
        DigestAlgorithm::Sha256 => "sha256",
    };
    let mut parameters = serde_json::Map::new();
    parameters.insert(
        "transfer_id".to_string(),
        serde_json::Value::String(transfer.id.0.to_string()),
    );
    parameters.insert(
        "artifact_id".to_string(),
        serde_json::Value::String(transfer.artifact_id.0.to_string()),
    );
    parameters.insert(
        "direction".to_string(),
        serde_json::Value::String(direction.to_string()),
    );
    parameters.insert(
        "digest_algorithm".to_string(),
        serde_json::Value::String(digest_algorithm.to_string()),
    );
    parameters.insert(
        "chunk_size".to_string(),
        serde_json::Value::Number(transfer.chunk_size.get().into()),
    );
    parameters
}

/// `ActionResult.detail`'s exact normative shape for the single M1 concrete
/// action (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-004;
/// Issue #26 "Action wire contract enforcement"). `detail`'s schema is
/// otherwise opaque to `bamep_agent_protocol` — it is owned by the
/// Specification that owns the concrete `action_type`, so this check lives
/// here, in Application, rather than in the wire crate. Only `detail.code` is
/// required to exactly match; unrecognized extra fields are tolerated
/// (forward compatibility, mirroring the rest of the wire contract).
/// `ActionResultOutcome::Cancelled` never matches — #26 never applies
/// evidence for it (#27 owns `Cancelled`).
pub fn m1_result_detail_matches(
    outcome: bamep_agent_protocol::ActionResultOutcome,
    detail: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let expected_code = match outcome {
        bamep_agent_protocol::ActionResultOutcome::Succeeded => "SIMULATED_COMPLETION",
        bamep_agent_protocol::ActionResultOutcome::Failed => "SIMULATED_FAILURE",
        bamep_agent_protocol::ActionResultOutcome::Cancelled => return false,
    };
    detail.get("code").and_then(|v| v.as_str()) == Some(expected_code)
}

#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    #[error("endpoint {0:?} not found")]
    EndpointNotFound(EndpointId),
    #[error("endpoint {0:?} is not enrolled")]
    EndpointNotEnrolled(EndpointId),
    #[error("job {0:?} not found")]
    JobNotFound(JobId),
    #[error("job {0:?} is not eligible for admission")]
    JobNotEligibleForAdmission(JobId),
    #[error("job {0:?} is not Running")]
    JobNotRunning(JobId),
    /// `job.state` was `Pending` — out of this WP's ACTIVE-Job-cancellation
    /// scope (Issue #27).
    #[error("job {0:?} is not eligible for a cancellation request")]
    JobNotEligibleForCancellation(JobId),
    /// [`crate::ports::CloseIndeterminateError::NoUncertainAttempt`] — no
    /// Attempt for this Job is currently `AwaitingReconciliation`.
    #[error("job {0:?} has no attempt currently awaiting reconciliation")]
    JobHasNoUncertainAttempt(JobId),
    /// The target Endpoint already has another active Job — the losing side
    /// of a same-Endpoint admission race
    /// (`m0-job-lifecycle-and-scheduling.md` "Resource leases"). The caller's
    /// Job remains durably `Pending`.
    #[error("endpoint already has an active job")]
    EndpointNotAvailable,
    #[error("job step {0:?} not found in job {1:?}")]
    JobStepNotFound(JobStepId, JobId),
    #[error("job step {0:?} is not eligible for destructive intent authorization")]
    JobStepNotEligible(JobStepId),
    #[error("job step {0:?} already has a destructive intent")]
    JobStepAlreadyAuthorized(JobStepId),
    #[error("job step {0:?} is not the current eligible step")]
    JobStepNotCurrent(JobStepId),
    #[error("endpoint {0:?} has no current durable inventory revision")]
    NoCurrentInventory(EndpointId),
    #[error("endpoint {0:?} has no current target fingerprint")]
    NoCurrentTarget(EndpointId),
    /// An unknown `action_id`, or a known `action_id` belonging to a Job
    /// that does not target the authenticated Endpoint — deliberately one
    /// generic value so a caller can never learn which case occurred
    /// (`m0-agent-protocol-contract.md`; Issue #26 "Authenticated Endpoint
    /// correlation").
    #[error("unknown action")]
    UnknownAction,
    /// Issue #36: `context.transfer.job_id` does not target
    /// `context.transfer.endpoint_id`.
    #[error("job {0:?} does not target endpoint {1:?}")]
    JobEndpointMismatch(JobId, EndpointId),
    /// Issue #36: no Transfer with this `TransferId` was ever created.
    #[error("transfer {0:?} not found")]
    TransferNotFound(bamep_domain::TransferId),
    #[error(transparent)]
    InvalidTransition(#[from] InvalidIdentityTransition),
    #[error(transparent)]
    EmptyWorkflow(#[from] EmptyWorkflow),
    #[error(transparent)]
    ChunkRecord(#[from] bamep_domain::ChunkRecordError),
    #[error(transparent)]
    ChunkAccept(#[from] bamep_domain::ChunkAcceptError),
    #[error(transparent)]
    Seal(#[from] bamep_domain::SealError),
    #[error(transparent)]
    ArtifactTransition(#[from] bamep_domain::ArtifactTransitionError),
    #[error(transparent)]
    TransferBinding(#[from] bamep_domain::TransferBindingError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

impl From<crate::ports::CreateTransferError> for ApplicationError {
    fn from(err: crate::ports::CreateTransferError) -> Self {
        use crate::ports::CreateTransferError as E;
        match err {
            E::EndpointNotFound(id) => ApplicationError::EndpointNotFound(id),
            E::JobNotFound(id) => ApplicationError::JobNotFound(id),
            E::JobEndpointMismatch(job_id, endpoint_id) => {
                ApplicationError::JobEndpointMismatch(job_id, endpoint_id)
            }
            E::JobStepNotFound(step_id, job_id) => {
                ApplicationError::JobStepNotFound(step_id, job_id)
            }
            E::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<crate::ports::RecordChunkError> for ApplicationError {
    fn from(err: crate::ports::RecordChunkError) -> Self {
        use crate::ports::RecordChunkError as E;
        match err {
            E::TransferNotFound(id) => ApplicationError::TransferNotFound(id),
            E::Domain(e) => ApplicationError::ChunkRecord(e),
            E::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<crate::ports::AcceptChunkError> for ApplicationError {
    fn from(err: crate::ports::AcceptChunkError) -> Self {
        use crate::ports::AcceptChunkError as E;
        match err {
            E::TransferNotFound(id) => ApplicationError::TransferNotFound(id),
            E::Domain(e) => ApplicationError::ChunkAccept(e),
            E::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<crate::ports::SealManifestError> for ApplicationError {
    fn from(err: crate::ports::SealManifestError) -> Self {
        use crate::ports::SealManifestError as E;
        match err {
            E::TransferNotFound(id) => ApplicationError::TransferNotFound(id),
            E::Domain(e) => ApplicationError::Seal(e),
            E::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<crate::ports::ArtifactTransitionRepoError> for ApplicationError {
    fn from(err: crate::ports::ArtifactTransitionRepoError) -> Self {
        use crate::ports::ArtifactTransitionRepoError as E;
        match err {
            E::TransferNotFound(id) => ApplicationError::TransferNotFound(id),
            E::Domain(e) => ApplicationError::ArtifactTransition(e),
            E::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<crate::ports::BindAttemptError> for ApplicationError {
    fn from(err: crate::ports::BindAttemptError) -> Self {
        use crate::ports::BindAttemptError as E;
        match err {
            E::TransferNotFound(id) => ApplicationError::TransferNotFound(id),
            E::Domain(e) => ApplicationError::TransferBinding(e),
            E::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<AdmitJobError> for ApplicationError {
    fn from(err: AdmitJobError) -> Self {
        match err {
            AdmitJobError::JobNotFound(id) => ApplicationError::JobNotFound(id),
            AdmitJobError::NotEligible(id) => ApplicationError::JobNotEligibleForAdmission(id),
            AdmitJobError::EndpointNotAvailable => ApplicationError::EndpointNotAvailable,
            AdmitJobError::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<SatisfyStepPreconditionsError> for ApplicationError {
    fn from(err: SatisfyStepPreconditionsError) -> Self {
        match err {
            SatisfyStepPreconditionsError::JobNotFound(id) => ApplicationError::JobNotFound(id),
            SatisfyStepPreconditionsError::JobNotRunning(id) => ApplicationError::JobNotRunning(id),
            SatisfyStepPreconditionsError::JobStepNotFound(step_id, job_id) => {
                ApplicationError::JobStepNotFound(step_id, job_id)
            }
            SatisfyStepPreconditionsError::NotCurrent(step_id) => {
                ApplicationError::JobStepNotCurrent(step_id)
            }
            SatisfyStepPreconditionsError::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<EndpointUpdateError> for ApplicationError {
    fn from(err: EndpointUpdateError) -> Self {
        match err {
            EndpointUpdateError::NotFound(id) => ApplicationError::EndpointNotFound(id),
            EndpointUpdateError::InvalidTransition(e) => ApplicationError::InvalidTransition(e),
            EndpointUpdateError::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<ApplyActionEvidenceError> for ApplicationError {
    fn from(err: ApplyActionEvidenceError) -> Self {
        match err {
            ApplyActionEvidenceError::UnknownAction => ApplicationError::UnknownAction,
            ApplyActionEvidenceError::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<RequestCancellationError> for ApplicationError {
    fn from(err: RequestCancellationError) -> Self {
        match err {
            RequestCancellationError::JobNotFound(id) => ApplicationError::JobNotFound(id),
            RequestCancellationError::NotEligible(id) => {
                ApplicationError::JobNotEligibleForCancellation(id)
            }
            RequestCancellationError::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<CreateWorkflowError> for ApplicationError {
    fn from(err: CreateWorkflowError) -> Self {
        match err {
            CreateWorkflowError::EndpointNotFound(id) => ApplicationError::EndpointNotFound(id),
            CreateWorkflowError::EndpointNotEnrolled(id) => {
                ApplicationError::EndpointNotEnrolled(id)
            }
            CreateWorkflowError::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

impl From<AuthorizeDestructiveIntentError> for ApplicationError {
    fn from(err: AuthorizeDestructiveIntentError) -> Self {
        match err {
            AuthorizeDestructiveIntentError::JobStepNotFound(step_id, job_id) => {
                ApplicationError::JobStepNotFound(step_id, job_id)
            }
            AuthorizeDestructiveIntentError::NotEligible(step_id) => {
                ApplicationError::JobStepNotEligible(step_id)
            }
            AuthorizeDestructiveIntentError::AlreadyAuthorized(step_id) => {
                ApplicationError::JobStepAlreadyAuthorized(step_id)
            }
            AuthorizeDestructiveIntentError::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

/// Wall-clock abstraction so [`EnrollmentService::redeem`] can obtain "now"
/// at *decision time* — inside the Adapter's lock/transaction scope, after it
/// has serialized against concurrent redemptions for the same routing target
/// — rather than at *call time*, before any lock is even requested. ADR-0012
/// requires that "the credential presented needs to remain valid at the
/// commit that accepts the redemption"; a `now` captured before a lock wait
/// and carried through unchanged cannot satisfy that if the wait is long
/// enough for the credential to expire in between. Deliberately
/// adapter-neutral and PostgreSQL-free — this is a pure Application-level
/// concern, not a Port/Adapter one, and Domain functions are unaffected:
/// they still take an explicit `now: DateTime<Utc>` parameter, preserving
/// Domain purity and deterministic unit testing.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Real wall-clock time — the production default.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Outcome of redeeming a presented credential in a fresh `AuthRequest`,
/// shaped for the eventual Agent Control Gateway adapter to translate
/// directly into `SessionEstablished` / `AuthError`
/// (`m0-agent-protocol-contract.md` "Transport and handshake").
#[derive(Debug, Clone)]
pub enum RedeemResult {
    Established {
        endpoint_id: EndpointId,
        runtime_credential: PresentedCredential,
        credential_expires_at: DateTime<Utc>,
    },
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapEvidenceResult {
    Established,
    Rejected,
}

pub struct InventoryService {
    repo: Arc<dyn InventoryRepository>,
    clock: Arc<dyn Clock>,
}

impl InventoryService {
    pub fn new(repo: Arc<dyn InventoryRepository>) -> Self {
        Self {
            repo,
            clock: Arc::new(SystemClock),
        }
    }

    pub fn with_clock(repo: Arc<dyn InventoryRepository>, clock: Arc<dyn Clock>) -> Self {
        Self { repo, clock }
    }

    pub async fn record(
        &self,
        endpoint_id: EndpointId,
        report: InventoryReportMessage,
    ) -> Result<Option<InventoryRevision>, ApplicationError> {
        self.repo
            .record_inventory(
                endpoint_id,
                InventorySnapshot(report.body.inventory),
                self.clock.now(),
            )
            .await
            .map_err(ApplicationError::from)
    }
}

/// The internal Simulator/harness workflow-creation control path
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-004; Issue
/// #24 "durable workflow creation" boundary). Callers of
/// [`create_workflow`](Self::create_workflow) must be structurally separate
/// from Agent Protocol message handling — an in-process test/development
/// harness, a future Simulator control path, or a CLI — mirroring
/// [`EnrollmentService::approve_enrollment`]'s separation requirement. This
/// is the only path through which Issue #24 creates a workflow; callers never
/// insert `jobs`/`job_steps` rows directly.
pub struct JobService<J: JobRepository> {
    repo: Arc<J>,
}

impl<J: JobRepository> JobService<J> {
    pub fn new(repo: Arc<J>) -> Self {
        Self { repo }
    }

    /// Constructs one linear workflow of `step_count` ordered `JobStep`s
    /// targeting `endpoint_id` (`bamep_domain::create_workflow`) and
    /// atomically persists it. Rejects an empty workflow before any I/O, and
    /// rejects a nonexistent or not-`Enrolled` target Endpoint without
    /// persisting partial state (`crate::ports::JobRepository::create_workflow`).
    /// Does not admit the Job into `Running`, evaluate JobStep preconditions,
    /// acquire leases, or create an Attempt — those belong to later
    /// scheduling/dispatch Work Packages.
    pub async fn create_workflow(
        &self,
        endpoint_id: EndpointId,
        step_count: usize,
    ) -> Result<Job, ApplicationError> {
        let job = bamep_domain::create_workflow(endpoint_id, step_count)?;
        self.repo.create_workflow(&job).await?;
        Ok(job)
    }
}

/// The internal Application/harness scheduling control path (Issue #32 "Job
/// admission and durable Endpoint exclusivity"; "Current ordered JobStep
/// preliminary eligibility"). Exposes exactly the two narrow M1 scheduling
/// operations `FinalDispatchService` (Issue #25) later composes: admitting a
/// `Pending` Job into `Running`, and advancing the current eligible
/// `JobStep` to `PreconditionsSatisfied`. This service does not evaluate the
/// destructive gate, acquire a technical-resource reservation, or create an
/// Attempt — those belong to `FinalDispatchService`.
pub struct JobSchedulingService<J: JobRepository> {
    repo: Arc<J>,
    clock: Arc<dyn Clock>,
}

impl<J: JobRepository> JobSchedulingService<J> {
    /// Uses [`SystemClock`] for the `JobStarted` event timestamp. Use
    /// [`with_clock`](Self::with_clock) to inject a deterministic clock for
    /// tests.
    pub fn new(repo: Arc<J>) -> Self {
        Self::with_clock(repo, Arc::new(SystemClock))
    }

    pub fn with_clock(repo: Arc<J>, clock: Arc<dyn Clock>) -> Self {
        Self { repo, clock }
    }

    /// Attempts to admit `job_id` into `Running`, acquiring its durable
    /// Job-scoped Endpoint exclusivity atomically with the required
    /// `JobStarted` domain event (`bamep_domain::admit_job`;
    /// `crate::ports::JobRepository::admit_job`). A competing Job for the
    /// same Endpoint remains `Pending` —
    /// [`ApplicationError::EndpointNotAvailable`] — never a partial
    /// `Running` state and never a second `JobStarted`.
    pub async fn admit(&self, job_id: JobId) -> Result<Job, ApplicationError> {
        let clock = Arc::clone(&self.clock);
        let decide: AdmitJobDecision =
            Box::new(move |job| bamep_domain::admit_job(job, clock.now()));
        self.repo
            .admit_job(job_id, decide)
            .await
            .map_err(ApplicationError::from)
    }

    /// Advances `step_id`'s current ordered preliminary eligibility to
    /// `PreconditionsSatisfied` (`bamep_domain::satisfy_preliminary_preconditions`;
    /// `crate::ports::JobRepository::satisfy_current_step_preconditions`). A
    /// later/non-current step cannot skip ahead, and the owning Job must
    /// already be `Running`.
    pub async fn satisfy_current_step_preconditions(
        &self,
        job_id: JobId,
        step_id: JobStepId,
    ) -> Result<bamep_domain::JobStep, ApplicationError> {
        let decide: SatisfyStepPreconditionsDecision =
            Box::new(move |job| bamep_domain::satisfy_preliminary_preconditions(job, step_id));
        self.repo
            .satisfy_current_step_preconditions(job_id, step_id, decide)
            .await
            .map_err(ApplicationError::from)
    }
}

/// The internal Application/harness path that authorizes one eligible
/// `Pending` JobStep's durable destructive intent (Issue #31 "Application /
/// internal harness path"). Callers identify the Job/JobStep only — this
/// service, not the caller, derives the authoritative evidence:
/// [`InventoryRepository::find_current_inventory`] for the Server-owned
/// current inventory revision (#18) and [`TargetRevalidationPort`] for the
/// current target fingerprint (#30). There is no parameter through which a
/// caller could supply an authoritative revision, fingerprint, or an
/// Endpoint-id override for the Job's own target.
///
/// If either evidence source is unavailable, no intent is persisted and no
/// partial authorization state is left behind — evidence is derived before
/// [`JobRepository::authorize_destructive_intent`] is ever called.
pub struct DestructiveIntentService<J: JobRepository, I: InventoryRepository> {
    job_repo: Arc<J>,
    inventory_repo: Arc<I>,
    target_revalidation: Arc<dyn TargetRevalidationPort>,
}

impl<J: JobRepository, I: InventoryRepository> DestructiveIntentService<J, I> {
    pub fn new(
        job_repo: Arc<J>,
        inventory_repo: Arc<I>,
        target_revalidation: Arc<dyn TargetRevalidationPort>,
    ) -> Self {
        Self {
            job_repo,
            inventory_repo,
            target_revalidation,
        }
    }

    /// Authorizes destructive intent for `step_id` under `job_id`.
    ///
    /// Sequence (Issue #31 "Application / internal harness path"):
    /// 1. resolve the Job/JobStep and its owning Endpoint;
    /// 2. verify the step belongs to that Job — a preliminary check; the
    ///    atomic authoritative check happens again under lock in step 6;
    /// 3. obtain the Endpoint's current durable inventory revision;
    /// 4. obtain the Endpoint's current target fingerprint;
    /// 5. construct the `DestructiveIntent` from those Server-owned facts;
    /// 6. atomically persist it on exactly that JobStep, re-verifying
    ///    eligibility under lock.
    pub async fn authorize(
        &self,
        job_id: JobId,
        step_id: JobStepId,
    ) -> Result<DestructiveIntent, ApplicationError> {
        let job = self
            .job_repo
            .find_job(job_id)
            .await?
            .ok_or(ApplicationError::JobNotFound(job_id))?;
        if !job.steps.iter().any(|step| step.id == step_id) {
            return Err(ApplicationError::JobStepNotFound(step_id, job_id));
        }

        let current_inventory = self
            .inventory_repo
            .find_current_inventory(job.endpoint_id)
            .await?
            .ok_or(ApplicationError::NoCurrentInventory(job.endpoint_id))?;
        let current_target = self
            .target_revalidation
            .current_target_fingerprint(job.endpoint_id)
            .ok_or(ApplicationError::NoCurrentTarget(job.endpoint_id))?;

        let authorized_inventory_revision_id = current_inventory.id;
        let decide: AuthorizeDestructiveIntentDecision = Box::new(move |step| {
            bamep_domain::authorize_destructive_intent(
                step,
                job_id,
                authorized_inventory_revision_id,
                current_target,
            )
        });

        self.job_repo
            .authorize_destructive_intent(job_id, step_id, decide)
            .await
            .map_err(ApplicationError::from)
    }
}

/// Outcome of one [`FinalDispatchService::commit_destructive_dispatch`] call
/// (Issue #25 "Transient resource reservation": the three cases the WP
/// requires the caller to distinguish).
#[derive(Debug)]
pub enum FinalDispatchResult {
    /// The required technical-resource reservation could not be acquired.
    /// Final revalidation never began: the candidate JobStep remains exactly
    /// `PreconditionsSatisfied`, and nothing was persisted.
    ResourceUnavailable,
    /// Final revalidation failed after the reservation was acquired
    /// (`bamep_domain::FinalDispatchRejection` identifies why). The
    /// reservation has already been released.
    Rejected(FinalDispatchRejection),
    /// The dispatch commitment durably succeeded: the reservation remains
    /// held, returned here together with the committed Attempt/JobStep
    /// context so a later Work Package (#26) can consume it.
    Committed {
        outcome: FinalDispatchOutcome,
        reservation: ReservationId,
    },
}

/// The internal Application/harness final destructive-dispatch authorization
/// path (Issue #25 "[WP] Schedule Jobs and enforce safe dispatch gate").
/// Composes #32's [`TechnicalResourceArbiter`], the Runtime Presence
/// Registry, and [`TargetRevalidationPort`] around the pure Domain gate
/// (`bamep_domain::evaluate_final_destructive_dispatch`), following `lock ->
/// freshly read -> Domain decision -> persist -> commit`
/// (`m0-job-lifecycle-and-scheduling.md` "Final pre-dispatch revalidation").
///
/// Callers identify only the Job/JobStep and the technical resource claims
/// this Attempt requires — every authoritative Endpoint/credential/inventory/
/// target/confidence/bootstrap fact, and the fresh `AttemptId`/`ActionId`
/// themselves, are resolved by this service and the Domain gate it calls, at
/// decision time, never accepted from the caller (Issue #25 "Application
/// boundary").
///
/// This service never constructs or sends `ActionDispatch`: its only output
/// is the durably committed Attempt/action correlation plus the transient
/// [`ReservationId`] context for #26.
pub struct FinalDispatchService<J: JobRepository> {
    repo: Arc<J>,
    clock: Arc<dyn Clock>,
    presence: Arc<PresenceRegistry>,
    target_revalidation: Arc<dyn TargetRevalidationPort>,
    arbiter: Arc<TechnicalResourceArbiter>,
}

impl<J: JobRepository> FinalDispatchService<J> {
    pub fn new(
        repo: Arc<J>,
        presence: Arc<PresenceRegistry>,
        target_revalidation: Arc<dyn TargetRevalidationPort>,
        arbiter: Arc<TechnicalResourceArbiter>,
    ) -> Self {
        Self::with_clock(
            repo,
            Arc::new(SystemClock),
            presence,
            target_revalidation,
            arbiter,
        )
    }

    pub fn with_clock(
        repo: Arc<J>,
        clock: Arc<dyn Clock>,
        presence: Arc<PresenceRegistry>,
        target_revalidation: Arc<dyn TargetRevalidationPort>,
        arbiter: Arc<TechnicalResourceArbiter>,
    ) -> Self {
        Self {
            repo,
            clock,
            presence,
            target_revalidation,
            arbiter,
        }
    }

    /// Attempts to commit exactly one destructive dispatch for `step_id`
    /// under `job_id`, acquiring `claims` from the technical-resource arbiter
    /// first (Issue #25 "Transient resource reservation").
    ///
    /// Sequence:
    /// 1. acquire `claims` atomically from the arbiter; unavailable capacity
    ///    returns [`FinalDispatchResult::ResourceUnavailable`] without
    ///    touching durable state;
    /// 2. lock the owning Job/JobStep/Endpoint/existing-Attempt state
    ///    (`JobRepository::commit_destructive_dispatch`);
    /// 3. inside that lock, resolve the transient Runtime Presence Registry
    ///    and `TargetRevalidationPort` reads, and "now", then call the pure
    ///    Domain gate;
    /// 4. on gate failure, release the reservation and return
    ///    [`FinalDispatchResult::Rejected`];
    /// 5. on persistence failure, release the reservation and return an
    ///    [`ApplicationError`];
    /// 6. on success, keep the reservation held and return
    ///    [`FinalDispatchResult::Committed`].
    pub async fn commit_destructive_dispatch(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        claims: Vec<ResourceClaim>,
    ) -> Result<FinalDispatchResult, ApplicationError> {
        let reservation = match self.arbiter.acquire(claims) {
            Ok(id) => id,
            Err(InsufficientCapacity) => return Ok(FinalDispatchResult::ResourceUnavailable),
        };

        let clock = Arc::clone(&self.clock);
        let presence = Arc::clone(&self.presence);
        let target_revalidation = Arc::clone(&self.target_revalidation);
        let decide: FinalDispatchDecision = Box::new(move |facts: FinalDispatchLockedFacts| {
            let now = clock.now();
            let identity_enrolled = facts.endpoint.identity == IdentityState::Enrolled;
            let credential_active = matches!(
                facts.endpoint.credential.dimension(now),
                CredentialDimension::CredentialActive
            );
            let agent_present = presence.is_present(facts.endpoint.id);
            let hardware_confidence = facts.endpoint.hardware_confidence;
            let trusted_bootstrap_established = facts
                .endpoint
                .current_boot
                .as_ref()
                .is_some_and(|cb| cb.trusted_bootstrap() == TrustedBootstrapState::Established);
            let current_target_fingerprint =
                target_revalidation.current_target_fingerprint(facts.endpoint.id);
            let endpoint_id = facts.endpoint.id;

            let inputs = FinalDispatchInputs {
                job: facts.job,
                step_id,
                existing_active_attempt: facts.existing_active_attempt,
                identity_enrolled,
                credential_active,
                agent_present,
                current_inventory_revision_id: facts.current_inventory_revision_id,
                current_target_fingerprint,
                hardware_confidence,
                trusted_bootstrap_established,
            };
            let outcome = evaluate_final_destructive_dispatch(&inputs)?;

            let audit = AuditRecord {
                audit_id: Uuid::new_v4(),
                endpoint_id,
                actor: Actor::System,
                occurred_at: now,
                detail: format!(
                    "destructive dispatch committed for job_step {:?} attempt {:?} action {:?}",
                    step_id, outcome.attempt.id, outcome.attempt.action_id
                ),
                job_id: Some(job_id),
                job_step_id: Some(step_id),
                attempt_id: Some(outcome.attempt.id),
                action_id: Some(outcome.attempt.action_id),
            };
            Ok(FinalDispatchCommit { outcome, audit })
        });

        match self
            .repo
            .commit_destructive_dispatch(job_id, step_id, decide)
            .await
        {
            Ok(outcome) => Ok(FinalDispatchResult::Committed {
                outcome,
                reservation,
            }),
            Err(CommitDestructiveDispatchError::Rejected(rejection)) => {
                self.arbiter.release(reservation);
                Ok(FinalDispatchResult::Rejected(rejection))
            }
            Err(CommitDestructiveDispatchError::JobNotFound(id)) => {
                self.arbiter.release(reservation);
                Err(ApplicationError::JobNotFound(id))
            }
            Err(CommitDestructiveDispatchError::Repository(e)) => {
                self.arbiter.release(reservation);
                Err(ApplicationError::Repository(e))
            }
        }
    }
}

/// Outcome of one [`TransferDispatchService::commit_transfer_dispatch`] call
/// — mirrors [`FinalDispatchResult`]'s three cases for the non-destructive
/// path.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum TransferDispatchResult {
    /// The required technical-resource reservation could not be acquired.
    /// Final revalidation never began: the candidate JobStep remains exactly
    /// `PreconditionsSatisfied`, and nothing was persisted.
    ResourceUnavailable,
    /// Final revalidation failed after the reservation was acquired
    /// (`bamep_domain::TransferDispatchRejection` identifies why). The
    /// reservation has already been released. The pre-dispatch Transfer/
    /// Artifact identities are unchanged and remain unauthorized.
    Rejected(TransferDispatchRejection),
    /// The dispatch commitment durably succeeded: the reservation remains
    /// held, returned here together with the committed
    /// JobStep/Attempt/bound-Transfer context so #26 can consume it.
    Committed {
        outcome: bamep_domain::TransferDispatchOutcome,
        reservation: ReservationId,
    },
}

/// The internal Application/harness final non-destructive transfer-dispatch
/// path for `bamep.m1.data-plane-transfer` Agent -> Server capture (Issue
/// #40 "[WP] Commit non-destructive transfer Attempts for dispatch"). The
/// non-destructive sibling of [`FinalDispatchService`]: it composes #32's
/// [`TechnicalResourceArbiter`] around the pure Domain gate
/// (`bamep_domain::evaluate_transfer_dispatch`), following the same `lock ->
/// freshly read -> Domain decision -> persist -> commit` pattern, but it
/// never resolves or requires Runtime Presence Registry, `TargetRevalidationPort`,
/// or any other destructive-only evidence — the seven-item destructive-
/// operation gate is structurally unreachable from this service.
///
/// Callers identify only the Job/JobStep, the durable pre-dispatch
/// `TransferId` (Issue #36), and the technical resource claims this Attempt
/// requires. The fresh `AttemptId`/`ActionId` and the Transfer -> Attempt
/// binding are produced by the Domain gate this service calls, at decision
/// time, never accepted from the caller.
///
/// This service never constructs or sends `ActionDispatch`: its only output
/// is the durably committed Attempt/action/Transfer-binding context plus the
/// transient [`ReservationId`] for #26.
pub struct TransferDispatchService<J: JobRepository> {
    repo: Arc<J>,
    arbiter: Arc<TechnicalResourceArbiter>,
}

impl<J: JobRepository> TransferDispatchService<J> {
    pub fn new(repo: Arc<J>, arbiter: Arc<TechnicalResourceArbiter>) -> Self {
        Self { repo, arbiter }
    }

    /// Attempts to commit exactly one non-destructive transfer dispatch for
    /// `step_id` under `job_id`, binding the durable pre-dispatch Transfer
    /// `transfer_id` (Issue #36) to the freshly committed Attempt, acquiring
    /// `claims` from the technical-resource arbiter first — mirrors
    /// [`FinalDispatchService::commit_destructive_dispatch`]'s identical
    /// sequence and failure/release semantics.
    pub async fn commit_transfer_dispatch(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        transfer_id: TransferId,
        claims: Vec<ResourceClaim>,
    ) -> Result<TransferDispatchResult, ApplicationError> {
        let reservation = match self.arbiter.acquire(claims) {
            Ok(id) => id,
            Err(InsufficientCapacity) => return Ok(TransferDispatchResult::ResourceUnavailable),
        };

        let decide: TransferDispatchDecision =
            Box::new(move |facts: TransferDispatchLockedFacts| {
                let inputs = TransferDispatchInputs {
                    job: facts.job,
                    step_id,
                    existing_active_attempt: facts.existing_active_attempt,
                    transfer: facts.transfer,
                };
                evaluate_transfer_dispatch(&inputs)
            });

        match self
            .repo
            .commit_transfer_dispatch(job_id, step_id, transfer_id, decide)
            .await
        {
            Ok(outcome) => Ok(TransferDispatchResult::Committed {
                outcome,
                reservation,
            }),
            Err(CommitTransferDispatchError::Rejected(rejection)) => {
                self.arbiter.release(reservation);
                Ok(TransferDispatchResult::Rejected(rejection))
            }
            Err(CommitTransferDispatchError::JobNotFound(id)) => {
                self.arbiter.release(reservation);
                Err(ApplicationError::JobNotFound(id))
            }
            Err(CommitTransferDispatchError::TransferNotFound(id)) => {
                self.arbiter.release(reservation);
                Err(ApplicationError::TransferNotFound(id))
            }
            Err(CommitTransferDispatchError::Repository(e)) => {
                self.arbiter.release(reservation);
                Err(ApplicationError::Repository(e))
            }
        }
    }
}

/// Outcome of [`ActionDispatchService::dispatch`]: whether the local
/// transport accepted the `ActionDispatch` frame, or why it did not, or why
/// no send was attempted at all. Neither `Sent` nor `SendFailed` implies
/// Agent receipt/execution (`m0-agent-protocol-contract.md`; Issue #26 "Send
/// failure boundary").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionDispatchOutcome {
    Sent,
    SendFailed(AgentDispatchError),
    /// This exact `attempt.id` was already registered in
    /// [`AttemptReservationRegistry`] by a prior call — a repeated
    /// Application call for an already-dispatched Attempt (e.g. a duplicate
    /// scheduling trigger). No send was attempted; the original reservation
    /// mapping is untouched (Issue #26 correction "Prevent a second
    /// server-side dispatch attempt").
    AlreadyDispatched,
    /// `attempt.state` was not [`AttemptState::Dispatched`] — a stale or
    /// terminal Attempt object must never be used to construct a fresh
    /// `ActionDispatch`. No mapping was registered and no send was
    /// attempted.
    NotDispatchable,
}

/// Transmits a durably committed `Attempt{Dispatched}` exactly once over the
/// selected real authenticated WSS session, using the exact persisted
/// `action_id` (Issue #26 "Exact action_id reuse", "Outbound authenticated
/// session delivery"). Composes [`AttemptReservationRegistry`] (registers the
/// transient `AttemptId -> ReservationId` mapping *before* dispatch becomes
/// reachable — `m0-job-lifecycle-and-scheduling.md`; "Reservation ownership")
/// and [`AgentDispatchPort`] (the transport boundary; never a
/// tungstenite/WebSocket type). Never creates an Attempt, never generates a
/// replacement `action_id`, and never falls back/redispatches on failure —
/// on any [`AgentDispatchError`] the Attempt simply remains durably
/// `Dispatched`; #28 owns subsequent uncertain-delivery reconciliation.
pub struct ActionDispatchService {
    reservations: Arc<AttemptReservationRegistry>,
    transport: Arc<dyn AgentDispatchPort>,
}

impl ActionDispatchService {
    pub fn new(
        reservations: Arc<AttemptReservationRegistry>,
        transport: Arc<dyn AgentDispatchPort>,
    ) -> Self {
        Self {
            reservations,
            transport,
        }
    }

    /// Guards against constructing a fresh `ActionDispatch` from a stale or
    /// terminal Attempt object, then registers `attempt.id -> reservation` —
    /// only sending when this call is the one that actually establishes that
    /// mapping. A second call for an already-registered `attempt.id` (e.g. a
    /// repeated Application trigger for the same already-committed Attempt)
    /// never sends again, regardless of whether the first call's send
    /// itself succeeded or failed — a first send failure still leaves the
    /// mapping registered, so it still refuses to resend
    /// (`m0-job-lifecycle-and-scheduling.md` "Reservation ownership"; Issue
    /// #26 correction "Prevent a second server-side dispatch attempt"). The
    /// reservation is never released merely because a send failed or was
    /// refused — #28 owns uncertain-delivery reconciliation. Constructs
    /// `ActionDispatch` for the single M1 concrete action, converting
    /// `attempt.action_id`'s exact persisted UUID into `ProtocolId` — never
    /// generating a replacement identity.
    pub async fn dispatch(
        &self,
        endpoint_id: EndpointId,
        attempt: Attempt,
        reservation: ReservationId,
    ) -> ActionDispatchOutcome {
        let action_id = ProtocolId::from_uuid(attempt.action_id.0)
            .expect("a Domain ActionId is always a valid UUID v4");
        let dispatch = ActionDispatchMessage::new(
            action_id,
            M1_SIMULATED_EXECUTION_ACTION_TYPE,
            M1_SIMULATED_EXECUTION_ACTION_VERSION,
            serde_json::Map::new(),
        );
        self.dispatch_message(endpoint_id, attempt, reservation, dispatch)
            .await
    }

    /// The non-destructive transfer-dispatch sibling of
    /// [`Self::dispatch`] (Issue #40 "Handoff to #26 outbound delivery"):
    /// identical guard/registration/exactly-once-send discipline, reusing
    /// this same outbound boundary rather than a second transport path.
    /// Builds `ActionDispatch` for `bamep.m1.data-plane-transfer` v1 with
    /// [`transfer_action_parameters`], reconstructed from authoritative
    /// durable `transfer` state alone — never a caller-invented
    /// `transfer_id`/`artifact_id`.
    pub async fn dispatch_transfer(
        &self,
        endpoint_id: EndpointId,
        attempt: Attempt,
        reservation: ReservationId,
        transfer: &Transfer,
    ) -> ActionDispatchOutcome {
        let action_id = ProtocolId::from_uuid(attempt.action_id.0)
            .expect("a Domain ActionId is always a valid UUID v4");
        let dispatch = ActionDispatchMessage::new(
            action_id,
            M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
            M1_DATA_PLANE_TRANSFER_ACTION_VERSION,
            transfer_action_parameters(transfer),
        );
        self.dispatch_message(endpoint_id, attempt, reservation, dispatch)
            .await
    }

    /// Guards against constructing/sending from a stale or terminal Attempt
    /// object, then registers `attempt.id -> reservation` — only sending
    /// when this call is the one that actually establishes that mapping.
    /// Shared by [`Self::dispatch`] and [`Self::dispatch_transfer`] so both
    /// M1 concrete actions get identical exactly-once-send/registration
    /// discipline from one implementation.
    async fn dispatch_message(
        &self,
        endpoint_id: EndpointId,
        attempt: Attempt,
        reservation: ReservationId,
        dispatch: ActionDispatchMessage,
    ) -> ActionDispatchOutcome {
        if attempt.state != AttemptState::Dispatched {
            return ActionDispatchOutcome::NotDispatchable;
        }

        match self.reservations.register(attempt.id, reservation) {
            RegistrationOutcome::AlreadyRegistered => {
                return ActionDispatchOutcome::AlreadyDispatched
            }
            RegistrationOutcome::Registered => {}
        }

        match self.transport.dispatch_action(endpoint_id, dispatch).await {
            Ok(()) => ActionDispatchOutcome::Sent,
            Err(e) => ActionDispatchOutcome::SendFailed(e),
        }
    }
}

/// Applies normal connected-session Agent action evidence
/// (`ActionAck{Accepted|Rejected}`, `ActionResult{Succeeded|Failed}`) to a
/// durable Attempt/JobStep/Job (Issue #26 "PostgreSQL evidence application",
/// "Server-side idempotency"). Composes the pure Domain decision
/// (`bamep_domain::apply_action_evidence`) with
/// [`JobRepository::apply_action_evidence`]'s lock/decide/persist boundary,
/// and — only for a terminal outcome, and only once, no matter how many
/// duplicate/concurrent terminal evidence attempts observe the same already-
/// committed outcome — removes the Attempt's reservation mapping and
/// releases it through [`TechnicalResourceArbiter`].
///
/// Takes `Arc<dyn JobRepository>` rather than being generic over a concrete
/// repository type, mirroring [`InventoryService`]: this keeps
/// `AgentControlGateway` (which owns an instance of this service) from
/// needing a third repository-type generic parameter merely to route
/// `ActionAck`/`ActionResult`.
pub struct ActionEvidenceService {
    repo: Arc<dyn JobRepository>,
    reservations: Arc<AttemptReservationRegistry>,
    arbiter: Arc<TechnicalResourceArbiter>,
    clock: Arc<dyn Clock>,
}

impl ActionEvidenceService {
    pub fn new(
        repo: Arc<dyn JobRepository>,
        reservations: Arc<AttemptReservationRegistry>,
        arbiter: Arc<TechnicalResourceArbiter>,
    ) -> Self {
        Self::with_clock(repo, reservations, arbiter, Arc::new(SystemClock))
    }

    pub fn with_clock(
        repo: Arc<dyn JobRepository>,
        reservations: Arc<AttemptReservationRegistry>,
        arbiter: Arc<TechnicalResourceArbiter>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            repo,
            reservations,
            arbiter,
            clock,
        }
    }

    /// Applies `evidence` for `action_id`, correlated to
    /// `authenticated_endpoint_id` (the Endpoint whose authenticated session
    /// this evidence arrived on — Issue #26 "Authenticated Endpoint
    /// correlation"). An unknown `action_id`, or one belonging to another
    /// Endpoint's Job, is [`ApplicationError::UnknownAction`] in both cases.
    pub async fn apply(
        &self,
        action_id: ProtocolId,
        authenticated_endpoint_id: EndpointId,
        evidence: ActionEvidence,
    ) -> Result<ApplyActionEvidenceResult, ApplicationError> {
        let domain_action_id = ActionId(action_id.as_uuid());
        let clock = Arc::clone(&self.clock);

        let decide: ApplyActionEvidenceDecision = Box::new(
            move |facts: ActionEvidenceLockedFacts| {
                let now = clock.now();
                let endpoint_id = facts.job.endpoint_id;
                let job_id = facts.job.id;
                match bamep_domain::apply_action_evidence(
                    &facts.job,
                    &facts.job_step,
                    &facts.attempt,
                    evidence,
                    now,
                ) {
                    ActionEvidenceOutcome::NoOp => ApplyActionEvidenceDecisionOutcome::NoOp,
                    ActionEvidenceOutcome::Conflict => ApplyActionEvidenceDecisionOutcome::Conflict,
                    ActionEvidenceOutcome::Applied(applied) => {
                        let audit = applied.terminal.then(|| AuditRecord {
                        audit_id: Uuid::new_v4(),
                        endpoint_id,
                        actor: Actor::System,
                        occurred_at: now,
                        detail: format!(
                            "attempt {:?} action {:?} reached terminal state {:?} for job_step {:?}",
                            applied.attempt.id,
                            applied.attempt.action_id,
                            applied.attempt.state,
                            applied.job_step.id
                        ),
                        job_id: Some(job_id),
                        job_step_id: Some(applied.job_step.id),
                        attempt_id: Some(applied.attempt.id),
                        action_id: Some(applied.attempt.action_id),
                    });
                        ApplyActionEvidenceDecisionOutcome::Applied(ActionEvidenceCommit {
                            outcome: applied,
                            audit,
                        })
                    }
                }
            },
        );

        let result = self
            .repo
            .apply_action_evidence(domain_action_id, authenticated_endpoint_id, decide)
            .await?;

        if let ApplyActionEvidenceResult::Applied(applied) = &result {
            if applied.terminal {
                // Remove the mapping exactly once — only the successful
                // remover releases through the arbiter, so duplicate/
                // concurrent terminal evidence can never double-release
                // (`m0-job-lifecycle-and-scheduling.md`; Issue #26
                // "Attempt reservation registry").
                if let Some(reservation) = self.reservations.take(applied.attempt.id) {
                    self.arbiter.release(reservation);
                }
            }
        }

        Ok(result)
    }

    /// Read-only `ActionProgress` correlation check (Issue #26 "Correlate
    /// ActionProgress to the authenticated Endpoint"): reports whether
    /// `action_id` belongs to an Attempt whose Job targets
    /// `authenticated_endpoint_id`. Never mutates lifecycle state, never
    /// persists anything — `ActionProgress` is transient advisory metadata,
    /// so this deliberately never reaches [`Self::apply`]'s lock/decide/
    /// persist boundary. An unknown or foreign `action_id` both report
    /// `false`, mirroring [`Self::apply`]'s identical non-enumeration
    /// policy for `UnknownAction`.
    pub async fn action_belongs_to_endpoint(
        &self,
        action_id: ProtocolId,
        authenticated_endpoint_id: EndpointId,
    ) -> Result<bool, ApplicationError> {
        let domain_action_id = ActionId(action_id.as_uuid());
        Ok(self
            .repo
            .action_targets_endpoint(domain_action_id, authenticated_endpoint_id)
            .await?)
    }
}

/// Outcome of [`CancellationService::request`]'s attempt to transmit
/// `CancelAction`, mirroring [`ActionDispatchOutcome`]'s send-result
/// distinction. Neither variant implies Agent receipt/execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelActionSendOutcome {
    Sent,
    SendFailed(AgentDispatchError),
}

/// Outcome of [`CancellationService::request`], mirroring
/// [`crate::ports::RequestCancellationResult`] with the `CancelAction` send
/// outcome attached for the case that requires one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationRequestResult {
    /// `Running -> Cancelling` committed durably; `send` reports whether the
    /// subsequent `CancelAction` transmission attempt succeeded locally. A
    /// [`CancelActionSendOutcome::SendFailed`] never causes a second
    /// automatic send by this WP — #28 owns subsequent uncertain-delivery
    /// reconciliation (`m0-job-lifecycle-and-scheduling.md`; Issue #27
    /// "Request idempotency / send-once").
    EnteredCancelling { send: CancelActionSendOutcome },
    /// `Running -> Cancelled` committed durably without sending
    /// `CancelAction` — no active/uncertain Attempt existed.
    CompletedImmediately,
    /// The Job was already `Cancelling` — idempotent no-op, no repeated
    /// send.
    AlreadyCancelling,
    /// The Job was already terminal — no-op.
    AlreadyTerminal,
}

/// The operator/internal Job-cancellation control path (Issue #27 "[WP]
/// Execute Job cancellation end to end"). Two structurally distinct
/// responsibilities share this one service instance:
///
/// - [`Self::request`] — the durable cancellation-request path. Callers of
///   this method must be structurally separate from Agent Protocol message
///   handling, mirroring [`EnrollmentService::approve_enrollment`]'s
///   separation requirement: the Agent must never be able to initiate Job
///   cancellation. An in-process test/development harness, a future
///   Administrative API handler, or a CLI may call this — never
///   `AgentControlGateway`'s inbound message loop.
/// - [`Self::apply_cancel_ack`] — inbound `CancelAck` evidence application,
///   invoked only by `AgentControlGateway`.
///
/// Composes [`AttemptReservationRegistry`]/[`TechnicalResourceArbiter`]
/// exactly like [`ActionEvidenceService`] (the same transient reservation
/// mapping #26 already registered — cancellation never creates a new one),
/// and [`AgentDispatchPort`] for `CancelAction` transmission, reusing the
/// same outbound session path #26's [`ActionDispatchService`] uses.
pub struct CancellationService {
    repo: Arc<dyn JobRepository>,
    reservations: Arc<AttemptReservationRegistry>,
    arbiter: Arc<TechnicalResourceArbiter>,
    dispatch: Arc<dyn AgentDispatchPort>,
    clock: Arc<dyn Clock>,
}

impl CancellationService {
    pub fn new(
        repo: Arc<dyn JobRepository>,
        reservations: Arc<AttemptReservationRegistry>,
        arbiter: Arc<TechnicalResourceArbiter>,
        dispatch: Arc<dyn AgentDispatchPort>,
    ) -> Self {
        Self::with_clock(repo, reservations, arbiter, dispatch, Arc::new(SystemClock))
    }

    pub fn with_clock(
        repo: Arc<dyn JobRepository>,
        reservations: Arc<AttemptReservationRegistry>,
        arbiter: Arc<TechnicalResourceArbiter>,
        dispatch: Arc<dyn AgentDispatchPort>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            repo,
            reservations,
            arbiter,
            dispatch,
            clock,
        }
    }

    /// Requests cancellation of `job_id` on behalf of `operator`
    /// (`m0-job-lifecycle-and-scheduling.md` "Job lifecycle"; Issue #27
    /// "Durable cancellation request"). Persist-before-send: the durable
    /// `Running -> Cancelling` (or immediate `-> Cancelled`) transition plus
    /// its required operator cancellation audit commit atomically first;
    /// only after that commit does this method attempt `CancelAction`
    /// transmission, and only for the `EnteredCancelling` outcome.
    pub async fn request(
        &self,
        job_id: JobId,
        operator: Actor,
    ) -> Result<CancellationRequestResult, ApplicationError> {
        let now = self.clock.now();
        let audit_actor = operator;
        let decide: RequestCancellationDecision =
            Box::new(move |facts: RequestCancellationLockedFacts| {
                let outcome = bamep_domain::request_cancellation(
                    &facts.job,
                    facts.active_attempt.as_ref(),
                    now,
                )?;
                Ok(match outcome {
                    CancellationRequestOutcome::EnteredCancelling {
                        job,
                        attempt_id,
                        action_id,
                    } => {
                        let audit = AuditRecord {
                            audit_id: Uuid::new_v4(),
                            endpoint_id: job.endpoint_id,
                            actor: audit_actor,
                            occurred_at: now,
                            detail: format!(
                                "operator cancellation requested for job {:?}; attempt {:?} \
                                 action {:?} remains active",
                                job.id, attempt_id, action_id
                            ),
                            job_id: Some(job.id),
                            job_step_id: None,
                            attempt_id: Some(attempt_id),
                            action_id: Some(action_id),
                        };
                        CancellationRequestDecided::EnteredCancelling {
                            job,
                            attempt_id,
                            action_id,
                            audit,
                        }
                    }
                    CancellationRequestOutcome::CompletedImmediately { job, event } => {
                        let audit = AuditRecord {
                            audit_id: Uuid::new_v4(),
                            endpoint_id: job.endpoint_id,
                            actor: audit_actor,
                            occurred_at: now,
                            detail: format!(
                                "operator cancellation completed immediately for job {:?} \
                                 (no active attempt)",
                                job.id
                            ),
                            job_id: Some(job.id),
                            job_step_id: None,
                            attempt_id: None,
                            action_id: None,
                        };
                        CancellationRequestDecided::CompletedImmediately { job, event, audit }
                    }
                    CancellationRequestOutcome::AlreadyCancelling => {
                        CancellationRequestDecided::AlreadyCancelling
                    }
                    CancellationRequestOutcome::AlreadyTerminal => {
                        CancellationRequestDecided::AlreadyTerminal
                    }
                })
            });

        let result = self.repo.request_cancellation(job_id, decide).await?;

        Ok(match result {
            RequestCancellationResult::EnteredCancelling {
                action_id,
                endpoint_id,
                ..
            } => {
                let protocol_action_id = ProtocolId::from_uuid(action_id.0)
                    .expect("a Domain ActionId is always a valid UUID v4");
                let cancel = CancelActionMessage::new(protocol_action_id);
                let send = match self.dispatch.cancel_action(endpoint_id, cancel).await {
                    Ok(()) => CancelActionSendOutcome::Sent,
                    Err(e) => CancelActionSendOutcome::SendFailed(e),
                };
                CancellationRequestResult::EnteredCancelling { send }
            }
            RequestCancellationResult::CompletedImmediately => {
                CancellationRequestResult::CompletedImmediately
            }
            RequestCancellationResult::AlreadyCancelling => {
                CancellationRequestResult::AlreadyCancelling
            }
            RequestCancellationResult::AlreadyTerminal => {
                CancellationRequestResult::AlreadyTerminal
            }
        })
    }

    /// Applies `CancelAck` evidence for `action_id`, correlated to
    /// `authenticated_endpoint_id` (Issue #27 "CancelAck handling"). Invoked
    /// only by `AgentControlGateway`'s inbound Agent Protocol message loop.
    /// An unknown `action_id`, or one belonging to another Endpoint's Job, is
    /// [`ApplicationError::UnknownAction`] in both cases, mirroring
    /// [`ActionEvidenceService::apply`].
    pub async fn apply_cancel_ack(
        &self,
        action_id: ProtocolId,
        authenticated_endpoint_id: EndpointId,
        evidence: CancelAckEvidence,
    ) -> Result<ApplyCancelAckResult, ApplicationError> {
        let domain_action_id = ActionId(action_id.as_uuid());
        let clock = Arc::clone(&self.clock);

        let decide: ApplyCancelAckDecision = Box::new(move |facts: ActionEvidenceLockedFacts| {
            let now = clock.now();
            let endpoint_id = facts.job.endpoint_id;
            let job_id = facts.job.id;
            match bamep_domain::apply_cancel_ack(
                &facts.job,
                &facts.job_step,
                &facts.attempt,
                evidence,
                now,
            ) {
                bamep_domain::CancelAckOutcome::NoOp => ApplyCancelAckDecisionOutcome::NoOp,
                bamep_domain::CancelAckOutcome::Applied(applied) => {
                    let audit = applied.terminal.then(|| AuditRecord {
                        audit_id: Uuid::new_v4(),
                        endpoint_id,
                        actor: Actor::System,
                        occurred_at: now,
                        detail: format!(
                            "attempt {:?} action {:?} reached terminal state {:?} for job_step \
                             {:?} via cancellation evidence",
                            applied.attempt.id,
                            applied.attempt.action_id,
                            applied.attempt.state,
                            applied.job_step.id
                        ),
                        job_id: Some(job_id),
                        job_step_id: Some(applied.job_step.id),
                        attempt_id: Some(applied.attempt.id),
                        action_id: Some(applied.attempt.action_id),
                    });
                    ApplyCancelAckDecisionOutcome::Applied(CancelAckCommit {
                        outcome: applied,
                        audit,
                    })
                }
            }
        });

        let result = self
            .repo
            .apply_cancel_ack(domain_action_id, authenticated_endpoint_id, decide)
            .await?;

        if let ApplyCancelAckResult::Applied(applied) = &result {
            if applied.terminal {
                // Remove the mapping exactly once — mirrors
                // `ActionEvidenceService::apply`'s identical exactly-once
                // release guarantee. A no-op when the reservation was
                // already released by an earlier terminal outcome (e.g. the
                // Attempt independently reached Succeeded/Failed before this
                // CancelAck arrived).
                if let Some(reservation) = self.reservations.take(applied.attempt.id) {
                    self.arbiter.release(reservation);
                }
            }
        }

        Ok(result)
    }
}

/// Outcome of [`ReconciliationService::reconcile_on_session_start`]'s attempt
/// to transmit `StatusQuery`, mirroring [`ActionDispatchOutcome`]/
/// [`CancelActionSendOutcome`]'s send-result distinction. Neither `Sent` nor
/// `SendFailed` implies Agent receipt/response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusQuerySendOutcome {
    /// No Attempt for this Endpoint is currently `AwaitingReconciliation` —
    /// nothing to query.
    NoneNeeded,
    Sent,
    SendFailed(AgentDispatchError),
}

/// Uncertain-execution reconciliation (Issue #28 "[WP] Reconcile interrupted
/// Attempts safely"; `m0-job-lifecycle-and-scheduling.md` "Reconciliation").
/// Five structurally distinct responsibilities share this one service
/// instance, mirroring [`CancellationService`]'s split between its inbound-
/// evidence and operator-control-path methods:
///
/// - [`Self::mark_endpoint_uncertain`] — connection-loss trigger. Called by
///   `AgentControlGateway` when an authenticated session for an Endpoint
///   ends (`m0-job-lifecycle-and-scheduling.md` "Attempt lifecycle":
///   "connection loss ... while `Dispatched` or `InProgress`"), and only when
///   that session actually carried an `ActionDispatch`
///   (`OutboundSessionDirectory::dispatch_relevant_action`). Scoped to the
///   exact `action_id` that session carried, not merely to the Endpoint, so a
///   stale correlation from an already-terminal Attempt can never move a
///   later, unrelated Attempt into `AwaitingReconciliation` (Issue #28 second
///   corrective pass "Attempt-scoped session correlation").
/// - [`Self::reconcile_on_startup`] — Server-restart recovery. Called once,
///   before any Agent session is accepted, by the Runtime harness/test
///   standing in for Server startup (Issue #28 "Server restart": "Do NOT
///   require an Agent to already be connected at Server startup").
/// - [`Self::reconcile_on_session_start`] — issues `StatusQuery` for any
///   `AwaitingReconciliation` Attempt once a valid authenticated session
///   (re-)establishes for its Endpoint. Called by `AgentControlGateway`
///   immediately after registering outbound delivery/presence, before the
///   authenticated message loop begins.
/// - [`Self::apply_status_report`] — inbound `StatusReport` evidence
///   application, invoked only by `AgentControlGateway`.
/// - [`Self::close_indeterminate`] — the explicit reconciliation-close
///   control path. Callers of this method must be structurally separate
///   from Agent Protocol message handling, mirroring
///   [`CancellationService::request`]'s identical separation requirement:
///   the Agent can never decide `Attempt -> Indeterminate` on its own.
///
/// Composes [`AttemptReservationRegistry`]/[`TechnicalResourceArbiter`]
/// exactly like [`ActionEvidenceService`]/[`CancellationService`] — entering
/// `AwaitingReconciliation` itself never releases the transient reservation
/// (only a later authoritative terminal outcome does); duplicate/delayed
/// terminal reconciliation evidence can never double-release, and the
/// mapping's absence after a Server restart (a fresh, empty in-memory
/// registry) is a safe no-op release, never a correctness problem for the
/// durable Attempt lifecycle.
pub struct ReconciliationService {
    repo: Arc<dyn JobRepository>,
    reservations: Arc<AttemptReservationRegistry>,
    arbiter: Arc<TechnicalResourceArbiter>,
    dispatch: Arc<dyn AgentDispatchPort>,
    clock: Arc<dyn Clock>,
}

impl ReconciliationService {
    pub fn new(
        repo: Arc<dyn JobRepository>,
        reservations: Arc<AttemptReservationRegistry>,
        arbiter: Arc<TechnicalResourceArbiter>,
        dispatch: Arc<dyn AgentDispatchPort>,
    ) -> Self {
        Self::with_clock(repo, reservations, arbiter, dispatch, Arc::new(SystemClock))
    }

    pub fn with_clock(
        repo: Arc<dyn JobRepository>,
        reservations: Arc<AttemptReservationRegistry>,
        arbiter: Arc<TechnicalResourceArbiter>,
        dispatch: Arc<dyn AgentDispatchPort>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            repo,
            reservations,
            arbiter,
            dispatch,
            clock,
        }
    }

    /// The single reusable Domain decide-closure every uncertain-entry
    /// operation below shares (`bamep_domain::mark_awaiting_reconciliation`
    /// captures no call-specific context — see [`crate::ports::MarkUncertainDecision`]
    /// docs).
    fn mark_uncertain_decision() -> crate::ports::MarkUncertainDecision {
        Box::new(bamep_domain::mark_awaiting_reconciliation)
    }

    /// Connection-loss trigger: marks `endpoint_id`'s current active Attempt
    /// `AwaitingReconciliation`, but ONLY when that Attempt's own `action_id`
    /// still matches `expected_action_id` — the exact `action_id` the
    /// disconnecting session actually carried via `ActionDispatch` (Issue #28
    /// second corrective pass "Attempt-scoped session correlation").
    ///
    /// This is the fix for the cross-Attempt race the first corrective pass
    /// left open: a disconnecting session may have carried an EARLIER Attempt
    /// that already reached a terminal state and was superseded by a new one
    /// — dispatched through a different (or the same) session — while the
    /// disconnect was still being handled. Without this check, this method
    /// would blindly mark whatever Attempt is *currently* active for the
    /// Endpoint, even though the disconnecting session was never relevant to
    /// it. Comparing `expected_action_id` against the freshly locked
    /// candidate Attempt's own `action_id` — inside the same
    /// [`crate::ports::MarkUncertainDecision`] closure hook the Adapter
    /// already invokes under its own lock — makes the mismatch a safe no-op
    /// (`Ok(None)`) instead of a false reconciliation, with no new Port
    /// method and no second Attempt/Job lock required.
    ///
    /// A safe no-op when no eligible Attempt exists, or the eligible
    /// Attempt's `action_id` does not match. Never touches any other
    /// Endpoint's Attempts.
    pub async fn mark_endpoint_uncertain(
        &self,
        endpoint_id: EndpointId,
        expected_action_id: ProtocolId,
    ) -> Result<Option<bamep_domain::AttemptId>, ApplicationError> {
        let expected_action_id = ActionId(expected_action_id.as_uuid());
        let decide: crate::ports::MarkUncertainDecision = Box::new(move |attempt: &Attempt| {
            if attempt.action_id != expected_action_id {
                return None;
            }
            bamep_domain::mark_awaiting_reconciliation(attempt)
        });
        Ok(self
            .repo
            .mark_endpoint_active_attempt_uncertain(endpoint_id, decide)
            .await?)
    }

    /// Server-restart recovery: moves every currently `Dispatched`/
    /// `InProgress` Attempt, across every Endpoint, to
    /// `AwaitingReconciliation`. Never redispatches, never creates a second
    /// Attempt, never sends anything.
    pub async fn reconcile_on_startup(
        &self,
    ) -> Result<Vec<bamep_domain::AttemptId>, ApplicationError> {
        Ok(self
            .repo
            .reconcile_all_active_attempts_on_startup(Self::mark_uncertain_decision())
            .await?)
    }

    /// Issues `StatusQuery{action_id}` for `endpoint_id`'s current
    /// `AwaitingReconciliation` Attempt, if any, over the now-live
    /// authenticated outbound session — never a fresh `ActionDispatch`, never
    /// a replacement `action_id` (Issue #28 "Outbound status query"). Called
    /// once a valid authenticated session (re-)establishes, whether after
    /// ordinary reconnect or after Server restart.
    pub async fn reconcile_on_session_start(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<StatusQuerySendOutcome, ApplicationError> {
        let Some(action_id) = self.repo.find_reconciliation_candidate(endpoint_id).await? else {
            return Ok(StatusQuerySendOutcome::NoneNeeded);
        };
        let protocol_action_id = ProtocolId::from_uuid(action_id.0)
            .expect("a Domain ActionId is always a valid UUID v4");
        let query = bamep_agent_protocol::StatusQueryMessage::new(protocol_action_id);
        Ok(match self.dispatch.status_query(endpoint_id, query).await {
            Ok(()) => StatusQuerySendOutcome::Sent,
            Err(e) => StatusQuerySendOutcome::SendFailed(e),
        })
    }

    /// Applies `evidence` for `action_id`, correlated to
    /// `authenticated_endpoint_id`, exactly mirroring
    /// [`ActionEvidenceService::apply`]'s Endpoint-correlation and terminal-
    /// reservation-release behavior. Only ever mutates an Attempt currently
    /// `AwaitingReconciliation` — `bamep_domain::apply_status_report` is the
    /// sole owner of that decision.
    pub async fn apply_status_report(
        &self,
        action_id: ProtocolId,
        authenticated_endpoint_id: EndpointId,
        evidence: bamep_domain::StatusReportEvidence,
    ) -> Result<crate::ports::ApplyReconciliationResult, ApplicationError> {
        let domain_action_id = ActionId(action_id.as_uuid());
        let clock = Arc::clone(&self.clock);

        let decide: crate::ports::ApplyReconciliationDecision =
            Box::new(move |facts: ActionEvidenceLockedFacts| {
                let now = clock.now();
                let endpoint_id = facts.job.endpoint_id;
                let job_id = facts.job.id;
                match bamep_domain::apply_status_report(
                    &facts.job,
                    &facts.job_step,
                    &facts.attempt,
                    evidence,
                    now,
                ) {
                    bamep_domain::ReconciliationOutcome::NoOp => {
                        crate::ports::ApplyReconciliationDecisionOutcome::NoOp
                    }
                    bamep_domain::ReconciliationOutcome::Applied(applied) => {
                        let audit = applied.terminal.then(|| AuditRecord {
                            audit_id: Uuid::new_v4(),
                            endpoint_id,
                            actor: Actor::System,
                            occurred_at: now,
                            detail: format!(
                                "attempt {:?} action {:?} reached terminal state {:?} for \
                                 job_step {:?} via reconciliation evidence",
                                applied.attempt.id,
                                applied.attempt.action_id,
                                applied.attempt.state,
                                applied.job_step.id
                            ),
                            job_id: Some(job_id),
                            job_step_id: Some(applied.job_step.id),
                            attempt_id: Some(applied.attempt.id),
                            action_id: Some(applied.attempt.action_id),
                        });
                        crate::ports::ApplyReconciliationDecisionOutcome::Applied(
                            crate::ports::ReconciliationCommit {
                                outcome: applied,
                                audit,
                            },
                        )
                    }
                }
            });

        let result = self
            .repo
            .apply_status_report(domain_action_id, authenticated_endpoint_id, decide)
            .await?;

        if let crate::ports::ApplyReconciliationResult::Applied(applied) = &result {
            if applied.terminal {
                if let Some(reservation) = self.reservations.take(applied.attempt.id) {
                    self.arbiter.release(reservation);
                }
            }
        }

        Ok(result)
    }

    /// The explicit reconciliation-close control path (Issue #28 "Explicit
    /// Indeterminate closure"). Persist-before-audit-together: the durable
    /// `Attempt -> Indeterminate` transition, the required
    /// `AttemptIndeterminate`/`JobStepFailed`/Job-terminal events, and the
    /// required operator-decision audit commit atomically. Idempotent:
    /// repeated closure never duplicates evidence.
    pub async fn close_indeterminate(
        &self,
        job_id: JobId,
        operator: Actor,
    ) -> Result<crate::ports::CloseIndeterminateResult, ApplicationError> {
        let now = self.clock.now();
        let audit_actor = operator;

        let decide: crate::ports::CloseIndeterminateDecision =
            Box::new(move |facts: ActionEvidenceLockedFacts| {
                let endpoint_id = facts.job.endpoint_id;
                let job_id = facts.job.id;
                match bamep_domain::close_indeterminate(
                    &facts.job,
                    &facts.job_step,
                    &facts.attempt,
                    now,
                ) {
                    bamep_domain::CloseIndeterminateOutcome::AlreadyIndeterminate => {
                        crate::ports::CloseIndeterminateDecisionOutcome::AlreadyIndeterminate
                    }
                    bamep_domain::CloseIndeterminateOutcome::NotEligible => {
                        crate::ports::CloseIndeterminateDecisionOutcome::NotEligible
                    }
                    bamep_domain::CloseIndeterminateOutcome::Applied(applied) => {
                        let audit = AuditRecord {
                            audit_id: Uuid::new_v4(),
                            endpoint_id,
                            actor: audit_actor,
                            occurred_at: now,
                            detail: format!(
                                "operator closed attempt {:?} action {:?} Indeterminate for \
                                 job_step {:?} (job {:?}): authoritative execution outcome \
                                 could not be established",
                                applied.attempt.id,
                                applied.attempt.action_id,
                                applied.job_step.id,
                                job_id
                            ),
                            job_id: Some(job_id),
                            job_step_id: Some(applied.job_step.id),
                            attempt_id: Some(applied.attempt.id),
                            action_id: Some(applied.attempt.action_id),
                        };
                        crate::ports::CloseIndeterminateDecisionOutcome::Applied(
                            crate::ports::CloseIndeterminateCommit {
                                outcome: applied,
                                audit,
                            },
                        )
                    }
                }
            });

        let result = self.repo.close_indeterminate(job_id, decide).await?;

        if let crate::ports::CloseIndeterminateResult::Applied(applied) = &result {
            // `Indeterminate` is always terminal — release exactly once,
            // mirroring `ActionEvidenceService`/`CancellationService`. A
            // no-op when the transient mapping no longer exists (e.g. after
            // a Server restart).
            if let Some(reservation) = self.reservations.take(applied.attempt.id) {
                self.arbiter.release(reservation);
            }
        }

        Ok(result)
    }
}

impl From<crate::ports::CloseIndeterminateError> for ApplicationError {
    fn from(err: crate::ports::CloseIndeterminateError) -> Self {
        match err {
            crate::ports::CloseIndeterminateError::JobNotFound(id) => {
                ApplicationError::JobNotFound(id)
            }
            crate::ports::CloseIndeterminateError::NoUncertainAttempt(id) => {
                ApplicationError::JobHasNoUncertainAttempt(id)
            }
            crate::ports::CloseIndeterminateError::Repository(e) => ApplicationError::Repository(e),
        }
    }
}

/// Independently verifies post-session evidence and correlates it to the
/// authoritative CurrentBoot under the Endpoint lock.
pub struct BootstrapEvidenceService<R: EndpointRepository> {
    repo: Arc<R>,
    accepted_site_keys: AcceptedSiteKeys,
}

impl<R: EndpointRepository> BootstrapEvidenceService<R> {
    pub fn new(repo: Arc<R>, accepted_site_keys: AcceptedSiteKeys) -> Self {
        Self {
            repo,
            accepted_site_keys,
        }
    }

    pub async fn verify_and_establish(
        &self,
        endpoint_id: EndpointId,
        evidence: &BootstrapEvidenceMessage,
        connection_fingerprint: ServerCertFingerprint,
    ) -> Result<BootstrapEvidenceResult, ApplicationError> {
        let Ok(declared_nonce) = BootNonce::parse_wire_value(&evidence.body.boot_nonce) else {
            return Ok(BootstrapEvidenceResult::Rejected);
        };
        let Ok(assertion) =
            BootstrapAssertion::parse_wire_value(&evidence.body.bootstrap_assertion)
        else {
            return Ok(BootstrapEvidenceResult::Rejected);
        };
        let Ok(verified) = assertion.verify(&self.accepted_site_keys) else {
            return Ok(BootstrapEvidenceResult::Rejected);
        };
        if verified.boot_nonce() != declared_nonce
            || verified.server_fingerprint() != connection_fingerprint
        {
            return Ok(BootstrapEvidenceResult::Rejected);
        }
        let decide: crate::ports::TrustedBootstrapDecision = Box::new(move |aggregate| {
            transitions::establish_trusted_bootstrap(&aggregate, declared_nonce)
        });
        let outcome = self
            .repo
            .establish_trusted_bootstrap(endpoint_id, decide)
            .await?;
        Ok(match outcome {
            transitions::TrustedBootstrapOutcome::Established(_) => {
                BootstrapEvidenceResult::Established
            }
            transitions::TrustedBootstrapOutcome::Rejected => BootstrapEvidenceResult::Rejected,
        })
    }
}

/// Boot Orchestration's Application-level responsibility
/// (`m0-stack-and-boundaries-baseline.md` "Component responsibilities and
/// boundaries" — Application: Boot Orchestration): issuing the boot-scoped
/// enrollment credential (ADR-0004 point 2) as a durable, self-locating
/// ADR-0014 credential, following the mandatory persist-before-deliver
/// ordering (ADR-0014 point 11). For WP1, the real PXE/boot-chain delivery of
/// this credential to an endpoint is faked by the Simulator fixture
/// (`m0-simulator-contract-and-validation-strategy.md`); this service's
/// issuance logic itself is real.
pub struct BootOrchestrationService<R: BootContextRepository> {
    repo: Arc<R>,
    enrollment_ttl: Duration,
}

impl<R: BootContextRepository> BootOrchestrationService<R> {
    pub fn new(repo: Arc<R>, enrollment_ttl: Duration) -> Self {
        Self {
            repo,
            enrollment_ttl,
        }
    }

    /// Issues a fresh boot-scoped enrollment credential: generates a
    /// self-locating `PresentedCredential::Enrollment`, derives its one-way
    /// verifier, and durably persists the backing `BootContext` — only after
    /// that persistence succeeds does this method return the credential
    /// (ADR-0014 point 11). A persistence failure returns an
    /// `ApplicationError` and never returns the generated credential; this
    /// method does not retry with a fresh credential of its own.
    ///
    /// `inventory_signal` is the current WP1 correlation-evidence stand-in
    /// stored on `BootContext` — evidence only, never authentication and
    /// never Endpoint identity (ADR-0004; ADR-0014 point 4).
    ///
    /// `boot_nonce` belongs to the trusted-bootstrap contract
    /// (`m0-trusted-bootstrap-and-server-fingerprint-contract.md` "(C)
    /// Authenticated and fresh bootstrap material") and is supplied by the
    /// caller — the trusted-bootstrap/boot boundary that generated it for
    /// this actual boot context. This service never generates or substitutes
    /// its own `BootNonce`; it only persists the one it was given, exactly.
    pub async fn issue_enrollment_credential(
        &self,
        inventory_signal: &str,
        boot_nonce: BootNonce,
        now: DateTime<Utc>,
    ) -> Result<PresentedCredential, ApplicationError> {
        let credential = PresentedCredential::generate(CredentialKind::Enrollment);
        let verifier = CredentialHash::of_bytes(credential.secret().expose_secret_bytes());
        let context = BootContext::new(
            credential.lookup_id().clone(),
            verifier,
            now,
            now + self.enrollment_ttl,
            inventory_signal.to_string(),
            boot_nonce,
        );
        self.repo.insert_boot_context(&context).await?;
        Ok(credential)
    }
}

/// Endpoint identity/credential enrollment operations
/// (`docs/decisions/0004-endpoint-identity-and-enrollment-bootstrap.md`;
/// ADR-0014).
pub struct EnrollmentService<R: EndpointRepository, C: CredentialRedemptionRepository> {
    endpoint_repo: Arc<R>,
    redemption_repo: Arc<C>,
    credential_ttl: Duration,
    clock: Arc<dyn Clock>,
}

impl<R: EndpointRepository, C: CredentialRedemptionRepository> EnrollmentService<R, C> {
    /// Uses [`SystemClock`] — real wall-clock time, evaluated at decision
    /// time by [`redeem`](Self::redeem). Use [`with_clock`](Self::with_clock)
    /// to inject a deterministic clock (e.g. for tests that must control
    /// simulated time precisely).
    pub fn new(endpoint_repo: Arc<R>, redemption_repo: Arc<C>) -> Self {
        Self::with_clock(endpoint_repo, redemption_repo, Arc::new(SystemClock))
    }

    pub fn with_clock(
        endpoint_repo: Arc<R>,
        redemption_repo: Arc<C>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            endpoint_repo,
            redemption_repo,
            credential_ttl: DEFAULT_CREDENTIAL_TTL,
            clock,
        }
    }

    pub fn with_credential_ttl(mut self, ttl: Duration) -> Self {
        self.credential_ttl = ttl;
        self
    }

    /// Redeems a presented credential in a fresh `AuthRequest`. Called by the
    /// Agent Control Gateway on every connection attempt, after the Server's
    /// own TLS layer has already
    /// completed — this method has no notion of TLS/WSS itself.
    ///
    /// `credential_wire` is the opaque value carried by `AuthRequest`
    /// (`m0-agent-protocol-contract.md`); this is the Application boundary
    /// that parses it into a [`PresentedCredential`] (ADR-0014 point 1: the
    /// wire shape carries no separate lookup/correlation field). A malformed
    /// value is rejected generically — `RedeemResult::Rejected` — never a
    /// detailed externally visible parse error.
    ///
    /// The decision (routing-target branching, credential verification,
    /// chain authentication, first-contact/genuine-reboot resolution) is
    /// handed to the repository as a closure so it executes *inside* the
    /// Adapter's lock/transaction scope on the routed target's current state
    /// — never on a state read before that lock was acquired (ADR-0012 point
    /// 7 commit-time concurrency; `crate::ports::CredentialRedemptionRepository`).
    /// `now` is deliberately not a parameter here: the closure reads
    /// `self.clock.now()` itself, at the moment the Adapter actually invokes
    /// it (i.e. after the lock), so credential-validity decisions are never
    /// made against a timestamp captured before a lock wait of unknown
    /// duration.
    pub async fn redeem(&self, credential_wire: &str) -> Result<RedeemResult, ApplicationError> {
        let Ok(presented) = PresentedCredential::parse(credential_wire) else {
            return Ok(RedeemResult::Rejected);
        };
        let kind = presented.kind();
        let lookup_id = presented.lookup_id().clone();
        let ttl = self.credential_ttl;
        let clock = Arc::clone(&self.clock);

        let decide: RedemptionDecision = Box::new(move |target| {
            // Read here, not before — this closure body only ever runs
            // after the Adapter has acquired every lock this target's
            // routing required.
            let now = clock.now();
            match target {
                RedemptionTarget::Endpoint(aggregate) => {
                    // Generated unconditionally for a path that may
                    // authenticate/establish; discarding a candidate whose
                    // redemption is ultimately rejected is acceptable
                    // (ADR-0014 "Runtime issuance").
                    let fresh = PresentedCredential::generate(CredentialKind::Runtime);
                    Ok(transitions::redeem_known(
                        &aggregate, &presented, &fresh, now, ttl,
                    ))
                }
                RedemptionTarget::UnresolvedBootContext {
                    context,
                    candidate_endpoint: None,
                } => {
                    let fresh = PresentedCredential::generate(CredentialKind::Runtime);
                    transitions::first_contact(&context, &presented, &fresh, now, ttl)
                }
                RedemptionTarget::UnresolvedBootContext {
                    context,
                    candidate_endpoint: Some(candidate),
                } => {
                    let fresh = PresentedCredential::generate(CredentialKind::Runtime);
                    transitions::genuine_reboot(&context, &candidate, &presented, &fresh, now, ttl)
                }
                RedemptionTarget::UnknownBootContext | RedemptionTarget::UnknownCredential => {
                    Ok(transitions::RedeemOutcome::Rejected)
                }
            }
        });

        let outcome = self
            .redemption_repo
            .redeem(kind, &lookup_id, decide)
            .await?;
        Ok(match outcome {
            transitions::RedeemOutcome::Established {
                outcome,
                issued,
                issued_expires_at,
                ..
            } => RedeemResult::Established {
                endpoint_id: outcome.endpoint.id,
                runtime_credential: issued,
                credential_expires_at: issued_expires_at,
            },
            transitions::RedeemOutcome::Rejected => RedeemResult::Rejected,
        })
    }

    /// The operator-approval control path
    /// (`docs/decisions/0004-endpoint-identity-and-enrollment-bootstrap.md`
    /// "Decision: operator-approval-gated first enrollment"; Issue #17
    /// "Safety constraints"). Callers of this method must be structurally
    /// separate from the Simulated Agent participant — an in-process
    /// test/development harness, a future Administrative API handler, or a
    /// CLI, never Agent Protocol message handling.
    pub async fn approve_enrollment(
        &self,
        endpoint_id: EndpointId,
        operator: Actor,
        now: DateTime<Utc>,
    ) -> Result<(), ApplicationError> {
        let decide: crate::ports::UpdateDecision =
            Box::new(move |aggregate| transitions::approve_enrollment(&aggregate, operator, now));
        self.endpoint_repo
            .update_endpoint(endpoint_id, decide)
            .await?;
        Ok(())
    }

    /// Exercises `CredentialRevoked` at the domain/persistence layer directly
    /// (Issue #17 "Safety constraints": no new operator-facing revocation API
    /// is introduced merely to demonstrate this for WP1).
    pub async fn revoke_credential(
        &self,
        endpoint_id: EndpointId,
        now: DateTime<Utc>,
    ) -> Result<(), ApplicationError> {
        let decide: crate::ports::UpdateDecision =
            Box::new(move |aggregate| Ok(transitions::revoke_credential(&aggregate, now)));
        self.endpoint_repo
            .update_endpoint(endpoint_id, decide)
            .await?;
        Ok(())
    }
}

/// The narrow pre-dispatch Transfer/Artifact/ChunkManifest metadata
/// operations Issue #36 requires
/// (`docs/specifications/m0-data-plane-and-storage-contracts.md`;
/// `m1-simulated-vertical-slice-and-baseline-validation.md` RF-005). Owns no
/// business rules itself — every method here calls exactly one
/// `bamep_domain` decision function and hands it to the `TransferRepository`
/// Port as a `decide` closure so the Adapter can invoke it under lock. This
/// service never hashes bulk bytes, reads files, writes storage, receives
/// HTTP requests, or verifies transfer capabilities — those responsibilities
/// belong to later Worker/HTTP Work Packages (#37/#38/#39); it only records
/// already-computed/verified facts handed to it.
pub struct TransferService<T: TransferRepository> {
    repo: Arc<T>,
}

impl<T: TransferRepository> TransferService<T> {
    pub fn new(repo: Arc<T>) -> Self {
        Self { repo }
    }

    /// Constructs and durably persists a fresh pre-dispatch
    /// `Transfer`/`Artifact`/empty `ChunkManifest` for one Endpoint/JobStep
    /// workflow context (`bamep_domain::create_transfer_context`; Issue #36
    /// "Pre-dispatch creation"). Never creates an Attempt or action
    /// identity, never transitions the JobStep, never evaluates the
    /// destructive-operation gate.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_transfer_context(
        &self,
        endpoint_id: EndpointId,
        job_id: JobId,
        job_step_id: JobStepId,
        direction: bamep_domain::TransferDirection,
        digest_algorithm: bamep_domain::DigestAlgorithm,
        chunk_size: bamep_domain::ChunkSize,
        source_provenance: bamep_domain::SourceProvenance,
    ) -> Result<bamep_domain::TransferContext, ApplicationError> {
        let context = bamep_domain::create_transfer_context(
            endpoint_id,
            job_id,
            job_step_id,
            direction,
            digest_algorithm,
            chunk_size,
            source_provenance,
        );
        self.repo.create_transfer_context(&context).await?;
        Ok(context)
    }

    /// Read-only reload of a persisted Transfer/Artifact/manifest plus its
    /// currently durably held/verified chunk indices (Issue #36 "Reload /
    /// restart").
    pub async fn find_transfer_context(
        &self,
        transfer_id: bamep_domain::TransferId,
    ) -> Result<
        Option<(
            bamep_domain::TransferContext,
            std::collections::BTreeSet<bamep_domain::ChunkIndex>,
        )>,
        ApplicationError,
    > {
        self.repo
            .find_transfer_context(transfer_id)
            .await
            .map_err(ApplicationError::from)
    }

    /// Records one expected chunk identity
    /// (`bamep_domain::ChunkManifest::record_expected_chunk`): a genuinely
    /// new index continues an unsealed manifest; an identical already-
    /// recorded index is idempotent; a conflicting index is rejected without
    /// ever rewriting the original expected identity.
    pub async fn record_expected_chunk(
        &self,
        transfer_id: bamep_domain::TransferId,
        index: bamep_domain::ChunkIndex,
        size: u32,
        digest_bytes: Vec<u8>,
    ) -> Result<bamep_domain::ChunkRecordOutcome, ApplicationError> {
        let decide: crate::ports::RecordChunkDecision = Box::new(move |facts| {
            facts
                .manifest
                .record_expected_chunk(index, size, digest_bytes)
        });
        self.repo
            .record_expected_chunk(transfer_id, decide)
            .await
            .map_err(ApplicationError::from)
    }

    /// Durably marks `index` held once its already independently verified
    /// digest matches the recorded expected identity
    /// (`bamep_domain::validate_verified_chunk`). Idempotent when `index` is
    /// already held.
    pub async fn accept_verified_chunk(
        &self,
        transfer_id: bamep_domain::TransferId,
        index: bamep_domain::ChunkIndex,
        verified_digest_bytes: Vec<u8>,
    ) -> Result<crate::ports::AcceptChunkOutcome, ApplicationError> {
        let decide: crate::ports::AcceptChunkDecision = Box::new(move |facts| {
            bamep_domain::validate_verified_chunk(&facts.manifest, index, &verified_digest_bytes)
        });
        self.repo
            .accept_verified_chunk(transfer_id, index, decide)
            .await
            .map_err(ApplicationError::from)
    }

    /// Seals the manifest (`bamep_domain::ChunkManifest::seal`): idempotent
    /// on an identical already-sealed retry, rejected on conflicting reseal.
    pub async fn seal_manifest(
        &self,
        transfer_id: bamep_domain::TransferId,
        chunk_count: u32,
        artifact_digest_bytes: Vec<u8>,
    ) -> Result<bamep_domain::SealOutcome, ApplicationError> {
        let decide: crate::ports::SealManifestDecision =
            Box::new(move |facts| facts.manifest.seal(chunk_count, artifact_digest_bytes));
        self.repo
            .seal_manifest(transfer_id, decide)
            .await
            .map_err(ApplicationError::from)
    }

    /// `Incomplete -> PendingVerification` (`bamep_domain::begin_verification`):
    /// requires a sealed manifest with every expected chunk durably held.
    pub async fn begin_artifact_verification(
        &self,
        transfer_id: bamep_domain::TransferId,
    ) -> Result<bamep_domain::Artifact, ApplicationError> {
        let decide: crate::ports::ArtifactTransitionDecision = Box::new(move |facts| {
            bamep_domain::begin_verification(
                &facts.artifact,
                &facts.manifest,
                &facts.held_chunk_indices,
            )
        });
        self.repo
            .begin_artifact_verification(transfer_id, decide)
            .await
            .map_err(ApplicationError::from)
    }

    /// `PendingVerification -> Verified | Failed`
    /// (`bamep_domain::complete_verification`), decided by an already
    /// independently computed full-Artifact digest match. This method
    /// performs no hashing itself.
    pub async fn complete_artifact_verification(
        &self,
        transfer_id: bamep_domain::TransferId,
        digest_matches: bool,
    ) -> Result<bamep_domain::Artifact, ApplicationError> {
        let decide: crate::ports::ArtifactTransitionDecision = Box::new(move |facts| {
            bamep_domain::complete_verification(&facts.artifact, digest_matches)
        });
        self.repo
            .complete_artifact_verification(transfer_id, decide)
            .await
            .map_err(ApplicationError::from)
    }

    /// `Incomplete -> Failed` (`bamep_domain::fail_incomplete`): a required
    /// chunk could not be reproduced/verified, or capture/transfer was
    /// abandoned/cancelled.
    pub async fn fail_incomplete_artifact(
        &self,
        transfer_id: bamep_domain::TransferId,
    ) -> Result<bamep_domain::Artifact, ApplicationError> {
        let decide: crate::ports::ArtifactTransitionDecision =
            Box::new(move |facts| bamep_domain::fail_incomplete(&facts.artifact));
        self.repo
            .fail_incomplete_artifact(transfer_id, decide)
            .await
            .map_err(ApplicationError::from)
    }

    /// Binds this Transfer to its owning `attempt` exactly once
    /// (`bamep_domain::bind_attempt`; Issue #36 "Transfer -> Attempt binding
    /// support"). This method never creates the Attempt itself — the caller
    /// supplies an already-committed `Attempt` value. Idempotent when
    /// already bound to this exact Attempt; rejects a conflicting rebind.
    pub async fn bind_attempt(
        &self,
        transfer_id: bamep_domain::TransferId,
        attempt: Attempt,
    ) -> Result<bamep_domain::Transfer, ApplicationError> {
        let decide: crate::ports::BindAttemptDecision =
            Box::new(move |facts| bamep_domain::bind_attempt(&facts.transfer, &attempt));
        self.repo
            .bind_attempt(transfer_id, decide)
            .await
            .map_err(ApplicationError::from)
    }
}

// ---------------------------------------------------------------------
// Transfer authorization (Issue #38)
// ---------------------------------------------------------------------

/// Outcome of [`TransferAuthorizationService::issue`], shaped for the Agent
/// Control Gateway to translate directly into
/// `TransferAuthorizationGrant`/`TransferAuthorizationDenied`/generic
/// `ProtocolError` (`m0-agent-protocol-contract.md` "Transfer authorization",
/// "Correlation"). `Denied` deliberately carries no reason — every internal
/// *semantic* denial cause (unknown transfer, wrong Endpoint, pre-dispatch
/// unbound Transfer, terminal Attempt, inactive credential, malformed proof
/// key) collapses into this one generic outcome before it ever reaches the
/// Gateway.
///
/// `ProtocolViolation` is the separate, narrower case where the request is
/// already known to belong to this authenticated Endpoint's exact Transfer
/// and current non-terminal Attempt, but presents a `correlation_id` that is
/// not that Attempt's own `action_id` (Issue #38 final correction: a wrong
/// action-scoped correlation is a protocol violation per
/// `m0-agent-protocol-contract.md` "Correlation"/"Message envelope", never a
/// `TransferAuthorizationDenied` — a `Denied` message wire-invalidly carrying
/// a `correlation_id` other than the owning `action_id` would itself violate
/// that same rule). The Gateway maps this to a generic `ProtocolError`
/// correlated to the offending request's `message_id`, never to the
/// presented `correlation_id` and never to the durable owning `action_id`,
/// which stays non-enumerable either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferAuthorizationOutcome {
    Granted {
        token: String,
        expires_at: DateTime<Utc>,
        data_plane_base_url: String,
    },
    Denied,
    ProtocolViolation,
}

/// The exact fields Worker forwards from one `AuthorizationQuery`
/// (`m1-worker-data-plane-control-contract.md` "Authorization query /
/// decision"), already converted from wire types into this layer's working
/// representation by the UDS Adapter — this struct itself carries no
/// `bamep-worker-protocol` dependency.
#[derive(Debug, Clone)]
pub struct WorkerAuthorizationQueryInput {
    pub token: String,
    pub operation: AuthorizationOperation,
    pub transfer_id: uuid::Uuid,
    pub artifact_id: uuid::Uuid,
    pub direction: TransferDirection,
    pub chunk_index: Option<u64>,
    pub proof_id: String,
    pub issued_at_millis: u64,
    pub signature: String,
}

/// Outcome of [`TransferAuthorizationService::decide`]
/// (`m1-worker-data-plane-control-contract.md` "Authorization query /
/// decision"). `Denied` never carries a reason, mirroring
/// [`TransferAuthorizationOutcome::Denied`] — Worker must never be able to
/// observe why. `expected_chunk_digest` is never populated by this Work
/// Package (durable chunk-identity lookup is #39's
/// `ChunkAcceptanceRequest`/`ChunkAcceptanceDecision` scope); the field
/// exists so a future #39 change extends this outcome rather than
/// introducing a second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAuthorizationOutcome {
    Approved {
        expected_chunk_digest: Option<String>,
    },
    Denied,
}

/// Issues and later authoritatively decides sender-constrained transfer
/// authorization (`m0-data-plane-and-storage-contracts.md` "Transfer
/// authorization"; Issue #38). The single Application-layer owner of both
/// directions of this boundary: [`Self::issue`] serves the authenticated
/// Agent WSS `TransferAuthorizationRequest`, and [`Self::decide`] serves the
/// Worker UDS `AuthorizationQuery` — the same [`CapabilityStore`]/
/// [`ReplayCache`]/[`TransferAuthorizationRepository`] back both, so a
/// capability issued through one path is exactly what the other later
/// validates. Business authorization logic lives here and in
/// `bamep_domain::transfer_authorization`, never in the Worker protocol
/// codec, UDS transport, or PostgreSQL Adapter (`AGENTS.md` "Application /
/// Port / Adapter boundary").
pub struct TransferAuthorizationService {
    repo: Arc<dyn TransferAuthorizationRepository>,
    capabilities: Arc<CapabilityStore>,
    replay: Arc<ReplayCache>,
    clock: Arc<dyn Clock>,
    capability_ttl: Duration,
    data_plane_base_url: String,
}

impl TransferAuthorizationService {
    pub fn new(
        repo: Arc<dyn TransferAuthorizationRepository>,
        capabilities: Arc<CapabilityStore>,
        replay: Arc<ReplayCache>,
        data_plane_base_url: impl Into<String>,
    ) -> Self {
        Self {
            repo,
            capabilities,
            replay,
            clock: Arc::new(SystemClock),
            capability_ttl: Duration::milliseconds(DEFAULT_CAPABILITY_TTL_MILLIS),
            data_plane_base_url: data_plane_base_url.into(),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_capability_ttl(mut self, ttl: Duration) -> Self {
        self.capability_ttl = ttl;
        self
    }

    /// Serves an authenticated Agent's `TransferAuthorizationRequest`
    /// (`m0-agent-protocol-contract.md` "Transfer authorization"): the
    /// authenticated session's `endpoint_id` and the message's own
    /// `correlation_id` are the caller's responsibility to supply from
    /// already-trusted transport/session state, never from anything else the
    /// request body could claim (`m0-agent-protocol-contract.md`; Issue #38
    /// "Agent authorization request handling": "The authenticated transport/
    /// session Endpoint identity is authoritative. Do NOT allow the request
    /// body to select another Endpoint").
    ///
    /// Also the exact renewal path (`m0-agent-protocol-contract.md` "Renewal
    /// and restart"): a second call for the same still-eligible `transfer_id`
    /// — with the same or a fresh `proof_public_key` — mints a fresh
    /// capability without creating any new Attempt/Artifact/Transfer, because
    /// this method never creates any of them; it only reads current durable
    /// state and issues transient authorization material.
    pub async fn issue(
        &self,
        endpoint_id: EndpointId,
        presented_action_id: ProtocolId,
        transfer_id: TransferId,
        proof_public_key_wire: &str,
    ) -> Result<TransferAuthorizationOutcome, ApplicationError> {
        let Ok(public_key) = ProofPublicKey::parse_wire_value(proof_public_key_wire) else {
            return Ok(TransferAuthorizationOutcome::Denied);
        };

        let Some(state) = self.repo.load_authorization_state(transfer_id).await? else {
            return Ok(TransferAuthorizationOutcome::Denied);
        };

        if state.transfer.endpoint_id != endpoint_id {
            return Ok(TransferAuthorizationOutcome::Denied);
        }

        // Pre-dispatch: `Transfer.attempt_id == None` is always denied
        // (`m0-data-plane-and-storage-contracts.md`; Issue #36 scope; Issue
        // #38 acceptance criteria).
        let Some(attempt) = state.attempt else {
            return Ok(TransferAuthorizationOutcome::Denied);
        };

        // The owning Attempt must be exactly `InProgress` — the durable phase
        // fact that the Agent's `ActionAck{outcome: Accepted}` has been
        // processed (`Dispatched --ActionAck{Accepted}--> InProgress`,
        // `bamep_domain::apply_action_evidence`). `m0-agent-protocol-contract.md`
        // "Transfer authorization": a `TransferAuthorizationRequest` is valid
        // only "After `SessionEstablished` and `ActionAck{outcome: Accepted}`
        // for a data-plane transfer action" — so a still-`Dispatched` Attempt
        // is too early and is denied. `AwaitingReconciliation` and every
        // terminal state (`Succeeded`/`Failed`/`Cancelled`/`Rejected`/
        // `Indeterminate`) are denied too: no Job-lifecycle mechanism yet
        // permits continuation from `AwaitingReconciliation`
        // (`m0-data-plane-and-storage-contracts.md` "Disconnect and restart"),
        // so the fail-closed default applies.
        if attempt.state != AttemptState::InProgress {
            return Ok(TransferAuthorizationOutcome::Denied);
        }

        // The request has already passed ownership/context checks that make
        // this comparison legitimate for this exact authenticated Endpoint
        // (Transfer belongs to `endpoint_id`, an owning Attempt exists and is
        // `InProgress`) — only now is a wrong presented `correlation_id`
        // classified as a protocol violation rather than folded into the
        // generic semantic denial (Issue #38 final correction §5: this
        // ordering is what prevents a cross-Endpoint/cross-Transfer oracle —
        // a request that has not yet proven it may legitimately compare
        // against this owning `action_id` never reaches this branch).
        let expected_action_id = ProtocolId::from_uuid(attempt.action_id.0)
            .expect("a Domain ActionId is always a valid UUID v4");
        if presented_action_id != expected_action_id {
            return Ok(TransferAuthorizationOutcome::ProtocolViolation);
        }

        let now = self.clock.now();
        if state.endpoint.credential.dimension(now) != CredentialDimension::CredentialActive {
            return Ok(TransferAuthorizationOutcome::Denied);
        }

        let token = CapabilityToken::generate();
        let capability_id = CapabilityId::from_token(&token);
        let expires_at = now + self.capability_ttl;
        let binding = CapabilityBinding {
            endpoint_id,
            transfer_id,
            artifact_id: state.transfer.artifact_id,
            direction: state.transfer.direction,
            attempt_id: attempt.id,
            proof_public_key: public_key,
            expires_at,
            epoch: self.capabilities.epoch(),
        };
        self.capabilities.evict_expired(now);
        if self.capabilities.issue(capability_id, binding).is_err() {
            // Either the bounded issued-capability store is saturated with
            // live capabilities, or (cryptographically negligible) this
            // freshly minted `capability_id` collided with a still-live
            // binding — `CapabilityStore::issue` never overwrites it either
            // way. Fail closed as the single generic denial — neither cause
            // is ever an externally enumerable reason (Issue #38 correction
            // §7/§25; final correction §9).
            return Ok(TransferAuthorizationOutcome::Denied);
        }

        Ok(TransferAuthorizationOutcome::Granted {
            token: token.as_str().to_string(),
            expires_at,
            data_plane_base_url: self.data_plane_base_url.clone(),
        })
    }

    /// Serves Worker's `AuthorizationQuery` (`m1-worker-data-plane-control-
    /// contract.md` "Authorization query / decision"): the authoritative
    /// decision `bamepd` alone may make (ADR-0018) — Worker only forwards
    /// mechanical request facts and consumes the result. Re-checks current
    /// durable state on every call (never trusts the capability-issuance-time
    /// snapshot alone), so `CredentialRevoked`/a newly terminal Attempt take
    /// effect immediately, per-request (Issue #38 "Credential active /
    /// revocation").
    pub async fn decide(
        &self,
        input: WorkerAuthorizationQueryInput,
    ) -> Result<WorkerAuthorizationOutcome, ApplicationError> {
        // Logical ordering (Issue #38 correction §5): parse/lookup ->
        // capability current/expiry/binding -> proof structural parse ->
        // freshness -> signature -> current durable authorization (including
        // operation eligibility) -> only then the atomic replay check+insert
        // -> approved. A request that would otherwise be denied never
        // permanently consumes its `proof_id`.

        // 1. token / capability parse and lookup
        let capability_id = CapabilityId::from_token_bytes(input.token.as_bytes());
        let Some(binding) = self.capabilities.lookup(&capability_id) else {
            return Ok(WorkerAuthorizationOutcome::Denied);
        };

        let now = self.clock.now();

        // 2. capability current / expiry / epoch and operation scope binding
        if capability_is_current(&binding, now, self.capabilities.epoch()).is_err() {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }
        let requested = RequestedOperation {
            operation: input.operation,
            transfer_id: TransferId(input.transfer_id),
            artifact_id: ArtifactId(input.artifact_id),
            direction: input.direction,
            chunk_index: input.chunk_index,
        };
        if capability_matches_request(&binding, &requested).is_err() {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }

        // 3. proof structural parsing
        let Ok(proof_id) = ProofId::parse_wire_value(&input.proof_id) else {
            return Ok(WorkerAuthorizationOutcome::Denied);
        };
        let Ok(signature) = ProofSignature::parse_wire_value(&input.signature) else {
            return Ok(WorkerAuthorizationOutcome::Denied);
        };

        // 4. freshness
        if proof_is_fresh(input.issued_at_millis, now).is_err() {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }

        // 5. signature over the independently reconstructed canonical transcript
        let transcript = build_proof_transcript(
            &capability_id,
            &ProofTranscriptFields {
                operation: input.operation,
                transfer_id: requested.transfer_id,
                artifact_id: requested.artifact_id,
                direction: input.direction,
                chunk_index: input.chunk_index,
                proof_id,
                issued_at_millis: input.issued_at_millis,
            },
        );
        if !verify_proof_signature(&binding.proof_public_key, &transcript, &signature) {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }

        // 6. current durable authorization, re-read fresh for this exact
        // request in one consistent locked snapshot — never merely the
        // capability-issuance-time snapshot (Issue #38 "Current attempt /
        // transfer state"; "Credential active / revocation").
        let Some(state) = self
            .repo
            .load_authorization_state(requested.transfer_id)
            .await?
        else {
            return Ok(WorkerAuthorizationOutcome::Denied);
        };
        if state.transfer.endpoint_id != binding.endpoint_id
            || state.transfer.artifact_id != binding.artifact_id
            || state.transfer.direction != binding.direction
        {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }
        let Some(attempt) = state.attempt else {
            return Ok(WorkerAuthorizationOutcome::Denied);
        };
        if attempt.id != binding.attempt_id {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }
        // Only an `InProgress` Attempt (post-`ActionAck{Accepted}`) may
        // continue: a regression to `Dispatched`, `AwaitingReconciliation`,
        // or any terminal state fails closed (Issue #38 correction §8–§9;
        // `m0-data-plane-and-storage-contracts.md` "Disconnect and restart").
        if attempt.state != AttemptState::InProgress {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }
        if state.endpoint.credential.dimension(now) != CredentialDimension::CredentialActive {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }

        // Current data-plane operation eligibility against the Artifact/
        // manifest state in this same snapshot (Issue #38 correction
        // §10–§11). A `chunk_index` beyond the manifest's 32-bit index space
        // can never have a durable expected identity — fail closed rather
        // than treat it as a fresh continuation.
        let chunk_index_u32 = match input.chunk_index {
            Some(index) => match u32::try_from(index) {
                Ok(index) => Some(bamep_domain::ChunkIndex(index)),
                Err(_) => return Ok(WorkerAuthorizationOutcome::Denied),
            },
            None => None,
        };
        let expected_chunk =
            chunk_index_u32.and_then(|index| state.manifest.expected_chunk(index).cloned());
        if bamep_domain::data_plane_operation_is_current(
            input.operation,
            state.artifact.state,
            state.manifest.sealed,
            expected_chunk.is_some(),
        )
        .is_err()
        {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }

        // 7. only now that the request would otherwise be approved: the
        // atomic replay check+insert. Retained until the proof's own signed
        // freshness deadline (`issued_at + PROOF_FRESHNESS_PAST_WINDOW`), so
        // every `proof_id` that could still pass `proof_is_fresh` — including
        // one issued at the maximum accepted future skew — stays
        // replay-protected (Issue #38 correction §3–§4). Both a replay and a
        // bounded-capacity saturation fail closed as the same generic denial
        // (§6).
        let replay_valid_until = DateTime::from_timestamp_millis(
            bamep_domain::proof_replay_valid_until_millis(input.issued_at_millis)
                .clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        )
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
        if self
            .replay
            .check_and_insert(proof_id, now, replay_valid_until)
            .is_err()
        {
            return Ok(WorkerAuthorizationOutcome::Denied);
        }

        // 8. approved. For an approved `chunk_upload` whose `chunk_index`
        // already carries a durable expected identity, carry that recorded
        // expected digest so the Worker can enforce the manifest identity
        // (`m1-worker-data-plane-control-contract.md` "Authorization query /
        // decision"). #38 only *carries* the expected digest; the Worker's
        // comparison against the Agent-declared digest and the resulting
        // HTTP `409` is #39 (correction §14).
        let expected_chunk_digest = match input.operation {
            AuthorizationOperation::ChunkUpload => {
                expected_chunk.map(|chunk| chunk.digest.to_wire_value())
            }
            AuthorizationOperation::ResumeDiscovery | AuthorizationOperation::SealManifest => None,
        };

        Ok(WorkerAuthorizationOutcome::Approved {
            expected_chunk_digest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Minimal in-memory `BootContextRepository` fake for Application-level
    /// unit tests that need precise, DB-free control over persistence
    /// success/failure and immediate visibility into what was persisted
    /// (`docs/development/testing.md` "Fakes and test boundaries"). The real
    /// PostgreSQL persistence path is covered separately by
    /// `crates/server/tests/boot_orchestration_service.rs`.
    #[derive(Default)]
    struct FakeBootContextRepository {
        contexts: Mutex<Vec<BootContext>>,
        fail: bool,
    }

    impl FakeBootContextRepository {
        fn new() -> Self {
            Self::default()
        }

        fn failing() -> Self {
            Self {
                contexts: Mutex::new(Vec::new()),
                fail: true,
            }
        }

        fn persisted(&self) -> Vec<BootContext> {
            self.contexts.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl BootContextRepository for FakeBootContextRepository {
        async fn insert_boot_context(&self, context: &BootContext) -> Result<(), RepositoryError> {
            if self.fail {
                return Err(RepositoryError::Backend(
                    "simulated persistence failure".into(),
                ));
            }
            self.contexts.lock().unwrap().push(context.clone());
            Ok(())
        }
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn test_boot_nonce() -> BootNonce {
        BootNonce::from_bytes([0x5A; 32])
    }

    #[tokio::test]
    async fn issuance_returns_a_valid_self_locating_enrollment_credential() {
        let repo = Arc::new(FakeBootContextRepository::new());
        let service = BootOrchestrationService::new(repo, Duration::minutes(5));

        let credential = service
            .issue_enrollment_credential("sim-boot-orch-01", test_boot_nonce(), now())
            .await
            .expect("issuance must succeed");

        assert_eq!(credential.kind(), CredentialKind::Enrollment);
        // Self-locating: round-trips through the wire encoding cleanly.
        let wire = credential.to_wire_value();
        let parsed = PresentedCredential::parse(&wire).expect("must parse");
        assert_eq!(parsed.lookup_id(), credential.lookup_id());
    }

    #[tokio::test]
    async fn boot_context_is_durably_persisted_before_the_credential_is_returned() {
        let repo = Arc::new(FakeBootContextRepository::new());
        let service = BootOrchestrationService::new(Arc::clone(&repo), Duration::minutes(5));

        assert!(repo.persisted().is_empty());
        let credential = service
            .issue_enrollment_credential("sim-boot-orch-02", test_boot_nonce(), now())
            .await
            .expect("issuance must succeed");

        let persisted = repo.persisted();
        assert_eq!(
            persisted.len(),
            1,
            "BootContext must be durably persisted exactly once by the time issuance returns"
        );
        assert_eq!(persisted[0].boot_context_id(), credential.lookup_id());
    }

    #[tokio::test]
    async fn persisted_boot_context_matches_the_returned_credential() {
        let repo = Arc::new(FakeBootContextRepository::new());
        let ttl = Duration::minutes(5);
        let service = BootOrchestrationService::new(Arc::clone(&repo), ttl);
        let issued_at = now();
        let boot_nonce = test_boot_nonce();

        let credential = service
            .issue_enrollment_credential("sim-boot-orch-03", boot_nonce, issued_at)
            .await
            .expect("issuance must succeed");

        let persisted = repo.persisted();
        let context = &persisted[0];

        assert_eq!(context.boot_context_id(), credential.lookup_id());
        assert!(context.verify_secret(credential.secret()));
        assert_eq!(context.issued_at(), issued_at);
        assert_eq!(context.expires_at(), issued_at + ttl);
        assert_eq!(context.inventory_signal(), "sim-boot-orch-03");
        assert_eq!(context.resolved_endpoint_id(), None);
        assert_eq!(context.boot_nonce(), boot_nonce);
    }

    #[tokio::test]
    async fn caller_supplied_boot_nonce_is_persisted_exactly_and_never_substituted() {
        let repo = Arc::new(FakeBootContextRepository::new());
        let service = BootOrchestrationService::new(Arc::clone(&repo), Duration::minutes(5));
        let boot_nonce = BootNonce::from_bytes([0x77; 32]);

        service
            .issue_enrollment_credential("sim-boot-orch-nonce-01", boot_nonce, now())
            .await
            .expect("issuance must succeed");

        let persisted = repo.persisted();
        assert_eq!(
            persisted[0].boot_nonce(),
            boot_nonce,
            "the service must persist exactly the caller-supplied BootNonce, never one of its own"
        );
    }

    #[tokio::test]
    async fn two_issuances_generate_distinct_lookup_ids_and_secrets() {
        let repo = Arc::new(FakeBootContextRepository::new());
        let service = BootOrchestrationService::new(Arc::clone(&repo), Duration::minutes(5));

        let a = service
            .issue_enrollment_credential("sim-boot-orch-04", test_boot_nonce(), now())
            .await
            .unwrap();
        let b = service
            .issue_enrollment_credential("sim-boot-orch-04", test_boot_nonce(), now())
            .await
            .unwrap();

        assert_ne!(a.lookup_id(), b.lookup_id());
        assert_ne!(
            a.secret().expose_secret_bytes(),
            b.secret().expose_secret_bytes()
        );
    }

    #[tokio::test]
    async fn persistence_failure_yields_an_application_error_and_no_credential() {
        let repo = Arc::new(FakeBootContextRepository::failing());
        let service = BootOrchestrationService::new(Arc::clone(&repo), Duration::minutes(5));

        let err = service
            .issue_enrollment_credential("sim-boot-orch-05", test_boot_nonce(), now())
            .await
            .unwrap_err();

        assert!(matches!(err, ApplicationError::Repository(_)));
        assert!(repo.persisted().is_empty());
    }

    mod destructive_intent_service {
        use super::*;
        use bamep_domain::{create_workflow, InventoryRevisionId, JobStep, TargetFingerprint};
        use std::collections::HashMap;

        /// In-memory `JobRepository` fake mirroring `PostgresJobRepository`'s
        /// `authorize_destructive_intent` contract (Issue #31 "Application /
        /// internal harness path"): `decide` is invoked with the current
        /// freshly-read `JobStep` and only an `Ok` result is applied, exactly
        /// like the real Adapter's lock-then-decide-then-persist sequence.
        /// The real PostgreSQL persistence/atomicity/race path is covered
        /// separately by `crates/server/tests/destructive_intent_authorization.rs`.
        #[derive(Default)]
        struct FakeJobRepository {
            jobs: Mutex<HashMap<JobId, Job>>,
            fail_persist: bool,
        }

        impl FakeJobRepository {
            fn with_job(job: Job) -> Self {
                let mut jobs = HashMap::new();
                jobs.insert(job.id, job);
                Self {
                    jobs: Mutex::new(jobs),
                    fail_persist: false,
                }
            }

            fn failing_persist(job: Job) -> Self {
                let mut jobs = HashMap::new();
                jobs.insert(job.id, job);
                Self {
                    jobs: Mutex::new(jobs),
                    fail_persist: true,
                }
            }

            fn step(&self, job_id: JobId, step_id: JobStepId) -> JobStep {
                self.jobs
                    .lock()
                    .unwrap()
                    .get(&job_id)
                    .unwrap()
                    .steps
                    .iter()
                    .find(|s| s.id == step_id)
                    .unwrap()
                    .clone()
            }
        }

        #[async_trait]
        impl JobRepository for FakeJobRepository {
            async fn create_workflow(&self, job: &Job) -> Result<(), CreateWorkflowError> {
                self.jobs.lock().unwrap().insert(job.id, job.clone());
                Ok(())
            }

            async fn find_job(&self, id: JobId) -> Result<Option<Job>, RepositoryError> {
                Ok(self.jobs.lock().unwrap().get(&id).cloned())
            }

            async fn authorize_destructive_intent(
                &self,
                job_id: JobId,
                step_id: JobStepId,
                decide: AuthorizeDestructiveIntentDecision,
            ) -> Result<DestructiveIntent, AuthorizeDestructiveIntentError> {
                let mut jobs = self.jobs.lock().unwrap();
                let Some(job) = jobs.get_mut(&job_id) else {
                    return Err(AuthorizeDestructiveIntentError::JobStepNotFound(
                        step_id, job_id,
                    ));
                };
                let Some(index) = job.steps.iter().position(|s| s.id == step_id) else {
                    return Err(AuthorizeDestructiveIntentError::JobStepNotFound(
                        step_id, job_id,
                    ));
                };

                let intent = match decide(&job.steps[index]) {
                    Ok(intent) => intent,
                    Err(bamep_domain::DestructiveIntentError::WrongJob) => {
                        return Err(AuthorizeDestructiveIntentError::JobStepNotFound(
                            step_id, job_id,
                        ))
                    }
                    Err(bamep_domain::DestructiveIntentError::NotEligible) => {
                        return Err(AuthorizeDestructiveIntentError::NotEligible(step_id))
                    }
                    Err(bamep_domain::DestructiveIntentError::AlreadyAuthorized) => {
                        return Err(AuthorizeDestructiveIntentError::AlreadyAuthorized(step_id))
                    }
                };

                if self.fail_persist {
                    return Err(AuthorizeDestructiveIntentError::Repository(
                        RepositoryError::Backend("simulated persistence failure".into()),
                    ));
                }
                job.steps[index].destructive_intent = Some(intent.clone());
                Ok(intent)
            }

            async fn admit_job(
                &self,
                _job_id: JobId,
                _decide: crate::ports::AdmitJobDecision,
            ) -> Result<Job, crate::ports::AdmitJobError> {
                unimplemented!("DestructiveIntentService never admits a Job")
            }

            async fn satisfy_current_step_preconditions(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _decide: crate::ports::SatisfyStepPreconditionsDecision,
            ) -> Result<JobStep, crate::ports::SatisfyStepPreconditionsError> {
                unimplemented!("DestructiveIntentService never advances a JobStep")
            }

            async fn commit_destructive_dispatch(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _decide: crate::ports::FinalDispatchDecision,
            ) -> Result<FinalDispatchOutcome, crate::ports::CommitDestructiveDispatchError>
            {
                unimplemented!("DestructiveIntentService never commits a dispatch")
            }

            async fn commit_transfer_dispatch(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _transfer_id: bamep_domain::TransferId,
                _decide: crate::ports::TransferDispatchDecision,
            ) -> Result<
                bamep_domain::TransferDispatchOutcome,
                crate::ports::CommitTransferDispatchError,
            > {
                unimplemented!("DestructiveIntentService never commits a transfer dispatch")
            }

            async fn find_attempt(
                &self,
                _attempt_id: bamep_domain::AttemptId,
            ) -> Result<Option<bamep_domain::Attempt>, RepositoryError> {
                unimplemented!("DestructiveIntentService never reads an Attempt")
            }

            async fn apply_action_evidence(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: crate::ports::ApplyActionEvidenceDecision,
            ) -> Result<
                crate::ports::ApplyActionEvidenceResult,
                crate::ports::ApplyActionEvidenceError,
            > {
                unimplemented!("DestructiveIntentService never applies action evidence")
            }

            async fn action_targets_endpoint(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
            ) -> Result<bool, RepositoryError> {
                unimplemented!("DestructiveIntentService never correlates ActionProgress")
            }

            async fn request_cancellation(
                &self,
                _job_id: JobId,
                _decide: crate::ports::RequestCancellationDecision,
            ) -> Result<
                crate::ports::RequestCancellationResult,
                crate::ports::RequestCancellationError,
            > {
                unimplemented!("DestructiveIntentService never requests cancellation")
            }

            async fn apply_cancel_ack(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: crate::ports::ApplyCancelAckDecision,
            ) -> Result<crate::ports::ApplyCancelAckResult, crate::ports::ApplyActionEvidenceError>
            {
                unimplemented!("DestructiveIntentService never applies CancelAck evidence")
            }

            async fn mark_endpoint_active_attempt_uncertain(
                &self,
                _endpoint_id: EndpointId,
                _decide: crate::ports::MarkUncertainDecision,
            ) -> Result<Option<bamep_domain::AttemptId>, RepositoryError> {
                unimplemented!("DestructiveIntentService never reconciles Attempts")
            }

            async fn reconcile_all_active_attempts_on_startup(
                &self,
                _decide: crate::ports::MarkUncertainDecision,
            ) -> Result<Vec<bamep_domain::AttemptId>, RepositoryError> {
                unimplemented!("DestructiveIntentService never reconciles Attempts")
            }

            async fn find_reconciliation_candidate(
                &self,
                _endpoint_id: EndpointId,
            ) -> Result<Option<bamep_domain::ActionId>, RepositoryError> {
                unimplemented!("DestructiveIntentService never reconciles Attempts")
            }

            async fn apply_status_report(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: crate::ports::ApplyReconciliationDecision,
            ) -> Result<
                crate::ports::ApplyReconciliationResult,
                crate::ports::ApplyActionEvidenceError,
            > {
                unimplemented!("DestructiveIntentService never applies StatusReport evidence")
            }

            async fn close_indeterminate(
                &self,
                _job_id: JobId,
                _decide: crate::ports::CloseIndeterminateDecision,
            ) -> Result<crate::ports::CloseIndeterminateResult, crate::ports::CloseIndeterminateError>
            {
                unimplemented!("DestructiveIntentService never closes Indeterminate")
            }
        }

        /// In-memory `InventoryRepository` fake exposing only a configurable
        /// current revision per Endpoint — `record_inventory` is unused by
        /// `DestructiveIntentService` and is not exercised here.
        #[derive(Default)]
        struct FakeInventoryRepository {
            current: Mutex<HashMap<EndpointId, InventoryRevision>>,
        }

        impl FakeInventoryRepository {
            fn new() -> Self {
                Self::default()
            }

            fn set_current(&self, endpoint_id: EndpointId, revision: InventoryRevision) {
                self.current.lock().unwrap().insert(endpoint_id, revision);
            }
        }

        #[async_trait]
        impl InventoryRepository for FakeInventoryRepository {
            async fn record_inventory(
                &self,
                _endpoint_id: EndpointId,
                _inventory: InventorySnapshot,
                _recorded_at: DateTime<Utc>,
            ) -> Result<Option<InventoryRevision>, EndpointUpdateError> {
                unimplemented!("DestructiveIntentService never records inventory")
            }

            async fn find_current_inventory(
                &self,
                endpoint_id: EndpointId,
            ) -> Result<Option<InventoryRevision>, EndpointUpdateError> {
                Ok(self.current.lock().unwrap().get(&endpoint_id).cloned())
            }
        }

        /// In-memory `TargetRevalidationPort` fake, independent of any
        /// `InventoryRevision` state — mirrors
        /// `crate::adapters::target_revalidation_fixture::FixtureTargetRevalidation`.
        #[derive(Default)]
        struct FakeTargetRevalidation {
            current: Mutex<HashMap<EndpointId, TargetFingerprint>>,
        }

        impl FakeTargetRevalidation {
            fn new() -> Self {
                Self::default()
            }

            fn set_current(&self, endpoint_id: EndpointId, fingerprint: TargetFingerprint) {
                self.current
                    .lock()
                    .unwrap()
                    .insert(endpoint_id, fingerprint);
            }
        }

        impl TargetRevalidationPort for FakeTargetRevalidation {
            fn current_target_fingerprint(
                &self,
                endpoint_id: EndpointId,
            ) -> Option<TargetFingerprint> {
                self.current.lock().unwrap().get(&endpoint_id).cloned()
            }
        }

        fn inventory_revision(endpoint_id: EndpointId) -> InventoryRevision {
            InventoryRevision {
                id: InventoryRevisionId(uuid::Uuid::new_v4()),
                endpoint_id,
                snapshot: InventorySnapshot(serde_json::Map::new()),
                recorded_at: now(),
            }
        }

        fn service(
            job_repo: FakeJobRepository,
            inventory_repo: FakeInventoryRepository,
            target: FakeTargetRevalidation,
        ) -> DestructiveIntentService<FakeJobRepository, FakeInventoryRepository> {
            DestructiveIntentService::new(
                Arc::new(job_repo),
                Arc::new(inventory_repo),
                Arc::new(target),
            )
        }

        #[tokio::test]
        async fn captures_current_inventory_revision_and_target_fingerprint() {
            let endpoint_id = EndpointId::new();
            let job = create_workflow(endpoint_id, 1).unwrap();
            let step_id = job.steps[0].id;
            let job_id = job.id;
            let revision = inventory_revision(endpoint_id);
            let revision_id = revision.id;
            let fingerprint = TargetFingerprint::new("disk-a");

            let job_repo = FakeJobRepository::with_job(job);
            let inventory_repo = FakeInventoryRepository::new();
            inventory_repo.set_current(endpoint_id, revision);
            let target = FakeTargetRevalidation::new();
            target.set_current(endpoint_id, fingerprint.clone());
            let svc = service(job_repo, inventory_repo, target);

            let intent = svc.authorize(job_id, step_id).await.unwrap();

            assert_eq!(intent.authorized_inventory_revision_id, revision_id);
            assert_eq!(intent.authorized_target_fingerprint, fingerprint);
        }

        #[tokio::test]
        async fn missing_current_inventory_blocks_authorization_without_persisting() {
            let endpoint_id = EndpointId::new();
            let job = create_workflow(endpoint_id, 1).unwrap();
            let step_id = job.steps[0].id;
            let job_id = job.id;

            let job_repo = FakeJobRepository::with_job(job);
            let inventory_repo = FakeInventoryRepository::new(); // no current revision set
            let target = FakeTargetRevalidation::new();
            target.set_current(endpoint_id, TargetFingerprint::new("disk-a"));
            let svc = service(job_repo, inventory_repo, target);

            let err = svc.authorize(job_id, step_id).await.unwrap_err();

            assert!(matches!(err, ApplicationError::NoCurrentInventory(id) if id == endpoint_id));
        }

        #[tokio::test]
        async fn missing_current_target_blocks_authorization_without_persisting() {
            let endpoint_id = EndpointId::new();
            let job = create_workflow(endpoint_id, 1).unwrap();
            let step_id = job.steps[0].id;
            let job_id = job.id;

            let job_repo = FakeJobRepository::with_job(job);
            let inventory_repo = FakeInventoryRepository::new();
            inventory_repo.set_current(endpoint_id, inventory_revision(endpoint_id));
            let target = FakeTargetRevalidation::new(); // no current target set
            let svc = service(job_repo, inventory_repo, target);

            let err = svc.authorize(job_id, step_id).await.unwrap_err();

            assert!(matches!(err, ApplicationError::NoCurrentTarget(id) if id == endpoint_id));
        }

        #[tokio::test]
        async fn wrong_job_step_correlation_is_rejected() {
            let endpoint_id = EndpointId::new();
            let job_a = create_workflow(endpoint_id, 1).unwrap();
            let job_b = create_workflow(endpoint_id, 1).unwrap();
            let job_a_id = job_a.id;
            let unrelated_step_id = job_b.steps[0].id;

            let job_repo = FakeJobRepository::with_job(job_a);
            let inventory_repo = FakeInventoryRepository::new();
            inventory_repo.set_current(endpoint_id, inventory_revision(endpoint_id));
            let target = FakeTargetRevalidation::new();
            target.set_current(endpoint_id, TargetFingerprint::new("disk-a"));
            let svc = service(job_repo, inventory_repo, target);

            let err = svc
                .authorize(job_a_id, unrelated_step_id)
                .await
                .unwrap_err();

            assert!(
                matches!(err, ApplicationError::JobStepNotFound(step_id, job_id) if step_id == unrelated_step_id && job_id == job_a_id)
            );
        }

        #[tokio::test]
        async fn ineligible_non_pending_step_is_rejected() {
            let endpoint_id = EndpointId::new();
            let mut job = create_workflow(endpoint_id, 1).unwrap();
            job.steps[0].state = bamep_domain::JobStepState::PreconditionsSatisfied;
            let job_id = job.id;
            let step_id = job.steps[0].id;

            let job_repo = FakeJobRepository::with_job(job);
            let inventory_repo = FakeInventoryRepository::new();
            inventory_repo.set_current(endpoint_id, inventory_revision(endpoint_id));
            let target = FakeTargetRevalidation::new();
            target.set_current(endpoint_id, TargetFingerprint::new("disk-a"));
            let svc = service(job_repo, inventory_repo, target);

            let err = svc.authorize(job_id, step_id).await.unwrap_err();

            assert!(matches!(err, ApplicationError::JobStepNotEligible(id) if id == step_id));
        }

        #[tokio::test]
        async fn an_already_authorized_step_cannot_be_silently_reauthorized() {
            let endpoint_id = EndpointId::new();
            let job = create_workflow(endpoint_id, 1).unwrap();
            let job_id = job.id;
            let step_id = job.steps[0].id;

            let job_repo = Arc::new(FakeJobRepository::with_job(job));
            let inventory_repo = Arc::new(FakeInventoryRepository::new());
            inventory_repo.set_current(endpoint_id, inventory_revision(endpoint_id));
            let target = Arc::new(FakeTargetRevalidation::new());
            target.set_current(endpoint_id, TargetFingerprint::new("disk-a"));
            let svc = DestructiveIntentService::new(
                Arc::clone(&job_repo),
                Arc::clone(&inventory_repo),
                Arc::clone(&target) as Arc<dyn TargetRevalidationPort>,
            );

            let first = svc.authorize(job_id, step_id).await.unwrap();

            // A later, independently-derived attempt must not silently
            // replace the original snapshot even though current evidence
            // could change between calls (here it does not, deliberately, to
            // isolate the "already authorized" rejection from evidence
            // drift, which the stale-inventory/target Postgres scenarios
            // cover separately).
            let err = svc.authorize(job_id, step_id).await.unwrap_err();

            assert!(matches!(err, ApplicationError::JobStepAlreadyAuthorized(id) if id == step_id));
            assert_eq!(
                job_repo.step(job_id, step_id).destructive_intent,
                Some(first)
            );
        }

        #[tokio::test]
        async fn persistence_failure_leaves_no_half_intent() {
            let endpoint_id = EndpointId::new();
            let job = create_workflow(endpoint_id, 1).unwrap();
            let job_id = job.id;
            let step_id = job.steps[0].id;

            let job_repo = FakeJobRepository::failing_persist(job);
            let inventory_repo = FakeInventoryRepository::new();
            inventory_repo.set_current(endpoint_id, inventory_revision(endpoint_id));
            let target = FakeTargetRevalidation::new();
            target.set_current(endpoint_id, TargetFingerprint::new("disk-a"));
            let job_repo = Arc::new(job_repo);
            let svc = DestructiveIntentService::new(
                Arc::clone(&job_repo),
                Arc::new(inventory_repo),
                Arc::new(target) as Arc<dyn TargetRevalidationPort>,
            );

            let err = svc.authorize(job_id, step_id).await.unwrap_err();

            assert!(matches!(err, ApplicationError::Repository(_)));
            assert_eq!(job_repo.step(job_id, step_id).destructive_intent, None);
        }
    }

    mod final_dispatch_service {
        use super::*;
        use crate::adapters::target_revalidation_fixture::FixtureTargetRevalidation;
        use crate::runtime::resource_arbiter::{ResourceClaim, ResourceKind};
        use bamep_domain::credential::{CredentialChain, CredentialHash};
        use bamep_domain::presented_credential::{CredentialKind, PresentedCredential};
        use bamep_domain::{
            create_workflow, Attempt, AttemptState, BootNonce, CurrentBoot, DestructiveIntent,
            EndpointAggregate, HardwareConfidence, IdentityState, InventoryRevisionId, JobStep,
            JobStepState, TargetFingerprint, TrustedBootstrapState,
        };
        use std::collections::HashMap;

        /// In-memory `JobRepository` fake mirroring
        /// `PostgresJobRepository::commit_destructive_dispatch`'s lock ->
        /// decide -> persist sequence closely enough to exercise
        /// `FinalDispatchService` end to end without PostgreSQL. The real
        /// atomicity/concurrency/reload behavior is covered separately by
        /// `crates/server/tests/final_dispatch_authorization.rs`.
        #[derive(Default)]
        struct FakeJobRepository {
            jobs: Mutex<HashMap<JobId, Job>>,
            endpoints: Mutex<HashMap<EndpointId, EndpointAggregate>>,
            current_inventory: Mutex<HashMap<EndpointId, InventoryRevisionId>>,
            attempts: Mutex<Vec<Attempt>>,
            audits: Mutex<Vec<AuditRecord>>,
            fail_persist: bool,
        }

        impl FakeJobRepository {
            fn new(job: Job, endpoint: EndpointAggregate) -> Self {
                let mut jobs = HashMap::new();
                let mut endpoints = HashMap::new();
                endpoints.insert(endpoint.id, endpoint);
                jobs.insert(job.id, job);
                Self {
                    jobs: Mutex::new(jobs),
                    endpoints: Mutex::new(endpoints),
                    current_inventory: Mutex::new(HashMap::new()),
                    attempts: Mutex::new(Vec::new()),
                    audits: Mutex::new(Vec::new()),
                    fail_persist: false,
                }
            }

            fn failing_persist(job: Job, endpoint: EndpointAggregate) -> Self {
                let mut fake = Self::new(job, endpoint);
                fake.fail_persist = true;
                fake
            }

            fn set_current_inventory(
                &self,
                endpoint_id: EndpointId,
                revision: InventoryRevisionId,
            ) {
                self.current_inventory
                    .lock()
                    .unwrap()
                    .insert(endpoint_id, revision);
            }

            fn step_state(&self, job_id: JobId, step_id: JobStepId) -> JobStepState {
                self.jobs.lock().unwrap()[&job_id]
                    .steps
                    .iter()
                    .find(|s| s.id == step_id)
                    .unwrap()
                    .state
            }

            fn attempt_count(&self) -> usize {
                self.attempts.lock().unwrap().len()
            }

            fn audit_count(&self) -> usize {
                self.audits.lock().unwrap().len()
            }
        }

        #[async_trait]
        impl JobRepository for FakeJobRepository {
            async fn create_workflow(&self, _job: &Job) -> Result<(), CreateWorkflowError> {
                unimplemented!("FinalDispatchService never creates a workflow")
            }

            async fn find_job(&self, id: JobId) -> Result<Option<Job>, RepositoryError> {
                Ok(self.jobs.lock().unwrap().get(&id).cloned())
            }

            async fn authorize_destructive_intent(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _decide: AuthorizeDestructiveIntentDecision,
            ) -> Result<DestructiveIntent, AuthorizeDestructiveIntentError> {
                unimplemented!("FinalDispatchService never authorizes destructive intent")
            }

            async fn admit_job(
                &self,
                _job_id: JobId,
                _decide: AdmitJobDecision,
            ) -> Result<Job, AdmitJobError> {
                unimplemented!("FinalDispatchService never admits a Job")
            }

            async fn satisfy_current_step_preconditions(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _decide: SatisfyStepPreconditionsDecision,
            ) -> Result<JobStep, SatisfyStepPreconditionsError> {
                unimplemented!("FinalDispatchService never advances a JobStep")
            }

            async fn commit_destructive_dispatch(
                &self,
                job_id: JobId,
                step_id: JobStepId,
                decide: FinalDispatchDecision,
            ) -> Result<FinalDispatchOutcome, CommitDestructiveDispatchError> {
                let Some(job) = self.jobs.lock().unwrap().get(&job_id).cloned() else {
                    return Err(CommitDestructiveDispatchError::JobNotFound(job_id));
                };
                let endpoint = self
                    .endpoints
                    .lock()
                    .unwrap()
                    .get(&job.endpoint_id)
                    .cloned()
                    .expect("test fixture endpoint must exist");
                let existing_active_attempt = self.attempts.lock().unwrap().iter().any(|a| {
                    a.job_step_id == step_id
                        && matches!(
                            a.state,
                            AttemptState::Dispatched
                                | AttemptState::InProgress
                                | AttemptState::AwaitingReconciliation
                        )
                });
                let current_inventory_revision_id = self
                    .current_inventory
                    .lock()
                    .unwrap()
                    .get(&job.endpoint_id)
                    .copied();

                let facts = FinalDispatchLockedFacts {
                    job,
                    endpoint,
                    existing_active_attempt,
                    current_inventory_revision_id,
                };

                match decide(facts) {
                    Ok(commit) => {
                        if self.fail_persist {
                            return Err(CommitDestructiveDispatchError::Repository(
                                RepositoryError::Backend("simulated persistence failure".into()),
                            ));
                        }
                        let mut jobs = self.jobs.lock().unwrap();
                        let job = jobs.get_mut(&job_id).unwrap();
                        if let Some(step) = job.steps.iter_mut().find(|s| s.id == step_id) {
                            step.state = commit.outcome.job_step.state;
                        }
                        drop(jobs);
                        self.attempts.lock().unwrap().push(commit.outcome.attempt);
                        self.audits.lock().unwrap().push(commit.audit);
                        Ok(commit.outcome)
                    }
                    Err(denial) => {
                        // Mirrors `PostgresJobRepository::commit_destructive_dispatch`:
                        // persist exactly `denial.pending_job_step`, never
                        // independently decide "revalidation failure means
                        // Pending".
                        if let Some(pending_step) = &denial.pending_job_step {
                            let mut jobs = self.jobs.lock().unwrap();
                            let job = jobs.get_mut(&job_id).unwrap();
                            if let Some(step) = job.steps.iter_mut().find(|s| s.id == step_id) {
                                step.state = pending_step.state;
                            }
                        }
                        Err(CommitDestructiveDispatchError::Rejected(denial.rejection))
                    }
                }
            }

            async fn commit_transfer_dispatch(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _transfer_id: bamep_domain::TransferId,
                _decide: crate::ports::TransferDispatchDecision,
            ) -> Result<
                bamep_domain::TransferDispatchOutcome,
                crate::ports::CommitTransferDispatchError,
            > {
                unimplemented!("FinalDispatchService tests never commit a transfer dispatch")
            }

            async fn find_attempt(
                &self,
                attempt_id: bamep_domain::AttemptId,
            ) -> Result<Option<Attempt>, RepositoryError> {
                Ok(self
                    .attempts
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|a| a.id == attempt_id)
                    .cloned())
            }

            async fn apply_action_evidence(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: crate::ports::ApplyActionEvidenceDecision,
            ) -> Result<
                crate::ports::ApplyActionEvidenceResult,
                crate::ports::ApplyActionEvidenceError,
            > {
                unimplemented!("FinalDispatchService tests never apply action evidence")
            }

            async fn action_targets_endpoint(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
            ) -> Result<bool, RepositoryError> {
                unimplemented!("FinalDispatchService tests never correlate ActionProgress")
            }

            async fn request_cancellation(
                &self,
                _job_id: JobId,
                _decide: crate::ports::RequestCancellationDecision,
            ) -> Result<
                crate::ports::RequestCancellationResult,
                crate::ports::RequestCancellationError,
            > {
                unimplemented!("FinalDispatchService tests never request cancellation")
            }

            async fn apply_cancel_ack(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: crate::ports::ApplyCancelAckDecision,
            ) -> Result<crate::ports::ApplyCancelAckResult, crate::ports::ApplyActionEvidenceError>
            {
                unimplemented!("FinalDispatchService tests never apply CancelAck evidence")
            }

            async fn mark_endpoint_active_attempt_uncertain(
                &self,
                _endpoint_id: EndpointId,
                _decide: crate::ports::MarkUncertainDecision,
            ) -> Result<Option<bamep_domain::AttemptId>, RepositoryError> {
                unimplemented!("FinalDispatchService tests never reconcile Attempts")
            }

            async fn reconcile_all_active_attempts_on_startup(
                &self,
                _decide: crate::ports::MarkUncertainDecision,
            ) -> Result<Vec<bamep_domain::AttemptId>, RepositoryError> {
                unimplemented!("FinalDispatchService tests never reconcile Attempts")
            }

            async fn find_reconciliation_candidate(
                &self,
                _endpoint_id: EndpointId,
            ) -> Result<Option<bamep_domain::ActionId>, RepositoryError> {
                unimplemented!("FinalDispatchService tests never reconcile Attempts")
            }

            async fn apply_status_report(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: crate::ports::ApplyReconciliationDecision,
            ) -> Result<
                crate::ports::ApplyReconciliationResult,
                crate::ports::ApplyActionEvidenceError,
            > {
                unimplemented!("FinalDispatchService tests never apply StatusReport evidence")
            }

            async fn close_indeterminate(
                &self,
                _job_id: JobId,
                _decide: crate::ports::CloseIndeterminateDecision,
            ) -> Result<crate::ports::CloseIndeterminateResult, crate::ports::CloseIndeterminateError>
            {
                unimplemented!("FinalDispatchService tests never close Indeterminate")
            }
        }

        fn intent(
            revision_id: InventoryRevisionId,
            fingerprint: TargetFingerprint,
        ) -> DestructiveIntent {
            DestructiveIntent {
                authorized_inventory_revision_id: revision_id,
                authorized_target_fingerprint: fingerprint,
            }
        }

        /// Builds a `Running` Job with one destructive JobStep at
        /// `PreconditionsSatisfied`, carrying `intent`, targeting
        /// `endpoint_id`.
        fn preconditions_satisfied_job(
            endpoint_id: EndpointId,
            intent: DestructiveIntent,
        ) -> (Job, JobStepId) {
            let mut job = create_workflow(endpoint_id, 1).unwrap();
            job.steps[0].destructive_intent = Some(intent);
            let running = bamep_domain::admit_job(&job, Utc::now()).unwrap().job;
            let step_id = running.steps[0].id;
            let advanced =
                bamep_domain::satisfy_preliminary_preconditions(&running, step_id).unwrap();
            let mut running = running;
            running.steps[0] = advanced;
            (running, step_id)
        }

        /// Builds a minimal `EndpointAggregate` with every safety dimension
        /// independently controllable. `credential_active` selects whether
        /// the credential chain's shared expiry sits in the future (`Active`)
        /// or the past (`Expired`) relative to `now` — never `Revoked`, kept
        /// out of scope for these tests.
        fn endpoint_with(
            identity_enrolled: bool,
            credential_active: bool,
            hardware_confidence: HardwareConfidence,
            trusted_bootstrap_established: bool,
            now: DateTime<Utc>,
        ) -> EndpointAggregate {
            let e1 = PresentedCredential::generate(CredentialKind::Enrollment);
            let verifier = CredentialHash::of_bytes(e1.secret().expose_secret_bytes());
            let r1 = PresentedCredential::generate(CredentialKind::Runtime);
            let ttl = if credential_active {
                Duration::hours(1)
            } else {
                Duration::hours(-1)
            };
            let chain = CredentialChain::establish(e1.lookup_id().clone(), verifier, &r1, now, ttl)
                .unwrap();
            let boot_nonce = BootNonce::generate().expect("OS CSPRNG must be available in tests");
            let current_boot = Some(CurrentBoot::new(
                e1.lookup_id().clone(),
                boot_nonce,
                if trusted_bootstrap_established {
                    TrustedBootstrapState::Established
                } else {
                    TrustedBootstrapState::NotEstablished
                },
            ));

            EndpointAggregate {
                id: EndpointId::new(),
                inventory_signal: format!("sim-final-dispatch-{}", Uuid::new_v4()),
                identity: if identity_enrolled {
                    IdentityState::Enrolled
                } else {
                    IdentityState::PendingEnrollment
                },
                credential: chain,
                hardware_confidence,
                current_boot,
                created_at: now,
                updated_at: now,
            }
        }

        /// Every safety dimension passing — the baseline every negative test
        /// starts from and flips exactly one field away from.
        fn all_pass_fixture() -> (
            FakeJobRepository,
            Arc<PresenceRegistry>,
            Arc<FixtureTargetRevalidation>,
            JobId,
            JobStepId,
        ) {
            let now = Utc::now();
            let endpoint = endpoint_with(true, true, HardwareConfidence::Consistent, true, now);
            let endpoint_id = endpoint.id;
            let revision_id = InventoryRevisionId(Uuid::new_v4());
            let fingerprint = TargetFingerprint::new("disk-a");
            let (job, step_id) =
                preconditions_satisfied_job(endpoint_id, intent(revision_id, fingerprint.clone()));
            let job_id = job.id;

            let repo = FakeJobRepository::new(job, endpoint);
            repo.set_current_inventory(endpoint_id, revision_id);

            let presence = Arc::new(PresenceRegistry::new());
            presence.register(endpoint_id, bamep_agent_protocol::ProtocolId::generate());

            let target = Arc::new(FixtureTargetRevalidation::new());
            target.set_current_target(endpoint_id, fingerprint);

            (repo, presence, target, job_id, step_id)
        }

        fn claims() -> Vec<ResourceClaim> {
            vec![ResourceClaim::new(ResourceKind::new("network"), 1)]
        }

        fn service(
            repo: FakeJobRepository,
            presence: Arc<PresenceRegistry>,
            target: Arc<FixtureTargetRevalidation>,
            arbiter: Arc<TechnicalResourceArbiter>,
        ) -> FinalDispatchService<FakeJobRepository> {
            FinalDispatchService::new(
                Arc::new(repo),
                presence,
                target as Arc<dyn TargetRevalidationPort>,
                arbiter,
            )
        }

        #[tokio::test]
        async fn all_pass_commits_dispatching_step_and_one_dispatched_attempt() {
            let (repo, presence, target, job_id, step_id) = all_pass_fixture();
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                10,
            )]));
            let svc = service(repo, presence, target, Arc::clone(&arbiter));

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();

            match result {
                FinalDispatchResult::Committed { outcome, .. } => {
                    assert_eq!(outcome.job_step.state, JobStepState::Dispatching);
                    assert_eq!(outcome.attempt.state, AttemptState::Dispatched);
                }
                other => panic!("expected Committed, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn credential_active_without_presence_blocks_dispatch() {
            let (repo, _presence, target, job_id, step_id) = all_pass_fixture();
            // A fresh, empty PresenceRegistry: CredentialActive holds (the
            // fixture endpoint's chain is still valid) but no authenticated
            // session is registered.
            let empty_presence = Arc::new(PresenceRegistry::new());
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                10,
            )]));
            let svc = service(repo, empty_presence, target, Arc::clone(&arbiter));

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                FinalDispatchResult::Rejected(FinalDispatchRejection::AgentNotPresent)
            ));
        }

        #[tokio::test]
        async fn presence_with_inactive_credential_blocks_dispatch() {
            let now = Utc::now();
            let endpoint = endpoint_with(true, false, HardwareConfidence::Consistent, true, now);
            let endpoint_id = endpoint.id;
            let revision_id = InventoryRevisionId(Uuid::new_v4());
            let fingerprint = TargetFingerprint::new("disk-a");
            let (job, step_id) =
                preconditions_satisfied_job(endpoint_id, intent(revision_id, fingerprint.clone()));
            let job_id = job.id;
            let repo = FakeJobRepository::new(job, endpoint);
            repo.set_current_inventory(endpoint_id, revision_id);

            let presence = Arc::new(PresenceRegistry::new());
            presence.register(endpoint_id, bamep_agent_protocol::ProtocolId::generate());
            let target = Arc::new(FixtureTargetRevalidation::new());
            target.set_current_target(endpoint_id, fingerprint);
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                10,
            )]));
            let svc = service(repo, presence, target, Arc::clone(&arbiter));

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                FinalDispatchResult::Rejected(FinalDispatchRejection::CredentialNotActive)
            ));
        }

        #[tokio::test]
        async fn current_inventory_equality_cannot_compensate_for_target_mismatch() {
            let (repo, presence, target, job_id, step_id) = all_pass_fixture();
            let endpoint_id = *repo.endpoints.lock().unwrap().keys().next().unwrap();
            // Inventory still matches; independently break the target.
            target.set_current_target(endpoint_id, TargetFingerprint::new("disk-mismatch"));
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                10,
            )]));
            let svc = service(repo, presence, target, Arc::clone(&arbiter));

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                FinalDispatchResult::Rejected(FinalDispatchRejection::TargetMismatch)
            ));
        }

        #[tokio::test]
        async fn matching_target_cannot_compensate_for_stale_inventory() {
            let (repo, presence, target, job_id, step_id) = all_pass_fixture();
            let endpoint_id = *repo.endpoints.lock().unwrap().keys().next().unwrap();
            // Target still matches; independently make inventory stale.
            repo.set_current_inventory(endpoint_id, InventoryRevisionId(Uuid::new_v4()));
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                10,
            )]));
            let svc = service(repo, presence, target, Arc::clone(&arbiter));

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                FinalDispatchResult::Rejected(FinalDispatchRejection::StaleInventory)
            ));
        }

        #[tokio::test]
        async fn consistent_hardware_confidence_is_required_independently() {
            let now = Utc::now();
            let endpoint =
                endpoint_with(true, true, HardwareConfidence::LoweredConfidence, true, now);
            let endpoint_id = endpoint.id;
            let revision_id = InventoryRevisionId(Uuid::new_v4());
            let fingerprint = TargetFingerprint::new("disk-a");
            let (job, step_id) =
                preconditions_satisfied_job(endpoint_id, intent(revision_id, fingerprint.clone()));
            let job_id = job.id;
            let repo = FakeJobRepository::new(job, endpoint);
            repo.set_current_inventory(endpoint_id, revision_id);
            let presence = Arc::new(PresenceRegistry::new());
            presence.register(endpoint_id, bamep_agent_protocol::ProtocolId::generate());
            let target = Arc::new(FixtureTargetRevalidation::new());
            target.set_current_target(endpoint_id, fingerprint);
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                10,
            )]));
            let svc = service(repo, presence, target, Arc::clone(&arbiter));

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                FinalDispatchResult::Rejected(
                    FinalDispatchRejection::HardwareConfidenceNotConsistent
                )
            ));
        }

        #[tokio::test]
        async fn trusted_bootstrap_is_required_independently() {
            let now = Utc::now();
            let endpoint = endpoint_with(true, true, HardwareConfidence::Consistent, false, now);
            let endpoint_id = endpoint.id;
            let revision_id = InventoryRevisionId(Uuid::new_v4());
            let fingerprint = TargetFingerprint::new("disk-a");
            let (job, step_id) =
                preconditions_satisfied_job(endpoint_id, intent(revision_id, fingerprint.clone()));
            let job_id = job.id;
            let repo = FakeJobRepository::new(job, endpoint);
            repo.set_current_inventory(endpoint_id, revision_id);
            let presence = Arc::new(PresenceRegistry::new());
            presence.register(endpoint_id, bamep_agent_protocol::ProtocolId::generate());
            let target = Arc::new(FixtureTargetRevalidation::new());
            target.set_current_target(endpoint_id, fingerprint);
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                10,
            )]));
            let svc = service(repo, presence, target, Arc::clone(&arbiter));

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                FinalDispatchResult::Rejected(
                    FinalDispatchRejection::TrustedBootstrapNotEstablished
                )
            ));
        }

        #[tokio::test]
        async fn resource_unavailable_leaves_step_preconditions_satisfied_without_persisting() {
            let (repo, presence, target, job_id, step_id) = all_pass_fixture();
            // Zero capacity: the arbiter must reject before final revalidation
            // ever begins.
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                0,
            )]));
            let repo = Arc::new(repo);
            let svc = FinalDispatchService::new(
                Arc::clone(&repo),
                presence,
                target as Arc<dyn TargetRevalidationPort>,
                Arc::clone(&arbiter),
            );

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();

            assert!(matches!(result, FinalDispatchResult::ResourceUnavailable));
            assert_eq!(
                repo.step_state(job_id, step_id),
                JobStepState::PreconditionsSatisfied
            );
        }

        #[tokio::test]
        async fn gate_failure_after_resource_acquisition_releases_reservation_and_becomes_pending()
        {
            let (repo, presence, target, job_id, step_id) = all_pass_fixture();
            let endpoint_id = *repo.endpoints.lock().unwrap().keys().next().unwrap();
            target.set_current_target(endpoint_id, TargetFingerprint::new("disk-mismatch"));
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                1,
            )]));
            let repo = Arc::new(repo);
            let svc = FinalDispatchService::new(
                Arc::clone(&repo),
                presence,
                target as Arc<dyn TargetRevalidationPort>,
                Arc::clone(&arbiter),
            );

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();
            assert!(matches!(
                result,
                FinalDispatchResult::Rejected(FinalDispatchRejection::TargetMismatch)
            ));
            assert_eq!(repo.step_state(job_id, step_id), JobStepState::Pending);

            // The reservation must have been released: full capacity (1 unit)
            // must be acquirable again.
            assert!(arbiter.acquire(claims()).is_ok());
        }

        #[tokio::test]
        async fn persistence_failure_releases_reservation_and_creates_nothing() {
            let now = Utc::now();
            let endpoint = endpoint_with(true, true, HardwareConfidence::Consistent, true, now);
            let endpoint_id = endpoint.id;
            let revision_id = InventoryRevisionId(Uuid::new_v4());
            let fingerprint = TargetFingerprint::new("disk-a");
            let (job, step_id) =
                preconditions_satisfied_job(endpoint_id, intent(revision_id, fingerprint.clone()));
            let job_id = job.id;
            let repo = FakeJobRepository::failing_persist(job, endpoint);
            repo.set_current_inventory(endpoint_id, revision_id);
            let presence = Arc::new(PresenceRegistry::new());
            presence.register(endpoint_id, bamep_agent_protocol::ProtocolId::generate());
            let target = Arc::new(FixtureTargetRevalidation::new());
            target.set_current_target(endpoint_id, fingerprint);
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                1,
            )]));
            let repo = Arc::new(repo);
            let svc = FinalDispatchService::new(
                Arc::clone(&repo),
                presence,
                target as Arc<dyn TargetRevalidationPort>,
                Arc::clone(&arbiter),
            );

            let err = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap_err();

            assert!(matches!(err, ApplicationError::Repository(_)));
            assert_eq!(repo.attempt_count(), 0);
            assert_eq!(repo.audit_count(), 0);
            // Reservation released: full capacity acquirable again.
            assert!(arbiter.acquire(claims()).is_ok());
        }

        #[tokio::test]
        async fn success_keeps_the_reservation_held() {
            let (repo, presence, target, job_id, step_id) = all_pass_fixture();
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                1,
            )]));
            let svc = service(repo, presence, target, Arc::clone(&arbiter));

            let result = svc
                .commit_destructive_dispatch(job_id, step_id, claims())
                .await
                .unwrap();
            assert!(matches!(result, FinalDispatchResult::Committed { .. }));

            // Full capacity (1 unit) is already held by the successful
            // commitment: a second claim must fail until it is explicitly
            // released.
            assert_eq!(
                arbiter.acquire(claims()),
                Err(crate::runtime::resource_arbiter::InsufficientCapacity)
            );
            if let FinalDispatchResult::Committed { reservation, .. } = result {
                arbiter.release(reservation);
            }
            assert!(arbiter.acquire(claims()).is_ok());
        }
    }

    mod transfer_dispatch_service {
        use super::*;
        use crate::runtime::resource_arbiter::{ResourceClaim, ResourceKind};
        use bamep_domain::{
            create_transfer_context, create_workflow, Attempt, ChunkSize, DigestAlgorithm,
            JobState, JobStep, JobStepState, SourceProvenance, Transfer, TransferDirection,
            TransferId,
        };
        use std::collections::HashMap;

        /// In-memory `JobRepository` fake mirroring
        /// `PostgresJobRepository::commit_transfer_dispatch`'s lock -> decide
        /// -> persist sequence closely enough to exercise
        /// `TransferDispatchService` end to end without PostgreSQL. Unlike
        /// `final_dispatch_service::FakeJobRepository`, this fake carries no
        /// `EndpointAggregate`/inventory/hardware-confidence/trusted-
        /// bootstrap state at all — proving structurally that this path
        /// never reads any of it. The real atomicity/concurrency/reload
        /// behavior is covered separately by
        /// `crates/server/tests/transfer_dispatch_commit.rs`.
        #[derive(Default)]
        struct FakeJobRepository {
            jobs: Mutex<HashMap<JobId, Job>>,
            transfers: Mutex<HashMap<TransferId, Transfer>>,
            attempts: Mutex<Vec<Attempt>>,
            fail_persist: bool,
        }

        impl FakeJobRepository {
            fn new(job: Job, transfer: Transfer) -> Self {
                let mut jobs = HashMap::new();
                let mut transfers = HashMap::new();
                jobs.insert(job.id, job);
                transfers.insert(transfer.id, transfer);
                Self {
                    jobs: Mutex::new(jobs),
                    transfers: Mutex::new(transfers),
                    attempts: Mutex::new(Vec::new()),
                    fail_persist: false,
                }
            }

            fn failing_persist(job: Job, transfer: Transfer) -> Self {
                let mut fake = Self::new(job, transfer);
                fake.fail_persist = true;
                fake
            }

            fn step_state(&self, job_id: JobId, step_id: JobStepId) -> JobStepState {
                self.jobs.lock().unwrap()[&job_id]
                    .steps
                    .iter()
                    .find(|s| s.id == step_id)
                    .unwrap()
                    .state
            }

            fn attempt_count(&self) -> usize {
                self.attempts.lock().unwrap().len()
            }

            fn transfer_attempt_id(
                &self,
                transfer_id: TransferId,
            ) -> Option<bamep_domain::AttemptId> {
                self.transfers.lock().unwrap()[&transfer_id].attempt_id
            }
        }

        #[async_trait]
        impl JobRepository for FakeJobRepository {
            async fn create_workflow(&self, _job: &Job) -> Result<(), CreateWorkflowError> {
                unimplemented!("TransferDispatchService never creates a workflow")
            }

            async fn find_job(&self, id: JobId) -> Result<Option<Job>, RepositoryError> {
                Ok(self.jobs.lock().unwrap().get(&id).cloned())
            }

            async fn authorize_destructive_intent(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _decide: AuthorizeDestructiveIntentDecision,
            ) -> Result<DestructiveIntent, AuthorizeDestructiveIntentError> {
                unimplemented!("TransferDispatchService never authorizes destructive intent")
            }

            async fn admit_job(
                &self,
                _job_id: JobId,
                _decide: AdmitJobDecision,
            ) -> Result<Job, AdmitJobError> {
                unimplemented!("TransferDispatchService never admits a Job")
            }

            async fn satisfy_current_step_preconditions(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _decide: SatisfyStepPreconditionsDecision,
            ) -> Result<JobStep, SatisfyStepPreconditionsError> {
                unimplemented!("TransferDispatchService never advances a JobStep")
            }

            async fn commit_destructive_dispatch(
                &self,
                _job_id: JobId,
                _step_id: JobStepId,
                _decide: FinalDispatchDecision,
            ) -> Result<FinalDispatchOutcome, CommitDestructiveDispatchError> {
                unimplemented!("TransferDispatchService never commits a destructive dispatch")
            }

            async fn commit_transfer_dispatch(
                &self,
                job_id: JobId,
                step_id: JobStepId,
                transfer_id: TransferId,
                decide: TransferDispatchDecision,
            ) -> Result<bamep_domain::TransferDispatchOutcome, CommitTransferDispatchError>
            {
                let Some(job) = self.jobs.lock().unwrap().get(&job_id).cloned() else {
                    return Err(CommitTransferDispatchError::JobNotFound(job_id));
                };
                let Some(transfer) = self.transfers.lock().unwrap().get(&transfer_id).cloned()
                else {
                    return Err(CommitTransferDispatchError::TransferNotFound(transfer_id));
                };
                let existing_active_attempt = self.attempts.lock().unwrap().iter().any(|a| {
                    a.job_step_id == step_id
                        && matches!(
                            a.state,
                            AttemptState::Dispatched
                                | AttemptState::InProgress
                                | AttemptState::AwaitingReconciliation
                        )
                });

                let facts = TransferDispatchLockedFacts {
                    job,
                    existing_active_attempt,
                    transfer,
                };

                match decide(facts) {
                    Ok(outcome) => {
                        if self.fail_persist {
                            return Err(CommitTransferDispatchError::Repository(
                                RepositoryError::Backend("simulated persistence failure".into()),
                            ));
                        }
                        let mut jobs = self.jobs.lock().unwrap();
                        let job = jobs.get_mut(&job_id).unwrap();
                        if let Some(step) = job.steps.iter_mut().find(|s| s.id == step_id) {
                            step.state = outcome.job_step.state;
                        }
                        drop(jobs);
                        self.attempts.lock().unwrap().push(outcome.attempt);
                        self.transfers
                            .lock()
                            .unwrap()
                            .insert(transfer_id, outcome.transfer.clone());
                        Ok(outcome)
                    }
                    Err(denial) => {
                        // Mirrors `PostgresJobRepository::commit_transfer_dispatch`:
                        // persist exactly `denial.pending_job_step`, never
                        // independently decide "revalidation failure means
                        // Pending". `transfers` is never touched here.
                        if let Some(pending_step) = &denial.pending_job_step {
                            let mut jobs = self.jobs.lock().unwrap();
                            let job = jobs.get_mut(&job_id).unwrap();
                            if let Some(step) = job.steps.iter_mut().find(|s| s.id == step_id) {
                                step.state = pending_step.state;
                            }
                        }
                        Err(CommitTransferDispatchError::Rejected(denial.rejection))
                    }
                }
            }

            async fn find_attempt(
                &self,
                attempt_id: bamep_domain::AttemptId,
            ) -> Result<Option<Attempt>, RepositoryError> {
                Ok(self
                    .attempts
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|a| a.id == attempt_id)
                    .cloned())
            }

            async fn apply_action_evidence(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: ApplyActionEvidenceDecision,
            ) -> Result<ApplyActionEvidenceResult, ApplyActionEvidenceError> {
                unimplemented!("TransferDispatchService never applies action evidence")
            }

            async fn action_targets_endpoint(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
            ) -> Result<bool, RepositoryError> {
                unimplemented!("TransferDispatchService never correlates ActionProgress")
            }

            async fn request_cancellation(
                &self,
                _job_id: JobId,
                _decide: RequestCancellationDecision,
            ) -> Result<RequestCancellationResult, RequestCancellationError> {
                unimplemented!("TransferDispatchService never requests cancellation")
            }

            async fn apply_cancel_ack(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: ApplyCancelAckDecision,
            ) -> Result<crate::ports::ApplyCancelAckResult, ApplyActionEvidenceError> {
                unimplemented!("TransferDispatchService never applies a CancelAck")
            }

            async fn mark_endpoint_active_attempt_uncertain(
                &self,
                _endpoint_id: EndpointId,
                _decide: crate::ports::MarkUncertainDecision,
            ) -> Result<Option<bamep_domain::AttemptId>, RepositoryError> {
                unimplemented!("TransferDispatchService never marks an Attempt uncertain")
            }

            async fn reconcile_all_active_attempts_on_startup(
                &self,
                _decide: crate::ports::MarkUncertainDecision,
            ) -> Result<Vec<bamep_domain::AttemptId>, RepositoryError> {
                unimplemented!("TransferDispatchService never reconciles on startup")
            }

            async fn find_reconciliation_candidate(
                &self,
                _endpoint_id: EndpointId,
            ) -> Result<Option<bamep_domain::ActionId>, RepositoryError> {
                unimplemented!("TransferDispatchService never finds a reconciliation candidate")
            }

            async fn apply_status_report(
                &self,
                _action_id: bamep_domain::ActionId,
                _authenticated_endpoint_id: EndpointId,
                _decide: crate::ports::ApplyReconciliationDecision,
            ) -> Result<crate::ports::ApplyReconciliationResult, ApplyActionEvidenceError>
            {
                unimplemented!("TransferDispatchService never applies a StatusReport")
            }

            async fn close_indeterminate(
                &self,
                _job_id: JobId,
                _decide: crate::ports::CloseIndeterminateDecision,
            ) -> Result<crate::ports::CloseIndeterminateResult, crate::ports::CloseIndeterminateError>
            {
                unimplemented!("TransferDispatchService never closes Indeterminate")
            }
        }

        fn claims() -> Vec<ResourceClaim> {
            vec![ResourceClaim::new(ResourceKind::new("network"), 1)]
        }

        /// A `Running` Job with its single JobStep at `PreconditionsSatisfied`
        /// (non-destructive: no `DestructiveIntent` ever attached), plus a
        /// fresh unbound pre-dispatch `Transfer` correlated to it — the
        /// minimum eligible fixture `evaluate_transfer_dispatch` accepts.
        fn preconditions_satisfied_fixture() -> (Job, JobStepId, Transfer) {
            let endpoint_id = EndpointId::new();
            let job = create_workflow(endpoint_id, 1).unwrap();
            let running = bamep_domain::admit_job(&job, Utc::now()).unwrap().job;
            let step_id = running.steps[0].id;
            let advanced =
                bamep_domain::satisfy_preliminary_preconditions(&running, step_id).unwrap();
            let mut job = running;
            job.steps[0] = advanced;
            let transfer = create_transfer_context(
                endpoint_id,
                job.id,
                step_id,
                TransferDirection::AgentToServer,
                DigestAlgorithm::Sha256,
                ChunkSize::new(4096).unwrap(),
                SourceProvenance::new("disk-0"),
            )
            .transfer;
            (job, step_id, transfer)
        }

        #[tokio::test]
        async fn eligible_transfer_dispatch_commits_and_binds_the_exact_transfer() {
            let (job, step_id, transfer) = preconditions_satisfied_fixture();
            let job_id = job.id;
            let transfer_id = transfer.id;
            let artifact_id = transfer.artifact_id;
            let repo = Arc::new(FakeJobRepository::new(job, transfer));
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                10,
            )]));
            let svc = TransferDispatchService::new(Arc::clone(&repo), Arc::clone(&arbiter));

            let result = svc
                .commit_transfer_dispatch(job_id, step_id, transfer_id, claims())
                .await
                .unwrap();

            let TransferDispatchResult::Committed { outcome, .. } = result else {
                panic!("expected a successful commitment, got {result:?}");
            };
            assert_eq!(outcome.job_step.state, JobStepState::Dispatching);
            assert_eq!(outcome.attempt.state, AttemptState::Dispatched);
            assert_eq!(
                outcome.transfer.id, transfer_id,
                "TransferId must never be regenerated"
            );
            assert_eq!(
                outcome.transfer.artifact_id, artifact_id,
                "ArtifactId must never be regenerated"
            );
            assert_eq!(outcome.transfer.attempt_id, Some(outcome.attempt.id));
            assert_eq!(repo.step_state(job_id, step_id), JobStepState::Dispatching);
            assert_eq!(repo.attempt_count(), 1);
            assert_eq!(
                repo.transfer_attempt_id(transfer_id),
                Some(outcome.attempt.id)
            );
        }

        #[tokio::test]
        async fn resource_unavailable_leaves_step_preconditions_satisfied_without_persisting() {
            let (job, step_id, transfer) = preconditions_satisfied_fixture();
            let job_id = job.id;
            let transfer_id = transfer.id;
            let repo = Arc::new(FakeJobRepository::new(job, transfer));
            // Zero capacity: the arbiter must reject before final
            // revalidation ever begins.
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                0,
            )]));
            let svc = TransferDispatchService::new(Arc::clone(&repo), Arc::clone(&arbiter));

            let result = svc
                .commit_transfer_dispatch(job_id, step_id, transfer_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                TransferDispatchResult::ResourceUnavailable
            ));
            assert_eq!(
                repo.step_state(job_id, step_id),
                JobStepState::PreconditionsSatisfied
            );
            assert_eq!(repo.attempt_count(), 0);
            assert_eq!(repo.transfer_attempt_id(transfer_id), None);
        }

        #[tokio::test]
        async fn revalidation_failure_releases_reservation_and_returns_step_to_pending() {
            let (mut job, step_id, transfer) = preconditions_satisfied_fixture();
            job.state = JobState::Cancelling;
            let job_id = job.id;
            let transfer_id = transfer.id;
            let repo = Arc::new(FakeJobRepository::new(job, transfer));
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                1,
            )]));
            let svc = TransferDispatchService::new(Arc::clone(&repo), Arc::clone(&arbiter));

            let result = svc
                .commit_transfer_dispatch(job_id, step_id, transfer_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                TransferDispatchResult::Rejected(TransferDispatchRejection::JobNotRunning)
            ));
            assert_eq!(repo.step_state(job_id, step_id), JobStepState::Pending);
            assert_eq!(repo.attempt_count(), 0);
            assert_eq!(repo.transfer_attempt_id(transfer_id), None);
            // The reservation must have been released: full capacity (1
            // unit) must be acquirable again.
            assert!(arbiter.acquire(claims()).is_ok());
        }

        #[tokio::test]
        async fn persistence_failure_releases_reservation_and_creates_nothing() {
            let (job, step_id, transfer) = preconditions_satisfied_fixture();
            let job_id = job.id;
            let transfer_id = transfer.id;
            let repo = Arc::new(FakeJobRepository::failing_persist(job, transfer));
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                1,
            )]));
            let svc = TransferDispatchService::new(Arc::clone(&repo), Arc::clone(&arbiter));

            let err = svc
                .commit_transfer_dispatch(job_id, step_id, transfer_id, claims())
                .await
                .unwrap_err();

            assert!(matches!(err, ApplicationError::Repository(_)));
            assert_eq!(repo.attempt_count(), 0);
            assert!(arbiter.acquire(claims()).is_ok());
        }

        #[tokio::test]
        async fn success_keeps_the_reservation_held_for_number_26() {
            let (job, step_id, transfer) = preconditions_satisfied_fixture();
            let job_id = job.id;
            let transfer_id = transfer.id;
            let repo = Arc::new(FakeJobRepository::new(job, transfer));
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                1,
            )]));
            let svc = TransferDispatchService::new(Arc::clone(&repo), Arc::clone(&arbiter));

            let result = svc
                .commit_transfer_dispatch(job_id, step_id, transfer_id, claims())
                .await
                .unwrap();
            assert!(matches!(result, TransferDispatchResult::Committed { .. }));

            // Full capacity (1 unit) is already held by the successful
            // commitment: a second claim must fail until it is explicitly
            // released — this is the exact reservation #26 later consumes.
            assert_eq!(
                arbiter.acquire(claims()),
                Err(crate::runtime::resource_arbiter::InsufficientCapacity)
            );
            if let TransferDispatchResult::Committed { reservation, .. } = result {
                arbiter.release(reservation);
            }
            assert!(arbiter.acquire(claims()).is_ok());
        }

        #[tokio::test]
        async fn an_already_bound_transfer_is_rejected_without_a_second_attempt() {
            let (job, step_id, transfer) = preconditions_satisfied_fixture();
            let job_id = job.id;
            let transfer_id = transfer.id;
            let other_attempt = Attempt {
                id: bamep_domain::AttemptId::new(),
                job_step_id: step_id,
                action_id: bamep_domain::ActionId::new(),
                state: AttemptState::Dispatched,
            };
            let bound_transfer = bamep_domain::bind_attempt(&transfer, &other_attempt).unwrap();
            let repo = Arc::new(FakeJobRepository::new(job, bound_transfer));
            let arbiter = Arc::new(TechnicalResourceArbiter::new([(
                ResourceKind::new("network"),
                1,
            )]));
            let svc = TransferDispatchService::new(Arc::clone(&repo), Arc::clone(&arbiter));

            let result = svc
                .commit_transfer_dispatch(job_id, step_id, transfer_id, claims())
                .await
                .unwrap();

            assert!(matches!(
                result,
                TransferDispatchResult::Rejected(TransferDispatchRejection::TransferAlreadyBound)
            ));
            assert_eq!(repo.attempt_count(), 0);
            assert_eq!(
                repo.transfer_attempt_id(transfer_id),
                Some(other_attempt.id),
                "the original binding must remain exactly as it was"
            );
            assert!(arbiter.acquire(claims()).is_ok());
        }
    }

    mod action_dispatch_service {
        use super::*;
        use crate::ports::AgentDispatchError;
        use crate::runtime::resource_arbiter::ResourceKind;
        use bamep_domain::{ActionId, AttemptId, JobStepId};
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// In-memory `AgentDispatchPort` fake that counts calls and can be
        /// configured to fail exactly once — enough to prove
        /// `ActionDispatchService`'s registration guard without any real
        /// transport (`crates/server/tests/action_dispatch_wss.rs` covers the
        /// real-WSS path).
        #[derive(Default)]
        struct FakeDispatchPort {
            calls: AtomicUsize,
            fail_next: Mutex<bool>,
            last_dispatch: Mutex<Option<ActionDispatchMessage>>,
        }

        impl FakeDispatchPort {
            fn new() -> Self {
                Self::default()
            }

            fn failing_once() -> Self {
                Self {
                    calls: AtomicUsize::new(0),
                    fail_next: Mutex::new(true),
                    last_dispatch: Mutex::new(None),
                }
            }

            fn call_count(&self) -> usize {
                self.calls.load(Ordering::SeqCst)
            }

            /// The most recently transmitted `ActionDispatch` body — used to
            /// assert the exact `action_type`/`action_version`/`parameters`
            /// #40 constructs from durable Transfer state.
            fn last_dispatch(&self) -> Option<ActionDispatchMessage> {
                self.last_dispatch.lock().unwrap().clone()
            }
        }

        #[async_trait]
        impl AgentDispatchPort for FakeDispatchPort {
            async fn dispatch_action(
                &self,
                _endpoint_id: EndpointId,
                dispatch: ActionDispatchMessage,
            ) -> Result<(), AgentDispatchError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                *self.last_dispatch.lock().unwrap() = Some(dispatch);
                let mut fail = self.fail_next.lock().unwrap();
                if *fail {
                    *fail = false;
                    return Err(AgentDispatchError::SendFailed);
                }
                Ok(())
            }

            async fn cancel_action(
                &self,
                _endpoint_id: EndpointId,
                _cancel: bamep_agent_protocol::CancelActionMessage,
            ) -> Result<(), AgentDispatchError> {
                unimplemented!("ActionDispatchService tests never send CancelAction")
            }

            async fn status_query(
                &self,
                _endpoint_id: EndpointId,
                _query: bamep_agent_protocol::StatusQueryMessage,
            ) -> Result<(), AgentDispatchError> {
                unimplemented!("ActionDispatchService tests never send StatusQuery")
            }
        }

        fn dispatched_attempt() -> Attempt {
            Attempt {
                id: AttemptId::new(),
                job_step_id: JobStepId::new(),
                action_id: ActionId::new(),
                state: AttemptState::Dispatched,
            }
        }

        fn arbiter() -> TechnicalResourceArbiter {
            TechnicalResourceArbiter::new([(ResourceKind::new("network"), 10)])
        }

        fn reservation(arbiter: &TechnicalResourceArbiter) -> ReservationId {
            arbiter
                .acquire(vec![ResourceClaim::new(ResourceKind::new("network"), 1)])
                .unwrap()
        }

        #[tokio::test]
        async fn first_dispatch_sends_and_registers_the_reservation() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::new());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let res = reservation(&arbiter);
            let attempt = dispatched_attempt();

            let outcome = svc.dispatch(EndpointId::new(), attempt, res).await;

            assert_eq!(outcome, ActionDispatchOutcome::Sent);
            assert_eq!(transport.call_count(), 1);
            assert_eq!(reservations.take(attempt.id), Some(res));
        }

        #[tokio::test]
        async fn repeated_call_for_the_same_attempt_sends_nothing() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::new());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let first_reservation = reservation(&arbiter);
            let attempt = dispatched_attempt();

            let first = svc
                .dispatch(EndpointId::new(), attempt, first_reservation)
                .await;
            assert_eq!(first, ActionDispatchOutcome::Sent);

            let second_reservation = reservation(&arbiter);
            let second = svc
                .dispatch(EndpointId::new(), attempt, second_reservation)
                .await;

            assert_eq!(second, ActionDispatchOutcome::AlreadyDispatched);
            assert_eq!(
                transport.call_count(),
                1,
                "the transport must not be invoked a second time"
            );
            assert_eq!(
                reservations.take(attempt.id),
                Some(first_reservation),
                "the original reservation must remain the mapping, never replaced"
            );
        }

        #[tokio::test]
        async fn repeated_call_after_first_send_failed_still_sends_nothing() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::failing_once());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let first_reservation = reservation(&arbiter);
            let attempt = dispatched_attempt();

            let first = svc
                .dispatch(EndpointId::new(), attempt, first_reservation)
                .await;
            assert_eq!(
                first,
                ActionDispatchOutcome::SendFailed(AgentDispatchError::SendFailed)
            );
            assert_eq!(transport.call_count(), 1);

            let second_reservation = reservation(&arbiter);
            let second = svc
                .dispatch(EndpointId::new(), attempt, second_reservation)
                .await;

            assert_eq!(
                second,
                ActionDispatchOutcome::AlreadyDispatched,
                "a first send failure must still refuse a second send attempt"
            );
            assert_eq!(
                transport.call_count(),
                1,
                "the failed first send must never be retried by a second call"
            );
            assert_eq!(
                reservations.take(attempt.id),
                Some(first_reservation),
                "the reservation must remain registered after a send failure, not released"
            );
        }

        #[tokio::test]
        async fn a_non_dispatched_attempt_is_rejected_without_registering_or_sending() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::new());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let res = reservation(&arbiter);
            let attempt = Attempt {
                state: AttemptState::Succeeded,
                ..dispatched_attempt()
            };

            let outcome = svc.dispatch(EndpointId::new(), attempt, res).await;

            assert_eq!(outcome, ActionDispatchOutcome::NotDispatchable);
            assert_eq!(transport.call_count(), 0);
            assert_eq!(
                reservations.take(attempt.id),
                None,
                "no mapping must be registered for a non-Dispatched attempt"
            );
        }

        fn transfer_fixture() -> bamep_domain::Transfer {
            bamep_domain::create_transfer_context(
                EndpointId::new(),
                bamep_domain::JobId::new(),
                JobStepId::new(),
                bamep_domain::TransferDirection::AgentToServer,
                bamep_domain::DigestAlgorithm::Sha256,
                bamep_domain::ChunkSize::new(4096).unwrap(),
                bamep_domain::SourceProvenance::new("disk-0"),
            )
            .transfer
        }

        /// Issue #40 "Action parameter reconstruction" / "Wire boundary":
        /// `dispatch_transfer` must produce the exact RF-005
        /// `bamep.m1.data-plane-transfer` v1 action, with `parameters`
        /// reconstructed only from durable `transfer` state — no arbitrary
        /// caller-supplied action surface.
        #[tokio::test]
        async fn dispatch_transfer_sends_the_exact_rf005_action() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::new());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let res = reservation(&arbiter);
            let transfer = transfer_fixture();
            let attempt = Attempt {
                job_step_id: transfer.job_step_id,
                ..dispatched_attempt()
            };

            let outcome = svc
                .dispatch_transfer(EndpointId::new(), attempt, res, &transfer)
                .await;

            assert_eq!(outcome, ActionDispatchOutcome::Sent);
            let sent = transport
                .last_dispatch()
                .expect("exactly one ActionDispatch must have been sent");
            assert_eq!(sent.body.action_id.as_uuid(), attempt.action_id.0);
            assert_eq!(
                sent.body.action_type,
                super::super::M1_DATA_PLANE_TRANSFER_ACTION_TYPE
            );
            assert_eq!(
                sent.body.action_version,
                super::super::M1_DATA_PLANE_TRANSFER_ACTION_VERSION
            );
            assert_eq!(
                sent.envelope.correlation_id,
                Some(ProtocolId::from_uuid(attempt.action_id.0).unwrap()),
                "correlation_id must equal action_id"
            );

            let params = &sent.body.parameters;
            assert_eq!(
                params.len(),
                5,
                "no extra/arbitrary parameter may be present"
            );
            assert_eq!(
                params.get("transfer_id").and_then(|v| v.as_str()),
                Some(transfer.id.0.to_string()).as_deref()
            );
            assert_eq!(
                params.get("artifact_id").and_then(|v| v.as_str()),
                Some(transfer.artifact_id.0.to_string()).as_deref()
            );
            assert_eq!(
                params.get("direction").and_then(|v| v.as_str()),
                Some("agent_to_server")
            );
            assert_eq!(
                params.get("digest_algorithm").and_then(|v| v.as_str()),
                Some("sha256")
            );
            assert_eq!(
                params.get("chunk_size").and_then(|v| v.as_u64()),
                Some(transfer.chunk_size.get() as u64)
            );
        }

        #[tokio::test]
        async fn dispatch_transfer_registers_the_reservation_like_dispatch() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::new());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let res = reservation(&arbiter);
            let transfer = transfer_fixture();
            let attempt = Attempt {
                job_step_id: transfer.job_step_id,
                ..dispatched_attempt()
            };

            svc.dispatch_transfer(EndpointId::new(), attempt, res, &transfer)
                .await;
            assert_eq!(reservations.take(attempt.id), Some(res));
        }

        #[tokio::test]
        async fn dispatch_transfer_repeated_call_sends_nothing_a_second_time() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::new());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let transfer = transfer_fixture();
            let attempt = Attempt {
                job_step_id: transfer.job_step_id,
                ..dispatched_attempt()
            };

            let first_reservation = reservation(&arbiter);
            let first = svc
                .dispatch_transfer(EndpointId::new(), attempt, first_reservation, &transfer)
                .await;
            assert_eq!(first, ActionDispatchOutcome::Sent);

            let second_reservation = reservation(&arbiter);
            let second = svc
                .dispatch_transfer(EndpointId::new(), attempt, second_reservation, &transfer)
                .await;
            assert_eq!(second, ActionDispatchOutcome::AlreadyDispatched);
            assert_eq!(transport.call_count(), 1);
        }

        #[tokio::test]
        async fn dispatch_transfer_a_non_dispatched_attempt_is_rejected() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::new());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let res = reservation(&arbiter);
            let transfer = transfer_fixture();
            let attempt = Attempt {
                job_step_id: transfer.job_step_id,
                state: AttemptState::Succeeded,
                ..dispatched_attempt()
            };

            let outcome = svc
                .dispatch_transfer(EndpointId::new(), attempt, res, &transfer)
                .await;

            assert_eq!(outcome, ActionDispatchOutcome::NotDispatchable);
            assert_eq!(transport.call_count(), 0);
        }

        /// Confirms the pre-existing single M1 action is untouched by #40's
        /// generalization: `dispatch` still sends exactly
        /// `bamep.m1.simulated-execution` v1 with empty `parameters`.
        #[tokio::test]
        async fn existing_simulated_execution_dispatch_is_unaffected() {
            let reservations = Arc::new(AttemptReservationRegistry::new());
            let transport = Arc::new(FakeDispatchPort::new());
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&transport) as Arc<dyn AgentDispatchPort>,
            );
            let arbiter = arbiter();
            let res = reservation(&arbiter);
            let attempt = dispatched_attempt();

            let outcome = svc.dispatch(EndpointId::new(), attempt, res).await;

            assert_eq!(outcome, ActionDispatchOutcome::Sent);
            let sent = transport.last_dispatch().unwrap();
            assert_eq!(
                sent.body.action_type,
                super::super::M1_SIMULATED_EXECUTION_ACTION_TYPE
            );
            assert_eq!(
                sent.body.action_version,
                super::super::M1_SIMULATED_EXECUTION_ACTION_VERSION
            );
            assert!(sent.body.parameters.is_empty());
        }
    }
}
