//! Issue #40 "[WP] Commit non-destructive transfer Attempts for dispatch"
//! boundary: `TransferDispatchService::commit_transfer_dispatch` against the
//! real `PostgresJobRepository` Adapter and a real PostgreSQL instance
//! (ADR-0013).
//!
//! This WP ends exactly at the durable persist-before-send dispatch
//! commitment: no `ActionDispatch` message is constructed, no WebSocket send
//! occurs, and `TransferDispatchService` does not even depend on an
//! `AgentDispatchPort` — sending is structurally unreachable from it.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::Arc;

use bamep_domain::{
    ArtifactState, AttemptState, ChunkSize, DigestAlgorithm, EndpointId, JobId, JobState,
    JobStepId, JobStepState, SourceProvenance, TransferDirection, TransferDispatchRejection,
    TransferId,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository, PostgresJobRepository,
    PostgresTransferRepository,
};
use bamep_server::adapters::target_revalidation_fixture::FixtureTargetRevalidation;
use bamep_server::application::{
    ApplicationError, BootOrchestrationService, DestructiveIntentService, EnrollmentService,
    InventoryService, JobSchedulingService, JobService, RedeemResult, TransferDispatchResult,
    TransferDispatchService, TransferService,
};
use bamep_server::ports::{InventoryRepository, JobRepository, TargetRevalidationPort};
use bamep_server::runtime::resource_arbiter::{
    InsufficientCapacity, ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Row};
use support::TestDatabase;

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
    transfers: TransferService<PostgresTransferRepository>,
    inventory: InventoryService,
    intents: DestructiveIntentService<PostgresJobRepository, PostgresInventoryRepository>,
    target: Arc<FixtureTargetRevalidation>,
    job_repo: Arc<PostgresJobRepository>,
}

fn build_services(pool: PgPool) -> Services {
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let transfer_repo = Arc::new(PostgresTransferRepository::new(pool.clone()));
    let inventory_repo = Arc::new(PostgresInventoryRepository::new(pool.clone()));
    let target = Arc::new(FixtureTargetRevalidation::new());

    Services {
        boot: BootOrchestrationService::new(boot_repo, Duration::minutes(5)),
        enrollment: EnrollmentService::new(endpoint_repo, redemption_repo),
        jobs: JobService::new(Arc::clone(&job_repo)),
        scheduling: JobSchedulingService::new(Arc::clone(&job_repo)),
        transfers: TransferService::new(transfer_repo),
        inventory: InventoryService::new(
            Arc::clone(&inventory_repo) as Arc<dyn InventoryRepository>
        ),
        intents: DestructiveIntentService::new(
            Arc::clone(&job_repo),
            Arc::clone(&inventory_repo),
            Arc::clone(&target) as Arc<dyn TargetRevalidationPort>,
        ),
        target,
        job_repo,
    }
}

