//! Issue #26 "[WP] Dispatch typed actions and complete Attempts" boundary:
//! `ActionEvidenceService::apply` (`PostgresJobRepository::apply_action_evidence`)
//! against a real PostgreSQL instance (ADR-0013). Proves the normal
//! `ActionAck{Accepted|Rejected}` / `ActionResult{Succeeded|Failed}`
//! evidence-application, event/audit, idempotency, concurrency, and
//! reservation-release contract from `m0-job-lifecycle-and-scheduling.md`
//! "Attempt lifecycle"/"Duplicate and delayed evidence" and
//! `m0-persistence-observability-and-domain-events.md` "Atomic persistence".
//!
//! This file never sends `ActionDispatch`/opens a WebSocket — it starts from
//! an already-committed `Attempt{Dispatched}` (via `FinalDispatchService`,
//! exactly like `final_dispatch_authorization.rs`) and applies evidence
//! directly through `ActionEvidenceService`.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::Arc;

use bamep_agent_protocol::{InventoryReportMessage, ProtocolId};
use bamep_domain::{
    ActionEvidence, Actor, AttemptState, BootNonce, EndpointId, Job, JobId, JobState, JobStepId,
    JobStepState, TargetFingerprint,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository, PostgresJobRepository,
};
use bamep_server::adapters::target_revalidation_fixture::FixtureTargetRevalidation;
use bamep_server::application::{
    ActionEvidenceService, ApplicationError, BootOrchestrationService, DestructiveIntentService,
    EnrollmentService, FinalDispatchResult, FinalDispatchService, InventoryService,
    JobSchedulingService, JobService, RedeemResult,
};
use bamep_server::ports::{ApplyActionEvidenceResult, InventoryRepository, JobRepository};
use bamep_server::runtime::presence::PresenceRegistry;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
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
        Arc::clone(&target) as Arc<dyn bamep_server::ports::TargetRevalidationPort>,
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
                label: "action-evidence-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

async fn establish_trust(pool: &PgPool, endpoint_id: EndpointId) {
    use bamep_server::adapters::postgres::PostgresEndpointRepository;
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
/// (via the real #25 `FinalDispatchService`), and returns everything a #26
/// evidence test needs. Later steps (if any) remain `Pending`, which is
/// sufficient for the "non-final step succeeded" scenario — Domain's
/// Job-completion decision only checks each step's *state*, never whether it
/// was itself dispatched.
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
    bamep_server::runtime::resource_arbiter::ReservationId,
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
        Arc::clone(&services.target) as Arc<dyn bamep_server::ports::TargetRevalidationPort>,
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

/// Counts only #26's own terminal-evidence audit rows for `attempt_id` —
/// deliberately excludes #25's always-present destructive-dispatch
/// commitment audit row (`FinalDispatchService`'s own `AuditRecord`, which
/// also carries this same `attempt_id`), so callers get an exact count of
/// #26-owned terminal audits rather than an off-by-one baseline.
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

#[tokio::test]
async fn accepted_moves_attempt_to_in_progress_and_nothing_else_changes() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-accepted",
        1,
    )
    .await;

    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let arbiter = arbiter();
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    );

    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    let result = svc
        .apply(action_id, endpoint_id, ActionEvidence::AckAccepted)
        .await
        .unwrap();
    assert!(matches!(result, ApplyActionEvidenceResult::Applied(_)));

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "InProgress");
    let (step_state, failure_reason) = job_step_row(&db.pool, step_ids[0]).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(failure_reason, None);
    assert_eq!(job_state_text(&db.pool, job_id).await, "Running");
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 0);
    // Accepted must never release the reservation.
    assert_eq!(reservations.take(attempt.id), Some(reservation));

    db.teardown().await;
}

