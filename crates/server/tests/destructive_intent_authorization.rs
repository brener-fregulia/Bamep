//! Issue #31 "Persist simulated destructive JobStep intent" boundary:
//! `DestructiveIntentService::authorize` against the real
//! `PostgresJobRepository`/`PostgresInventoryRepository`/
//! `FixtureTargetRevalidation` Adapters and a real PostgreSQL instance
//! (ADR-0013).
//!
//! No direct `job_steps` authorization-column mutation anywhere in this file
//! except in exactly the two tests that must prove the schema itself rejects
//! a half-intent (`schema_rejects_*`) and the one test that constructs an
//! ineligible, non-`Pending` JobStep to exercise a state this Work Package's
//! own harness cannot otherwise produce — every other JobStep authorization
//! is performed through `DestructiveIntentService::authorize`, the internal
//! Application/harness control path (Issue #31).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::Arc;

use bamep_agent_protocol::InventoryReportMessage;
use bamep_domain::{
    Actor, BootNonce, EndpointId, JobState, JobStepId, JobStepState, TargetFingerprint,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository, PostgresJobRepository,
};
use bamep_server::adapters::target_revalidation_fixture::FixtureTargetRevalidation;
use bamep_server::application::{
    ApplicationError, BootOrchestrationService, DestructiveIntentService, EnrollmentService,
    InventoryService, JobService, RedeemResult,
};
use bamep_server::ports::{InventoryRepository, JobRepository, TargetRevalidationPort};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use support::TestDatabase;

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type BootOrchestration = BootOrchestrationService<PostgresBootContextRepository>;
type Jobs = JobService<PostgresJobRepository>;
type DestructiveIntent =
    DestructiveIntentService<PostgresJobRepository, PostgresInventoryRepository>;

fn object(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

struct Services {
    boot: BootOrchestration,
    enrollment: Enrollment,
    jobs: Jobs,
    job_repo: Arc<PostgresJobRepository>,
    inventory: InventoryService,
    inventory_repo: Arc<PostgresInventoryRepository>,
    target: Arc<FixtureTargetRevalidation>,
    intents: DestructiveIntent,
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
        job_repo,
        inventory,
        inventory_repo,
        target,
        intents,
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
                label: "destructive-intent-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

async fn job_step_row(
    pool: &PgPool,
    step_id: JobStepId,
) -> (String, Option<uuid::Uuid>, Option<String>) {
    sqlx::query_as(
        "SELECT state::text, authorized_inventory_revision_id, authorized_target_fingerprint \
         FROM job_steps WHERE id = $1",
    )
    .bind(step_id.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn authorize_persists_current_inventory_and_target_and_leaves_job_pending() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-01", now).await;

    let revision = services
        .inventory
        .record(
            endpoint_id,
            InventoryReportMessage::new(object(json!({"disk": "a"}))),
        )
        .await
        .unwrap()
        .expect("first inventory report must create a revision");
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));

    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;

    let intent = services.intents.authorize(job.id, step_id).await.unwrap();

    assert_eq!(intent.authorized_inventory_revision_id, revision.id);
    assert_eq!(
        intent.authorized_target_fingerprint,
        TargetFingerprint::new("disk-a")
    );

    let reloaded = services.job_repo.find_job(job.id).await.unwrap().unwrap();
    assert_eq!(reloaded.state, JobState::Pending, "Job must remain Pending");
    assert_eq!(
        reloaded.steps[0].state,
        JobStepState::Pending,
        "the authorized JobStep must remain Pending"
    );
    assert_eq!(reloaded.steps[0].destructive_intent, Some(intent));

    db.teardown().await;
}

#[tokio::test]
async fn reload_reconstructs_the_persisted_intent_unchanged() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-02", now).await;

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
    let intent = services.intents.authorize(job.id, step_id).await.unwrap();

    db.pool.close().await;
    let reloaded_pool = bamep_server::adapters::postgres::connect(&db.db_url)
        .await
        .unwrap();
    let reloaded_repo = PostgresJobRepository::new(reloaded_pool.clone());

    let reloaded_job = reloaded_repo
        .find_job(job.id)
        .await
        .unwrap()
        .expect("the workflow must survive reload");

    assert_eq!(reloaded_job.steps[0].destructive_intent, Some(intent));

    reloaded_pool.close().await;
    db.teardown().await;
}