async fn enrolled_endpoint(
    services: &Services,
    inventory_signal: &str,
    now: DateTime<Utc>,
) -> EndpointId {
    let boot_nonce =
        bamep_domain::BootNonce::generate().expect("OS CSPRNG must be available in tests");
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
            bamep_domain::Actor::Operator {
                label: "transfer-dispatch-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

/// Full non-destructive setup: enrolled Endpoint, `Running` Job with
/// Endpoint exclusivity, its single (never destructive-intent-carrying)
/// JobStep at `PreconditionsSatisfied`, and a durable pre-dispatch Transfer
/// (#36) correlated to exactly that Job/JobStep/Endpoint, with no Attempt
/// bound. Deliberately never touches inventory, target fingerprint,
/// hardware confidence, presence, or trusted bootstrap — none of that
/// destructive-only evidence is required by this non-destructive path.
async fn preconditions_satisfied_transfer_step(
    services: &Services,
    inventory_signal: &str,
) -> (JobId, JobStepId, EndpointId, TransferId) {
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(services, inventory_signal, now).await;

    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;
    services.scheduling.admit(job.id).await.unwrap();
    services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();

    let context = services
        .transfers
        .create_transfer_context(
            endpoint_id,
            job.id,
            step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap();

    (job.id, step_id, endpoint_id, context.transfer.id)
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

async fn transfer_attempt_id(pool: &PgPool, transfer_id: TransferId) -> Option<uuid::Uuid> {
    sqlx::query_scalar("SELECT attempt_id FROM transfers WHERE id = $1")
        .bind(transfer_id.0)
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

#[tokio::test]
async fn successful_commitment_atomically_dispatches_step_attempt_and_binding() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, _endpoint_id, transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-01").await;

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());

    let result = svc
        .commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
        .await
        .unwrap();

    let TransferDispatchResult::Committed { outcome, .. } = result else {
        panic!("expected a successful commitment");
    };
    assert_eq!(outcome.job_step.state, JobStepState::Dispatching);
    assert_eq!(outcome.attempt.state, AttemptState::Dispatched);
    assert_eq!(outcome.transfer.id, transfer_id);
    assert_eq!(outcome.transfer.attempt_id, Some(outcome.attempt.id));

    assert_eq!(job_step_state(&db.pool, step_id).await, "Dispatching");
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 1);
    let attempt = attempt_row(&db.pool, step_id).await;
    assert_eq!(attempt.id, outcome.attempt.id.0);
    assert_eq!(attempt.action_id, outcome.attempt.action_id.0);
    assert_eq!(attempt.state, "Dispatched");

    assert_eq!(
        transfer_attempt_id(&db.pool, transfer_id).await,
        Some(outcome.attempt.id.0)
    );

    // No destructive-dispatch audit is required for this non-destructive
    // commitment.
    assert_eq!(audit_count_for_step(&db.pool, step_id).await, 0);

    assert_eq!(job_state_text(&db.pool, job_id).await, "Running");

    db.teardown().await;
}

#[tokio::test]
async fn transfer_identity_and_manifest_context_are_unchanged_by_dispatch() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, endpoint_id, transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-02").await;
    let (before, _held) = services
        .transfers
        .find_transfer_context(transfer_id)
        .await
        .unwrap()
        .unwrap();

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());
    svc.commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
        .await
        .unwrap();

    let (after, _held) = services
        .transfers
        .find_transfer_context(transfer_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(after.transfer.id, before.transfer.id);
    assert_eq!(after.transfer.artifact_id, before.transfer.artifact_id);
    assert_eq!(after.transfer.endpoint_id, endpoint_id);
    assert_eq!(after.transfer.job_id, job_id);
    assert_eq!(after.transfer.job_step_id, step_id);
    assert_eq!(after.transfer.direction, before.transfer.direction);
    assert_eq!(
        after.transfer.digest_algorithm,
        before.transfer.digest_algorithm
    );
    assert_eq!(after.transfer.chunk_size, before.transfer.chunk_size);
    assert_eq!(
        after.transfer.source_provenance.as_str(),
        before.transfer.source_provenance.as_str()
    );
    // Artifact/manifest lifecycle is untouched by dispatch commitment — #40
    // never changes Artifact lifecycle.
    assert_eq!(after.artifact.state, ArtifactState::Incomplete);
    assert_eq!(after.manifest, before.manifest);

    db.teardown().await;
}

#[tokio::test]
async fn reload_reconstructs_the_committed_attempt_dispatching_step_and_binding() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, _endpoint_id, transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-03").await;

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());
    let TransferDispatchResult::Committed { outcome, .. } = svc
        .commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
        .await
        .unwrap()
    else {
        panic!("expected a successful commitment");
    };

    db.pool.close().await;
    let reloaded_pool = bamep_server::adapters::postgres::connect(&db.db_url)
        .await
        .unwrap();
    let reloaded_job_repo = PostgresJobRepository::new(reloaded_pool.clone());
    let reloaded_transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(
        reloaded_pool.clone(),
    )));

    let reloaded_job = reloaded_job_repo
        .find_job(job_id)
        .await
        .unwrap()
        .expect("the workflow must survive reload");
    assert_eq!(reloaded_job.state, JobState::Running);
    assert_eq!(reloaded_job.steps[0].state, JobStepState::Dispatching);

    let reloaded_attempt = reloaded_job_repo
        .find_attempt(outcome.attempt.id)
        .await
        .unwrap()
        .expect("the committed Attempt must survive reload");
    assert_eq!(reloaded_attempt.job_step_id, step_id);
    assert_eq!(reloaded_attempt.action_id, outcome.attempt.action_id);
    assert_eq!(reloaded_attempt.state, AttemptState::Dispatched);

    let (reloaded_context, _held) = reloaded_transfers
        .find_transfer_context(transfer_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded_context.transfer.attempt_id,
        Some(outcome.attempt.id)
    );

    reloaded_pool.close().await;
    db.teardown().await;
}

