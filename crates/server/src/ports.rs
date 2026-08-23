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
    BootContext, BootContextResolveError, DestructiveIntent, DestructiveIntentError,
    EndpointAggregate, EndpointId, InvalidIdentityTransition, InventoryRevision, InventorySnapshot,
    Job, JobId, JobStep, JobStepId, RedeemOutcome, TargetFingerprint, TransitionOutcome,
    TrustedBootstrapOutcome,
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

/// Durable Job/JobStep workflow persistence
/// (`m0-job-lifecycle-and-scheduling.md` "Domain model"). Issue #24
/// established durable workflow creation; Issue #31 extended this Port with
/// destructive-intent authorization. Later scheduling/dispatch/reconciliation
/// Work Packages extend this Port further as they add that persistence.
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
