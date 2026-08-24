//! Issue #28 "[WP] Reconcile interrupted Attempts safely" boundary:
//! `ReconciliationService` (`PostgresJobRepository::mark_endpoint_active_attempt_uncertain`/
//! `reconcile_all_active_attempts_on_startup`/`find_reconciliation_candidate`/
//! `apply_status_report`/`close_indeterminate`) against a real PostgreSQL
//! instance (ADR-0013). Proves the durable uncertain-entry, `StatusQuery`
//! trigger, `StatusReport` evidence-application, explicit `Indeterminate`
//! closure, event/audit, idempotency, and reservation-release contract from
//! `m0-job-lifecycle-and-scheduling.md` "Attempt lifecycle"/"Reconciliation"
//! and `m0-persistence-observability-and-domain-events.md` "Atomic
//! persistence".
//!
//! This file never opens a WebSocket — it starts from an already-committed
//! `Attempt{Dispatched}` (via `FinalDispatchService`, exactly like
//! `job_cancellation.rs`) and drives reconciliation through
//! `ReconciliationService` directly, with `AgentDispatchPort` faked (the
//! real-WSS `StatusQuery`/`StatusReport` transmission path is proven
//! separately by `reconciliation_wss.rs`).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bamep_agent_protocol::{ActionDispatchMessage, CancelActionMessage, InventoryReportMessage};
use bamep_domain::{
    Actor, AttemptId, BootNonce, CancelAckEvidence, EndpointId, JobId, JobStepId,
    StatusReportEvidence, TargetFingerprint,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository, PostgresJobRepository,
};
use bamep_server::adapters::target_revalidation_fixture::FixtureTargetRevalidation;
use bamep_server::application::{
    BootOrchestrationService, CancellationService, DestructiveIntentService, EnrollmentService,
    FinalDispatchResult, FinalDispatchService, InventoryService, JobSchedulingService, JobService,
    ReconciliationService, RedeemResult, StatusQuerySendOutcome,
};
use bamep_server::ports::{
    AgentDispatchError, AgentDispatchPort, ApplyReconciliationResult, CloseIndeterminateResult,
    InventoryRepository, JobRepository, TargetRevalidationPort,
};
use bamep_server::runtime::presence::PresenceRegistry;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ReservationId, ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};
use sqlx::{PgPool, Row};
use support::TestDatabase;

fn object(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn network_claims() -> Vec<ResourceClaim> {
    vec![ResourceClaim::new(ResourceKind::new("network"), 1)]
}

fn arbiter() -> Arc<TechnicalResourceArbiter> {
    Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        10,
    )]))
}

/// In-memory `AgentDispatchPort` fake — this file's own real-transport
/// counterpart is `reconciliation_wss.rs`. Records every `status_query`
/// call's exact `action_id` and can be configured to fail exactly once,
/// mirroring `job_cancellation.rs`'s identical test double.
#[derive(Default)]
struct FakeDispatchPort {
    status_query_calls: AtomicUsize,
    fail_next_status_query: Mutex<bool>,
    last_status_query_action_id: Mutex<Option<bamep_agent_protocol::ProtocolId>>,
}

impl FakeDispatchPort {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn failing_once() -> Arc<Self> {
        Arc::new(Self {
            status_query_calls: AtomicUsize::new(0),
            fail_next_status_query: Mutex::new(true),
            last_status_query_action_id: Mutex::new(None),
        })
    }

    fn status_query_call_count(&self) -> usize {
        self.status_query_calls.load(Ordering::SeqCst)
    }

    fn last_status_query_action_id(&self) -> Option<bamep_agent_protocol::ProtocolId> {
        *self.last_status_query_action_id.lock().unwrap()
    }
}

#[async_trait]
impl AgentDispatchPort for FakeDispatchPort {
    async fn dispatch_action(
        &self,
        _endpoint_id: EndpointId,
        _dispatch: ActionDispatchMessage,
    ) -> Result<(), AgentDispatchError> {
        unimplemented!("reconciliation.rs never sends ActionDispatch through this fake")
    }

