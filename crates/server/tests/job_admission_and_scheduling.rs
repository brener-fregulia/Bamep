//! Issue #32 "[WP] Admit Jobs and establish scheduling leases" boundary:
//! `JobSchedulingService::admit`/`satisfy_current_step_preconditions` against
//! the real `PostgresJobRepository` Adapter and a real PostgreSQL instance
//! (ADR-0013).
//!
//! No direct `jobs`/`job_steps` row mutation anywhere in this file except in
//! the one test that must construct a durably `Succeeded` earlier JobStep —
//! a state this Work Package's own harness cannot otherwise produce, exactly
//! as #31's `ineligible_non_pending_step_is_rejected_without_persisting`
//! already does for its own out-of-reach state. Every other admission/
//! eligibility transition is performed through `JobSchedulingService`, the
//! internal Application/harness control path (Issue #32).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::Arc;

use bamep_domain::{Actor, BootNonce, EndpointId, JobId, JobState, JobStepState};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository,
};
use bamep_server::application::{
    ApplicationError, BootOrchestrationService, EnrollmentService, JobSchedulingService,
    JobService, RedeemResult,
};
use bamep_server::ports::JobRepository;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use support::TestDatabase;

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type BootOrchestration = BootOrchestrationService<PostgresBootContextRepository>;
type Jobs = JobService<PostgresJobRepository>;
type Scheduling = JobSchedulingService<PostgresJobRepository>;

struct Services {
    boot: BootOrchestration,
    enrollment: Enrollment,
    jobs: Jobs,
    scheduling: Scheduling,
    job_repo: Arc<PostgresJobRepository>,
}

fn build_services(pool: PgPool) -> Services {
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let job_repo = Arc::new(PostgresJobRepository::new(pool));

    let boot = BootOrchestrationService::new(boot_repo, Duration::minutes(5));
    let enrollment = EnrollmentService::new(endpoint_repo, redemption_repo);
    let jobs = JobService::new(Arc::clone(&job_repo));
    let scheduling = JobSchedulingService::new(Arc::clone(&job_repo));

    Services {
        boot,
        enrollment,
        jobs,
        scheduling,
        job_repo,
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
                label: "job-admission-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

async fn job_state(pool: &PgPool, job_id: JobId) -> String {
    sqlx::query_scalar("SELECT state::text FROM jobs WHERE id = $1")
        .bind(job_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn domain_event_count_for_job(pool: &PgPool, job_id: JobId, event_type: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM domain_events WHERE job_id = $1 AND event_type::text = $2",
    )
    .bind(job_id.0)
    .bind(event_type)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn admitting_a_pending_job_makes_it_running_with_one_job_started_event() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-admit-01", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();

    let admitted = services.scheduling.admit(job.id).await.unwrap();

    assert_eq!(admitted.state, JobState::Running);
    assert_eq!(job_state(&db.pool, job.id).await, "Running");
    assert_eq!(
        domain_event_count_for_job(&db.pool, job.id, "JobStarted").await,
        1
    );

    db.teardown().await;
}

#[tokio::test]
async fn a_repeated_admission_attempt_is_rejected_without_a_second_job_started_event() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-admit-02", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();

    services.scheduling.admit(job.id).await.unwrap();
    let err = services.scheduling.admit(job.id).await.unwrap_err();

    assert!(matches!(
        err,
        ApplicationError::JobNotEligibleForAdmission(id) if id == job.id
    ));
    assert_eq!(job_state(&db.pool, job.id).await, "Running");
    assert_eq!(
        domain_event_count_for_job(&db.pool, job.id, "JobStarted").await,
        1,
        "a rejected repeated admission must never emit a second JobStarted"
    );

    db.teardown().await;
}

#[tokio::test]
async fn a_nonexistent_job_is_rejected() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());

    let bogus_job = JobId::new();
    let err = services.scheduling.admit(bogus_job).await.unwrap_err();

    assert!(matches!(err, ApplicationError::JobNotFound(id) if id == bogus_job));

    db.teardown().await;
}

#[tokio::test]
async fn two_pending_jobs_for_the_same_endpoint_race_admission_and_exactly_one_wins() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-admit-race-01", now).await;

    let job_a = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let job_b = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();

    let scheduling_a =
        JobSchedulingService::new(Arc::new(PostgresJobRepository::new(db.pool.clone())));
    let scheduling_b =
        JobSchedulingService::new(Arc::new(PostgresJobRepository::new(db.pool.clone())));

    let (result_a, result_b) =
        tokio::join!(scheduling_a.admit(job_a.id), scheduling_b.admit(job_b.id));

    let outcomes = [result_a, result_b];
    let successes = outcomes.iter().filter(|r| r.is_ok()).count();
    let blocked = outcomes
        .iter()
        .filter(|r| matches!(r, Err(ApplicationError::EndpointNotAvailable)))
        .count();

    assert_eq!(
        successes, 1,
        "exactly one concurrent same-Endpoint admission attempt must succeed"
    );
    assert_eq!(
        blocked, 1,
        "the losing attempt must be rejected as EndpointNotAvailable, never silently admitted"
    );

    let states = [
        job_state(&db.pool, job_a.id).await,
        job_state(&db.pool, job_b.id).await,
    ];
    assert_eq!(
        states.iter().filter(|s| s.as_str() == "Running").count(),
        1,
        "exactly one Job for this Endpoint must be Running"
    );
    assert_eq!(
        states.iter().filter(|s| s.as_str() == "Pending").count(),
        1,
        "the losing Job must remain Pending"
    );

    let started_events = domain_event_count_for_job(&db.pool, job_a.id, "JobStarted").await
        + domain_event_count_for_job(&db.pool, job_b.id, "JobStarted").await;
    assert_eq!(
        started_events, 1,
        "exactly one JobStarted event must exist across both competing Jobs"
    );

    db.teardown().await;
}