#[tokio::test]
async fn duplicate_accepted_against_in_progress_is_a_no_op() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (_job_id, _step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-dup-accepted",
        1,
    )
    .await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    svc.apply(action_id, endpoint_id, ActionEvidence::AckAccepted)
        .await
        .unwrap();
    let second = svc
        .apply(action_id, endpoint_id, ActionEvidence::AckAccepted)
        .await
        .unwrap();
    assert_eq!(second, ApplyActionEvidenceResult::NoOp);
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "InProgress");

    db.teardown().await;
}

#[tokio::test]
async fn rejected_atomically_fails_step_and_job_with_events_audit_and_release() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-rejected",
        1,
    )
    .await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let arbiter = arbiter();
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    let result = svc
        .apply(action_id, endpoint_id, ActionEvidence::AckRejected)
        .await
        .unwrap();
    assert!(matches!(result, ApplyActionEvidenceResult::Applied(_)));

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Rejected");
    let (step_state, failure_reason) = job_step_row(&db.pool, step_ids[0]).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(failure_reason.as_deref(), Some("DispatchRejected"));
    assert_eq!(job_state_text(&db.pool, job_id).await, "Failed");
    assert_eq!(event_count(&db.pool, job_id, "JobStepFailed").await, 1);
    assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 1);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);

    // The reservation must be released exactly once, after commit.
    assert_eq!(reservations.take(attempt.id), None, "already released");
    let reacquired = arbiter.acquire(network_claims());
    assert!(
        reacquired.is_ok(),
        "capacity must be available again after release"
    );

    db.teardown().await;
}

#[tokio::test]
async fn duplicate_rejected_is_a_no_op_and_never_double_releases() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-dup-rejected",
        1,
    )
    .await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let arbiter = arbiter();
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    svc.apply(action_id, endpoint_id, ActionEvidence::AckRejected)
        .await
        .unwrap();
    let second = svc
        .apply(action_id, endpoint_id, ActionEvidence::AckRejected)
        .await
        .unwrap();
    assert_eq!(second, ApplyActionEvidenceResult::NoOp);
    assert_eq!(event_count(&db.pool, job_id, "JobStepFailed").await, 1);
    assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 1);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);

    db.teardown().await;
}

#[tokio::test]
async fn succeeded_direct_from_dispatched_never_synthesizes_in_progress() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-succ-direct",
        1,
    )
    .await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let arbiter = arbiter();
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    let result = svc
        .apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
        .await
        .unwrap();
    assert!(matches!(result, ApplyActionEvidenceResult::Applied(_)));

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Succeeded");
    let (step_state, _) = job_step_row(&db.pool, step_ids[0]).await;
    assert_eq!(step_state, "Succeeded");
    // The single-step Job's only step just succeeded, so the Job itself
    // reaches Succeeded.
    assert_eq!(job_state_text(&db.pool, job_id).await, "Succeeded");
    assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 1);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);
    assert_eq!(reservations.take(attempt.id), None);

    db.teardown().await;
}

#[tokio::test]
async fn succeeded_from_in_progress_on_a_non_final_step_leaves_job_running() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-succ-nonfinal",
        2,
    )
    .await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    svc.apply(action_id, endpoint_id, ActionEvidence::AckAccepted)
        .await
        .unwrap();
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "InProgress");

    let result = svc
        .apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
        .await
        .unwrap();
    assert!(matches!(result, ApplyActionEvidenceResult::Applied(_)));

    let (step_state, _) = job_step_row(&db.pool, step_ids[0]).await;
    assert_eq!(step_state, "Succeeded");
    assert_eq!(
        job_state_text(&db.pool, job_id).await,
        "Running",
        "the Job must remain Running while a later ordered JobStep is still Pending"
    );
    assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 0);
    let (second_state, _) = job_step_row(&db.pool, step_ids[1]).await;
    assert_eq!(second_state, "Pending");

    db.teardown().await;
}

