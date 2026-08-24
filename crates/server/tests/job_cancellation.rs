//! Issue #27 "[WP] Execute Job cancellation end to end" boundary:
//! `CancellationService::request`/`apply_cancel_ack`
//! (`PostgresJobRepository::request_cancellation`/`apply_cancel_ack`) against
//! a real PostgreSQL instance (ADR-0013). Proves the durable cancellation-
//! request, `CancelAck` evidence-application, event/audit, idempotency,
//! concurrency, and reservation-release contract from
//! `m0-job-lifecycle-and-scheduling.md` "Job lifecycle"/"Attempt lifecycle"
//! and `m0-persistence-observability-and-domain-events.md` "Atomic
//! persistence".
//!
//! This file never opens a WebSocket — it starts from an already-committed
//! `Attempt{Dispatched}` (via `FinalDispatchService`, exactly like
//! `action_evidence.rs`) and drives cancellation through `CancellationService`
//! directly, with `AgentDispatchPort` faked (the real-WSS `CancelAction`
//! transmission path is proven separately by `job_cancellation_wss.rs`).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bamep_agent_protocol::{
    ActionDispatchMessage, CancelActionMessage, InventoryReportMessage, ProtocolId,
};
use bamep_domain::{
    ActionEvidence, Actor, BootNonce, CancelAckEvidence, EndpointId, JobId, JobStepId,
    TargetFingerprint,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository, PostgresJobRepository,
};
use bamep_server::adapters::target_revalidation_fixture::FixtureTargetRevalidation;
use bamep_server::application::{
    ActionEvidenceService, ApplicationError, BootOrchestrationService, CancelActionSendOutcome,
    CancellationRequestResult, CancellationService, DestructiveIntentService, EnrollmentService,
    FinalDispatchResult, FinalDispatchService, InventoryService, JobSchedulingService, JobService,
    RedeemResult,
};
use bamep_server::ports::{
    AgentDispatchError, AgentDispatchPort, ApplyActionEvidenceResult, ApplyCancelAckResult,
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
/// counterpart is `job_cancellation_wss.rs`. Counts `cancel_action` calls and
/// can be configured to fail exactly once, mirroring `ActionDispatchService`'s
/// existing `FakeDispatchPort` test double.
#[derive(Default)]
struct FakeDispatchPort {
    cancel_calls: AtomicUsize,
    fail_next_cancel: Mutex<bool>,
}

impl FakeDispatchPort {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn failing_once() -> Arc<Self> {
        Arc::new(Self {
            cancel_calls: AtomicUsize::new(0),
            fail_next_cancel: Mutex::new(true),
        })
    }

    fn cancel_call_count(&self) -> usize {
        self.cancel_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AgentDispatchPort for FakeDispatchPort {
    async fn dispatch_action(
        &self,
        _endpoint_id: EndpointId,
        _dispatch: ActionDispatchMessage,
    ) -> Result<(), AgentDispatchError> {
        unimplemented!("job_cancellation.rs never sends ActionDispatch through this fake")
    }

    async fn cancel_action(
        &self,
        _endpoint_id: EndpointId,
        _cancel: CancelActionMessage,
    ) -> Result<(), AgentDispatchError> {
        self.cancel_calls.fetch_add(1, Ordering::SeqCst);
        let mut fail = self.fail_next_cancel.lock().unwrap();
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
                label: "job-cancellation-harness".into(),
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

/// Builds an enrolled, fully-trusted Endpoint with a `Running` Job of
/// `step_count` ordered steps whose first step is durably `Attempt{Dispatched}`
/// (via the real #25 `FinalDispatchService`), mirroring
/// `action_evidence.rs::dispatched_attempt`.
async fn dispatched_attempt(
    pool: &PgPool,
    services: &Services,
    presence: &Arc<PresenceRegistry>,
    inventory_signal: &str,
    step_count: usize,
) -> (
    JobId,
    Vec<JobStepId>,
    EndpointId,
    bamep_domain::Attempt,
    ReservationId,
) {
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(services, inventory_signal, now).await;
    presence.register(endpoint_id, ProtocolId::generate());
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

    let job = services
        .jobs
        .create_workflow(endpoint_id, step_count)
        .await
        .unwrap();
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

async fn cancellation_audit_count(pool: &PgPool, job_id: JobId) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_records WHERE job_id = $1 AND detail LIKE 'operator cancellation%'",
    )
    .bind(job_id.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn terminal_audit_count(pool: &PgPool, attempt_id: uuid::Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_records WHERE attempt_id = $1 AND detail LIKE 'attempt %reached terminal state%'",
    )
    .bind(attempt_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn evidence_service(
    job_repo: Arc<PostgresJobRepository>,
    reservations: Arc<AttemptReservationRegistry>,
    arbiter: Arc<TechnicalResourceArbiter>,
) -> ActionEvidenceService {
    ActionEvidenceService::new(job_repo as Arc<dyn JobRepository>, reservations, arbiter)
}

fn cancellation_service(
    job_repo: Arc<PostgresJobRepository>,
    reservations: Arc<AttemptReservationRegistry>,
    arbiter: Arc<TechnicalResourceArbiter>,
    dispatch: Arc<FakeDispatchPort>,
) -> CancellationService {
    CancellationService::new(
        job_repo as Arc<dyn JobRepository>,
        reservations,
        arbiter,
        dispatch as Arc<dyn AgentDispatchPort>,
    )
}

fn operator() -> Actor {
    Actor::Operator {
        label: "job-cancellation-test".into(),
    }
}

// ---------------------------------------------------------------------
// Durable cancellation request
// ---------------------------------------------------------------------

#[tokio::test]
async fn running_active_job_enters_cancelling_with_one_audit_and_sends_cancel_action() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _steps, _endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "cancel-enters-cancelling",
        1,
    )
    .await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let dispatch = FakeDispatchPort::new();
    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        Arc::clone(&dispatch),
    );

    let result = svc.request(job_id, operator()).await.unwrap();
    let CancellationRequestResult::EnteredCancelling { send } = result else {
        panic!("expected EnteredCancelling, got {result:?}")
    };
    assert_eq!(send, CancelActionSendOutcome::Sent);
    assert_eq!(dispatch.cancel_call_count(), 1);

    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");
    assert_eq!(cancellation_audit_count(&db.pool, job_id).await, 1);
    // No JobCancelled yet — the Job merely entered Cancelling.
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 0);
    // Reservation remains held while cancellation is pending.
    assert_eq!(reservations.take(attempt.id), Some(reservation));

    db.teardown().await;
}

#[tokio::test]
async fn repeated_request_while_cancelling_is_idempotent_and_never_resends() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _steps, _endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "cancel-repeated-idempotent",
        1,
    )
    .await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let dispatch = FakeDispatchPort::new();
    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        Arc::clone(&dispatch),
    );

    svc.request(job_id, operator()).await.unwrap();
    assert_eq!(dispatch.cancel_call_count(), 1);

    let second = svc.request(job_id, operator()).await.unwrap();
    assert_eq!(second, CancellationRequestResult::AlreadyCancelling);
    assert_eq!(
        dispatch.cancel_call_count(),
        1,
        "a repeated request must never send CancelAction again"
    );
    assert_eq!(
        cancellation_audit_count(&db.pool, job_id).await,
        1,
        "a repeated request must never duplicate the cancellation audit"
    );

    db.teardown().await;
}