    async fn cancel_action(
        &self,
        _endpoint_id: EndpointId,
        _cancel: CancelActionMessage,
    ) -> Result<(), AgentDispatchError> {
        // A small number of reconciliation scenarios compose with #27
        // cancellation (`CancellationService::request`), which requires a
        // working `cancel_action` — this fake's own transmission is not
        // itself under test here (`job_cancellation_wss.rs` proves the real
        // transport), so it always reports local acceptance.
        Ok(())
    }

    async fn status_query(
        &self,
        _endpoint_id: EndpointId,
        query: bamep_agent_protocol::StatusQueryMessage,
    ) -> Result<(), AgentDispatchError> {
        self.status_query_calls.fetch_add(1, Ordering::SeqCst);
        *self.last_status_query_action_id.lock().unwrap() = Some(query.body.action_id);
        let mut fail = self.fail_next_status_query.lock().unwrap();
        if *fail {
            *fail = false;
            return Err(AgentDispatchError::SendFailed);
        }
        Ok(())
    }
}

struct Services {
    boot: BootOrchestrationService<PostgresBootContextRepository>,
    enrollment:
        EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>,
    jobs: JobService<PostgresJobRepository>,
    scheduling: JobSchedulingService<PostgresJobRepository>,
    intents: DestructiveIntentService<PostgresJobRepository, PostgresInventoryRepository>,
    inventory: InventoryService,
    job_repo: Arc<PostgresJobRepository>,
    target: Arc<FixtureTargetRevalidation>,
}

fn build_services(pool: PgPool) -> Services {
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let inventory_repo = Arc::new(PostgresInventoryRepository::new(pool.clone()));
    let target = Arc::new(FixtureTargetRevalidation::new());

    let boot = BootOrchestrationService::new(boot_repo, Duration::minutes(5));
    let enrollment = EnrollmentService::new(endpoint_repo, redemption_repo);
    let jobs = JobService::new(Arc::clone(&job_repo));
    let scheduling = JobSchedulingService::new(Arc::clone(&job_repo));
    let inventory =
        InventoryService::new(Arc::clone(&inventory_repo) as Arc<dyn InventoryRepository>);
    let intents = DestructiveIntentService::new(
        Arc::clone(&job_repo),
        Arc::clone(&inventory_repo),
        Arc::clone(&target) as Arc<dyn TargetRevalidationPort>,
    );

    Services {
        boot,
        enrollment,
        jobs,
        scheduling,
        intents,
        inventory,
        job_repo,
        target,
    }
}

async fn enrolled_endpoint(
    services: &Services,
    inventory_signal: &str,
    now: DateTime<Utc>,
) -> EndpointId {
    let boot_nonce = BootNonce::generate().expect("OS CSPRNG must be available in tests");
    let credential = services
        .boot
        .issue_enrollment_credential(inventory_signal, boot_nonce, now)
        .await
        .expect("issuance must succeed");
    let RedeemResult::Established { endpoint_id, .. } = services
        .enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("first contact must establish a session");
    };
    services
        .enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "reconciliation-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

async fn establish_trust(pool: &PgPool, endpoint_id: EndpointId) {
    use bamep_server::ports::{EndpointRepository, TrustedBootstrapDecision};

    let repo = PostgresEndpointRepository::new(pool.clone());
    let endpoint = repo
        .find_by_id(endpoint_id)
        .await
        .unwrap()
        .expect("endpoint must exist");
    let boot_nonce = endpoint
        .current_boot
        .as_ref()
        .expect("first contact must set CurrentBoot")
        .boot_nonce();

    let decide: TrustedBootstrapDecision = Box::new(move |aggregate| {
        bamep_domain::transitions::establish_trusted_bootstrap(&aggregate, boot_nonce)
    });
    repo.establish_trusted_bootstrap(endpoint_id, decide)
        .await
        .unwrap();
}

