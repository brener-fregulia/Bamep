//! Issue #25 "[WP] Schedule Jobs and enforce safe dispatch gate" boundary:
//! `FinalDispatchService::commit_destructive_dispatch` against the real
//! `PostgresJobRepository` Adapter and a real PostgreSQL instance
//! (ADR-0013).
//!
//! This WP ends exactly at the durable persist-before-send dispatch
//! commitment: no `ActionDispatch` message is constructed, no WebSocket send
//! occurs, and no `AgentControlGateway` transmission method is called
//! anywhere in this file or the code it exercises.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::Arc;

use bamep_agent_protocol::{InventoryReportMessage, ProtocolId};
use bamep_domain::{
    Actor, AttemptState, BootNonce, EndpointId, FinalDispatchRejection, JobId, JobState, JobStepId,
    JobStepState, TargetFingerprint,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository, PostgresJobRepository,
};
use bamep_server::adapters::target_revalidation_fixture::FixtureTargetRevalidation;
use bamep_server::application::{
    ApplicationError, BootOrchestrationService, DestructiveIntentService, EnrollmentService,
    FinalDispatchResult, FinalDispatchService, InventoryService, JobSchedulingService, JobService,
    RedeemResult,
};
use bamep_server::ports::{InventoryRepository, JobRepository, TargetRevalidationPort};
use bamep_server::runtime::presence::PresenceRegistry;
use bamep_server::runtime::resource_arbiter::{
    InsufficientCapacity, ResourceClaim, ResourceKind, TechnicalResourceArbiter,
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
                label: "final-dispatch-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

/// Full setup: enrolled Endpoint, active durable credential, registered
/// presence, Consistent hardware confidence, current inventory equal to
/// authorized, independent target equal to authorized, `Running` Job with
/// Endpoint exclusivity, and the current destructive JobStep at
/// `PreconditionsSatisfied`. `BootstrapEvidence` is deliberately never sent,
/// so the authoritative current boot exists but trusted bootstrap remains
/// `NotEstablished` — preconditions 1-6 pass and only precondition 7 is left
/// to the caller to satisfy or not.
async fn preconditions_satisfied_destructive_step(
    services: &Services,
    presence: &PresenceRegistry,
    inventory_signal: &str,
) -> (JobId, JobStepId, EndpointId) {
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(services, inventory_signal, now).await;
    presence.register(endpoint_id, ProtocolId::generate());

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
    let step_id = job.steps[0].id;
    services.intents.authorize(job.id, step_id).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();
    services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();

    (job.id, step_id, endpoint_id)
}

async fn job_step_state(pool: &PgPool, step_id: JobStepId) -> String {
    sqlx::query_scalar("SELECT state::text FROM job_steps WHERE id = $1")
        .bind(step_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn job_state_text(pool: &PgPool, job_id: JobId) -> String {
    sqlx::query_scalar("SELECT state::text FROM jobs WHERE id = $1")
        .bind(job_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn attempt_count_for_step(pool: &PgPool, step_id: JobStepId) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE job_step_id = $1")
        .bind(step_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn audit_count_for_step(pool: &PgPool, step_id: JobStepId) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_records WHERE job_step_id = $1")
        .bind(step_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

struct AttemptRow {
    id: uuid::Uuid,
    action_id: uuid::Uuid,
    state: String,
}

async fn attempt_row(pool: &PgPool, step_id: JobStepId) -> AttemptRow {
    let row = sqlx::query("SELECT id, action_id, state::text FROM attempts WHERE job_step_id = $1")
        .bind(step_id.0)
        .fetch_one(pool)
        .await
        .unwrap();
    AttemptRow {
        id: row.get("id"),
        action_id: row.get("action_id"),
        state: row.get(2),
    }
}

async fn audit_row(pool: &PgPool, step_id: JobStepId) -> (uuid::Uuid, uuid::Uuid, uuid::Uuid) {
    let row = sqlx::query(
        "SELECT endpoint_id, attempt_id, action_id FROM audit_records WHERE job_step_id = $1",
    )
    .bind(step_id.0)
    .fetch_one(pool)
    .await
    .unwrap();
    (
        row.get("endpoint_id"),
        row.get::<uuid::Uuid, _>("attempt_id"),
        row.get::<uuid::Uuid, _>("action_id"),
    )
}

#[tokio::test]
async fn successful_commitment_dispatches_step_and_commits_one_attempt() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = PresenceRegistry::new();
    let (job_id, step_id, endpoint_id) =
        preconditions_satisfied_destructive_step(&services, &presence, "final-dispatch-01").await;

    // Establish trusted bootstrap for the current boot so all seven
    // preconditions hold.
    establish_trust(&services, &db.pool, endpoint_id).await;

    let svc = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        Arc::new(presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        arbiter(),
    );

    let result = svc
        .commit_destructive_dispatch(job_id, step_id, network_claims())
        .await
        .unwrap();

    let FinalDispatchResult::Committed { outcome, .. } = result else {
        panic!("expected a successful commitment");
    };
    assert_eq!(outcome.job_step.state, JobStepState::Dispatching);
    assert_eq!(outcome.attempt.state, AttemptState::Dispatched);

    assert_eq!(job_step_state(&db.pool, step_id).await, "Dispatching");
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 1);
    let attempt = attempt_row(&db.pool, step_id).await;
    assert_eq!(attempt.id, outcome.attempt.id.0);
    assert_eq!(attempt.action_id, outcome.attempt.action_id.0);
    assert_eq!(attempt.state, "Dispatched");

    assert_eq!(audit_count_for_step(&db.pool, step_id).await, 1);
    let (audit_endpoint, audit_attempt, audit_action) = audit_row(&db.pool, step_id).await;
    assert_eq!(audit_endpoint, endpoint_id.0);
    assert_eq!(audit_attempt, outcome.attempt.id.0);
    assert_eq!(audit_action, outcome.attempt.action_id.0);

    assert_eq!(job_state_text(&db.pool, job_id).await, "Running");

    db.teardown().await;
}

#[tokio::test]
async fn reload_reconstructs_the_committed_attempt_and_dispatching_step() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = PresenceRegistry::new();
    let (job_id, step_id, endpoint_id) =
        preconditions_satisfied_destructive_step(&services, &presence, "final-dispatch-02").await;
    establish_trust(&services, &db.pool, endpoint_id).await;

    let svc = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        Arc::new(presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        arbiter(),
    );
    let FinalDispatchResult::Committed { outcome, .. } = svc
        .commit_destructive_dispatch(job_id, step_id, network_claims())
        .await
        .unwrap()
    else {
        panic!("expected a successful commitment");
    };

    db.pool.close().await;
    let reloaded_pool = bamep_server::adapters::postgres::connect(&db.db_url)
        .await
        .unwrap();
    let reloaded_repo = PostgresJobRepository::new(reloaded_pool.clone());

    let reloaded_job = reloaded_repo
        .find_job(job_id)
        .await
        .unwrap()
        .expect("the workflow must survive reload");
    assert_eq!(reloaded_job.state, JobState::Running);
    assert_eq!(reloaded_job.steps[0].state, JobStepState::Dispatching);

    // Reload proves durable persistence only — never Agent receipt/execution.
    // The authoritative reconstruction proof goes through the JobRepository
    // read boundary (`find_attempt`), not raw SQL — raw SQL remains only for
    // independent persistence/count assertions elsewhere in this file.
    let reloaded_attempt = reloaded_repo
        .find_attempt(outcome.attempt.id)
        .await
        .unwrap()
        .expect("the committed Attempt must survive reload");
    assert_eq!(reloaded_attempt.id, outcome.attempt.id);
    assert_eq!(reloaded_attempt.job_step_id, step_id);
    assert_eq!(reloaded_attempt.action_id, outcome.attempt.action_id);
    assert_eq!(reloaded_attempt.state, AttemptState::Dispatched);

    reloaded_pool.close().await;
    db.teardown().await;
}

#[tokio::test]
async fn forced_audit_persistence_failure_rolls_back_the_entire_transaction() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = PresenceRegistry::new();
    let (job_id, step_id, endpoint_id) =
        preconditions_satisfied_destructive_step(&services, &presence, "final-dispatch-03").await;
    establish_trust(&services, &db.pool, endpoint_id).await;

    // Test-only trigger technique (mirrors job_admission_and_scheduling.rs's
    // `reject_job_started_event`): force the audit_records insert for this
    // destructive-dispatch commitment to fail after job_steps/attempts
    // writes have already been issued in the same transaction, proving the
    // whole transaction rolls back rather than leaving a partial commitment.
    sqlx::query(
        "CREATE FUNCTION reject_dispatch_audit() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.job_step_id IS NOT NULL THEN RAISE EXCEPTION 'forced dispatch audit failure'; END IF; RETURN NEW; END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_dispatch_audit BEFORE INSERT ON audit_records \
         FOR EACH ROW EXECUTE FUNCTION reject_dispatch_audit()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let svc = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        Arc::new(presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        arbiter(),
    );

    let err = svc
        .commit_destructive_dispatch(job_id, step_id, network_claims())
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    assert_eq!(
        job_step_state(&db.pool, step_id).await,
        "PreconditionsSatisfied",
        "the JobStep must remain durably PreconditionsSatisfied when the commit transaction fails"
    );
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 0);
    assert_eq!(audit_count_for_step(&db.pool, step_id).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn concurrent_dispatch_commitment_exactly_one_wins() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = Arc::new(PresenceRegistry::new());
    let (job_id, step_id, endpoint_id) =
        preconditions_satisfied_destructive_step(&services, &presence, "final-dispatch-04").await;
    establish_trust(&services, &db.pool, endpoint_id).await;

    let repo_a = Arc::new(PostgresJobRepository::new(db.pool.clone()));
    let repo_b = Arc::new(PostgresJobRepository::new(db.pool.clone()));
    // A SHARED arbiter with capacity 2 and a 1-unit claim per call, rather
    // than separate arbiters with slack capacity: with separate arbiters (or
    // one arbiter with capacity far above what either call claims), a later
    // `acquire` succeeding proves nothing about whether the loser's specific
    // reservation was released, since spare capacity would let it succeed
    // either way. With a shared 2-unit arbiter, both calls can acquire their
    // 1-unit claim concurrently (so the DB row lock alone decides the race,
    // not resource contention), and the post-race capacity is then an exact,
    // deterministic function of whether the loser's unit was released.
    let shared_arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        2,
    )]));
    let svc_a = FinalDispatchService::new(
        repo_a,
        Arc::clone(&presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        Arc::clone(&shared_arbiter),
    );
    let svc_b = FinalDispatchService::new(
        repo_b,
        Arc::clone(&presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        Arc::clone(&shared_arbiter),
    );

    let (result_a, result_b) = tokio::join!(
        svc_a.commit_destructive_dispatch(job_id, step_id, network_claims()),
        svc_b.commit_destructive_dispatch(job_id, step_id, network_claims())
    );

    let outcomes = [result_a.unwrap(), result_b.unwrap()];
    let successes = outcomes
        .iter()
        .filter(|r| matches!(r, FinalDispatchResult::Committed { .. }))
        .count();
    let losers = outcomes
        .iter()
        .filter(|r| {
            matches!(
                r,
                FinalDispatchResult::Rejected(FinalDispatchRejection::NotPreconditionsSatisfied)
            )
        })
        .count();

    assert_eq!(
        successes, 1,
        "exactly one concurrent commitment must succeed"
    );
    assert_eq!(
        losers, 1,
        "the losing attempt must observe the step is no longer PreconditionsSatisfied"
    );

    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 1);
    assert_eq!(audit_count_for_step(&db.pool, step_id).await, 1);
    assert_eq!(job_step_state(&db.pool, step_id).await, "Dispatching");

    // Deterministic reservation-release proof: exactly one unit (the
    // winner's) must remain reserved. A 1-unit probe must still fit — proving
    // the loser's unit was actually released, not merely that unrelated
    // slack capacity existed — and a second probe must then fail, since both
    // units of the shared 2-unit arbiter are now accounted for.
    let probe = shared_arbiter
        .acquire(network_claims())
        .expect("one more 1-unit claim must fit: the loser's reservation was released");
    assert_eq!(
        shared_arbiter.acquire(network_claims()),
        Err(InsufficientCapacity),
        "capacity must now be fully exhausted by the winner's reservation plus this probe"
    );
    shared_arbiter.release(probe);

    let winner_reservation = outcomes
        .into_iter()
        .find_map(|r| match r {
            FinalDispatchResult::Committed { reservation, .. } => Some(reservation),
            _ => None,
        })
        .expect("exactly one commitment must have succeeded");
    shared_arbiter.release(winner_reservation);

    db.teardown().await;
}