#[tokio::test]
async fn no_active_attempt_completes_cancellation_immediately_without_sending_cancel_action() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "cancel-no-active-attempt", now).await;
    // Two-step workflow, admitted, but no step ever reaches Dispatching.
    let job = services.jobs.create_workflow(endpoint_id, 2).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();

    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        reservations,
        arbiter(),
        Arc::clone(&dispatch),
    );

    let result = svc.request(job.id, operator()).await.unwrap();
    assert_eq!(result, CancellationRequestResult::CompletedImmediately);
    assert_eq!(
        dispatch.cancel_call_count(),
        0,
        "no CancelAction is ever sent when no active attempt exists"
    );

    assert_eq!(job_state_text(&db.pool, job.id).await, "Cancelled");
    assert_eq!(event_count(&db.pool, job.id, "JobCancelled").await, 1);
    assert_eq!(cancellation_audit_count(&db.pool, job.id).await, 1);

    // Untouched Pending steps are never fabricated into Cancelled.
    for step in &job.steps {
        let (state, _) = job_step_row(&db.pool, step.id).await;
        assert_eq!(state, "Pending");
    }

    db.teardown().await;
}

#[tokio::test]
async fn a_terminal_job_cannot_be_overwritten_by_a_late_cancellation_request() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _steps, endpoint_id, attempt, reservation) =
        dispatched_attempt(&db.pool, &services, &presence, "cancel-terminal-job", 1).await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let evidence_svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    evidence_svc
        .apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
        .await
        .unwrap();
    assert_eq!(job_state_text(&db.pool, job_id).await, "Succeeded");

    let dispatch = FakeDispatchPort::new();
    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        Arc::clone(&dispatch),
    );
    let result = svc.request(job_id, operator()).await.unwrap();
    assert_eq!(result, CancellationRequestResult::AlreadyTerminal);
    assert_eq!(job_state_text(&db.pool, job_id).await, "Succeeded");
    assert_eq!(dispatch.cancel_call_count(), 0);
    assert_eq!(
        cancellation_audit_count(&db.pool, job_id).await,
        0,
        "no cancellation audit for a rejected/no-op cancellation request"
    );

    db.teardown().await;
}