#[tokio::test]
async fn missing_current_inventory_blocks_authorization_without_persisting() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-03", now).await;
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;

    let err = services
        .intents
        .authorize(job.id, step_id)
        .await
        .unwrap_err();

    assert!(matches!(err, ApplicationError::NoCurrentInventory(id) if id == endpoint_id));
    let (state, revision, fingerprint) = job_step_row(&db.pool, step_id).await;
    assert_eq!(state, "Pending");
    assert_eq!(revision, None);
    assert_eq!(fingerprint, None);

    db.teardown().await;
}

#[tokio::test]
async fn missing_current_target_blocks_authorization_without_persisting() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-04", now).await;
    services
        .inventory
        .record(
            endpoint_id,
            InventoryReportMessage::new(object(json!({"disk": "a"}))),
        )
        .await
        .unwrap();
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;

    let err = services
        .intents
        .authorize(job.id, step_id)
        .await
        .unwrap_err();

    assert!(matches!(err, ApplicationError::NoCurrentTarget(id) if id == endpoint_id));
    let (state, revision, fingerprint) = job_step_row(&db.pool, step_id).await;
    assert_eq!(state, "Pending");
    assert_eq!(revision, None);
    assert_eq!(fingerprint, None);

    db.teardown().await;
}

#[tokio::test]
async fn wrong_job_step_correlation_is_rejected() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-05", now).await;
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

    let job_a = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let job_b = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();

    let err = services
        .intents
        .authorize(job_a.id, job_b.steps[0].id)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        ApplicationError::JobStepNotFound(step_id, job_id)
            if step_id == job_b.steps[0].id && job_id == job_a.id
    ));

    db.teardown().await;
}

#[tokio::test]
async fn ineligible_non_pending_step_is_rejected_without_persisting() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-06", now).await;
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

    // #31's own harness cannot construct a non-Pending JobStep — that
    // transition belongs to #25/#32. Direct SQL here only reaches a state
    // this Work Package's public path can never itself produce, to prove the
    // Domain eligibility rule holds even if it were reached.
    sqlx::query("UPDATE job_steps SET state = 'PreconditionsSatisfied' WHERE id = $1")
        .bind(step_id.0)
        .execute(&db.pool)
        .await
        .unwrap();

    let err = services
        .intents
        .authorize(job.id, step_id)
        .await
        .unwrap_err();

    assert!(matches!(err, ApplicationError::JobStepNotEligible(id) if id == step_id));
    let (state, revision, fingerprint) = job_step_row(&db.pool, step_id).await;
    assert_eq!(state, "PreconditionsSatisfied");
    assert_eq!(revision, None);
    assert_eq!(fingerprint, None);

    db.teardown().await;
}

#[tokio::test]
async fn an_already_authorized_step_cannot_be_reauthorized_sequentially() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-07", now).await;
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

    let first = services.intents.authorize(job.id, step_id).await.unwrap();

    // Independently changed evidence between the two calls: proves the
    // second attempt is rejected by the already-authorized rule, not merely
    // because evidence happened to be identical.
    services
        .inventory
        .record(
            endpoint_id,
            InventoryReportMessage::new(object(json!({"disk": "b"}))),
        )
        .await
        .unwrap();
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-b"));

    let err = services
        .intents
        .authorize(job.id, step_id)
        .await
        .unwrap_err();

    assert!(matches!(err, ApplicationError::JobStepAlreadyAuthorized(id) if id == step_id));
    let reloaded = services.job_repo.find_job(job.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.steps[0].destructive_intent,
        Some(first),
        "the original authorization snapshot must remain exactly as first captured"
    );

    db.teardown().await;
}

