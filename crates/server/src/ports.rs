//! Repository Port (`m0-stack-and-boundaries-baseline.md` "Component
//! responsibilities and boundaries" — Ports: repositories). Application and
//! Domain depend only on this trait; concrete persistence lives entirely in
//! `adapters::postgres` (ADR-0013 "PostgreSQL persistence backend
//! baseline").
//!
//! `update_endpoint` and `CredentialRedemptionRepository::redeem` each bound
//! one durable transaction: the Adapter is responsible for locking the
//! affected row(s) *before* invoking the supplied `decide` closure, and for
//! committing the closure's result atomically within that same lock/
//! transaction scope. This is the mechanism that satisfies ADR-0012's
//! commit-time concurrency requirement ("the credential presented needs to
//! remain valid at the commit that accepts the redemption"). `decide` itself
//! never touches the database: it only calls into `bamep_domain::transitions`,
//! so the Domain remains the sole owner of transition/business-rule
//! decisions; the Adapter never reimplements them in SQL.

use std::collections::BTreeSet;

use async_trait::async_trait;
use bamep_domain::presented_credential::{CredentialKind, CredentialLookupId};
use bamep_domain::{
    ActionEvidenceApplied, ActionId, Artifact, ArtifactId, ArtifactTransitionError, Attempt,
    AttemptId, AuditRecord, BootContext, BootContextResolveError, CancelAckApplied,
    CancellationRequestError, ChunkAcceptError, ChunkIndex, ChunkRecordError, ChunkRecordOutcome,
    DestructiveIntent, DestructiveIntentError, Digest, DigestAlgorithm, EndpointAggregate,
    EndpointId, FinalDispatchDenial, FinalDispatchOutcome, FinalDispatchRejection,
    InvalidIdentityTransition, InventoryRevision, InventoryRevisionId, InventorySnapshot, Job,
    JobAdmissionError, JobAdmissionOutcome, JobId, JobStep, JobStepEligibilityError, JobStepId,
    ReconciliationApplied, RedeemOutcome, SealError, SealOutcome, TargetFingerprint, Transfer,
    TransferBindingError, TransferContext, TransferId, TransitionOutcome, TrustedBootstrapOutcome,
};

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("persistence backend error: {0}")]
    Backend(String),
}

/// A pure decision over a known Endpoint's current state, producing the
/// transition to persist or the reason it is illegal.
pub type UpdateDecision = Box<
    dyn FnOnce(EndpointAggregate) -> Result<TransitionOutcome, InvalidIdentityTransition> + Send,
>;

pub type TrustedBootstrapDecision =
    Box<dyn FnOnce(EndpointAggregate) -> TrustedBootstrapOutcome + Send>;