/// Builds an enrolled, fully-trusted Endpoint with a `Running` Job of one
/// step whose first step is durably `Attempt{Dispatched}` (via the real #25
/// `FinalDispatchService`), mirroring `job_cancellation.rs::dispatched_attempt`.
async fn dispatched_attempt(
    pool: &PgPool,
    services: &Services,
    presence: &Arc<PresenceRegistry>,
    inventory_signal: &str,
) -> (
    JobId,
    Vec<JobStepId>,
    EndpointId,
    bamep_domain::Attempt,
    ReservationId,
) {
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(services, inventory_signal, now).await;
    presence.register(endpoint_id, bamep_agent_protocol::ProtocolId::generate());
    establish_trust(pool, endpoint_id).await;

    services
        .inventory
        .record(
            endpoint_id,
            InventoryReportMessage::new(object(json!({"disk": "a"}))),
        )
        .await
        .unwrap();
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));

    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_ids: Vec<JobStepId> = job.steps.iter().map(|s| s.id).collect();
    services
        .intents
        .authorize(job.id, step_ids[0])
        .await
        .unwrap();
    services.scheduling.admit(job.id).await.unwrap();
    services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_ids[0])
        .await
        .unwrap();

    let dispatch_service = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        Arc::clone(presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        arbiter(),
    );
    let FinalDispatchResult::Committed {
        outcome,
        reservation,
    } = dispatch_service
        .commit_destructive_dispatch(job.id, step_ids[0], network_claims())
        .await
        .unwrap()
    else {
        panic!("expected a successful final-dispatch commitment");
    };

    (job.id, step_ids, endpoint_id, outcome.attempt, reservation)
}