#[tokio::test]
async fn two_concurrent_authorization_attempts_do_not_overwrite_each_other() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-08", now).await;
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
    let job_id = job.id;
    let step_id = job.steps[0].id;

    let job_repo_a = Arc::new(PostgresJobRepository::new(db.pool.clone()));
    let inventory_repo_a = Arc::new(PostgresInventoryRepository::new(db.pool.clone()));
    let target_a = Arc::clone(&services.target);
    let intents_a = DestructiveIntentService::new(
        Arc::clone(&job_repo_a),
        inventory_repo_a,
        target_a as Arc<dyn TargetRevalidationPort>,
    );

    let job_repo_b = Arc::new(PostgresJobRepository::new(db.pool.clone()));
    let inventory_repo_b = Arc::new(PostgresInventoryRepository::new(db.pool.clone()));
    let target_b = Arc::clone(&services.target);
    let intents_b = DestructiveIntentService::new(
        job_repo_b,
        inventory_repo_b,
        target_b as Arc<dyn TargetRevalidationPort>,
    );

    let (result_a, result_b) = tokio::join!(
        intents_a.authorize(job_id, step_id),
        intents_b.authorize(job_id, step_id)
    );

    let outcomes = [result_a, result_b];
    let successes = outcomes.iter().filter(|r| r.is_ok()).count();
    let already_authorized = outcomes
        .iter()
        .filter(
            |r| matches!(r, Err(ApplicationError::JobStepAlreadyAuthorized(id)) if *id == step_id),
        )
        .count();

    assert_eq!(
        successes, 1,
        "exactly one concurrent authorization attempt must succeed"
    );
    assert_eq!(
        already_authorized, 1,
        "the losing attempt must be rejected as already authorized, never silently overwrite"
    );

    let reloaded = services.job_repo.find_job(job_id).await.unwrap().unwrap();
    assert!(
        reloaded.steps[0].destructive_intent.is_some(),
        "exactly one durable intent must be persisted"
    );

    db.teardown().await;
}

#[tokio::test]
async fn stale_inventory_setup_preserves_originally_authorized_revision() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-09", now).await;
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));

    // 1. current inventory is R1.
    let r1 = services
        .inventory
        .record(
            endpoint_id,
            InventoryReportMessage::new(object(json!({"disk": "a", "memory_bytes": 1}))),
        )
        .await
        .unwrap()
        .unwrap();
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;

    // 2. authorize the JobStep.
    let intent = services.intents.authorize(job.id, step_id).await.unwrap();
    // 3. verify stored authorized revision is R1.
    assert_eq!(intent.authorized_inventory_revision_id, r1.id);

    // 4. record changed inventory R2.
    let r2 = services
        .inventory
        .record(
            endpoint_id,
            InventoryReportMessage::new(object(json!({"disk": "a", "memory_bytes": 2}))),
        )
        .await
        .unwrap()
        .expect("changed inventory must create a new revision");
    assert_ne!(r2.id, r1.id);

    // 5. verify current revision is R2.
    let current = services
        .inventory_repo
        .find_current_inventory(endpoint_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.id, r2.id);

    // 6. verify JobStep still stores R1.
    let reloaded = services.job_repo.find_job(job.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.steps[0]
            .destructive_intent
            .as_ref()
            .unwrap()
            .authorized_inventory_revision_id,
        r1.id
    );

    db.teardown().await;
}

#[tokio::test]
async fn independent_target_setup_preserves_originally_authorized_target() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-10", now).await;

    // 1. current inventory remains unchanged throughout this test.
    let revision = services
        .inventory
        .record(
            endpoint_id,
            InventoryReportMessage::new(object(json!({"disk": "a"}))),
        )
        .await
        .unwrap()
        .unwrap();
    // 2. target fixture exposes A.
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;

    // 3. authorize the JobStep.
    let intent = services.intents.authorize(job.id, step_id).await.unwrap();
    assert_eq!(
        intent.authorized_target_fingerprint,
        TargetFingerprint::new("disk-a")
    );

    // 4. fixture changes to B.
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-b"));

    // 5. verify JobStep still stores A.
    let reloaded = services.job_repo.find_job(job.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.steps[0]
            .destructive_intent
            .as_ref()
            .unwrap()
            .authorized_target_fingerprint,
        TargetFingerprint::new("disk-a")
    );

    // 6. verify the inventory revision did not need to change.
    let current = services
        .inventory_repo
        .find_current_inventory(endpoint_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.id, revision.id);

    db.teardown().await;
}

#[tokio::test]
async fn schema_rejects_revision_without_fingerprint() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-11", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;

    let result =
        sqlx::query("UPDATE job_steps SET authorized_inventory_revision_id = $1 WHERE id = $2")
            .bind(uuid::Uuid::new_v4())
            .bind(step_id.0)
            .execute(&db.pool)
            .await;

    assert!(
        result.is_err(),
        "the all-or-none CHECK constraint must reject a revision without a fingerprint"
    );

    db.teardown().await;
}

#[tokio::test]
async fn schema_rejects_fingerprint_without_revision() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "destructive-intent-12", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;

    let result =
        sqlx::query("UPDATE job_steps SET authorized_target_fingerprint = 'disk-a' WHERE id = $1")
            .bind(step_id.0)
            .execute(&db.pool)
            .await;

    assert!(
        result.is_err(),
        "the all-or-none CHECK constraint must reject a fingerprint without a revision"
    );

    db.teardown().await;
}