#[tokio::test]
async fn failed_from_dispatched_and_from_in_progress_both_fail_step_and_job() {
    for start_with_accept in [false, true] {
        let db = TestDatabase::setup().await;
        let services = build_services(db.pool.clone());
        let presence = Arc::new(PresenceRegistry::new());
        let (job_id, step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
            &db.pool,
            &services,
            &presence,
            &format!("action-evidence-failed-{start_with_accept}"),
            1,
        )
        .await;
        let reservations = Arc::new(AttemptReservationRegistry::new());
        reservations.register(attempt.id, reservation);
        let svc = evidence_service(
            Arc::clone(&services.job_repo),
            Arc::clone(&reservations),
            arbiter(),
        );
        let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

        if start_with_accept {
            svc.apply(action_id, endpoint_id, ActionEvidence::AckAccepted)
                .await
                .unwrap();
        }

        let result = svc
            .apply(action_id, endpoint_id, ActionEvidence::ResultFailed)
            .await
            .unwrap();
        assert!(matches!(result, ApplyActionEvidenceResult::Applied(_)));

        assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Failed");
        let (step_state, failure_reason) = job_step_row(&db.pool, step_ids[0]).await;
        assert_eq!(step_state, "Failed");
        assert_eq!(failure_reason.as_deref(), Some("ExecutionFailed"));
        assert_eq!(job_state_text(&db.pool, job_id).await, "Failed");
        assert_eq!(event_count(&db.pool, job_id, "JobStepFailed").await, 1);
        assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 1);
        assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);
        assert_eq!(reservations.take(attempt.id), None);

        db.teardown().await;
    }
}

#[tokio::test]
async fn conflicting_late_terminal_evidence_never_overwrites_the_first_committed_outcome() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-conflict",
        1,
    )
    .await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    svc.apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
        .await
        .unwrap();
    assert_eq!(job_state_text(&db.pool, job_id).await, "Succeeded");

    let conflict = svc
        .apply(action_id, endpoint_id, ActionEvidence::ResultFailed)
        .await
        .unwrap();
    assert_eq!(conflict, ApplyActionEvidenceResult::Conflict);

    // The first committed terminal outcome must be exactly preserved.
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Succeeded");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Succeeded");
    assert_eq!(event_count(&db.pool, job_id, "JobFailed").await, 0);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);

    db.teardown().await;
}

#[tokio::test]
async fn unknown_action_id_never_mutates_state() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (_job_id, _step_ids, endpoint_id, _attempt, _reservation) =
        dispatched_attempt(&db.pool, &services, &presence, "action-evidence-unknown", 1).await;
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::new(AttemptReservationRegistry::new()),
        arbiter(),
    );

    let err = svc
        .apply(
            ProtocolId::generate(),
            endpoint_id,
            ActionEvidence::ResultSucceeded,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::UnknownAction));

    db.teardown().await;
}

#[tokio::test]
async fn evidence_belonging_to_another_endpoint_is_indistinguishable_from_unknown() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (_job_id, _step_ids, _endpoint_id, attempt, _reservation) =
        dispatched_attempt(&db.pool, &services, &presence, "action-evidence-foreign", 1).await;
    let other_endpoint_id =
        enrolled_endpoint(&services, "action-evidence-foreign-other", Utc::now()).await;
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::new(AttemptReservationRegistry::new()),
        arbiter(),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    let err = svc
        .apply(action_id, other_endpoint_id, ActionEvidence::AckAccepted)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::UnknownAction));
    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Dispatched");

    db.teardown().await;
}