async fn attempt_state(pool: &PgPool, attempt_id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state::text FROM attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn job_step_row(pool: &PgPool, step_id: JobStepId) -> (String, Option<String>) {
    let row = sqlx::query("SELECT state::text, failure_reason::text FROM job_steps WHERE id = $1")
        .bind(step_id.0)
        .fetch_one(pool)
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

async fn job_state_text(pool: &PgPool, job_id: JobId) -> String {
    sqlx::query_scalar("SELECT state::text FROM jobs WHERE id = $1")
        .bind(job_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn event_count(pool: &PgPool, job_id: JobId, event_type: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM domain_events WHERE job_id = $1 AND event_type = $2::domain_event_type",
    )
    .bind(job_id.0)
    .bind(event_type)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn attempt_indeterminate_event_count(pool: &PgPool, attempt_id: uuid::Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM domain_events \
         WHERE attempt_id = $1 AND event_type = 'AttemptIndeterminate'::domain_event_type",
    )
    .bind(attempt_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn terminal_audit_count(pool: &PgPool, attempt_id: uuid::Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_records \
         WHERE attempt_id = $1 AND (detail LIKE 'attempt %reached terminal state%' \
                                     OR detail LIKE 'operator closed attempt%')",
    )
    .bind(attempt_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn reconciliation_service(
    job_repo: Arc<PostgresJobRepository>,
    reservations: Arc<AttemptReservationRegistry>,
    arbiter: Arc<TechnicalResourceArbiter>,
    dispatch: Arc<FakeDispatchPort>,
) -> ReconciliationService {
    ReconciliationService::new(
        job_repo as Arc<dyn JobRepository>,
        reservations,
        arbiter,
        dispatch as Arc<dyn AgentDispatchPort>,
    )
}

fn cancellation_service(
    job_repo: Arc<PostgresJobRepository>,
    reservations: Arc<AttemptReservationRegistry>,
    arbiter: Arc<TechnicalResourceArbiter>,
) -> CancellationService {
    CancellationService::new(
        job_repo as Arc<dyn JobRepository>,
        reservations,
        arbiter,
        FakeDispatchPort::new() as Arc<dyn AgentDispatchPort>,
    )
}

fn operator() -> Actor {
    Actor::Operator {
        label: "reconciliation-test".into(),
    }
}

// ---------------------------------------------------------------------
// Connection loss: entering AwaitingReconciliation
// ---------------------------------------------------------------------

#[tokio::test]
async fn dispatched_attempt_enters_awaiting_reconciliation_on_connection_loss() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (_job_id, _steps, endpoint_id, attempt, reservation) =
        dispatched_attempt(&db.pool, &services, &presence, "reconcile-connection-loss").await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );

    let reconciled = svc.mark_endpoint_uncertain(endpoint_id).await.unwrap();
    assert_eq!(reconciled, Some(attempt.id));
    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "AwaitingReconciliation"
    );
    // No event/audit required for entering AwaitingReconciliation.
    assert_eq!(
        attempt_indeterminate_event_count(&db.pool, attempt.id.0).await,
        0
    );
    // Reservation is never released merely for entering AwaitingReconciliation.
    assert_eq!(reservations.take(attempt.id), Some(reservation));

    db.teardown().await;
}

#[tokio::test]
async fn no_active_attempt_is_a_safe_no_op() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let endpoint_id = enrolled_endpoint(&services, "reconcile-no-active-attempt", Utc::now()).await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::new(AttemptReservationRegistry::new()),
        arbiter(),
        FakeDispatchPort::new(),
    );

    let reconciled = svc.mark_endpoint_uncertain(endpoint_id).await.unwrap();
    assert_eq!(reconciled, None);

    db.teardown().await;
}

#[tokio::test]
async fn repeated_connection_loss_is_idempotent() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (_job_id, _steps, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "reconcile-repeated-connection-loss",
    )
    .await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        reservations,
        arbiter(),
        FakeDispatchPort::new(),
    );

    let first = svc.mark_endpoint_uncertain(endpoint_id).await.unwrap();
    assert_eq!(first, Some(attempt.id));
    let second = svc.mark_endpoint_uncertain(endpoint_id).await.unwrap();
    assert_eq!(
        second, None,
        "an already-AwaitingReconciliation Attempt is never re-reported as newly reconciled"
    );
    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "AwaitingReconciliation"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Server restart: bulk reconciliation sweep
// ---------------------------------------------------------------------

