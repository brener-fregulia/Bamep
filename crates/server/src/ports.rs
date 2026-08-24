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

use async_trait::async_trait;
use bamep_domain::presented_credential::{CredentialKind, CredentialLookupId};
use bamep_domain::{
    ActionEvidenceApplied, ActionId, Attempt, AttemptId, AuditRecord, BootContext,
    BootContextResolveError, CancelAckApplied, CancellationRequestError, DestructiveIntent,
    DestructiveIntentError, EndpointAggregate, EndpointId, FinalDispatchDenial,
    FinalDispatchOutcome, FinalDispatchRejection, InvalidIdentityTransition, InventoryRevision,
    InventoryRevisionId, InventorySnapshot, Job, JobAdmissionError, JobAdmissionOutcome, JobId,
    JobStep, JobStepEligibilityError, JobStepId, ReconciliationApplied, RedeemOutcome,
    TargetFingerprint, TransitionOutcome, TrustedBootstrapOutcome,
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