#[tokio::test]
async fn pending_job_is_rejected_as_not_eligible() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "cancel-pending-job", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();

    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        reservations,
        arbiter(),
        dispatch,
    );
    let err = svc.request(job.id, operator()).await.unwrap_err();
    assert!(matches!(
        err,
        ApplicationError::JobNotEligibleForCancellation(_)
    ));
    assert_eq!(job_state_text(&db.pool, job.id).await, "Pending");

    db.teardown().await;
}

#[tokio::test]
async fn a_first_send_failure_leaves_the_job_durably_cancelling_without_a_second_send() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _steps, _endpoint_id, attempt, reservation) =
        dispatched_attempt(&db.pool, &services, &presence, "cancel-send-failure", 1).await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let dispatch = FakeDispatchPort::failing_once();
    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        Arc::clone(&dispatch),
    );

    let result = svc.request(job_id, operator()).await.unwrap();
    let CancellationRequestResult::EnteredCancelling { send } = result else {
        panic!("expected EnteredCancelling, got {result:?}")
    };
    assert!(matches!(send, CancelActionSendOutcome::SendFailed(_)));

    // Job remains durably Cancelling despite the send failure.
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Dispatched");
    // Reservation is never released merely because a send failed.
    assert_eq!(reservations.take(attempt.id), Some(reservation));
    reservations.register(attempt.id, reservation);

    // A repeated request while Cancelling never automatically resends.
    let second = svc.request(job_id, operator()).await.unwrap();
    assert_eq!(second, CancellationRequestResult::AlreadyCancelling);
    assert_eq!(
        dispatch.cancel_call_count(),
        1,
        "#27 never automatically retries a failed CancelAction send"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// CancelAck evidence application
// ---------------------------------------------------------------------

/// Puts a freshly dispatched Attempt's owning Job into `Cancelling` (via the
/// real `CancellationService::request` path) and returns everything a
/// `CancelAck` test needs.
async fn cancelling_attempt(
    pool: &PgPool,
    services: &Services,
    presence: &Arc<PresenceRegistry>,
    reservations: &Arc<AttemptReservationRegistry>,
    dispatch: &Arc<FakeDispatchPort>,
    inventory_signal: &str,
) -> (JobId, JobStepId, EndpointId, bamep_domain::Attempt) {
    let (job_id, steps, endpoint_id, attempt, reservation) =
        dispatched_attempt(pool, services, presence, inventory_signal, 1).await;
    reservations.register(attempt.id, reservation);
    let cancel_svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(reservations),
        arbiter(),
        Arc::clone(dispatch),
    );
    let result = cancel_svc.request(job_id, operator()).await.unwrap();
    assert!(matches!(
        result,
        CancellationRequestResult::EnteredCancelling { .. }
    ));
    (job_id, steps[0], endpoint_id, attempt)
}

#[tokio::test]
async fn cancel_ack_cancelled_produces_terminal_cancelled_state_atomically() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-ack-cancelled",
    )
    .await;

    let arbiter = arbiter();
    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
        Arc::clone(&dispatch),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    let result = svc
        .apply_cancel_ack(action_id, endpoint_id, CancelAckEvidence::Cancelled)
        .await
        .unwrap();
    assert!(matches!(result, ApplyCancelAckResult::Applied(_)));

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Cancelled");
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Cancelled");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelled");
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 1);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);

    // Reservation released exactly once.
    assert!(arbiter.acquire(network_claims()).is_ok());
    assert_eq!(reservations.take(attempt.id), None);

    // Duplicate CancelAck{Cancelled} never double-releases or re-emits an
    // event/audit.
    let duplicate = svc
        .apply_cancel_ack(action_id, endpoint_id, CancelAckEvidence::Cancelled)
        .await
        .unwrap();
    assert_eq!(duplicate, ApplyCancelAckResult::NoOp);
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 1);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);

    db.teardown().await;
}