#[tokio::test]
async fn jobs_for_different_endpoints_may_both_become_running_independently() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_a = enrolled_endpoint(&services, "job-admit-diff-01", now).await;
    let endpoint_b = enrolled_endpoint(&services, "job-admit-diff-02", now).await;

    let job_a = services.jobs.create_workflow(endpoint_a, 1).await.unwrap();
    let job_b = services.jobs.create_workflow(endpoint_b, 1).await.unwrap();

    let admitted_a = services.scheduling.admit(job_a.id).await.unwrap();
    let admitted_b = services.scheduling.admit(job_b.id).await.unwrap();

    assert_eq!(admitted_a.state, JobState::Running);
    assert_eq!(admitted_b.state, JobState::Running);

    db.teardown().await;
}

#[tokio::test]
async fn reload_preserves_running_state_after_admission() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-admit-reload-01", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();

    db.pool.close().await;
    let reloaded_pool = bamep_server::adapters::postgres::connect(&db.db_url)
        .await
        .unwrap();
    let reloaded_repo = PostgresJobRepository::new(reloaded_pool.clone());

    let reloaded_job = reloaded_repo
        .find_job(job.id)
        .await
        .unwrap()
        .expect("the Job must survive reload");
    assert_eq!(reloaded_job.state, JobState::Running);

    reloaded_pool.close().await;
    db.teardown().await;
}

#[tokio::test]
async fn the_first_pending_step_of_a_running_job_becomes_eligible() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-step-01", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 2).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();
    let first_step_id = job.steps[0].id;

    let advanced = services
        .scheduling
        .satisfy_current_step_preconditions(job.id, first_step_id)
        .await
        .unwrap();

    assert_eq!(advanced.state, JobStepState::PreconditionsSatisfied);

    let reloaded = services.job_repo.find_job(job.id).await.unwrap().unwrap();
    assert_eq!(reloaded.state, JobState::Running, "Job must remain Running");
    assert_eq!(
        reloaded.steps[0].state,
        JobStepState::PreconditionsSatisfied
    );
    assert_eq!(
        reloaded.steps[1].state,
        JobStepState::Pending,
        "the later step must be untouched"
    );

    db.teardown().await;
}

#[tokio::test]
async fn a_later_pending_step_cannot_skip_an_earlier_unfinished_step() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-step-02", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 2).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();
    let second_step_id = job.steps[1].id;

    let err = services
        .scheduling
        .satisfy_current_step_preconditions(job.id, second_step_id)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        ApplicationError::JobStepNotCurrent(id) if id == second_step_id
    ));

    let reloaded = services.job_repo.find_job(job.id).await.unwrap().unwrap();
    assert_eq!(reloaded.steps[1].state, JobStepState::Pending);

    db.teardown().await;
}

#[tokio::test]
async fn the_next_step_becomes_eligible_once_the_prior_step_is_durably_succeeded() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-step-03", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 2).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();
    let first_step_id = job.steps[0].id;
    let second_step_id = job.steps[1].id;

    // This Work Package's own harness cannot reach `Succeeded` — that
    // transition belongs to #26. Direct SQL here only reaches a state
    // #32 itself never produces, to prove the "current step" rule holds once
    // it is reached, mirroring #31's precedent for its own out-of-reach state.
    sqlx::query("UPDATE job_steps SET state = 'Succeeded' WHERE id = $1")
        .bind(first_step_id.0)
        .execute(&db.pool)
        .await
        .unwrap();

    let advanced = services
        .scheduling
        .satisfy_current_step_preconditions(job.id, second_step_id)
        .await
        .unwrap();

    assert_eq!(advanced.state, JobStepState::PreconditionsSatisfied);

    db.teardown().await;
}

#[tokio::test]
async fn a_non_running_job_cannot_advance_any_step() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-step-04", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    // Deliberately not admitted — the Job remains Pending.
    let step_id = job.steps[0].id;

    let err = services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        ApplicationError::JobNotRunning(id) if id == job.id
    ));

    let reloaded = services.job_repo.find_job(job.id).await.unwrap().unwrap();
    assert_eq!(reloaded.steps[0].state, JobStepState::Pending);

    db.teardown().await;
}

#[tokio::test]
async fn an_unknown_step_id_under_a_running_job_is_rejected() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-step-05", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();

    let bogus_step = bamep_domain::JobStepId::new();
    let err = services
        .scheduling
        .satisfy_current_step_preconditions(job.id, bogus_step)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        ApplicationError::JobStepNotFound(step_id, job_id)
            if step_id == bogus_step && job_id == job.id
    ));

    db.teardown().await;
}

#[tokio::test]
async fn reload_preserves_preconditions_satisfied_step_state() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "job-step-reload-01", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();
    let step_id = job.steps[0].id;
    services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();

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
        reloaded_job.steps[0].state,
        JobStepState::PreconditionsSatisfied
    );

    reloaded_pool.close().await;
    db.teardown().await;
}