#[derive(Debug, thiserror::Error)]
pub enum EndpointUpdateError {
    #[error("endpoint {0:?} not found")]
    NotFound(EndpointId),
    #[error(transparent)]
    InvalidTransition(#[from] InvalidIdentityTransition),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

#[async_trait]
pub trait EndpointRepository: Send + Sync {
    /// Read-only lookup, for verification/reporting only. Never used to
    /// drive a transition decision — `update_endpoint`/
    /// `CredentialRedemptionRepository::redeem` own that path so the read
    /// and the eventual commit share one lock/transaction.
    async fn find_by_id(
        &self,
        id: EndpointId,
    ) -> Result<Option<EndpointAggregate>, RepositoryError>;

    /// Locks the Endpoint identified by `id`, invokes `decide` with its
    /// current state, and atomically persists the returned
    /// [`TransitionOutcome`] in the same transaction. Used by operator
    /// approval and credential revocation — operations that require the
    /// Endpoint to already exist and never touch the credential-lookup
    /// projection (`crate::adapters::postgres` "Lookup projection on
    /// accepted redemption" — that projection is owned exclusively by
    /// [`CredentialRedemptionRepository`]).
    async fn update_endpoint(
        &self,
        id: EndpointId,
        decide: UpdateDecision,
    ) -> Result<TransitionOutcome, EndpointUpdateError>;

    /// Locks and freshly reads exactly one Endpoint. Accepted (including
    /// idempotent) outcomes commit atomically; rejection rolls back without
    /// persistence.
    async fn establish_trusted_bootstrap(
        &self,
        id: EndpointId,
        decide: TrustedBootstrapDecision,
    ) -> Result<TrustedBootstrapOutcome, EndpointUpdateError>;
}

#[async_trait]
pub trait InventoryRepository: Send + Sync {
    /// Locks the Endpoint, compares against its durable current revision, and
    /// atomically commits a changed revision/current pointer/event. Returns
    /// `None` for a semantically unchanged snapshot.
    async fn record_inventory(
        &self,
        endpoint_id: EndpointId,
        inventory: InventorySnapshot,
        recorded_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<InventoryRevision>, EndpointUpdateError>;

    async fn find_current_inventory(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<Option<InventoryRevision>, EndpointUpdateError>;
}

/// Errors from [`JobRepository::create_workflow`]. Distinct from
/// [`EndpointUpdateError`]: workflow creation never invokes a Domain
/// transition decision closure on the Endpoint, it only verifies the target
/// Endpoint's existence/state before persisting the already-constructed
/// [`Job`].
#[derive(Debug, thiserror::Error)]
pub enum CreateWorkflowError {
    #[error("endpoint {0:?} not found")]
    EndpointNotFound(EndpointId),
    #[error("endpoint {0:?} is not enrolled")]
    EndpointNotEnrolled(EndpointId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// A pure decision over one freshly locked [`JobStep`]'s current state,
/// producing the [`DestructiveIntent`] to persist or the reason it cannot be
/// attached right now (`m0-job-lifecycle-and-scheduling.md`; Issue #31). The
/// closure captures the already Server-derived authorized inventory revision
/// and target fingerprint — it never reads them from the `JobStep` itself —
/// and only calls into `bamep_domain::authorize_destructive_intent`, mirroring
/// [`UpdateDecision`]/[`TrustedBootstrapDecision`]: the Adapter locks and
/// reads current state, the Domain decides, the Adapter persists the result
/// atomically in the same transaction.
pub type AuthorizeDestructiveIntentDecision =
    Box<dyn FnOnce(&JobStep) -> Result<DestructiveIntent, DestructiveIntentError> + Send>;

/// Errors from [`JobRepository::authorize_destructive_intent`].
#[derive(Debug, thiserror::Error)]
pub enum AuthorizeDestructiveIntentError {
    #[error("job step {0:?} not found in job {1:?}")]
    JobStepNotFound(JobStepId, JobId),
    #[error("job step {0:?} is not eligible for destructive intent authorization")]
    NotEligible(JobStepId),
    #[error("job step {0:?} already has a destructive intent")]
    AlreadyAuthorized(JobStepId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// A pure decision over one freshly locked [`Job`]'s current state, producing
/// the [`JobAdmissionOutcome`] to persist or the reason admission is illegal
/// right now (`m0-job-lifecycle-and-scheduling.md` "Job lifecycle"; Issue
/// #32). Mirrors [`AuthorizeDestructiveIntentDecision`]: the Adapter locks
/// and reads current state, the Domain decides, the Adapter persists the
/// result atomically in the same transaction. This closure never itself
/// verifies or acquires Job-scoped Endpoint exclusivity — the Adapter's
/// active-Job uniqueness constraint is the durable guarantee for that.
pub type AdmitJobDecision =
    Box<dyn FnOnce(&Job) -> Result<JobAdmissionOutcome, JobAdmissionError> + Send>;

/// Errors from [`JobRepository::admit_job`].
#[derive(Debug, thiserror::Error)]
pub enum AdmitJobError {
    #[error("job {0:?} not found")]
    JobNotFound(JobId),
    #[error("job {0:?} is not eligible for admission")]
    NotEligible(JobId),
    /// The target Endpoint already has another `Running`/`Cancelling` Job —
    /// the losing side of a same-Endpoint admission race
    /// (`m0-job-lifecycle-and-scheduling.md` "Resource leases": "a competing
    /// Job for that Endpoint remains `Pending`"). Not a repository failure:
    /// the caller's Job remains durably `Pending`, exactly as before the
    /// attempt.
    #[error("endpoint already has an active job")]
    EndpointNotAvailable,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// A pure decision over one freshly locked [`Job`] (including its current
/// ordered `JobStep`s), producing the advanced [`JobStep`] to persist or the
/// reason `step_id` is not currently eligible
/// (`m0-job-lifecycle-and-scheduling.md` "JobStep lifecycle"; Issue #32). The
/// closure captures `step_id` itself, mirroring
/// [`AuthorizeDestructiveIntentDecision`]/[`AdmitJobDecision`].
pub type SatisfyStepPreconditionsDecision =
    Box<dyn FnOnce(&Job) -> Result<JobStep, JobStepEligibilityError> + Send>;

/// Errors from [`JobRepository::satisfy_current_step_preconditions`].
#[derive(Debug, thiserror::Error)]
pub enum SatisfyStepPreconditionsError {
    #[error("job {0:?} not found")]
    JobNotFound(JobId),
    #[error("job {0:?} is not Running")]
    JobNotRunning(JobId),
    #[error("job step {0:?} not found in job {1:?}")]
    JobStepNotFound(JobStepId, JobId),
    #[error("job step {0:?} is not the current eligible step")]
    NotCurrent(JobStepId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Durable Job/JobStep workflow persistence
/// (`m0-job-lifecycle-and-scheduling.md` "Domain model"). Issue #24
/// established durable workflow creation; Issue #31 extended this Port with
/// destructive-intent authorization; Issue #32 extended it further with Job
/// admission and preliminary JobStep eligibility. Later scheduling/dispatch/
/// reconciliation Work Packages extend this Port further as they add that
/// persistence.
#[async_trait]
pub trait JobRepository: Send + Sync {
    /// Verifies `job.endpoint_id` is an existing `Enrolled` Endpoint, then
    /// atomically persists `job` and every one of its `JobStep`s
    /// (`m0-persistence-observability-and-domain-events.md` "Atomic
    /// persistence"). Rejects and persists nothing — no partial Job or
    /// JobStep row — when the target Endpoint does not exist or is not
    /// `Enrolled`, or when persistence itself fails partway through.
    async fn create_workflow(&self, job: &Job) -> Result<(), CreateWorkflowError>;

    /// Read-only lookup of a persisted workflow and its ordered `JobStep`s,
    /// for verification/reporting only.
    async fn find_job(&self, id: JobId) -> Result<Option<Job>, RepositoryError>;

    /// Locks exactly the `JobStep` identified by `(job_id, step_id)`, invokes
    /// `decide` with its current freshly-read state, and — only on `Ok` —
    /// atomically persists the returned [`DestructiveIntent`] on that row in
    /// the same transaction (Issue #31 "Persistence"). Persists nothing when
    /// the step does not exist under `job_id`, or `decide` rejects it: this
    /// is the atomicity boundary that prevents a concurrent authorization
    /// attempt from silently overwriting an already-attached intent — the
    /// second attempt observes it under the same lock and `decide` rejects
    /// it with [`DestructiveIntentError::AlreadyAuthorized`].
    async fn authorize_destructive_intent(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        decide: AuthorizeDestructiveIntentDecision,
    ) -> Result<DestructiveIntent, AuthorizeDestructiveIntentError>;

    /// Locks exactly the `Job` identified by `job_id`, invokes `decide` with
    /// its current freshly-read state, and — only on `Ok` — atomically
    /// persists `Pending -> Running` and the required `JobStarted` domain
    /// event in the same transaction (Issue #32 "Job admission"). Job-scoped
    /// Endpoint exclusivity is enforced by the Adapter's active-Job
    /// uniqueness constraint at this same commit: a concurrent admission
    /// attempt already `Running`/`Cancelling` against the same Endpoint fails
    /// with [`AdmitJobError::EndpointNotAvailable`] rather than persisting a
    /// second active Job, and no transaction ever commits `Running` without
    /// its `JobStarted` event or vice versa.
    async fn admit_job(
        &self,
        job_id: JobId,
        decide: AdmitJobDecision,
    ) -> Result<Job, AdmitJobError>;

    /// Locks the `Job` identified by `job_id` and its current ordered
    /// `JobStep`s, invokes `decide` with that freshly-read aggregate, and —
    /// only on `Ok` — atomically persists the returned `JobStep`'s
    /// `Pending -> PreconditionsSatisfied` transition (Issue #32 "Current
    /// ordered JobStep preliminary eligibility"). No domain event is
    /// required for this transition under the current persistence contract.
    async fn satisfy_current_step_preconditions(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        decide: SatisfyStepPreconditionsDecision,
    ) -> Result<JobStep, SatisfyStepPreconditionsError>;

    /// Locks exactly the `Job` identified by `job_id` (including its current
    /// ordered `JobStep`s), the owning `Endpoint`, and any existing
    /// non-terminal `Attempt` for `step_id`, then invokes `decide` with that
    /// freshly-read [`FinalDispatchLockedFacts`] (Issue #25 "Commit-time
    /// revalidation/locking"). Every durable lock this method acquires is
    /// held before `decide` runs, so a transient read the closure performs
    /// internally (Runtime Presence Registry, `TargetRevalidationPort`, "now")
    /// always observes state no older than that lock acquisition.
    ///
    /// On `Ok`, atomically persists — in the same transaction — the
    /// candidate JobStep's `PreconditionsSatisfied -> Dispatching`
    /// transition, the new `attempts` row, and the destructive-dispatch
    /// audit record together (`m0-persistence-observability-and-domain-events.md`
    /// "Atomic persistence"). No `ActionDispatch` is sent by this method or
    /// anything it calls.
    ///
    /// On `Err`, persists exactly the `FinalDispatchDenial::pending_job_step`
    /// the Domain decision returned — `Some(step in Pending)` for a
    /// final-revalidation failure, `None` (nothing persisted) for a
    /// structural mismatch (the JobStep was not, or is no longer,
    /// `PreconditionsSatisfied`). This Adapter never independently decides
    /// that a revalidation failure means `Pending`; it only persists the
    /// state the Domain decision already supplied. Never creates an Attempt,
    /// action correlation, or audit record on any `Err` path.
    async fn commit_destructive_dispatch(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        decide: FinalDispatchDecision,
    ) -> Result<FinalDispatchOutcome, CommitDestructiveDispatchError>;

    /// Locks exactly the `Job` identified by `job_id` (including its current
    /// ordered `JobStep`s), whether an existing non-terminal `Attempt`
    /// exists for `step_id`, and the durable pre-dispatch `Transfer`
    /// identified by `transfer_id` (Issue #36), then invokes `decide` with
    /// that freshly-read [`TransferDispatchLockedFacts`] (Issue #40
    /// "Commit non-destructive transfer Attempts for dispatch"). This is the
    /// non-destructive sibling of [`Self::commit_destructive_dispatch`]: it
    /// never resolves or requires Endpoint identity/credential/presence/
    /// hardware-confidence/trusted-bootstrap/target-fingerprint evidence —
    /// the seven-item destructive-operation gate is structurally
    /// unreachable from this method.
    ///
    /// On `Ok`, atomically persists — in the same transaction — the
    /// candidate JobStep's `PreconditionsSatisfied -> Dispatching`
    /// transition, the new `attempts` row, and the one-time Transfer ->
    /// Attempt binding together
    /// (`m0-persistence-observability-and-domain-events.md` "Atomic
    /// persistence"). No `ActionDispatch` is sent by this method or
    /// anything it calls, and no destructive-dispatch audit record is
    /// created — this action is non-destructive and the persistence
    /// contract does not require one for this commitment.
    ///
    /// On `Err`, persists exactly the
    /// `TransferDispatchDenial::pending_job_step` the Domain decision
    /// returned, mirroring [`Self::commit_destructive_dispatch`]'s identical
    /// contract — this Adapter never independently decides that a
    /// revalidation failure means `Pending`. Never mutates `transfers` on
    /// any `Err` path: the existing pre-dispatch Transfer/Artifact remain
    /// exactly as they were.
    async fn commit_transfer_dispatch(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        transfer_id: bamep_domain::TransferId,
        decide: TransferDispatchDecision,
    ) -> Result<bamep_domain::TransferDispatchOutcome, CommitTransferDispatchError>;

    /// Read-only lookup of one persisted `Attempt` by its Server Domain
    /// identity (Issue #25 correction: persistence reload must reconstruct
    /// the committed Attempt/action correlation through this Port, not
    /// through raw SQL in a test). Returns `None` when no `Attempt` with
    /// `attempt_id` has ever been committed. Does not imply Agent receipt or
    /// execution — reload/reconstruction proves durable persistence only.
    /// This is deliberately the narrowest read this correction requires; it
    /// does not introduce #26/#28 querying/reconciliation APIs.
    async fn find_attempt(&self, attempt_id: AttemptId)
        -> Result<Option<Attempt>, RepositoryError>;

    /// Resolves `action_id` to its owning Attempt/JobStep/Job, locks all
    /// three in that stable order, verifies the owning Job targets
    /// `authenticated_endpoint_id`, invokes `decide` with the freshly locked
    /// [`ActionEvidenceLockedFacts`], and — only for
    /// [`ApplyActionEvidenceDecisionOutcome::Applied`] — atomically persists
    /// the returned [`ActionEvidenceCommit`]'s Attempt/JobStep/Job state,
    /// required events, and (for a terminal outcome) required audit record
    /// in the same transaction (Issue #26 "PostgreSQL evidence application").
    ///
    /// An unknown `action_id`, or a known `action_id` whose owning Job
    /// targets a different Endpoint, is
    /// [`ApplyActionEvidenceError::UnknownAction`] in both cases — this
    /// method never distinguishes the two outcomes, so a caller can never
    /// learn whether a foreign/unknown `action_id` exists
    /// (`m0-agent-protocol-contract.md`; Issue #26 "Authenticated Endpoint
    /// correlation"). `decide` is never invoked in that case.
    async fn apply_action_evidence(
        &self,
        action_id: ActionId,
        authenticated_endpoint_id: EndpointId,
        decide: ApplyActionEvidenceDecision,
    ) -> Result<ApplyActionEvidenceResult, ApplyActionEvidenceError>;

    /// Read-only correlation check for `ActionProgress` (Issue #26
    /// "Correlate ActionProgress to the authenticated Endpoint"): resolves
    /// `action_id` to its owning Attempt -> JobStep -> Job and reports
    /// whether that Job targets `authenticated_endpoint_id`. Unlike
    /// [`Self::apply_action_evidence`], this never locks, decides, or
    /// persists anything — a plain read, since `ActionProgress` is transient
    /// advisory metadata that must never reach a lifecycle transition. An
    /// unknown `action_id` and a known `action_id` belonging to another
    /// Endpoint's Job both report `false` — this method never distinguishes
    /// the two, mirroring `apply_action_evidence`'s identical non-enumeration
    /// policy.
    async fn action_targets_endpoint(
        &self,
        action_id: ActionId,
        authenticated_endpoint_id: EndpointId,
    ) -> Result<bool, RepositoryError>;

    /// Locks exactly the `Job` identified by `job_id` (including its current
    /// ordered `JobStep`s) and — when one currently exists — the JobStep-
    /// current Attempt in `Dispatched`/`InProgress`/`AwaitingReconciliation`,
    /// under the same Attempt -> JobStep -> Job lock order
    /// [`Self::apply_action_evidence`] uses (Issue #27 "Lock order /
    /// concurrency": "Do NOT introduce a competing Job -> Attempt lock
    /// cycle"), invokes `decide` with that freshly-read
    /// [`RequestCancellationLockedFacts`], and — only for
    /// [`CancellationRequestDecided::EnteredCancelling`] /
    /// [`CancellationRequestDecided::CompletedImmediately`] — atomically
    /// persists the returned Job state, required domain event (for
    /// `CompletedImmediately`), and required operator cancellation audit in
    /// the same transaction.
    ///
    /// More than one simultaneously active/uncertain Attempt for `job_id` is
    /// never guessed at — it is an invariant/backend error
    /// ([`RepositoryError::Backend`]) rather than an arbitrarily selected
    /// candidate (Issue #27 "Active Attempt selection").
    async fn request_cancellation(
        &self,
        job_id: JobId,
        decide: RequestCancellationDecision,
    ) -> Result<RequestCancellationResult, RequestCancellationError>;

    /// Resolves `action_id` to its owning Attempt/JobStep/Job, locks all
    /// three in the same Attempt -> JobStep -> Job order
    /// [`Self::apply_action_evidence`] uses, verifies the owning Job targets
    /// `authenticated_endpoint_id`, invokes `decide` with the freshly locked
    /// [`ActionEvidenceLockedFacts`], and — only for
    /// [`ApplyCancelAckDecisionOutcome::Applied`] — atomically persists the
    /// returned [`CancelAckCommit`]'s Attempt/JobStep/Job state, required
    /// `JobCancelled` event, and (for a terminal outcome) required audit
    /// record in the same transaction (Issue #27 "CancelAck handling").
    ///
    /// Mirrors [`Self::apply_action_evidence`]'s non-enumeration contract: an
    /// unknown `action_id`, or a known `action_id` whose owning Job targets a
    /// different Endpoint, is [`ApplyActionEvidenceError::UnknownAction`] in
    /// both cases.
    async fn apply_cancel_ack(
        &self,
        action_id: ActionId,
        authenticated_endpoint_id: EndpointId,
        decide: ApplyCancelAckDecision,
    ) -> Result<ApplyCancelAckResult, ApplyActionEvidenceError>;

    /// Locks `endpoint_id`'s current `Running`/`Cancelling` Job's
    /// JobStep-current Attempt (if any), invokes `decide` with it, and —
    /// only when `decide` returns `Some` — atomically persists the resulting
    /// state (Issue #28 "Connection loss"). No event or audit is required
    /// for this transition (`m0-persistence-observability-and-domain-events.md`
    /// "Domain events" never lists one for entering `AwaitingReconciliation`).
    /// Returns the reconciled `AttemptId`, or `None` when no eligible
    /// candidate exists (no active Job, or its Attempt is already
    /// `AwaitingReconciliation`/terminal). Never mutates an unrelated
    /// Endpoint's Attempts.
    async fn mark_endpoint_active_attempt_uncertain(
        &self,
        endpoint_id: EndpointId,
        decide: MarkUncertainDecision,
    ) -> Result<Option<AttemptId>, RepositoryError>;

    /// Server-restart recovery (Issue #28 "Server restart"): locks and
    /// invokes `decide` against every currently `Dispatched`/`InProgress`
    /// Attempt, across every Endpoint, and atomically persists every `Some`
    /// result (`m0-job-lifecycle-and-scheduling.md` "Reconciliation": "On
    /// Server restart: persisted `Dispatched` and `InProgress` Attempts
    /// become `AwaitingReconciliation`"). No event or audit is required.
    /// Returns every reconciled `AttemptId`. Never creates a second Attempt
    /// and never sends anything — sending `StatusQuery` happens only once the
    /// relevant Agent session re-establishes, through
    /// [`Self::find_reconciliation_candidate`].
    async fn reconcile_all_active_attempts_on_startup(
        &self,
        decide: MarkUncertainDecision,
    ) -> Result<Vec<AttemptId>, RepositoryError>;

    /// Read-only lookup (no lock, no mutation) of `endpoint_id`'s current
    /// `AwaitingReconciliation` Attempt, if any — used only to decide whether
    /// to issue `StatusQuery{action_id}` once a valid authenticated session
    /// (re-)establishes for this Endpoint (Issue #28 "Reconciliation").
    /// `None` when no Attempt for this Endpoint is currently
    /// `AwaitingReconciliation`.
    async fn find_reconciliation_candidate(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<Option<ActionId>, RepositoryError>;

    /// Resolves `action_id` to its owning Attempt/JobStep/Job, locks all
    /// three in the same Attempt -> JobStep -> Job order
    /// [`Self::apply_action_evidence`] uses, verifies the owning Job targets
    /// `authenticated_endpoint_id`, invokes `decide` with the freshly locked
    /// [`ActionEvidenceLockedFacts`], and — only for
    /// [`ApplyReconciliationDecisionOutcome::Applied`] — atomically persists
    /// the returned [`ReconciliationCommit`]'s Attempt/JobStep/Job state,
    /// required events, and (for a terminal outcome) required audit record in
    /// the same transaction (Issue #28 "Gateway": inbound `StatusReport`
    /// evidence application). Mirrors [`Self::apply_action_evidence`]'s
    /// non-enumeration contract: an unknown `action_id`, or a known
    /// `action_id` whose owning Job targets a different Endpoint, is
    /// [`ApplyActionEvidenceError::UnknownAction`] in both cases.
    async fn apply_status_report(
        &self,
        action_id: ActionId,
        authenticated_endpoint_id: EndpointId,
        decide: ApplyReconciliationDecision,
    ) -> Result<ApplyReconciliationResult, ApplyActionEvidenceError>;

    /// Locates `job_id`'s current `AwaitingReconciliation` Attempt (at most
    /// one can ever exist, by the same JobStep-current-Attempt invariant
    /// every other locking method in this trait already relies on), locks it
    /// with its owning JobStep/Job in the same Attempt -> JobStep -> Job
    /// order, invokes `decide` with the freshly locked
    /// [`ActionEvidenceLockedFacts`], and — only for
    /// [`ApplyReconciliationDecisionOutcome::Applied`] — atomically persists
    /// the returned [`ReconciliationCommit`]'s Attempt/JobStep/Job state, the
    /// required `AttemptIndeterminate`/`JobStepFailed`/Job-terminal events,
    /// and the required operator-decision audit record in the same
    /// transaction (Issue #28 "Explicit Indeterminate closure"). Structurally
    /// separate from [`Self::apply_status_report`]: only an explicit
    /// operator/internal control path calls this — the Agent can never reach
    /// `Indeterminate` on its own.
    async fn close_indeterminate(
        &self,
        job_id: JobId,
        decide: CloseIndeterminateDecision,
    ) -> Result<CloseIndeterminateResult, CloseIndeterminateError>;
}

/// A pure decision over one freshly locked, currently `Dispatched`/
/// `InProgress` [`Attempt`], producing the `AwaitingReconciliation` result to
/// persist, or `None` when the Attempt no longer qualifies by the time it is
/// locked (`bamep_domain::mark_awaiting_reconciliation`; Issue #28
/// "Connection loss", "Server restart"). Unlike every other decide-closure
/// in this trait, this one is `Fn`, not `FnOnce`: it captures no
/// call-specific context (no clock, no audit, no Cancelling-composition
/// judgment — entering `AwaitingReconciliation` requires none of those), so
/// the same closure instance is reused across every candidate Attempt
/// [`JobRepository::reconcile_all_active_attempts_on_startup`] locks.
pub type MarkUncertainDecision = Box<dyn Fn(&Attempt) -> Option<Attempt> + Send + Sync>;

/// Durable facts read under lock immediately before one `StatusReport`/
/// explicit-Indeterminate reconciliation decision — identical shape to
/// [`ActionEvidenceLockedFacts`], reused directly rather than duplicated.
pub type ReconciliationLockedFacts = ActionEvidenceLockedFacts;

/// A pure decision over freshly locked [`ReconciliationLockedFacts`],
/// mirroring [`ApplyActionEvidenceDecision`]/[`ApplyCancelAckDecision`]: the
/// Adapter locks and reads current state, this closure decides (calling
/// `bamep_domain::apply_status_report` or `bamep_domain::close_indeterminate`
/// and, only for a terminal outcome, constructing the required
/// [`AuditRecord`]), and the Adapter persists the result atomically in the
/// same transaction.
pub type ApplyReconciliationDecision =
    Box<dyn FnOnce(ReconciliationLockedFacts) -> ApplyReconciliationDecisionOutcome + Send>;

pub type CloseIndeterminateDecision =
    Box<dyn FnOnce(ReconciliationLockedFacts) -> CloseIndeterminateDecisionOutcome + Send>;

/// A successful reconciliation "Applied" result: the Domain outcome plus the
/// immutable terminal [`AuditRecord`] that must commit atomically alongside
/// it when `outcome.terminal` is `true` — `None` when `outcome.terminal` is
/// `false` (the `Accepted`/`Running` recovery to `InProgress`), mirroring
/// [`ActionEvidenceCommit`]/[`CancelAckCommit`].
pub struct ReconciliationCommit {
    pub outcome: ReconciliationApplied,
    pub audit: Option<AuditRecord>,
}

/// The two outcomes [`ApplyReconciliationDecision`] may produce, mirroring
/// `bamep_domain::ReconciliationOutcome` at the Application boundary. No
/// `Conflict` variant — see `bamep_domain::reconciliation` module docs.
#[allow(clippy::large_enum_variant)]
pub enum ApplyReconciliationDecisionOutcome {
    Applied(ReconciliationCommit),
    NoOp,
}

/// The result of [`JobRepository::apply_status_report`] after successful
/// resolution/locking/correlation — mirrors
/// [`ApplyReconciliationDecisionOutcome`] without the audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ApplyReconciliationResult {
    Applied(ReconciliationApplied),
    NoOp,
}

/// A successful [`CloseIndeterminateDecision`] "Applied" result: the Domain
/// outcome plus the required operator-decision [`AuditRecord`], which always
/// commits atomically alongside it — closing `Indeterminate` is always a
/// terminal, always-audited transition
/// (`m0-persistence-observability-and-domain-events.md` "Auditability":
/// "closing an Attempt `Indeterminate`" is a required safety-relevant
/// operator decision).
pub struct CloseIndeterminateCommit {
    pub outcome: ReconciliationApplied,
    pub audit: AuditRecord,
}

/// The three outcomes [`CloseIndeterminateDecision`] may produce, mirroring
/// `bamep_domain::CloseIndeterminateOutcome` at the Application boundary.
#[allow(clippy::large_enum_variant)]
pub enum CloseIndeterminateDecisionOutcome {
    Applied(CloseIndeterminateCommit),
    AlreadyIndeterminate,
    NotEligible,
}

/// The result of [`JobRepository::close_indeterminate`] after successful
/// resolution/locking — mirrors [`CloseIndeterminateDecisionOutcome`] without
/// the audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum CloseIndeterminateResult {
    Applied(ReconciliationApplied),
    AlreadyIndeterminate,
    NotEligible,
}

/// Errors from [`JobRepository::close_indeterminate`].
#[derive(Debug, thiserror::Error)]
pub enum CloseIndeterminateError {
    #[error("job {0:?} not found")]
    JobNotFound(JobId),
    /// No Attempt for `job_id` is currently `AwaitingReconciliation` (and
    /// none is `Indeterminate` either) — there is nothing to close.
    #[error("job {0:?} has no attempt currently awaiting reconciliation")]
    NoUncertainAttempt(JobId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Durable facts read under lock immediately before one cancellation-request
/// decision (Issue #27 "Durable cancellation request"): the current Job
/// (including every ordered JobStep) and — when one currently exists — the
/// JobStep-current Attempt in `Dispatched`/`InProgress`/`AwaitingReconciliation`.
pub struct RequestCancellationLockedFacts {
    pub job: Job,
    pub active_attempt: Option<Attempt>,
}

/// A pure decision over freshly locked [`RequestCancellationLockedFacts`],
/// mirroring [`FinalDispatchDecision`]/[`ApplyActionEvidenceDecision`]: the
/// Adapter locks and reads current state, this closure decides (calling
/// `bamep_domain::request_cancellation` and, for a mutating outcome,
/// constructing the required operator cancellation [`AuditRecord`] — Domain
/// itself never constructs one), and the Adapter persists the result
/// atomically in the same transaction.
pub type RequestCancellationDecision = Box<
    dyn FnOnce(
            RequestCancellationLockedFacts,
        ) -> Result<CancellationRequestDecided, CancellationRequestError>
        + Send,
>;

/// The Application-level decision [`RequestCancellationDecision`] returns,
/// mirroring `bamep_domain::CancellationRequestOutcome` with the required
/// [`AuditRecord`] attached for the two mutating cases.
pub enum CancellationRequestDecided {
    EnteredCancelling {
        job: Job,
        attempt_id: AttemptId,
        action_id: ActionId,
        audit: AuditRecord,
    },
    CompletedImmediately {
        job: Job,
        event: bamep_domain::DomainEvent,
        audit: AuditRecord,
    },
    AlreadyCancelling,
    AlreadyTerminal,
}

/// The result of [`JobRepository::request_cancellation`] after successful
/// resolution/locking, mirroring [`CancellationRequestDecided`] without the
/// audit record (a persistence-only concern). `EnteredCancelling` carries
/// only what the Application-level `CancelAction` send needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestCancellationResult {
    EnteredCancelling {
        attempt_id: AttemptId,
        action_id: ActionId,
        endpoint_id: EndpointId,
    },
    CompletedImmediately,
    AlreadyCancelling,
    AlreadyTerminal,
}

/// Errors from [`JobRepository::request_cancellation`].
#[derive(Debug, thiserror::Error)]
pub enum RequestCancellationError {
    #[error("job {0:?} not found")]
    JobNotFound(JobId),
    /// `job.state` was `Pending` — out of this WP's ACTIVE-Job-cancellation
    /// scope (`bamep_domain::CancellationRequestError::NotEligible`).
    #[error("job {0:?} is not eligible for a cancellation request")]
    NotEligible(JobId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// A pure decision over freshly locked [`ActionEvidenceLockedFacts`] for
/// `CancelAck` evidence, mirroring [`ApplyActionEvidenceDecision`]: the
/// Adapter locks and reads current state, this closure decides (calling
/// `bamep_domain::apply_cancel_ack` and, only for a terminal outcome,
/// constructing the required [`AuditRecord`]), and the Adapter persists the
/// result atomically in the same transaction.
pub type ApplyCancelAckDecision =
    Box<dyn FnOnce(ActionEvidenceLockedFacts) -> ApplyCancelAckDecisionOutcome + Send>;

/// A successful [`ApplyCancelAckDecision`] "Applied" result: the Domain
/// outcome plus the immutable terminal [`AuditRecord`] that must commit
/// atomically alongside it when `outcome.terminal` is `true` — `None`
/// otherwise, mirroring [`ActionEvidenceCommit`].
pub struct CancelAckCommit {
    pub outcome: CancelAckApplied,
    pub audit: Option<AuditRecord>,
}

/// The two outcomes [`ApplyCancelAckDecision`] may produce, mirroring
/// `bamep_domain::CancelAckOutcome` at the Application boundary. Unlike
/// [`ApplyActionEvidenceDecisionOutcome`] there is no `Conflict` variant —
/// see `bamep_domain::cancellation` module docs.
#[allow(clippy::large_enum_variant)]
pub enum ApplyCancelAckDecisionOutcome {
    Applied(CancelAckCommit),
    NoOp,
}

/// The result of [`JobRepository::apply_cancel_ack`] after successful
/// resolution/locking/correlation — mirrors [`ApplyCancelAckDecisionOutcome`]
/// without the audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ApplyCancelAckResult {
    Applied(CancelAckApplied),
    NoOp,
}

/// Durable facts read under lock immediately before one normal Agent action
/// evidence decision (Issue #26 "PostgreSQL evidence application"): the
/// current Attempt, its owning JobStep, and the owning Job (including every
/// ordered JobStep, needed to decide whether a `Succeeded` JobStep completes
/// the Job) — locked in that stable order (Attempt -> JobStep -> Job).
pub struct ActionEvidenceLockedFacts {
    pub job: Job,
    pub job_step: JobStep,
    pub attempt: Attempt,
}

/// A pure decision over freshly locked [`ActionEvidenceLockedFacts`],
/// mirroring [`FinalDispatchDecision`]: the Adapter locks and reads current
/// state, this closure decides (calling
/// `bamep_domain::apply_action_evidence` and, only for a terminal outcome,
/// constructing the required [`AuditRecord`] — mirroring how
/// `FinalDispatchDecision` builds [`FinalDispatchCommit`]'s audit), and the
/// Adapter persists the result atomically in the same transaction.
pub type ApplyActionEvidenceDecision =
    Box<dyn FnOnce(ActionEvidenceLockedFacts) -> ApplyActionEvidenceDecisionOutcome + Send>;

/// A successful [`ApplyActionEvidenceDecision`] "Applied" result: the Domain
/// outcome plus the immutable terminal [`AuditRecord`] that must commit
/// atomically alongside it when `outcome.terminal` is `true` — `None` when
/// `outcome.terminal` is `false` (the `AckAccepted` `Dispatched ->
/// InProgress` transition, which requires no audit record).
pub struct ActionEvidenceCommit {
    pub outcome: ActionEvidenceApplied,
    pub audit: Option<AuditRecord>,
}

/// The three outcomes [`ApplyActionEvidenceDecision`] may produce, mirroring
/// `bamep_domain::ActionEvidenceOutcome` at the Application boundary (the
/// `Applied` case additionally carries the audit record Domain itself never
/// constructs). Crosses one evidence application at a time, never a hot
/// per-message path (`ActionProgress`, the actually high-frequency message,
/// never reaches this type at all), mirroring
/// `bamep_domain::action_evidence::ActionEvidenceOutcome`'s identical
/// allowance.
#[allow(clippy::large_enum_variant)]
pub enum ApplyActionEvidenceDecisionOutcome {
    Applied(ActionEvidenceCommit),
    NoOp,
    Conflict,
}

/// The result of [`JobRepository::apply_action_evidence`] after successful
/// resolution/locking/correlation — mirrors [`ApplyActionEvidenceDecisionOutcome`]
/// without the audit record, which is a persistence-only concern.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ApplyActionEvidenceResult {
    Applied(ActionEvidenceApplied),
    NoOp,
    Conflict,
}

/// Errors from [`JobRepository::apply_action_evidence`].
#[derive(Debug, thiserror::Error)]
pub enum ApplyActionEvidenceError {
    /// Unknown `action_id`, or a known `action_id` belonging to a Job that
    /// does not target the authenticated Endpoint — deliberately one
    /// generic value so a caller can never learn which case occurred.
    #[error("unknown action")]
    UnknownAction,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Durable facts read under lock immediately before the final destructive-
/// dispatch decision (Issue #25 "Commit-time revalidation/locking"):
/// everything [`bamep_domain::evaluate_final_destructive_dispatch`] needs
/// except the transient Runtime Presence Registry / `TargetRevalidationPort`
/// reads and "now", which [`FinalDispatchDecision`] performs/obtains itself
/// when the Adapter invokes it — always after every lock represented here was
/// already acquired, never before.
pub struct FinalDispatchLockedFacts {
    /// The owning Job, including every ordered `JobStep`, locked and freshly
    /// read in the same transaction that may later persist this decision.
    pub job: Job,
    /// The Job's owning Endpoint, locked and freshly read in the same
    /// transaction — its `identity`, `credential`, `hardware_confidence`, and
    /// `current_boot` fields are the durable halves of destructive
    /// preconditions 1, 2, 6, and 7.
    pub endpoint: EndpointAggregate,
    /// Whether an `Attempt` already exists for the candidate JobStep in a
    /// non-terminal state, read under lock so a concurrent dispatch
    /// commitment cannot race past it.
    pub existing_active_attempt: bool,
    /// The Endpoint's current durable inventory revision id, read from the
    /// same locked `endpoints` row — `None` when no inventory has ever been
    /// recorded.
    pub current_inventory_revision_id: Option<InventoryRevisionId>,
}

/// A pure decision over freshly locked [`FinalDispatchLockedFacts`],
/// producing the durable commitment to persist — the advanced `JobStep` +
/// `Attempt` plus the audit record that must commit atomically with them —
/// or the reason final dispatch is not currently authorized
/// (`bamep_domain::evaluate_final_destructive_dispatch`; Issue #25). Mirrors
/// [`AdmitJobDecision`]/[`SatisfyStepPreconditionsDecision`]: the Adapter
/// locks and reads current state, this closure decides (performing its own
/// transient Runtime Presence Registry / `TargetRevalidationPort` reads and
/// obtaining "now" only once invoked, i.e. after every lock is held), and the
/// Adapter persists the result atomically in the same transaction.
pub type FinalDispatchDecision = Box<
    dyn FnOnce(FinalDispatchLockedFacts) -> Result<FinalDispatchCommit, FinalDispatchDenial> + Send,
>;

/// A successful [`FinalDispatchDecision`] result: the Domain outcome plus the
/// immutable destructive-dispatch [`AuditRecord`] that must commit
/// atomically alongside it (`m0-persistence-observability-and-domain-events.md`
/// "Auditability"). Assembled by the Application-level decision closure, not
/// by Domain itself — `bamep_domain::final_dispatch` intentionally has no
/// audit/event concept, and no new `DomainEvent` is introduced for this
/// commitment.
pub struct FinalDispatchCommit {
    pub outcome: FinalDispatchOutcome,
    pub audit: AuditRecord,
}

/// Errors from [`JobRepository::commit_destructive_dispatch`].
#[derive(Debug, thiserror::Error)]
pub enum CommitDestructiveDispatchError {
    #[error("job {0:?} not found")]
    JobNotFound(JobId),
    /// Final revalidation rejected the candidate JobStep
    /// (`bamep_domain::FinalDispatchRejection`). The caller's Job/JobStep
    /// durable state reflects exactly the `FinalDispatchDenial` the Domain
    /// decision returned — this is not itself a repository failure.
    #[error("final dispatch was not authorized: {0}")]
    Rejected(FinalDispatchRejection),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Durable facts read under lock immediately before one final non-
/// destructive transfer-dispatch decision (Issue #40 "Commit non-
/// destructive transfer Attempts for dispatch"): everything
/// `bamep_domain::evaluate_transfer_dispatch` needs. Structurally distinct
/// from [`FinalDispatchLockedFacts`] — it carries no `EndpointAggregate`
/// and no transient Runtime Presence Registry / `TargetRevalidationPort`
/// evidence, so the destructive gate cannot be reached from this type.
pub struct TransferDispatchLockedFacts {
    /// The owning Job, including every ordered `JobStep`, locked and freshly
    /// read in the same transaction that may later persist this decision.
    pub job: Job,
    /// Whether an `Attempt` already exists for the candidate JobStep in a
    /// non-terminal state, read under lock so a concurrent dispatch
    /// commitment cannot race past it.
    pub existing_active_attempt: bool,
    /// The durable pre-dispatch `Transfer` (Issue #36), locked and freshly
    /// read in the same transaction.
    pub transfer: bamep_domain::Transfer,
}

/// A pure decision over freshly locked [`TransferDispatchLockedFacts`],
/// producing the durable commitment to persist — the advanced `JobStep` +
/// `Attempt` + bound `Transfer` — or the reason final dispatch is not
/// currently authorized (`bamep_domain::evaluate_transfer_dispatch`; Issue
/// #40). Mirrors [`FinalDispatchDecision`]: the Adapter locks and reads
/// current state, this closure decides, and the Adapter persists the result
/// atomically in the same transaction. Unlike [`FinalDispatchDecision`],
/// this closure requires no audit record — no destructive-dispatch audit
/// is required for this non-destructive commitment.
pub type TransferDispatchDecision = Box<
    dyn FnOnce(
            TransferDispatchLockedFacts,
        ) -> Result<
            bamep_domain::TransferDispatchOutcome,
            bamep_domain::TransferDispatchDenial,
        > + Send,
>;

/// Errors from [`JobRepository::commit_transfer_dispatch`].
#[derive(Debug, thiserror::Error)]
pub enum CommitTransferDispatchError {
    #[error("job {0:?} not found")]
    JobNotFound(JobId),
    /// No Transfer with this `TransferId` was ever created (Issue #36).
    #[error("transfer {0:?} not found")]
    TransferNotFound(bamep_domain::TransferId),
    /// Final revalidation rejected the candidate JobStep
    /// (`bamep_domain::TransferDispatchRejection`). The caller's Job/JobStep
    /// durable state reflects exactly the `TransferDispatchDenial` the
    /// Domain decision returned — this is not itself a repository failure.
    #[error("transfer dispatch was not authorized: {0}")]
    Rejected(bamep_domain::TransferDispatchRejection),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Persistence for newly issued `BootContext`s (ADR-0014 point 11
/// "Persist-before-deliver issuance ordering"). Kept as its own narrow Port,
/// separate from [`EndpointRepository`]: BootContext issuance is not an
/// Endpoint aggregate operation — no Endpoint exists yet for an unresolved
/// enrollment credential.
#[async_trait]
pub trait BootContextRepository: Send + Sync {
    /// Durably inserts a newly issued, not-yet-persisted `BootContext`. A
    /// successful return means the row has committed — the caller (a future
    /// Application issuance sequence) may then deliver the enrollment
    /// credential to the booting Endpoint, per the persist-before-deliver
    /// ordering this Port exists to support.
    ///
    /// Rejects, rather than overwrites, a `boot_context_id` that already
    /// exists: `boot_context_id` is randomly generated per issuance, so a
    /// collision indicates a caller error, not a legitimate re-issuance.
    async fn insert_boot_context(&self, context: &BootContext) -> Result<(), RepositoryError>;
}

/// The routed target a presented credential's lookup identifier resolved to
/// (ADR-0014 point 7 "Safe AuthRequest routing"), handed to
/// [`RedemptionDecision`] only after the Adapter has acquired every lock the
/// chosen path requires.
///
/// An already-resolved `BootContext` is deliberately not its own variant: it
/// is routed to, and locked as, its resolved [`Endpoint`](RedemptionTarget::Endpoint)
/// instead of introducing a permanent fifth business-state variant (ADR-0014
/// checkpoint "New Port").
pub enum RedemptionTarget {
    /// The presented credential's lookup id resolved (directly, or through
    /// an already-resolved `BootContext`) to this locked, currently-persisted
    /// Endpoint.
    Endpoint(EndpointAggregate),
    /// The presented credential is an unresolved boot-scoped enrollment
    /// credential. `candidate_endpoint` is `Some` only when
    /// `context.inventory_signal()` correlated to an existing Endpoint under
    /// lock (a genuine-reboot candidate); `None` means first contact.
    UnresolvedBootContext {
        context: BootContext,
        candidate_endpoint: Option<EndpointAggregate>,
    },
    /// The presented credential is `Enrollment`-kinded, but no `BootContext`
    /// row exists for its lookup id.
    UnknownBootContext,
    /// The presented credential's lookup id has no persisted mapping and is
    /// not `Enrollment`-kinded — a `Runtime` credential never falls through
    /// to a `BootContext` lookup (ADR-0014 point 7).
    UnknownCredential,
}

/// A pure decision over a routed [`RedemptionTarget`], producing the
/// transition to persist or a rejection. `Err` is reserved for the
/// [`BootContextResolveError`] Domain invariant failure
/// (`bamep_domain::boot_context::BootContext::resolve`) — never a normal
/// authentication rejection, which is `Ok(RedeemOutcome::Rejected)` instead.
pub type RedemptionDecision =
    Box<dyn FnOnce(RedemptionTarget) -> Result<RedeemOutcome, BootContextResolveError> + Send>;

/// Agent credential redemption (ADR-0012; ADR-0014). Deliberately separate
/// from [`EndpointRepository`]: routing a presented credential may resolve to
/// an Endpoint, an unresolved `BootContext`, or nothing at all, and the
/// concrete PostgreSQL routing/locking algorithm
/// (`crate::adapters::postgres` module docs) is materially different from
/// `EndpointRepository`'s Endpoint-only operations.
///
/// The Port operation receives only `kind`, `lookup_id`, and `decide` — never
/// the presented credential's secret or its full wire value. The Adapter can
/// therefore never inspect secret material; the Application captures the
/// presented credential inside `decide`'s closure and only Domain code
/// (invoked from within that closure) ever verifies it.
#[async_trait]
pub trait CredentialRedemptionRepository: Send + Sync {
    /// Routes `(kind, lookup_id)` to a [`RedemptionTarget`] under the lock
    /// order `crate::adapters::postgres` documents, invokes `decide`, and —
    /// only for an `Ok(RedeemOutcome::Established { .. })` decision — persists
    /// the resulting state, domain events, audit record, final credential
    /// lookup projection, and (when applicable) `BootContext.resolved_endpoint_id`
    /// atomically in the same transaction. `Ok(RedeemOutcome::Rejected)`
    /// persists nothing.
    ///
    /// If `decide` returns an accepted transition for
    /// [`RedemptionTarget::UnknownBootContext`] or
    /// [`RedemptionTarget::UnknownCredential`], that is an internal
    /// contract/invariant violation, not a legitimate state to persist — the
    /// Adapter rejects it with [`RepositoryError::Backend`] rather than
    /// silently creating state.
    async fn redeem(
        &self,
        kind: CredentialKind,
        lookup_id: &CredentialLookupId,
        decide: RedemptionDecision,
    ) -> Result<RedeemOutcome, RepositoryError>;
}

/// Independent target-disk revalidation evidence for destructive-operation
/// precondition 5 (`m0-endpoint-identity-lifecycle.md` "Destructive-operation
/// authorization preconditions"; `m0-simulator-contract-and-validation-strategy.md`
/// "Target-disk revalidation fidelity boundary"). Deliberately separate from
/// [`InventoryRepository`]: the value this Port returns must never be derived
/// from inventory-revision equality, so a later Work Package's preconditions
/// 4 and 5 remain independently testable/failable. M1 implementations are
/// deterministic fixtures only (`crate::adapters::target_revalidation_fixture`)
/// — no physical disk probe, no PostgreSQL persistence, no Agent Protocol
/// message.
pub trait TargetRevalidationPort: Send + Sync {
    /// The currently observed/revalidated target-disk fingerprint for
    /// `endpoint_id`, or `None` when no target evidence is currently
    /// available for it.
    fn current_target_fingerprint(&self, endpoint_id: EndpointId) -> Option<TargetFingerprint>;
}

/// Outbound authenticated-session `ActionDispatch` delivery (Issue #26
/// "Outbound authenticated session delivery"). Application depends on this
/// Port, never on `tokio-tungstenite`/WebSocket types directly
/// (`m0-stack-and-boundaries-baseline.md` "Dependency constraints").
///
/// A successful [`dispatch_action`](Self::dispatch_action) means only that
/// the local transport accepted the frame for the selected authenticated
/// session — never Agent receipt, `ActionAck`, execution, or success. The
/// concrete Runtime Service implementing this Port
/// (`crate::runtime::outbound_sessions::OutboundSessionDirectory`) selects
/// exactly one live session per Endpoint (the most recently registered),
/// never fans out, and never falls back to another overlapping session after
/// one send attempt.
#[async_trait]
pub trait AgentDispatchPort: Send + Sync {
    async fn dispatch_action(
        &self,
        endpoint_id: EndpointId,
        dispatch: bamep_agent_protocol::ActionDispatchMessage,
    ) -> Result<(), AgentDispatchError>;

    /// Transmits `CancelAction{action_id}` for the already-existing action —
    /// never a replacement identity (Issue #27 "Reuse the existing outbound
    /// session path"). A successful return means only that the local
    /// transport accepted the frame, exactly like
    /// [`Self::dispatch_action`]'s identical caveat.
    async fn cancel_action(
        &self,
        endpoint_id: EndpointId,
        cancel: bamep_agent_protocol::CancelActionMessage,
    ) -> Result<(), AgentDispatchError>;

    /// Transmits `StatusQuery{action_id}` for the already-existing
    /// `AwaitingReconciliation` action — never a replacement identity and
    /// never an `ActionDispatch` retry (Issue #28 "Outbound status query").
    /// A successful return means only that the local transport accepted the
    /// frame, exactly like [`Self::dispatch_action`]'s identical caveat.
    async fn status_query(
        &self,
        endpoint_id: EndpointId,
        query: bamep_agent_protocol::StatusQueryMessage,
    ) -> Result<(), AgentDispatchError>;
}

/// Errors from [`AgentDispatchPort::dispatch_action`]. None of these prove
/// non-delivery, execution failure, or Agent non-receipt — they only
/// describe why the local transport could not accept the frame at all
/// (`m0-agent-protocol-contract.md` "Idempotency, retry, and uncertain
/// delivery": "Failure to receive `ActionAck` is an uncertain delivery
/// outcome"). The caller must leave the Attempt durably `Dispatched` on any
/// of these — #26 never creates another Attempt, marks the Attempt
/// terminal, or redispatches merely because of a send failure; #28 owns
/// subsequent uncertain-delivery reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AgentDispatchError {
    #[error("no authenticated session is currently registered for this endpoint")]
    NoSession,
    #[error("the authenticated session's outbound channel is gone")]
    ChannelClosed,
    #[error("the local websocket send failed")]
    SendFailed,
}

/// Durable facts read under lock immediately before one Transfer/Artifact/
/// manifest decision (Issue #36 "Concurrency / atomicity"): the current
/// `Transfer`, its owning `Artifact`, its `ChunkManifest`, and the set of
/// chunk indices currently durably held/verified. `held_chunk_indices`
/// reflects only durable acceptance — never Worker-local transient state.
pub struct TransferLockedFacts {
    pub transfer: Transfer,
    pub artifact: Artifact,
    pub manifest: bamep_domain::ChunkManifest,
    pub held_chunk_indices: BTreeSet<ChunkIndex>,
}

/// A pure decision over freshly locked [`TransferLockedFacts`], mirroring
/// [`FinalDispatchDecision`]/[`AdmitJobDecision`]: the Adapter locks and
/// reads current state, this closure decides (calling
/// `bamep_domain::ChunkManifest::record_expected_chunk`), and the Adapter
/// persists the result atomically in the same transaction.
pub type RecordChunkDecision =
    Box<dyn FnOnce(&TransferLockedFacts) -> Result<ChunkRecordOutcome, ChunkRecordError> + Send>;

/// A pure decision over freshly locked [`TransferLockedFacts`] for one
/// verified-chunk acceptance, calling
/// `bamep_domain::validate_verified_chunk`. This closure never itself marks
/// a chunk held — it only validates that the already independently verified
/// bytes match the durable expected identity; the Adapter performs the
/// idempotent durable `held` write only when this returns `Ok`.
pub type AcceptChunkDecision =
    Box<dyn FnOnce(&TransferLockedFacts) -> Result<(), ChunkAcceptError> + Send>;

/// A pure decision over freshly locked [`TransferLockedFacts`], calling
/// `bamep_domain::ChunkManifest::seal`.
pub type SealManifestDecision =
    Box<dyn FnOnce(&TransferLockedFacts) -> Result<SealOutcome, SealError> + Send>;

/// A pure decision over freshly locked [`TransferLockedFacts`], calling one
/// of `bamep_domain::begin_verification`/`complete_verification`/
/// `fail_incomplete`. Shared by every Artifact-lifecycle Port method below —
/// each supplies the Domain call appropriate to its own transition.
pub type ArtifactTransitionDecision =
    Box<dyn FnOnce(&TransferLockedFacts) -> Result<Artifact, ArtifactTransitionError> + Send>;

/// A pure decision over freshly locked [`TransferLockedFacts`], calling
/// `bamep_domain::bind_attempt`.
pub type BindAttemptDecision =
    Box<dyn FnOnce(&TransferLockedFacts) -> Result<Transfer, TransferBindingError> + Send>;

/// Errors from [`TransferRepository::create_transfer_context`]. Distinct
/// from [`CreateWorkflowError`]: this verifies Job/JobStep correlation, not
/// Endpoint enrollment state — a Transfer's eligibility beyond structural
/// correlation belongs to a later Work Package (#40).
#[derive(Debug, thiserror::Error)]
pub enum CreateTransferError {
    #[error("endpoint {0:?} not found")]
    EndpointNotFound(EndpointId),
    #[error("job {0:?} not found")]
    JobNotFound(JobId),
    #[error("job {0:?} does not target endpoint {1:?}")]
    JobEndpointMismatch(JobId, EndpointId),
    #[error("job step {0:?} not found in job {1:?}")]
    JobStepNotFound(JobStepId, JobId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

#[derive(Debug, thiserror::Error)]
pub enum RecordChunkError {
    #[error("transfer {0:?} not found")]
    TransferNotFound(TransferId),
    #[error(transparent)]
    Domain(#[from] ChunkRecordError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// The result of [`TransferRepository::accept_verified_chunk`] after
/// successful validation — distinguishes a genuinely new durable acceptance
/// from an idempotent resubmission of an already-held chunk (Issue #36
/// "Concurrency / atomicity": "duplicate verified-held acceptance").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptChunkOutcome {
    Accepted,
    AlreadyHeld,
}

#[derive(Debug, thiserror::Error)]
pub enum AcceptChunkError {
    #[error("transfer {0:?} not found")]
    TransferNotFound(TransferId),
    #[error(transparent)]
    Domain(#[from] ChunkAcceptError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// The durable outcome of one authorized `ChunkAcceptanceRequest`
/// (`m1-worker-data-plane-control-contract.md` "Verified-chunk durable
/// acceptance"; Issue #39 Phase C1). `FailClosed` is **not** a wire outcome —
/// `bamepd` sends no `ChunkAcceptanceDecision` and the Worker fails the HTTP
/// request closed. It covers a contract-violating follow-up (a `size` outside
/// the manifest bound, a non-canonical `digest`, or a `size` contradicting the
/// size already durable for an identical-digest `chunk_index`) that the
/// Specification's closed `rejected` vocabulary (`chunk_identity_conflict`,
/// `transfer_not_continuable`) deliberately does not describe, and that must
/// never become an enumerable reason (Issue #39 Phase C1 item 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkAcceptanceCommit {
    Committed,
    AlreadyCommitted,
    RejectedConflict,
    RejectedNotContinuable,
    FailClosed,
}

/// What the Application decided one authorized `ChunkAcceptanceRequest`
/// should durably do, over freshly locked [`TransferLockedFacts`] plus the
/// owning `Attempt` (both locked in the same transaction). The Adapter turns
/// this into the atomic persistence and the [`ChunkAcceptanceCommit`] it
/// reports — it never makes this decision itself.
pub enum ChunkAcceptanceDecided {
    /// Record the expected identity if new and mark it durably held, in one
    /// transaction. Carries the Domain outcome
    /// (`bamep_domain::ChunkManifest::record_expected_chunk`) so the Adapter
    /// knows whether to insert a new identity row (`Added`) or only flip
    /// `held` on an existing one (`AlreadyRecorded`).
    Commit(ChunkRecordOutcome),
    /// A *different* digest is already durable for this `chunk_index`.
    RejectConflict,
    /// The owning Transfer/Artifact/Attempt is terminal, or the manifest is
    /// sealed and this `chunk_index` was never part of the sealed set.
    RejectNotContinuable,
    /// The request is not a legal follow-up at all (see
    /// [`ChunkAcceptanceCommit::FailClosed`]).
    FailClosed,
}

pub type CommitChunkAcceptanceDecision =
    Box<dyn FnOnce(&TransferLockedFacts, Option<&Attempt>) -> ChunkAcceptanceDecided + Send>;

#[derive(Debug, thiserror::Error)]
pub enum CommitChunkAcceptanceError {
    #[error("transfer {0:?} not found")]
    TransferNotFound(TransferId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

// ---------------------------------------------------------------------
// Atomic manifest seal (Issue #39 Phase C2)
// (`m1-worker-data-plane-control-contract.md` "Seal-manifest first durable
// commit"; `m0-data-plane-and-storage-contracts.md` "Durable chunk
// acceptance ordering")
// ---------------------------------------------------------------------

/// The authoritative durable sealed-manifest facts a committed
/// (`sealed` / `already_pending_verification`) seal decision carries back to
/// the Worker (`m1-worker-data-plane-control-contract.md` "Seal-manifest
/// first durable commit": "the **authoritative durable sealed values**").
/// Every field is sourced from the locked durable snapshot the seal
/// transaction produced or confirmed — never echoed from the Agent's request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedManifestDurableFacts {
    pub artifact_id: ArtifactId,
    pub digest_algorithm: DigestAlgorithm,
    pub chunk_size: u32,
    pub chunk_count: u64,
    /// Canonical base64url-no-pad.
    pub expected_artifact_digest: String,
}

/// What the Application decided one `ManifestSealRequest` should durably do,
/// over the freshly locked [`AuthorizationDurableState`] (Transfer, Artifact,
/// owning Attempt, Endpoint credential, manifest, held chunks — all in the
/// same transaction the seal commits in). The Adapter turns this into the
/// atomic persistence and the [`ManifestSealCommit`] it reports; it never
/// makes this decision itself (ADR-0018).
pub enum ManifestSealDecided {
    /// First valid seal: atomically persist the sealed manifest facts
    /// (`chunk_count`, `artifact_digest`, `sealed = true`) **and**
    /// `Artifact Incomplete -> PendingVerification` in one transaction. Both
    /// `bamep_domain::ChunkManifest::seal` and
    /// `bamep_domain::begin_verification` have already succeeded against the
    /// locked snapshot inside the closure — the Adapter only persists.
    Seal {
        chunk_count: u32,
        artifact_digest: Digest,
    },
    /// Idempotent crash-recovery retry: durable state is already
    /// `manifest sealed` + `Artifact PendingVerification` with the identical
    /// `(transfer_id, chunk_count, artifact_digest)`. No second seal
    /// transition, no Artifact transition.
    AlreadyPending,
    /// `bamepd` does not durably hold every chunk `0..chunk_count-1`
    /// individually verified (or the recorded identity set is
    /// incomplete/non-contiguous) — `rejected { incomplete_manifest }`. No
    /// durable seal; Artifact stays `Incomplete`.
    RejectIncomplete,
    /// The manifest is already sealed with a *different*
    /// `chunk_count`/`artifact_digest` — `rejected { manifest_already_sealed }`.
    /// The original sealed tuple is never rewritten.
    RejectAlreadySealed,
    /// The authorization check failed (wrong Endpoint/Artifact/Attempt, an
    /// Attempt that is not `InProgress`, a revoked credential, or a terminal
    /// Artifact) — generic non-enumerable `denied`, leaking no terminal-state
    /// detail (`m1-worker-data-plane-control-contract.md`: a terminal owning
    /// Transfer/Artifact/Attempt is a `denied`, never a `409`).
    Denied,
    /// Not a legal seal at all — a non-canonical `artifact_digest`, or an
    /// internally contradictory durable state (`sealed` manifest with an
    /// `Incomplete` Artifact, or vice versa). No `ManifestSealDecision` is
    /// sent; `bamepd` logs the invariant violation and the Worker fails the
    /// HTTP request closed. Never mapped to an enumerable `rejected` reason
    /// (Issue #39 Phase C2).
    FailClosed,
}

pub type CommitManifestSealDecision =
    Box<dyn FnOnce(&AuthorizationDurableState) -> ManifestSealDecided + Send>;

/// The durable outcome of one `ManifestSealRequest`
/// (`m1-worker-data-plane-control-contract.md` "Seal-manifest first durable
/// commit"; Issue #39 Phase C2). `FailClosed` is **not** a wire outcome — see
/// [`ManifestSealDecided::FailClosed`].
pub enum ManifestSealCommit {
    Sealed(SealedManifestDurableFacts),
    AlreadyPending(SealedManifestDurableFacts),
    RejectedIncomplete,
    RejectedAlreadySealed,
    Denied,
    FailClosed,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitManifestSealError {
    #[error("transfer {0:?} not found")]
    TransferNotFound(TransferId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

// ---------------------------------------------------------------------
// Atomic full-Artifact verification commit (Issue #39 Phase C2)
// ---------------------------------------------------------------------

/// What the Application decided one `ArtifactVerificationReport` should
/// durably do, over the freshly locked [`TransferLockedFacts`]. The digest
/// comparison itself (`bamep_domain::complete_verification`) has already run
/// inside the closure against `bamepd`'s **own durable** expected digest —
/// never the value the transient `verification_handle` carried
/// (`m1-worker-data-plane-control-contract.md` "Full-Artifact verification
/// result": "`bamepd` **independently** compares").
pub enum ArtifactVerificationDecided {
    /// Persist `PendingVerification -> Verified | Failed` (carried on the
    /// `Artifact`).
    Commit(Artifact),
    /// The report cannot be committed — the Artifact is not
    /// `PendingVerification`, the manifest is not sealed, or the consumed
    /// `verification_handle`'s bound identity does not exactly match current
    /// durable state. No `ArtifactVerificationAck`, no mutation; a fresh seal
    /// retry re-drives verification.
    FailClosed,
}

pub type CommitArtifactVerificationDecision =
    Box<dyn FnOnce(&TransferLockedFacts) -> ArtifactVerificationDecided + Send>;

/// The durable outcome of one `ArtifactVerificationReport`
/// (`m1-worker-data-plane-control-contract.md` "Full-Artifact verification
/// result"; Issue #39 Phase C2). Both `Verified` and `Failed` are
/// **successful** verification commits; only `FailClosed` produces no `Ack`.
pub enum ArtifactVerificationCommit {
    /// `true` -> `Verified`, `false` -> `Failed`.
    Committed {
        verified: bool,
    },
    FailClosed,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitArtifactVerificationError {
    #[error("transfer {0:?} not found")]
    TransferNotFound(TransferId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

#[derive(Debug, thiserror::Error)]
pub enum SealManifestError {
    #[error("transfer {0:?} not found")]
    TransferNotFound(TransferId),
    #[error(transparent)]
    Domain(#[from] SealError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Errors shared by every Artifact-lifecycle Port method below (Issue #36:
/// `begin_artifact_verification`/`complete_artifact_verification`/
/// `fail_incomplete_artifact`).
#[derive(Debug, thiserror::Error)]
pub enum ArtifactTransitionRepoError {
    #[error("transfer {0:?} not found")]
    TransferNotFound(TransferId),
    #[error(transparent)]
    Domain(#[from] ArtifactTransitionError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

#[derive(Debug, thiserror::Error)]
pub enum BindAttemptError {
    #[error("transfer {0:?} not found")]
    TransferNotFound(TransferId),
    #[error(transparent)]
    Domain(#[from] TransferBindingError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

/// Durable Transfer/Artifact/ChunkManifest metadata persistence (Issue #36
/// "Persist Transfer, Artifact, and ChunkManifest lifecycle";
/// `m0-data-plane-and-storage-contracts.md`). Never persists bulk Artifact
/// bytes — those remain later Worker/storage work (#39). Every mutating
/// method here locks exactly the named `Transfer` (and its Artifact/
/// manifest/held-chunk facts) before invoking its `decide` closure, mirroring
/// [`JobRepository`]'s lock/decide/persist discipline, so Domain remains the
/// sole owner of transition/business-rule decisions.
#[async_trait]
pub trait TransferRepository: Send + Sync {
    /// Verifies `context.transfer.job_id`/`job_step_id` correlate to an
    /// existing JobStep for an existing Job targeting
    /// `context.transfer.endpoint_id`, then atomically persists the
    /// already-constructed pre-dispatch `Transfer`/`Artifact`/empty
    /// `ChunkManifest` (Issue #36 "Pre-dispatch creation"). Never creates an
    /// Attempt or action identity, never transitions the JobStep, never
    /// evaluates the destructive-operation gate.
    async fn create_transfer_context(
        &self,
        context: &TransferContext,
    ) -> Result<(), CreateTransferError>;

    /// Read-only reload of a persisted `Transfer`/`Artifact`/`ChunkManifest`
    /// plus its currently durably held/verified chunk indices, for
    /// verification/reporting and restart/recovery (Issue #36 "Reload /
    /// restart"). `None` when no Transfer with `transfer_id` was ever
    /// created.
    async fn find_transfer_context(
        &self,
        transfer_id: TransferId,
    ) -> Result<Option<(TransferContext, BTreeSet<ChunkIndex>)>, RepositoryError>;

    /// Locks exactly the `Transfer` identified by `transfer_id`, invokes
    /// `decide` with its freshly locked [`TransferLockedFacts`], and — only
    /// for `Ok(ChunkRecordOutcome::Added(..))` — atomically persists the new
    /// expected chunk-identity row in the same transaction.
    /// `Ok(ChunkRecordOutcome::AlreadyRecorded)` persists nothing
    /// (idempotent no-op).
    async fn record_expected_chunk(
        &self,
        transfer_id: TransferId,
        decide: RecordChunkDecision,
    ) -> Result<ChunkRecordOutcome, RecordChunkError>;

    /// Locks exactly the `Transfer` identified by `transfer_id`, invokes
    /// `decide` with its freshly locked [`TransferLockedFacts`], and — only
    /// on `Ok(())` — atomically marks `index` durably held/verified in the
    /// same transaction. Reports [`AcceptChunkOutcome::AlreadyHeld`] without
    /// a second write when `index` is already held under the same lock.
    async fn accept_verified_chunk(
        &self,
        transfer_id: TransferId,
        index: ChunkIndex,
        decide: AcceptChunkDecision,
    ) -> Result<AcceptChunkOutcome, AcceptChunkError>;

    /// Locks exactly the `Transfer` identified by `transfer_id` **and its
    /// owning `Attempt`** in one transaction, invokes `decide` with the
    /// freshly locked [`TransferLockedFacts`] plus that `Attempt`, and
    /// atomically persists the durable chunk acceptance the closure decided —
    /// recording a new expected identity as first-writer and/or marking the
    /// `chunk_index` durably held — before committing (Issue #39 Phase C1;
    /// `m1-worker-data-plane-control-contract.md` "Verified-chunk durable
    /// acceptance"; `m0-data-plane-and-storage-contracts.md` "Durable chunk
    /// acceptance ordering" step 6–7). The `chunk_index` and its verified
    /// `size`/`digest` are captured by `decide`; `index` is passed
    /// separately only so the Adapter can address the `held` write. A lost
    /// response after this commits is recovered by a fresh retry reaching
    /// `AlreadyCommitted`, never a second commit.
    async fn commit_chunk_acceptance(
        &self,
        transfer_id: TransferId,
        index: ChunkIndex,
        decide: CommitChunkAcceptanceDecision,
    ) -> Result<ChunkAcceptanceCommit, CommitChunkAcceptanceError>;

    /// Locks the `Transfer`, its owning `Attempt`, its `Endpoint`/credential,
    /// and its Artifact/manifest/held-chunk facts **together in one
    /// transaction**, assembles the [`AuthorizationDurableState`] snapshot,
    /// invokes `decide` with it, and — only for
    /// [`ManifestSealDecided::Seal`] — atomically persists the sealed manifest
    /// facts *and* `Artifact Incomplete -> PendingVerification` before
    /// committing once (Issue #39 Phase C2;
    /// `m1-worker-data-plane-control-contract.md` "Seal-manifest first durable
    /// commit"; `m0-data-plane-and-storage-contracts.md` "Durable chunk
    /// acceptance ordering"). The current durable authorization facts are read
    /// in the *same* transaction as the seal so no authorization -> mutation
    /// TOCTOU exists. [`ManifestSealDecided::AlreadyPending`] and every
    /// rejection persist nothing.
    async fn commit_manifest_seal(
        &self,
        transfer_id: TransferId,
        decide: CommitManifestSealDecision,
    ) -> Result<ManifestSealCommit, CommitManifestSealError>;

    /// Locks exactly the `Transfer` identified by `transfer_id` and its
    /// Artifact/manifest/held-chunk facts, invokes `decide` with the freshly
    /// locked [`TransferLockedFacts`] (so it can revalidate the consumed
    /// `verification_handle`'s bound sealed identity against current durable
    /// state and compare the Worker-computed digest against the durable
    /// expected digest), and — only for
    /// [`ArtifactVerificationDecided::Commit`] — atomically persists
    /// `PendingVerification -> Verified | Failed` in the same transaction
    /// (Issue #39 Phase C2; `m1-worker-data-plane-control-contract.md`
    /// "Full-Artifact verification result"). Distinct from the lower-level
    /// #36 [`Self::complete_artifact_verification`] primitive, which cannot
    /// express the fail-closed binding-revalidation branch.
    async fn commit_artifact_verification(
        &self,
        transfer_id: TransferId,
        decide: CommitArtifactVerificationDecision,
    ) -> Result<ArtifactVerificationCommit, CommitArtifactVerificationError>;

    /// Locks exactly the `Transfer` identified by `transfer_id`, invokes
    /// `decide` with its freshly locked [`TransferLockedFacts`], and — only
    /// for `Ok(SealOutcome::Sealed(..))` — atomically persists the sealed
    /// manifest facts (`chunk_count`, `artifact_digest`, `sealed = true`) in
    /// the same transaction. `Ok(SealOutcome::AlreadySealed)` persists
    /// nothing.
    async fn seal_manifest(
        &self,
        transfer_id: TransferId,
        decide: SealManifestDecision,
    ) -> Result<SealOutcome, SealManifestError>;

    /// Locks exactly the `Transfer` identified by `transfer_id`, invokes
    /// `decide` (`bamep_domain::begin_verification`), and — only on `Ok` —
    /// atomically persists `Incomplete -> PendingVerification` in the same
    /// transaction.
    async fn begin_artifact_verification(
        &self,
        transfer_id: TransferId,
        decide: ArtifactTransitionDecision,
    ) -> Result<Artifact, ArtifactTransitionRepoError>;

    /// Locks exactly the `Transfer` identified by `transfer_id`, invokes
    /// `decide` (`bamep_domain::complete_verification`), and — only on `Ok`
    /// — atomically persists `PendingVerification -> Verified | Failed` in
    /// the same transaction.
    async fn complete_artifact_verification(
        &self,
        transfer_id: TransferId,
        decide: ArtifactTransitionDecision,
    ) -> Result<Artifact, ArtifactTransitionRepoError>;

    /// Locks exactly the `Transfer` identified by `transfer_id`, invokes
    /// `decide` (`bamep_domain::fail_incomplete`), and — only on `Ok` —
    /// atomically persists `Incomplete -> Failed` in the same transaction.
    async fn fail_incomplete_artifact(
        &self,
        transfer_id: TransferId,
        decide: ArtifactTransitionDecision,
    ) -> Result<Artifact, ArtifactTransitionRepoError>;

    /// Locks exactly the `Transfer` identified by `transfer_id`, invokes
    /// `decide` (`bamep_domain::bind_attempt`), and — only on `Ok` —
    /// atomically persists the one-time Transfer -> Attempt binding (Issue
    /// #36 "Transfer -> Attempt binding support"). This Port method opens
    /// and commits its own transaction, sufficient for #36's own scope. A
    /// later dispatch boundary (#40) that must commit this binding
    /// atomically alongside its own JobStep/Attempt commitment composes the
    /// lower-level `adapters::postgres::transfer_repository` locking/
    /// persistence primitives directly into its own transaction instead of
    /// calling this method — see that module's docs.
    async fn bind_attempt(
        &self,
        transfer_id: TransferId,
        decide: BindAttemptDecision,
    ) -> Result<Transfer, BindAttemptError>;
}

/// The complete current durable authorization-relevant state for one
/// Transfer, read consistently in a single bounded transaction (Issue #38
/// "PostgreSQL transaction/repository composition": "Transfer, Attempt,
/// Endpoint credential, Artifact must not be independently loaded in a way
/// that permits a contradictory cross-transaction snapshot to authorize
/// unsafe work"). `attempt` is `None` exactly when `transfer.attempt_id` is
/// `None` — a pre-dispatch Transfer never eligible for authorization
/// (`m0-data-plane-and-storage-contracts.md`; Issue #36 scope).
#[derive(Debug, Clone)]
pub struct AuthorizationDurableState {
    pub transfer: Transfer,
    pub artifact: Artifact,
    /// The owning Artifact's `ChunkManifest`, read in the *same* locked
    /// snapshot as `transfer`/`artifact`/`attempt`/`endpoint` (Issue #38
    /// correction §11–§12): whether it is sealed, and which `chunk_index`
    /// values already carry a durable expected identity, decides current
    /// data-plane operation eligibility
    /// (`bamep_domain::data_plane_operation_is_current`) and supplies the
    /// `expected_chunk_digest` the Worker UDS decision must carry for an
    /// approved `chunk_upload` of an already-recorded chunk.
    pub manifest: bamep_domain::ChunkManifest,
    /// The `chunk_index` values `bamepd` durably holds and has individually
    /// verified, from the same locked snapshot — never Worker-local
    /// transient state.
    pub held_chunk_indices: BTreeSet<ChunkIndex>,
    pub attempt: Option<Attempt>,
    pub endpoint: EndpointAggregate,
}

/// Read-only current-authorization-state Port (Issue #38). Deliberately
/// separate from [`TransferRepository`] (Issue #36's own narrower scope) and
/// from [`EndpointRepository`]/[`JobRepository`]: this Port exists purely to
/// give the Application authorization services one consistent snapshot
/// composed from all three underlying aggregates, reused identically by both
/// the Agent WSS capability-issuance path and the Worker UDS per-request
/// decision path.
#[async_trait]
pub trait TransferAuthorizationRepository: Send + Sync {
    /// `None` when no Transfer with `transfer_id` was ever created. Every
    /// authorization caller must treat "unknown transfer" and every other
    /// denial cause identically at the external boundary — this Port itself
    /// only reports the fact.
    async fn load_authorization_state(
        &self,
        transfer_id: TransferId,
    ) -> Result<Option<AuthorizationDurableState>, RepositoryError>;
}