#[tokio::test]
async fn cancel_ack_unknown_moves_the_attempt_to_awaiting_reconciliation() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-ack-unknown",
    )
    .await;

    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        dispatch,
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    let result = svc
        .apply_cancel_ack(action_id, endpoint_id, CancelAckEvidence::Unknown)
        .await
        .unwrap();
    assert!(matches!(result, ApplyCancelAckResult::Applied(_)));

    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "AwaitingReconciliation"
    );
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 0);
    // No fabricated Cancelled/Failed/Indeterminate; reservation remains held
    // — proven through the registry mapping (not a fresh, unrelated
    // arbiter's capacity, which would never reflect the original dispatch's
    // reservation).
    let held = reservations
        .take(attempt.id)
        .expect("the reservation mapping must remain registered while AwaitingReconciliation");
    reservations.register(attempt.id, held);

    // Repeated Unknown while already AwaitingReconciliation is idempotent.
    let repeated = svc
        .apply_cancel_ack(action_id, endpoint_id, CancelAckEvidence::Unknown)
        .await
        .unwrap();
    assert_eq!(repeated, ApplyCancelAckResult::NoOp);

    db.teardown().await;
}

#[tokio::test]
async fn cancel_ack_already_completed_with_unknown_result_awaits_reconciliation() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-ack-already-completed",
    )
    .await;

    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        dispatch,
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    let result = svc
        .apply_cancel_ack(action_id, endpoint_id, CancelAckEvidence::AlreadyCompleted)
        .await
        .unwrap();
    assert!(matches!(result, ApplyCancelAckResult::Applied(_)));

    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "AwaitingReconciliation"
    );
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");

    db.teardown().await;
}

#[tokio::test]
async fn cancel_ack_cannot_cancel_preserves_the_active_attempt_untouched() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-ack-cannot-cancel",
    )
    .await;

    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
        dispatch,
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    let result = svc
        .apply_cancel_ack(action_id, endpoint_id, CancelAckEvidence::CannotCancel)
        .await
        .unwrap();
    assert_eq!(result, ApplyCancelAckResult::NoOp);

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Dispatched");
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");
    assert!(
        reservations.take(attempt.id).is_some(),
        "reservation mapping must remain registered — CannotCancel never releases it"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Normal action evidence while Job is Cancelling
// ---------------------------------------------------------------------

#[tokio::test]
async fn accepted_ack_while_cancelling_moves_to_in_progress_and_job_remains_cancelling() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-normal-accepted",
    )
    .await;

    let evidence_svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    evidence_svc
        .apply(action_id, endpoint_id, ActionEvidence::AckAccepted)
        .await
        .unwrap();

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "InProgress");
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");

    db.teardown().await;
}

#[tokio::test]
async fn rejected_ack_while_cancelling_preserves_rejected_but_job_ends_cancelled() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-normal-rejected",
    )
    .await;

    let evidence_svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    evidence_svc
        .apply(action_id, endpoint_id, ActionEvidence::AckRejected)
        .await
        .unwrap();

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Rejected");
    let (step_state, failure_reason) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(failure_reason.as_deref(), Some("DispatchRejected"));
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelled");
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 1);
    assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 0);
    assert_eq!(
        reservations.take(attempt.id),
        None,
        "the reservation must be released exactly once through the registry"
    );

    db.teardown().await;
}

#[tokio::test]
async fn succeeded_result_while_cancelling_preserves_success_but_job_ends_cancelled() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-normal-succeeded",
    )
    .await;

    let evidence_svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    evidence_svc
        .apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
        .await
        .unwrap();

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Succeeded");
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Succeeded");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelled");
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 1);
    assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 0);
    assert_eq!(
        reservations.take(attempt.id),
        None,
        "the reservation must be released exactly once through the registry"
    );

    db.teardown().await;
}