#[tokio::test]
async fn forced_attempt_insert_failure_rolls_back_step_and_binding_together() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, _endpoint_id, transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-04").await;

    // Force the `attempts` insert to fail after the JobStep -> Dispatching
    // write has already been issued in the same transaction, proving the
    // whole transaction rolls back rather than leaving a partial
    // commitment: no Dispatching JobStep, no Attempt, and no Transfer
    // binding.
    sqlx::query(
        "CREATE FUNCTION reject_transfer_attempt_insert() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'forced transfer attempt insert failure'; END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_transfer_attempt_insert BEFORE INSERT ON attempts \
         FOR EACH ROW EXECUTE FUNCTION reject_transfer_attempt_insert()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());
    let err = svc
        .commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    sqlx::query("DROP TRIGGER reject_transfer_attempt_insert ON attempts")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION reject_transfer_attempt_insert")
        .execute(&db.pool)
        .await
        .unwrap();

    assert_eq!(
        job_step_state(&db.pool, step_id).await,
        "PreconditionsSatisfied",
        "the JobStep must remain durably PreconditionsSatisfied when the commit transaction fails"
    );
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 0);
    assert_eq!(transfer_attempt_id(&db.pool, transfer_id).await, None);

    db.teardown().await;
}

#[tokio::test]
async fn forced_transfer_binding_failure_rolls_back_step_and_attempt_together() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, _endpoint_id, transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-05").await;

    // Force the `transfers` UPDATE (the binding write) to fail after
    // job_steps/attempts writes already committed within the same
    // transaction — proving the whole transaction, including those earlier
    // writes, rolls back together. mirrors
    // final_dispatch_authorization.rs's forced-audit-failure technique
    // against this disposable database.
    sqlx::query(
        "CREATE FUNCTION reject_transfer_binding() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.attempt_id IS NOT NULL THEN RAISE EXCEPTION 'forced transfer binding failure'; END IF; RETURN NEW; END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_transfer_binding BEFORE UPDATE ON transfers \
         FOR EACH ROW EXECUTE FUNCTION reject_transfer_binding()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());
    let err = svc
        .commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    sqlx::query("DROP TRIGGER reject_transfer_binding ON transfers")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION reject_transfer_binding")
        .execute(&db.pool)
        .await
        .unwrap();

    assert_eq!(
        job_step_state(&db.pool, step_id).await,
        "PreconditionsSatisfied",
        "the JobStep insert must roll back together with the failed binding"
    );
    assert_eq!(
        attempt_count_for_step(&db.pool, step_id).await,
        0,
        "the Attempt insert must roll back together with the failed binding"
    );
    assert_eq!(transfer_attempt_id(&db.pool, transfer_id).await, None);

    db.teardown().await;
}

#[tokio::test]
async fn concurrent_dispatch_commitment_exactly_one_wins() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, _endpoint_id, transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-06").await;

    let repo_a = Arc::new(PostgresJobRepository::new(db.pool.clone()));
    let repo_b = Arc::new(PostgresJobRepository::new(db.pool.clone()));
    // A SHARED arbiter with capacity 2 and a 1-unit claim per call, mirroring
    // final_dispatch_authorization.rs's identical deterministic-release
    // rationale: both calls can acquire concurrently, so the DB row lock
    // alone decides the race, and the post-race capacity exactly proves
    // whether the loser's reservation was released.
    let shared_arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        2,
    )]));
    let svc_a = TransferDispatchService::new(repo_a, Arc::clone(&shared_arbiter));
    let svc_b = TransferDispatchService::new(repo_b, Arc::clone(&shared_arbiter));

    let (result_a, result_b) = tokio::join!(
        svc_a.commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims()),
        svc_b.commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
    );

    let outcomes = [result_a.unwrap(), result_b.unwrap()];
    let successes = outcomes
        .iter()
        .filter(|r| matches!(r, TransferDispatchResult::Committed { .. }))
        .count();
    let losers = outcomes
        .iter()
        .filter(|r| {
            matches!(
                r,
                TransferDispatchResult::Rejected(
                    TransferDispatchRejection::NotPreconditionsSatisfied
                )
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
    assert_eq!(job_step_state(&db.pool, step_id).await, "Dispatching");
    let bound = transfer_attempt_id(&db.pool, transfer_id).await;
    assert!(bound.is_some(), "exactly one binding must exist");

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
            TransferDispatchResult::Committed { reservation, .. } => Some(reservation),
            _ => None,
        })
        .expect("exactly one commitment must have succeeded");
    shared_arbiter.release(winner_reservation);

    db.teardown().await;
}

#[tokio::test]
async fn a_job_that_is_no_longer_running_is_rejected_and_returns_step_to_pending() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, _endpoint_id, transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-07").await;

    sqlx::query("UPDATE jobs SET state = 'Cancelling' WHERE id = $1")
        .bind(job_id.0)
        .execute(&db.pool)
        .await
        .unwrap();

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());
    let result = svc
        .commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
        .await
        .unwrap();

    assert!(matches!(
        result,
        TransferDispatchResult::Rejected(TransferDispatchRejection::JobNotRunning)
    ));
    assert_eq!(job_step_state(&db.pool, step_id).await, "Pending");
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 0);
    assert_eq!(transfer_attempt_id(&db.pool, transfer_id).await, None);

    db.teardown().await;
}

