//! Issue #24 "durable workflow creation" boundary: `JobService::create_workflow`
//! against the real `PostgresJobRepository`/`PostgresEndpointRepository`/
//! `PostgresCredentialRedemptionRepository` Adapters and a real PostgreSQL
//! instance (ADR-0013).
//!
//! No direct `jobs`/`job_steps` row mutation anywhere in this file — every
//! workflow is created through `JobService::create_workflow`, the internal
//! Application/harness control path (`m0-job-lifecycle-and-scheduling.md`;
//! Issue #24). Direct SQL is used only to *read* and assert on durable state
//! the Application already committed, and — in exactly one test — to install
//! a test-local trigger that forces a deterministic mid-transaction failure
//! inside this disposable database, mirroring
//! `crates/server/tests/inventory_report_wss.rs`'s
//! `inventory_revision_current_pointer_and_event_roll_back_together`.
//!
//! Pure Domain-level construction invariants (stable identities, `JobStep ->
//! Job`/`Job -> Endpoint` correlation, deterministic ordering, initial
//! `Pending` states, empty-workflow rejection) are covered directly in
//! `crates/domain/src/job.rs`, not duplicated here.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::Arc;

use bamep_domain::{Actor, BootNonce, EndpointId, JobState, JobStepState};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository,
};
use bamep_server::application::{
    ApplicationError, BootOrchestrationService, EnrollmentService, JobService, RedeemResult,
};
use bamep_server::ports::JobRepository;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use support::TestDatabase;

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type BootOrchestration = BootOrchestrationService<PostgresBootContextRepository>;
type Jobs = JobService<PostgresJobRepository>;

fn build_services(pool: PgPool) -> (BootOrchestration, Enrollment, Jobs) {
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let job_repo = Arc::new(PostgresJobRepository::new(pool));
    let boot_orchestration = BootOrchestrationService::new(boot_repo, Duration::minutes(5));
    let enrollment = EnrollmentService::new(endpoint_repo, redemption_repo);
    let jobs = JobService::new(job_repo);
    (boot_orchestration, enrollment, jobs)
}

/// First contact only — `PendingEnrollment`, never `Enrolled`. Used for the
/// not-yet-enrolled rejection test.
async fn pending_enrollment_endpoint(
    boot: &BootOrchestration,
    enrollment: &Enrollment,
    inventory_signal: &str,
    now: DateTime<Utc>,
) -> EndpointId {
    let boot_nonce = BootNonce::generate().expect("OS CSPRNG must be available in tests");
    let credential = boot
        .issue_enrollment_credential(inventory_signal, boot_nonce, now)
        .await
        .expect("issuance must succeed");
    let RedeemResult::Established { endpoint_id, .. } = enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("first contact must establish a session");
    };
    endpoint_id
}

/// First contact plus operator approval — the only Endpoint state Issue #24
/// accepts as a workflow target.
async fn enrolled_endpoint(
    boot: &BootOrchestration,
    enrollment: &Enrollment,
    inventory_signal: &str,
    now: DateTime<Utc>,
) -> EndpointId {
    let endpoint_id = pending_enrollment_endpoint(boot, enrollment, inventory_signal, now).await;
    enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "job-wp-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

async fn job_row_count_for_endpoint(pool: &PgPool, endpoint_id: EndpointId) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE endpoint_id = $1")
        .bind(endpoint_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn job_step_row_count_for_endpoint(pool: &PgPool, endpoint_id: EndpointId) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM job_steps js \
         JOIN jobs j ON j.id = js.job_id WHERE j.endpoint_id = $1",
    )
    .bind(endpoint_id.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn total_job_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn create_workflow_persists_job_and_ordered_steps_atomically() {
    let db = TestDatabase::setup().await;
    let (boot, enrollment, jobs) = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&boot, &enrollment, "job-wp-endpoint-01", now).await;

    let job = jobs.create_workflow(endpoint_id, 3).await.unwrap();

    // The returned aggregate itself.
    assert_eq!(job.endpoint_id, endpoint_id);
    assert_eq!(job.state, JobState::Pending);
    assert_eq!(job.steps.len(), 3);
    for (index, step) in job.steps.iter().enumerate() {
        assert_eq!(step.job_id, job.id);
        assert_eq!(step.order, index as i32);
        assert_eq!(step.state, JobStepState::Pending);
    }

    // Durable persistence, not merely the in-memory constructed value.
    let (persisted_endpoint_id, persisted_state): (uuid::Uuid, String) =
        sqlx::query_as("SELECT endpoint_id, state::text FROM jobs WHERE id = $1")
            .bind(job.id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(persisted_endpoint_id, endpoint_id.0);
    assert_eq!(persisted_state, "Pending");

    let step_rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT step_order, state::text FROM job_steps WHERE job_id = $1 ORDER BY step_order ASC",
    )
    .bind(job.id.0)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        step_rows,
        vec![
            (0, "Pending".to_string()),
            (1, "Pending".to_string()),
            (2, "Pending".to_string()),
        ]
    );

    // #24 introduces no Attempt/lease/dispatch persistence whatsoever — no
    // such table exists yet to touch, and every durably created row above
    // being exactly `Pending` is the complete proof for this checkpoint.
    db.teardown().await;
}