#[tokio::test]
async fn server_restart_sweep_moves_every_dispatched_or_in_progress_attempt_and_nothing_else() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());

    let (_job_a, _steps_a, _ep_a, attempt_dispatched, _res_a) =
        dispatched_attempt(&db.pool, &services, &presence, "restart-sweep-dispatched").await;
    let (_job_b, _steps_b, _ep_b, attempt_in_progress, _res_b) =
        dispatched_attempt(&db.pool, &services, &presence, "restart-sweep-in-progress").await;

    // Move the second Attempt to InProgress via a real ActionAck{Accepted}.
    let evidence_reservations = Arc::new(AttemptReservationRegistry::new());
    let evidence = bamep_server::application::ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        evidence_reservations,
        arbiter(),
    );
    let protocol_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt_in_progress.action_id.0).unwrap();
    evidence
        .apply(
            protocol_action_id,
            _ep_b,
            bamep_domain::ActionEvidence::AckAccepted,
        )
        .await
        .unwrap();
    assert_eq!(
        attempt_state(&db.pool, attempt_in_progress.id.0).await,
        "InProgress"
    );

    // A third Job's Attempt is a distractor: already terminal (Succeeded)
    // before the sweep — must remain untouched.
    let (_job_c, _steps_c, ep_c, attempt_terminal, _res_c) =
        dispatched_attempt(&db.pool, &services, &presence, "restart-sweep-terminal").await;
    let terminal_reservations = Arc::new(AttemptReservationRegistry::new());
    let terminal_evidence = bamep_server::application::ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        terminal_reservations,
        arbiter(),
    );
    let terminal_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt_terminal.action_id.0).unwrap();
    terminal_evidence
        .apply(
            terminal_action_id,
            ep_c,
            bamep_domain::ActionEvidence::ResultSucceeded,
        )
        .await
        .unwrap();
    assert_eq!(
        attempt_state(&db.pool, attempt_terminal.id.0).await,
        "Succeeded"
    );

    // Simulate Server restart: every in-memory Runtime Service is freshly
    // constructed; only durable state survives.
    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::new(AttemptReservationRegistry::new()),
        arbiter(),
        FakeDispatchPort::new(),
    );

    let mut reconciled = svc.reconcile_on_startup().await.unwrap();
    reconciled.sort_by_key(|id| id.0);
    let mut expected = vec![attempt_dispatched.id, attempt_in_progress.id];
    expected.sort_by_key(|id| id.0);
    assert_eq!(reconciled, expected);

    assert_eq!(
        attempt_state(&db.pool, attempt_dispatched.id.0).await,
        "AwaitingReconciliation"
    );
    assert_eq!(
        attempt_state(&db.pool, attempt_in_progress.id.0).await,
        "AwaitingReconciliation"
    );
    assert_eq!(
        attempt_state(&db.pool, attempt_terminal.id.0).await,
        "Succeeded",
        "an already-terminal Attempt must never be disturbed by the restart sweep"
    );

    // A second sweep finds nothing left to reconcile — idempotent, no
    // duplicate Attempt, no redispatch.
    let second_sweep = svc.reconcile_on_startup().await.unwrap();
    assert!(second_sweep.is_empty());

    db.teardown().await;
}

// ---------------------------------------------------------------------
// StatusQuery on session (re-)establishment
// ---------------------------------------------------------------------

#[tokio::test]
async fn session_start_issues_status_query_for_the_exact_existing_action_id() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (_job_id, _steps, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "reconcile-session-start-query",
    )
    .await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let dispatch = FakeDispatchPort::new();
    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        reservations,
        arbiter(),
        Arc::clone(&dispatch),
    );

    // Enter AwaitingReconciliation first (e.g. via a prior disconnect).
    svc.mark_endpoint_uncertain(endpoint_id).await.unwrap();

    let outcome = svc.reconcile_on_session_start(endpoint_id).await.unwrap();
    assert_eq!(outcome, StatusQuerySendOutcome::Sent);
    assert_eq!(dispatch.status_query_call_count(), 1);
    let expected_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    assert_eq!(
        dispatch.last_status_query_action_id(),
        Some(expected_action_id),
        "StatusQuery must reuse the exact existing action_id, never a replacement"
    );

    db.teardown().await;
}

#[tokio::test]
async fn session_start_with_nothing_uncertain_sends_no_status_query() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (_job_id, _steps, endpoint_id, _attempt, _reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "reconcile-session-start-none-needed",
    )
    .await;

    let dispatch = FakeDispatchPort::new();
    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::new(AttemptReservationRegistry::new()),
        arbiter(),
        Arc::clone(&dispatch),
    );

    // The Attempt is still Dispatched (never entered AwaitingReconciliation).
    let outcome = svc.reconcile_on_session_start(endpoint_id).await.unwrap();
    assert_eq!(outcome, StatusQuerySendOutcome::NoneNeeded);
    assert_eq!(dispatch.status_query_call_count(), 0);

    db.teardown().await;
}

#[tokio::test]
async fn session_start_send_failure_never_causes_a_second_automatic_send() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (_job_id, _steps, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "reconcile-session-start-fail",
    )
    .await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let dispatch = FakeDispatchPort::failing_once();
    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        reservations,
        arbiter(),
        Arc::clone(&dispatch),
    );
    svc.mark_endpoint_uncertain(endpoint_id).await.unwrap();

    let outcome = svc.reconcile_on_session_start(endpoint_id).await.unwrap();
    assert!(matches!(outcome, StatusQuerySendOutcome::SendFailed(_)));
    assert_eq!(dispatch.status_query_call_count(), 1);
    // The Attempt remains durably AwaitingReconciliation — no fabricated
    // outcome, no automatic retry from this call alone.
    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "AwaitingReconciliation"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// StatusReport evidence application
