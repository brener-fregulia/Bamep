//! Application layer: orchestrates Domain transitions/constructions against
//! the `EndpointRepository`/`CredentialRedemptionRepository`/`JobRepository`
//! Ports. Owns no business rules of its own — every decision about whether a
//! transition or construction is legal, and what it produces, comes from
//! `bamep_domain`. This layer's job is sequencing (fetch, decide, one atomic
//! commit) and translating Domain outcomes into results the Runtime Services
//! (Agent Control Gateway, operator-approval harness, workflow-creation
//! harness) can act on.

use std::sync::Arc;

use bamep_agent_protocol::{BootstrapEvidenceMessage, InventoryReportMessage};
use bamep_domain::credential::CredentialHash;
use bamep_domain::presented_credential::{CredentialKind, PresentedCredential};
use bamep_domain::{
    transitions, Actor, BootContext, BootNonce, EmptyWorkflow, EndpointId,
    InvalidIdentityTransition, InventoryRevision, InventorySnapshot, Job, DEFAULT_CREDENTIAL_TTL,
};
use bamep_trusted_bootstrap::{AcceptedSiteKeys, BootstrapAssertion, ServerCertFingerprint};
use chrono::{DateTime, Duration, Utc};

use crate::ports::{
    BootContextRepository, CreateWorkflowError, CredentialRedemptionRepository, EndpointRepository,
    EndpointUpdateError, InventoryRepository, JobRepository, RedemptionDecision, RedemptionTarget,
    RepositoryError,
};

#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    #[error("endpoint {0:?} not found")]
    EndpointNotFound(EndpointId),
    #[error("endpoint {0:?} is not enrolled")]
    EndpointNotEnrolled(EndpointId),
    #[error(transparent)]
    InvalidTransition(#[from] InvalidIdentityTransition),
    #[error(transparent)]
    EmptyWorkflow(#[from] EmptyWorkflow),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
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
    /// acquire leases, or create an Attempt — those belong to #25.
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
}