#[tokio::test]
async fn a_transfer_for_a_different_job_step_fails_closed_structurally() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, _endpoint_id, _transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-08a").await;
    let (_job_id_b, _step_id_b, _endpoint_id_b, transfer_id_b) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-08b").await;

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());
    let result = svc
        .commit_transfer_dispatch(job_id, step_id, transfer_id_b, network_claims())
        .await
        .unwrap();

    assert!(matches!(
        result,
        TransferDispatchResult::Rejected(TransferDispatchRejection::TransferCorrelationMismatch)
    ));
    // Structural mismatch: nothing to revert — the JobStep must remain
    // exactly PreconditionsSatisfied, not Pending.
    assert_eq!(
        job_step_state(&db.pool, step_id).await,
        "PreconditionsSatisfied"
    );
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn an_already_bound_transfer_is_rejected_without_creating_a_second_attempt() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (job_id, step_id, _endpoint_id, transfer_id) =
        preconditions_satisfied_transfer_step(&services, "xfer-dispatch-09").await;

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());
    let TransferDispatchResult::Committed { outcome, .. } = svc
        .commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
        .await
        .unwrap()
    else {
        panic!("expected the first commitment to succeed");
    };
    let first_attempt_id = outcome.attempt.id;

    // Directly force the first Attempt terminal and the JobStep back to
    // PreconditionsSatisfied (states this WP's own harness cannot otherwise
    // reach a second time for the same Transfer) purely to isolate the
    // already-bound-Transfer rejection from the earlier-checked
    // `ExistingActiveAttempt` rejection: with the prior Attempt still
    // non-terminal, `evaluate_transfer_dispatch` would reject on
    // `ExistingActiveAttempt` first, before ever reaching the Transfer ->
    // Attempt binding check this test targets.
    sqlx::query("UPDATE attempts SET state = 'Succeeded' WHERE id = $1")
        .bind(first_attempt_id.0)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE job_steps SET state = 'PreconditionsSatisfied' WHERE id = $1")
        .bind(step_id.0)
        .execute(&db.pool)
        .await
        .unwrap();

    let result = svc
        .commit_transfer_dispatch(job_id, step_id, transfer_id, network_claims())
        .await
        .unwrap();

    assert!(matches!(
        result,
        TransferDispatchResult::Rejected(TransferDispatchRejection::TransferAlreadyBound)
    ));
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 1);
    assert_eq!(
        transfer_attempt_id(&db.pool, transfer_id).await,
        Some(first_attempt_id.0),
        "the original binding must remain exactly as it was"
    );

    db.teardown().await;
}

#[tokio::test]
async fn a_structurally_destructive_job_step_is_rejected_and_the_destructive_path_is_unchanged() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "xfer-dispatch-10", now).await;

    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;

    // Attaches a DestructiveIntent, structurally classifying this JobStep as
    // the destructive M1 path (Issue #31) — mirrors
    // final_dispatch_authorization.rs's identical authorize-before-admit
    // ordering. #40 never authorizes destructive intent itself; this setup
    // exists only to prove #40 refuses to treat an already-destructive
    // JobStep as a transfer dispatch.
    services
        .inventory
        .record(
            endpoint_id,
            bamep_agent_protocol::InventoryReportMessage::new(
                serde_json::json!({"disk": "a"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    services
        .target
        .set_current_target(endpoint_id, bamep_domain::TargetFingerprint::new("disk-a"));
    services.intents.authorize(job.id, step_id).await.unwrap();

    services.scheduling.admit(job.id).await.unwrap();
    services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();

    let context = services
        .transfers
        .create_transfer_context(
            endpoint_id,
            job.id,
            step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap();

    let svc = TransferDispatchService::new(Arc::clone(&services.job_repo), arbiter());
    let result = svc
        .commit_transfer_dispatch(job.id, step_id, context.transfer.id, network_claims())
        .await
        .unwrap();

    assert!(matches!(
        result,
        TransferDispatchResult::Rejected(TransferDispatchRejection::StepIsDestructive)
    ));
    assert_eq!(
        job_step_state(&db.pool, step_id).await,
        "PreconditionsSatisfied",
        "a structural mismatch must never mutate the JobStep"
    );
    assert_eq!(attempt_count_for_step(&db.pool, step_id).await, 0);

    db.teardown().await;
}