// ---------------------------------------------------------------------

async fn awaiting_reconciliation_attempt(
    db: &TestDatabase,
    services: &Services,
    presence: &Arc<PresenceRegistry>,
    reservations: &Arc<AttemptReservationRegistry>,
    inventory_signal: &str,
) -> (JobId, JobStepId, EndpointId, bamep_domain::Attempt) {
    let (job_id, steps, endpoint_id, attempt, reservation) =
        dispatched_attempt(&db.pool, services, presence, inventory_signal).await;
    reservations.register(attempt.id, reservation);
    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    svc.mark_endpoint_uncertain(endpoint_id).await.unwrap();
    (job_id, steps[0], endpoint_id, attempt)
}

#[tokio::test]
async fn status_report_accepted_recovers_in_progress() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let (_job_id, _step_id, endpoint_id, attempt) = awaiting_reconciliation_attempt(
        &db,
        &services,
        &presence,
        &reservations,
        "reconcile-status-accepted",
    )
    .await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    let protocol_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    let result = svc
        .apply_status_report(
            protocol_action_id,
            endpoint_id,
            StatusReportEvidence::Accepted,
        )
        .await
        .unwrap();
    assert!(matches!(result, ApplyReconciliationResult::Applied(_)));
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "InProgress");
    // Non-terminal — reservation remains held.
    assert!(reservation_still_held(&reservations, attempt.id));

    db.teardown().await;
}

/// Checks whether `attempt_id`'s reservation mapping is still registered,
/// without disturbing it — `take` then immediately re-register on `Some`.
fn reservation_still_held(
    reservations: &Arc<AttemptReservationRegistry>,
    attempt_id: AttemptId,
) -> bool {
    match reservations.take(attempt_id) {
        Some(id) => {
            reservations.register(attempt_id, id);
            true
        }
        None => false,
    }
}

#[tokio::test]
async fn status_report_succeeded_on_final_step_reaches_job_succeeded_and_releases_reservation() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let (job_id, step_id, endpoint_id, attempt) = awaiting_reconciliation_attempt(
        &db,
        &services,
        &presence,
        &reservations,
        "reconcile-status-succeeded",
    )
    .await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    let protocol_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    let result = svc
        .apply_status_report(
            protocol_action_id,
            endpoint_id,
            StatusReportEvidence::Succeeded,
        )
        .await
        .unwrap();
    assert!(matches!(result, ApplyReconciliationResult::Applied(_)));
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Succeeded");
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Succeeded");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Succeeded");
    assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 1);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);
    assert_eq!(reservations.take(attempt.id), None, "released exactly once");

    db.teardown().await;
}

#[tokio::test]
async fn status_report_failed_uses_execution_failed_reason() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let (job_id, step_id, endpoint_id, attempt) = awaiting_reconciliation_attempt(
        &db,
        &services,
        &presence,
        &reservations,
        "reconcile-status-failed",
    )
    .await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    let protocol_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    svc.apply_status_report(
        protocol_action_id,
        endpoint_id,
        StatusReportEvidence::Failed,
    )
    .await
    .unwrap();

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Failed");
    let (step_state, reason) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(reason.as_deref(), Some("ExecutionFailed"));
    assert_eq!(job_state_text(&db.pool, job_id).await, "Failed");
    assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 1);
    assert_eq!(reservations.take(attempt.id), None);

    db.teardown().await;
}