#[tokio::test]
async fn empty_workflow_is_rejected_before_any_persistence() {
    let db = TestDatabase::setup().await;
    let (boot, enrollment, jobs) = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&boot, &enrollment, "job-wp-endpoint-02", now).await;

    let err = jobs.create_workflow(endpoint_id, 0).await.unwrap_err();

    assert!(matches!(err, ApplicationError::EmptyWorkflow(_)));
    assert_eq!(total_job_count(&db.pool).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn nonexistent_target_endpoint_is_rejected_without_partial_state() {
    let db = TestDatabase::setup().await;
    let (_boot, _enrollment, jobs) = build_services(db.pool.clone());

    let bogus_endpoint = EndpointId::new();
    let err = jobs.create_workflow(bogus_endpoint, 2).await.unwrap_err();

    assert!(matches!(err, ApplicationError::EndpointNotFound(id) if id == bogus_endpoint));
    assert_eq!(total_job_count(&db.pool).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn pending_enrollment_target_endpoint_is_rejected_without_partial_state() {
    let db = TestDatabase::setup().await;
    let (boot, enrollment, jobs) = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id =
        pending_enrollment_endpoint(&boot, &enrollment, "job-wp-endpoint-03", now).await;

    let err = jobs.create_workflow(endpoint_id, 2).await.unwrap_err();

    assert!(matches!(err, ApplicationError::EndpointNotEnrolled(id) if id == endpoint_id));
    assert_eq!(job_row_count_for_endpoint(&db.pool, endpoint_id).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn failure_partway_through_jobstep_persistence_leaves_no_partial_workflow() {
    let db = TestDatabase::setup().await;
    let (boot, enrollment, jobs) = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&boot, &enrollment, "job-wp-endpoint-04", now).await;

    // Deliberately force the third JobStep insert to fail, proving the whole
    // transaction — including the already-inserted Job row and the first two
    // JobStep rows — rolls back rather than leaving a partial workflow.
    sqlx::query(
        "CREATE FUNCTION reject_third_job_step() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.step_order = 2 THEN RAISE EXCEPTION 'forced job_step failure'; END IF; RETURN NEW; END $$"
    ).execute(&db.pool).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_third_job_step BEFORE INSERT ON job_steps \
         FOR EACH ROW EXECUTE FUNCTION reject_third_job_step()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let err = jobs.create_workflow(endpoint_id, 3).await.unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    assert_eq!(
        job_row_count_for_endpoint(&db.pool, endpoint_id).await,
        0,
        "the Job row must roll back together with its JobSteps"
    );
    assert_eq!(
        job_step_row_count_for_endpoint(&db.pool, endpoint_id).await,
        0
    );

    db.teardown().await;
}

#[tokio::test]
async fn reload_reconstructs_the_same_job_and_ordered_jobsteps() {
    let db = TestDatabase::setup().await;
    let (boot, enrollment, jobs) = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&boot, &enrollment, "job-wp-endpoint-05", now).await;

    let job = jobs.create_workflow(endpoint_id, 4).await.unwrap();

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

    assert_eq!(
        reloaded_job, job,
        "identities, target, ordering, and states must be unchanged after reload"
    );

    reloaded_pool.close().await;
    db.teardown().await;
}