#[tokio::test]
async fn gate_failure_returns_step_to_pending_with_no_attempt_or_audit() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = PresenceRegistry::new();
    let (job_id, step_id, endpoint_id) =
        preconditions_satisfied_destructive_step(&services, &presence, "final-dispatch-05").await;
    establish_trust(&services, &db.pool, endpoint_id).await;

    // Break precondition 5 independently: current target no longer matches
    // the authorized fingerprint.
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-mismatch"));

    let svc = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        Arc::new(presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        arbiter(),
    );

    let result = svc
        .commit_destructive_dispatch(job_id, step_id, network_claims())
        .await
        .unwrap();
    assert!(matches!(
        result,
        FinalDispatchResult::Rejected(FinalDispatchRejection::TargetMismatch)
    ));

    assert_eq!(job_step_state(&db.pool, step_id).await, "Pending");
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 0);
    assert_eq!(audit_count_for_step(&db.pool, step_id).await, 0);
    assert_eq!(job_state_text(&db.pool, job_id).await, "Running");

    db.teardown().await;
}

/// The mandatory "preconditions 1-6 pass / only precondition 7 fails"
/// scenario (Issue #25 "REQUIRED 1-6 PASS / ONLY 7 FAIL SCENARIO"). Trusted
/// bootstrap is deliberately never established for this Endpoint's current
/// boot — `BootstrapEvidence` is never sent — proving every other precondition
/// passing does not imply trusted current bootstrap.
#[tokio::test]
async fn preconditions_one_through_six_pass_and_only_trusted_bootstrap_fails() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let presence = PresenceRegistry::new();
    let (job_id, step_id, endpoint_id) = preconditions_satisfied_destructive_step(
        &services,
        &presence,
        "final-dispatch-boot-only-fail",
    )
    .await;
    // Deliberately do NOT establish trusted bootstrap: the Endpoint has an
    // authoritative current boot (set atomically at first contact) that
    // remains NotEstablished.

    let svc = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        Arc::new(presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        arbiter(),
    );

    let result = svc
        .commit_destructive_dispatch(job_id, step_id, network_claims())
        .await
        .unwrap();

    assert!(
        matches!(
            result,
            FinalDispatchResult::Rejected(FinalDispatchRejection::TrustedBootstrapNotEstablished)
        ),
        "failure must be specifically trusted-current-bootstrap, got {result:?}"
    );

    assert_eq!(
        job_step_state(&db.pool, step_id).await,
        "Pending",
        "the JobStep must return to Pending"
    );
    assert_eq!(
        attempt_count_for_step(&db.pool, step_id).await,
        0,
        "zero Attempts may exist"
    );
    assert_eq!(
        audit_count_for_step(&db.pool, step_id).await,
        0,
        "zero destructive-dispatch audit records may exist"
    );

    // No Agent Protocol transmission occurs anywhere in this WP: nothing in
    // this test, FinalDispatchService, or PostgresJobRepository constructs an
    // ActionDispatch or opens a WebSocket send.
    let endpoint = sqlx::query("SELECT trusted_bootstrap_state::text FROM endpoints WHERE id = $1")
        .bind(endpoint_id.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let trusted_bootstrap_state: String = endpoint.get(0);
    assert_eq!(trusted_bootstrap_state, "NotEstablished");

    db.teardown().await;
}

/// Establishes trusted bootstrap for `endpoint_id`'s current boot by reading
/// the durable `CurrentBoot` the enrollment flow already set and driving
/// `bamep_domain::transitions::establish_trusted_bootstrap` through the real
/// `EndpointRepository` port — the same production path
/// `BootstrapEvidenceService` uses, without needing a real Agent Protocol
/// message/assertion for this Component/Integration-level test.
async fn establish_trust(services: &Services, pool: &PgPool, endpoint_id: EndpointId) {
    use bamep_server::adapters::postgres::PostgresEndpointRepository;
    use bamep_server::ports::{EndpointRepository, TrustedBootstrapDecision};
    let _ = services; // Services is unused here beyond type inference context.

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
    let outcome = repo
        .establish_trusted_bootstrap(endpoint_id, decide)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        bamep_domain::TrustedBootstrapOutcome::Established(_)
    ));
}