#[tokio::test]
async fn concurrent_terminal_evidence_produces_exactly_one_authoritative_commit_and_release() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, _step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-concurrent",
        1,
    )
    .await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let shared_arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        1,
    )]));
    // The single unit is already reserved by `reservation` above (the
    // network claim `dispatched_attempt` acquired through its own arbiter
    // instance is unrelated capacity bookkeeping — this shared arbiter
    // starts fresh and only this test's reservation is registered against
    // it, so re-registering it here mirrors what #26's real composition
    // would have acquired through this exact arbiter).
    let svc_a = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&shared_arbiter),
    );
    let svc_b = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&shared_arbiter),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    let (result_a, result_b) = tokio::join!(
        svc_a.apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded),
        svc_b.apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
    );
    let outcomes = [result_a.unwrap(), result_b.unwrap()];
    let applied = outcomes
        .iter()
        .filter(|r| matches!(r, ApplyActionEvidenceResult::Applied(_)))
        .count();
    let no_ops = outcomes
        .iter()
        .filter(|r| matches!(r, ApplyActionEvidenceResult::NoOp))
        .count();
    assert_eq!(applied, 1, "exactly one concurrent commit must apply");
    assert_eq!(
        no_ops, 1,
        "the other must observe the already-committed outcome as a no-op"
    );

    assert_eq!(job_state_text(&db.pool, job_id).await, "Succeeded");
    assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 1);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 1);
    // Exactly one release: capacity must be available again for a fresh
    // acquisition of the full (1-unit) capacity.
    assert!(shared_arbiter.acquire(network_claims()).is_ok());

    db.teardown().await;
}

#[tokio::test]
async fn forced_terminal_persistence_failure_rolls_back_and_does_not_release() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, step_ids, endpoint_id, attempt, reservation) = dispatched_attempt(
        &db.pool,
        &services,
        &presence,
        "action-evidence-forced-failure",
        1,
    )
    .await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let arbiter = arbiter();
    let svc = evidence_service(
        Arc::clone(&services.job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    );
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    sqlx::query(
        "CREATE FUNCTION reject_terminal_audit() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.detail LIKE 'attempt %reached terminal state%' THEN \
         RAISE EXCEPTION 'forced terminal audit failure'; END IF; RETURN NEW; END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_terminal_audit BEFORE INSERT ON audit_records \
         FOR EACH ROW EXECUTE FUNCTION reject_terminal_audit()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let err = svc
        .apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "Dispatched",
        "the Attempt must remain durably Dispatched when the terminal transaction fails"
    );
    let (step_state, _) = job_step_row(&db.pool, step_ids[0]).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(job_state_text(&db.pool, job_id).await, "Running");
    assert_eq!(event_count(&db.pool, job_id, "JobSucceeded").await, 0);
    assert_eq!(terminal_audit_count(&db.pool, attempt.id.0).await, 0);

    // The reservation must not have been released before the rollback was
    // known: it must still be registered, and the arbiter must still
    // consider it held.
    assert_eq!(reservations.take(attempt.id), Some(reservation));

    db.teardown().await;
}

#[tokio::test]
async fn reload_reconstructs_terminal_attempt_step_and_job_state() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, step_ids, endpoint_id, attempt, reservation) =
        dispatched_attempt(&db.pool, &services, &presence, "action-evidence-reload", 1).await;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    reservations.register(attempt.id, reservation);
    let svc = evidence_service(Arc::clone(&services.job_repo), reservations, arbiter());
    let action_id = ProtocolId::from_uuid(attempt.action_id.0).unwrap();

    svc.apply(action_id, endpoint_id, ActionEvidence::ResultSucceeded)
        .await
        .unwrap();

    db.pool.close().await;
    let reloaded_pool = bamep_server::adapters::postgres::connect(&db.db_url)
        .await
        .unwrap();
    let reloaded_repo = PostgresJobRepository::new(reloaded_pool.clone());

    let reloaded_job: Job = reloaded_repo
        .find_job(job_id)
        .await
        .unwrap()
        .expect("the Job must survive reload");
    assert_eq!(reloaded_job.state, JobState::Succeeded);
    assert_eq!(reloaded_job.steps[0].id, step_ids[0]);
    assert_eq!(reloaded_job.steps[0].state, JobStepState::Succeeded);

    let reloaded_attempt = reloaded_repo
        .find_attempt(attempt.id)
        .await
        .unwrap()
        .expect("the Attempt must survive reload");
    assert_eq!(reloaded_attempt.state, AttemptState::Succeeded);
    assert_eq!(reloaded_attempt.action_id, attempt.action_id);

    reloaded_pool.close().await;
    db.teardown().await;
}