#[tokio::test]
async fn status_report_unknown_leaves_the_attempt_uncertain_and_never_fabricates_indeterminate() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let (_job_id, _step_id, endpoint_id, attempt) = awaiting_reconciliation_attempt(
        &db,
        &services,
        &presence,
        &reservations,
        "reconcile-status-unknown",
    )
    .await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    let protocol_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    for _ in 0..3 {
        let result = svc
            .apply_status_report(
                protocol_action_id,
                endpoint_id,
                StatusReportEvidence::Unknown,
            )
            .await
            .unwrap();
        assert_eq!(result, ApplyReconciliationResult::NoOp);
    }

    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "AwaitingReconciliation",
        "repeated Unknown must never automatically produce Indeterminate"
    );
    assert_eq!(
        attempt_indeterminate_event_count(&db.pool, attempt.id.0).await,
        0
    );
    assert!(
        reservation_still_held(&reservations, attempt.id),
        "Unknown must never release the reservation"
    );

    db.teardown().await;
}

#[tokio::test]
async fn duplicate_terminal_status_report_after_commit_is_a_no_op() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let (_job_id, _step_id, endpoint_id, attempt) = awaiting_reconciliation_attempt(
        &db,
        &services,
        &presence,
        &reservations,
        "reconcile-status-duplicate-terminal",
    )
    .await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    let protocol_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    svc.apply_status_report(
        protocol_action_id,
        endpoint_id,
        StatusReportEvidence::Succeeded,
    )
    .await
    .unwrap();

    // A late/duplicate StatusReport{Failed} racing the already-committed
    // Succeeded outcome must never overwrite it.
    let result = svc
        .apply_status_report(
            protocol_action_id,
            endpoint_id,
            StatusReportEvidence::Failed,
        )
        .await
        .unwrap();
    assert_eq!(result, ApplyReconciliationResult::NoOp);
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Succeeded");

    db.teardown().await;
}

#[tokio::test]
async fn status_report_while_job_cancelling_still_ends_the_job_cancelled() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, steps, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "reconcile-status-cancelling",
    )
    .await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let cancellation = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    cancellation.request(job_id, operator()).await.unwrap();
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");

    // The CancelAck itself is Unknown (e.g. Agent restart mid-cancel) — the
    // Attempt now needs StatusQuery/StatusReport reconciliation.
    let protocol_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    cancellation
        .apply_cancel_ack(protocol_action_id, endpoint_id, CancelAckEvidence::Unknown)
        .await
        .unwrap();
    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "AwaitingReconciliation"
    );

    let reconciliation = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    reconciliation
        .apply_status_report(
            protocol_action_id,
            endpoint_id,
            StatusReportEvidence::Succeeded,
        )
        .await
        .unwrap();

    // Real execution result preserved on Attempt/JobStep...
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Succeeded");
    let (step_state, _) = job_step_row(&db.pool, steps[0]).await;
    assert_eq!(step_state, "Succeeded");
    // ...but the Job ends Cancelled, not Succeeded.
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelled");
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 1);
    assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 0);

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Explicit Indeterminate closure
// ---------------------------------------------------------------------

#[tokio::test]
async fn close_indeterminate_from_awaiting_reconciliation_applies_full_consequence() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let (job_id, step_id, _endpoint_id, attempt) = awaiting_reconciliation_attempt(
        &db,
        &services,
        &presence,
        &reservations,
        "reconcile-close-indeterminate",
    )
    .await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );

    let result = svc.close_indeterminate(job_id, operator()).await.unwrap();
    assert!(matches!(result, CloseIndeterminateResult::Applied(_)));

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Indeterminate");
    let (step_state, reason) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(reason.as_deref(), Some("ReconciliationIndeterminate"));
    assert_eq!(job_state_text(&db.pool, job_id).await, "Failed");
    assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 1);
    assert_eq!(
        attempt_indeterminate_event_count(&db.pool, attempt.id.0).await,
        1
    );
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);
    assert_eq!(reservations.take(attempt.id), None, "released exactly once");

    db.teardown().await;
}

