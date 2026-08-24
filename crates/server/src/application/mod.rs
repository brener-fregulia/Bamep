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
    evaluate_final_destructive_dispatch, transitions, ActionEvidence, ActionEvidenceOutcome,
    ActionId, Actor, Attempt, AttemptState, AuditRecord, BootContext, BootNonce, CancelAckEvidence,
    CancellationRequestOutcome, DestructiveIntent, EmptyWorkflow, EndpointId, FinalDispatchInputs,
    FinalDispatchOutcome, FinalDispatchRejection, IdentityState, InvalidIdentityTransition,
    InventoryRevision, InventorySnapshot, Job, JobId, JobStepId, TrustedBootstrapState,
    DEFAULT_CREDENTIAL_TTL,
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
    CreateWorkflowError, CredentialRedemptionRepository, EndpointRepository, EndpointUpdateError,
    FinalDispatchCommit, FinalDispatchDecision, FinalDispatchLockedFacts, InventoryRepository,
    JobRepository, RedemptionDecision, RedemptionTarget, RepositoryError,
    RequestCancellationDecision, RequestCancellationError, RequestCancellationLockedFacts,
    RequestCancellationResult, SatisfyStepPreconditionsDecision, SatisfyStepPreconditionsError,
    TargetRevalidationPort,
};
use crate::runtime::presence::PresenceRegistry;
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
    #[error(transparent)]
    InvalidTransition(#[from] InvalidIdentityTransition),
    #[error(transparent)]
    EmptyWorkflow(#[from] EmptyWorkflow),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
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
        if attempt.state != AttemptState::Dispatched {
            return ActionDispatchOutcome::NotDispatchable;
        }

        match self.reservations.register(attempt.id, reservation) {
            RegistrationOutcome::AlreadyRegistered => {
                return ActionDispatchOutcome::AlreadyDispatched
            }
            RegistrationOutcome::Registered => {}
        }

        let action_id = ProtocolId::from_uuid(attempt.action_id.0)
            .expect("a Domain ActionId is always a valid UUID v4");
        let dispatch = ActionDispatchMessage::new(
            action_id,
            M1_SIMULATED_EXECUTION_ACTION_TYPE,
            M1_SIMULATED_EXECUTION_ACTION_VERSION,
            serde_json::Map::new(),
        );

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
        }

        impl FakeDispatchPort {
            fn new() -> Self {
                Self::default()
            }

            fn failing_once() -> Self {
                Self {
                    calls: AtomicUsize::new(0),
                    fail_next: Mutex::new(true),
                }
            }

            fn call_count(&self) -> usize {
                self.calls.load(Ordering::SeqCst)
            }
        }

        #[async_trait]
        impl AgentDispatchPort for FakeDispatchPort {
            async fn dispatch_action(
                &self,
                _endpoint_id: EndpointId,
                _dispatch: ActionDispatchMessage,
            ) -> Result<(), AgentDispatchError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
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
    }
}