#[tokio::test]
async fn failed_result_while_cancelling_preserves_failure_but_job_ends_cancelled() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-normal-failed",
    )
    .await;

    let evidence_svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    evidence_svc
        .apply(action_id, endpoint_id, ActionEvidence::ResultFailed)
        .await
        .unwrap();

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Failed");
    let (step_state, failure_reason) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(failure_reason.as_deref(), Some("ExecutionFailed"));
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelled");
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 1);
    assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 0);
    assert_eq!(
        reservations.take(attempt.id),
        None,
        "the reservation must be released exactly once through the registry"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Forced persistence failure / rollback
// ---------------------------------------------------------------------

#[tokio::test]
async fn forced_terminal_audit_failure_rolls_back_the_whole_cancel_ack_transition() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let dispatch = FakeDispatchPort::new();
    let (job_id, step_id, endpoint_id, attempt) = cancelling_attempt(
        &db.pool,
        &services,
        &presence,
        &reservations,
        &dispatch,
        "cancel-forced-audit-failure",
    )
    .await;

    // Force the audit_records insert to fail, mirroring
    // `job_admission_and_scheduling.rs`'s forced-failure trigger technique.
    sqlx::query(
        "CREATE FUNCTION reject_cancel_audit() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.job_id IS NOT NULL THEN RAISE EXCEPTION 'forced cancel audit failure'; END IF; RETURN NEW; END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_cancel_audit BEFORE INSERT ON audit_records \
         FOR EACH ROW EXECUTE FUNCTION reject_cancel_audit()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let arbiter = arbiter();
    let svc = cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
        dispatch,
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    let err = svc
        .apply_cancel_ack(action_id, endpoint_id, CancelAckEvidence::Cancelled)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    // The whole transition rolled back: Attempt/JobStep/Job remain exactly
    // as they were before this call.
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Dispatched");
    let (step_state, _) = job_step_row(&db.pool, step_id).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Cancelling");
    assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 0);
    // The reservation must not have been released: a repository-level error
    // short-circuits before `CancellationService::apply_cancel_ack` ever
    // reaches its reservation-release step.
    assert!(
        reservations.take(attempt.id).is_some(),
        "the reservation mapping must remain registered after a rolled-back transition"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Concurrency: cancellation request vs terminal Agent evidence
// ---------------------------------------------------------------------

#[tokio::test]
async fn concurrent_cancellation_request_and_terminal_result_serialize_to_one_outcome() {
    let db = TestDatabase::setup().await;
    let services = Arc::new(build_services(db.pool.clone()));
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _steps, endpoint_id, attempt, reservation) =
        dispatched_attempt(&db.pool, &services, &presence, "cancel-concurrency-race", 1).await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let arbiter = arbiter();
    let dispatch = FakeDispatchPort::new();

    let cancel_svc = Arc::new(cancellation_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
        Arc::clone(&dispatch),
    ));
    let evidence_svc = Arc::new(evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    ));
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    let cancel_task = {
        let cancel_svc = Arc::clone(&cancel_svc);
        tokio::spawn(async move { cancel_svc.request(job_id, operator()).await })
    };
    let evidence_task = {
        let evidence_svc = Arc::clone(&evidence_svc);
        tokio::spawn(async move {
            evidence_svc
                .apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
                .await
        })
    };

    let cancel_result = cancel_task.await.unwrap().unwrap();
    let evidence_result = evidence_task.await.unwrap().unwrap();

    // No deadlock: both calls completed. Exactly one coherent Job outcome.
    let final_state = job_state_text(&db.pool, job_id).await;
    assert!(
        final_state == "Cancelled" || final_state == "Succeeded",
        "unexpected final job state {final_state}"
    );

    match final_state.as_str() {
        "Succeeded" => {
            // ActionResult committed first: cancellation observed the
            // already-terminal Job and never overwrote it.
            assert_eq!(cancel_result, CancellationRequestResult::AlreadyTerminal);
            assert!(matches!(
                evidence_result,
                ApplyActionEvidenceResult::Applied(_)
            ));
            assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 1);
            assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 0);
        }
        "Cancelled" => {
            // Cancellation intent committed first: the subsequent terminal
            // result was interpreted under Cancelling semantics.
            assert!(matches!(
                cancel_result,
                CancellationRequestResult::EnteredCancelling { .. }
            ));
            assert!(matches!(
                evidence_result,
                ApplyActionEvidenceResult::Applied(_)
            ));
            assert_eq!(event_count(&db.pool, job_id, "JobCancelled").await, 1);
            assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 0);
        }
        other => panic!("unexpected state {other}"),
    }

    // Reservation released exactly once regardless of ordering.
    assert_eq!(
        reservations.take(attempt.id),
        None,
        "the reservation must be released exactly once regardless of race outcome"
    );

    db.teardown().await;
}