#[tokio::test]
async fn repeated_close_indeterminate_is_idempotent_and_never_duplicates_evidence() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let (job_id, _step_id, _endpoint_id, attempt) = awaiting_reconciliation_attempt(
        &db,
        &services,
        &presence,
        &reservations,
        "reconcile-close-indeterminate-repeated",
    )
    .await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    let first = svc.close_indeterminate(job_id, operator()).await.unwrap();
    assert!(matches!(first, CloseIndeterminateResult::Applied(_)));

    let second = svc.close_indeterminate(job_id, operator()).await;
    assert!(
        matches!(
            second,
            Err(bamep_server::application::ApplicationError::JobHasNoUncertainAttempt(id)) if id == job_id
        ),
        "no candidate AwaitingReconciliation attempt remains — a safe, idempotent no-op: {second:?}"
    );

    assert_eq!(
        attempt_indeterminate_event_count(&db.pool, attempt.id.0).await,
        1,
        "repeated closure must never duplicate the AttemptIndeterminate event"
    );
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);

    db.teardown().await;
}

#[tokio::test]
async fn close_indeterminate_with_no_uncertain_attempt_is_rejected() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _steps, _endpoint_id, _attempt, _reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "reconcile-close-no-uncertain",
    )
    .await;

    let svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::new(AttemptReservationRegistry::new()),
        arbiter(),
        FakeDispatchPort::new(),
    );

    // The Attempt is still Dispatched — never AwaitingReconciliation.
    let result = svc.close_indeterminate(job_id, operator()).await;
    assert!(matches!(
        result,
        Err(bamep_server::application::ApplicationError::JobHasNoUncertainAttempt(id)) if id == job_id
    ));
    assert_eq!(
        attempt_indeterminate_event_count(&db.pool, _attempt.id.0).await,
        0
    );

    db.teardown().await;
}

#[tokio::test]
async fn close_indeterminate_while_cancelling_completes_the_job_as_cancelled() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, steps, endpoint_id, attempt, reservation) =
        dispatched_attempt(&db.pool, &services, &presence, "reconcile-close-cancelling").await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let cancellation = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    cancellation.request(job_id, operator()).await.unwrap();
    let protocol_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    cancellation
        .apply_cancel_ack(protocol_action_id, endpoint_id, CancelAckEvidence::Unknown)
        .await
        .unwrap();
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");

    let reconciliation = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    let result = reconciliation
        .close_indeterminate(job_id, operator())
        .await
        .unwrap();
    assert!(matches!(result, CloseIndeterminateResult::Applied(_)));

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Indeterminate");
    let (step_state, reason) = job_step_row(&db.pool, steps[0]).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(reason.as_deref(), Some("ReconciliationIndeterminate"));
    assert_eq!(
        job_state_text(&db.pool, job_id).await,
        "Cancelled",
        "cancellation intent owns the Job outcome"
    );
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 1);
    assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn close_indeterminate_reservation_absence_after_restart_never_corrupts_the_transition() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    // Deliberately never register the reservation into this registry —
    // stands in for a Server restart, where the transient mapping no longer
    // exists.
    let stale_reservations = Arc::new(AttemptReservationRegistry::new());
    let (job_id, steps, endpoint_id, attempt, _reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "reconcile-close-after-restart",
    )
    .await;
    let step_id = steps[0];

    let mark_svc = reconciliation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&stale_reservations),
        arbiter(),
        FakeDispatchPort::new(),
    );
    mark_svc.mark_endpoint_uncertain(endpoint_id).await.unwrap();

    let result = mark_svc
        .close_indeterminate(job_id, operator())
        .await
        .unwrap();
    assert!(matches!(result, CloseIndeterminateResult::Applied(_)));
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Indeterminate");
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(stale_reservations.take(attempt.id), None);

    db.teardown().await;
}
