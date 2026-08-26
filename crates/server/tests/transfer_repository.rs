//! Issue #36 "[WP] Persist Transfer, Artifact, and ChunkManifest lifecycle"
//! boundary: `TransferService`/`PostgresTransferRepository` against a real
//! PostgreSQL instance (ADR-0013).
//!
//! Pure Domain-level construction/transition invariants (identity
//! stability, lifecycle legality, manifest sealing/immutability, held-chunk
//! validation, one-time Attempt binding) are covered directly in
//! `crates/domain/src/{artifact,chunk_manifest,transfer}.rs`; this file
//! proves the PostgreSQL durability/reload/concurrency/rollback behavior
//! those Domain decisions compose into.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use bamep_domain::{
    ActionId, Actor, ArtifactState, Attempt, AttemptId, AttemptState, BootNonce,
    CaptureConsistency, ChunkIndex, ChunkRecordOutcome, DigestAlgorithm, EndpointId, JobId,
    JobStepId, SealOutcome, SourceProvenance, TransferDirection, TransferId,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository, PostgresTransferRepository,
};
use bamep_server::application::{
    ApplicationError, BootOrchestrationService, EnrollmentService, JobService, RedeemResult,
    TransferService,
};
use bamep_server::ports::AcceptChunkOutcome;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use support::TestDatabase;

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type BootOrchestration = BootOrchestrationService<PostgresBootContextRepository>;
type Jobs = JobService<PostgresJobRepository>;
type Transfers = TransferService<PostgresTransferRepository>;

struct Services {
    boot: BootOrchestration,
    enrollment: Enrollment,
    jobs: Jobs,
    transfers: Transfers,
}

fn build_services(pool: PgPool) -> Services {
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let transfer_repo = Arc::new(PostgresTransferRepository::new(pool));

    Services {
        boot: BootOrchestrationService::new(boot_repo, chrono::Duration::minutes(5)),
        enrollment: EnrollmentService::new(endpoint_repo, redemption_repo),
        jobs: JobService::new(job_repo),
        transfers: TransferService::new(transfer_repo),
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
                label: "transfer-repository-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

/// Builds an Endpoint plus one durable `Pending` Job/JobStep workflow
/// context — the minimum structural correlation `create_transfer_context`
/// requires. Deliberately never admits the Job into `Running`: Issue #36's
/// creation checks are structural correlation only, not JobStep eligibility
/// (that belongs to #40).
async fn workflow_context(
    services: &Services,
    inventory_signal: &str,
    now: DateTime<Utc>,
) -> (EndpointId, JobId, JobStepId) {
    let endpoint_id = enrolled_endpoint(services, inventory_signal, now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    (endpoint_id, job.id, job.steps[0].id)
}

fn digest32(byte: u8) -> Vec<u8> {
    vec![byte; 32]
}

async fn create_context(
    services: &Services,
    endpoint_id: EndpointId,
    job_id: JobId,
    job_step_id: JobStepId,
) -> bamep_domain::TransferContext {
    services
        .transfers
        .create_transfer_context(
            endpoint_id,
            job_id,
            job_step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            bamep_domain::ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap()
}

/// Directly inserts an `attempts` row for `job_step_id` — Issue #36's own
/// harness cannot commit an Attempt (that belongs to #40), so this mirrors
/// `job_workflow_creation.rs`'s precedent of reaching an otherwise out-of-
/// reach durable state through direct SQL only for test setup.
async fn insert_attempt(pool: &PgPool, job_step_id: JobStepId) -> Attempt {
    let attempt = Attempt {
        id: AttemptId::new(),
        job_step_id,
        action_id: ActionId::new(),
        state: AttemptState::Dispatched,
    };
    sqlx::query("INSERT INTO attempts (id, job_step_id, action_id, state) VALUES ($1, $2, $3, 'Dispatched')")
        .bind(attempt.id.0)
        .bind(attempt.job_step_id.0)
        .bind(attempt.action_id.0)
        .execute(pool)
        .await
        .unwrap();
    attempt
}

// ---------------------------------------------------------------------
// Pre-dispatch creation
// ---------------------------------------------------------------------

#[tokio::test]
async fn pre_dispatch_transfer_and_artifact_are_created_without_an_attempt() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-create-01", now).await;

    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    assert_eq!(context.transfer.endpoint_id, endpoint_id);
    assert_eq!(context.transfer.job_id, job_id);
    assert_eq!(context.transfer.job_step_id, job_step_id);
    assert_eq!(context.transfer.attempt_id, None);
    assert!(!context.transfer.is_attempt_bound());
    assert_eq!(context.artifact.state, ArtifactState::Incomplete);
    assert_eq!(
        context.artifact.capture_consistency,
        CaptureConsistency::NotEstablished
    );
    assert!(!context.manifest.sealed);

    db.teardown().await;
}

#[tokio::test]
async fn creation_rejects_unknown_endpoint() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (_endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-create-02", now).await;

    let bogus_endpoint = EndpointId::new();
    let err = services
        .transfers
        .create_transfer_context(
            bogus_endpoint,
            job_id,
            job_step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            bamep_domain::ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap_err();

    assert!(matches!(err, ApplicationError::EndpointNotFound(id) if id == bogus_endpoint));

    db.teardown().await;
}

#[tokio::test]
async fn creation_rejects_unknown_job() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "xfer-create-03", now).await;

    let bogus_job = JobId::new();
    let err = services
        .transfers
        .create_transfer_context(
            endpoint_id,
            bogus_job,
            JobStepId::new(),
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            bamep_domain::ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap_err();

    assert!(matches!(err, ApplicationError::JobNotFound(id) if id == bogus_job));

    db.teardown().await;
}

#[tokio::test]
async fn creation_rejects_a_job_targeting_a_different_endpoint() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (_endpoint_a, job_id, job_step_id) =
        workflow_context(&services, "xfer-create-04a", now).await;
    let endpoint_b = enrolled_endpoint(&services, "xfer-create-04b", now).await;

    let err = services
        .transfers
        .create_transfer_context(
            endpoint_b,
            job_id,
            job_step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            bamep_domain::ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        ApplicationError::JobEndpointMismatch(j, e) if j == job_id && e == endpoint_b
    ));

    db.teardown().await;
}

#[tokio::test]
async fn creation_rejects_unknown_job_step() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "xfer-create-05", now).await;
    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();

    let bogus_step = JobStepId::new();
    let err = services
        .transfers
        .create_transfer_context(
            endpoint_id,
            job.id,
            bogus_step,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            bamep_domain::ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        ApplicationError::JobStepNotFound(s, j) if s == bogus_step && j == job.id
    ));

    db.teardown().await;
}

#[tokio::test]
async fn reload_preserves_pre_dispatch_identities_and_correlation() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-reload-01", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    db.pool.close().await;
    let reloaded_pool = bamep_server::adapters::postgres::connect(&db.db_url)
        .await
        .unwrap();
    let reloaded_repo = PostgresTransferRepository::new(reloaded_pool.clone());
    let reloaded_transfers = TransferService::new(Arc::new(reloaded_repo));

    let (reloaded_context, held) = reloaded_transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .expect("the transfer must survive reload");

    assert_eq!(reloaded_context.transfer, context.transfer);
    assert_eq!(reloaded_context.artifact, context.artifact);
    assert_eq!(reloaded_context.manifest, context.manifest);
    assert!(held.is_empty());

    reloaded_pool.close().await;
    db.teardown().await;
}

#[tokio::test]
async fn find_transfer_context_returns_none_for_unknown_transfer() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());

    let found = services
        .transfers
        .find_transfer_context(TransferId::new())
        .await
        .unwrap();
    assert!(found.is_none());

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Chunk identity continuation / idempotence / conflict
// ---------------------------------------------------------------------

#[tokio::test]
async fn recording_a_new_chunk_continues_the_manifest_and_survives_reload() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-chunk-01", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    let outcome = services
        .transfers
        .record_expected_chunk(context.transfer.id, ChunkIndex(0), 100, digest32(1))
        .await
        .unwrap();
    assert!(matches!(outcome, ChunkRecordOutcome::Added(_)));

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.manifest.recorded_chunk_count(), 1);
    assert_eq!(
        reloaded
            .manifest
            .expected_chunk(ChunkIndex(0))
            .unwrap()
            .size,
        100
    );

    db.teardown().await;
}

#[tokio::test]
async fn recording_the_identical_chunk_twice_is_idempotent() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-chunk-02", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    services
        .transfers
        .record_expected_chunk(context.transfer.id, ChunkIndex(0), 100, digest32(1))
        .await
        .unwrap();
    let second = services
        .transfers
        .record_expected_chunk(context.transfer.id, ChunkIndex(0), 100, digest32(1))
        .await
        .unwrap();

    assert_eq!(second, ChunkRecordOutcome::AlreadyRecorded);

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.manifest.recorded_chunk_count(),
        1,
        "idempotent resubmission must never create a duplicate row"
    );

    db.teardown().await;
}

#[tokio::test]
async fn conflicting_chunk_identity_is_rejected_and_never_rewrites_the_original() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-chunk-03", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    services
        .transfers
        .record_expected_chunk(context.transfer.id, ChunkIndex(0), 100, digest32(1))
        .await
        .unwrap();

    let err = services
        .transfers
        .record_expected_chunk(context.transfer.id, ChunkIndex(0), 100, digest32(2))
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::ChunkRecord(_)));

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded
            .manifest
            .expected_chunk(ChunkIndex(0))
            .unwrap()
            .digest
            .as_bytes(),
        digest32(1).as_slice(),
        "the original expected digest must remain durable and unrewritten"
    );

    db.teardown().await;
}

#[tokio::test]
async fn two_concurrent_conflicting_chunk_identities_serialize_and_only_one_wins() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-chunk-race-01", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    let repo_a = Arc::new(PostgresTransferRepository::new(db.pool.clone()));
    let repo_b = Arc::new(PostgresTransferRepository::new(db.pool.clone()));
    let transfers_a = TransferService::new(repo_a);
    let transfers_b = TransferService::new(repo_b);
    let transfer_id = context.transfer.id;

    let (result_a, result_b) = tokio::join!(
        transfers_a.record_expected_chunk(transfer_id, ChunkIndex(0), 100, digest32(1)),
        transfers_b.record_expected_chunk(transfer_id, ChunkIndex(0), 100, digest32(2)),
    );

    let outcomes = [result_a, result_b];
    let successes = outcomes.iter().filter(|r| r.is_ok()).count();
    let conflicts = outcomes
        .iter()
        .filter(|r| matches!(r, Err(ApplicationError::ChunkRecord(_))))
        .count();
    assert_eq!(successes, 1, "exactly one concurrent write must win");
    assert_eq!(
        conflicts, 1,
        "the loser must observe a durable conflict, never a race"
    );

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(transfer_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.manifest.recorded_chunk_count(), 1);

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Sealing
// ---------------------------------------------------------------------

async fn sealed_single_chunk_context(
    services: &Services,
    endpoint_id: EndpointId,
    job_id: JobId,
    job_step_id: JobStepId,
) -> bamep_domain::TransferContext {
    let context = create_context(services, endpoint_id, job_id, job_step_id).await;
    services
        .transfers
        .record_expected_chunk(context.transfer.id, ChunkIndex(0), 100, digest32(1))
        .await
        .unwrap();
    context
}

#[tokio::test]
async fn sealing_persists_and_survives_reload() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-seal-01", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;

    let outcome = services
        .transfers
        .seal_manifest(context.transfer.id, 1, digest32(9))
        .await
        .unwrap();
    assert!(matches!(outcome, SealOutcome::Sealed(_)));

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert!(reloaded.manifest.sealed);
    assert_eq!(reloaded.manifest.chunk_count, Some(1));
    assert_eq!(
        reloaded.manifest.artifact_digest.unwrap().as_bytes(),
        digest32(9).as_slice()
    );

    db.teardown().await;
}

#[tokio::test]
async fn resealing_with_identical_facts_is_idempotent() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-seal-02", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;

    services
        .transfers
        .seal_manifest(context.transfer.id, 1, digest32(9))
        .await
        .unwrap();
    let second = services
        .transfers
        .seal_manifest(context.transfer.id, 1, digest32(9))
        .await
        .unwrap();

    assert_eq!(second, SealOutcome::AlreadySealed);

    db.teardown().await;
}

#[tokio::test]
async fn conflicting_reseal_is_rejected_and_original_facts_remain() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-seal-03", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;

    services
        .transfers
        .seal_manifest(context.transfer.id, 1, digest32(9))
        .await
        .unwrap();

    let err = services
        .transfers
        .seal_manifest(context.transfer.id, 1, digest32(8))
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Seal(_)));

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.manifest.artifact_digest.unwrap().as_bytes(),
        digest32(9).as_slice(),
        "sealed facts must remain exactly as first sealed"
    );

    db.teardown().await;
}

#[tokio::test]
async fn a_new_chunk_index_cannot_be_added_after_sealing() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-seal-04", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;
    services
        .transfers
        .seal_manifest(context.transfer.id, 1, digest32(9))
        .await
        .unwrap();

    let err = services
        .transfers
        .record_expected_chunk(context.transfer.id, ChunkIndex(1), 50, digest32(3))
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::ChunkRecord(_)));

    db.teardown().await;
}

#[tokio::test]
async fn two_concurrent_conflicting_seals_serialize_and_only_one_wins() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-seal-race-01", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;

    let transfers_a =
        TransferService::new(Arc::new(PostgresTransferRepository::new(db.pool.clone())));
    let transfers_b =
        TransferService::new(Arc::new(PostgresTransferRepository::new(db.pool.clone())));
    let transfer_id = context.transfer.id;

    let (result_a, result_b) = tokio::join!(
        transfers_a.seal_manifest(transfer_id, 1, digest32(9)),
        transfers_b.seal_manifest(transfer_id, 1, digest32(8)),
    );

    let outcomes = [result_a, result_b];
    let successes = outcomes.iter().filter(|r| r.is_ok()).count();
    let conflicts = outcomes
        .iter()
        .filter(|r| matches!(r, Err(ApplicationError::Seal(_))))
        .count();
    assert_eq!(successes, 1);
    assert_eq!(conflicts, 1);

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Held / verified chunk acceptance
// ---------------------------------------------------------------------

#[tokio::test]
async fn accepting_a_verified_chunk_marks_it_held_and_survives_reload() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-held-01", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;

    let outcome = services
        .transfers
        .accept_verified_chunk(context.transfer.id, ChunkIndex(0), digest32(1))
        .await
        .unwrap();
    assert_eq!(outcome, AcceptChunkOutcome::Accepted);

    db.pool.close().await;
    let reloaded_pool = bamep_server::adapters::postgres::connect(&db.db_url)
        .await
        .unwrap();
    let reloaded_transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(
        reloaded_pool.clone(),
    )));
    let (_reloaded, held) = reloaded_transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(held, BTreeSet::from([ChunkIndex(0)]));

    reloaded_pool.close().await;
    db.teardown().await;
}

#[tokio::test]
async fn accepting_the_same_verified_chunk_twice_is_idempotent() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-held-02", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;

    services
        .transfers
        .accept_verified_chunk(context.transfer.id, ChunkIndex(0), digest32(1))
        .await
        .unwrap();
    let second = services
        .transfers
        .accept_verified_chunk(context.transfer.id, ChunkIndex(0), digest32(1))
        .await
        .unwrap();

    assert_eq!(second, AcceptChunkOutcome::AlreadyHeld);

    db.teardown().await;
}

#[tokio::test]
async fn accepting_a_chunk_with_a_mismatched_digest_is_rejected_and_never_marked_held() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-held-03", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;

    let err = services
        .transfers
        .accept_verified_chunk(context.transfer.id, ChunkIndex(0), digest32(99))
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::ChunkAccept(_)));

    let (_reloaded, held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        held.is_empty(),
        "invalid bytes must never become valid held state"
    );

    db.teardown().await;
}

#[tokio::test]
async fn accepting_an_unknown_chunk_index_is_rejected() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-held-04", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    let err = services
        .transfers
        .accept_verified_chunk(context.transfer.id, ChunkIndex(0), digest32(1))
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::ChunkAccept(_)));

    db.teardown().await;
}

#[tokio::test]
async fn two_concurrent_identical_verified_chunk_acceptances_never_double_write() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-held-race-01", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;

    let transfers_a =
        TransferService::new(Arc::new(PostgresTransferRepository::new(db.pool.clone())));
    let transfers_b =
        TransferService::new(Arc::new(PostgresTransferRepository::new(db.pool.clone())));
    let transfer_id = context.transfer.id;

    let (result_a, result_b) = tokio::join!(
        transfers_a.accept_verified_chunk(transfer_id, ChunkIndex(0), digest32(1)),
        transfers_b.accept_verified_chunk(transfer_id, ChunkIndex(0), digest32(1)),
    );

    let outcomes = [result_a.unwrap(), result_b.unwrap()];
    let accepted = outcomes
        .iter()
        .filter(|o| **o == AcceptChunkOutcome::Accepted)
        .count();
    let already_held = outcomes
        .iter()
        .filter(|o| **o == AcceptChunkOutcome::AlreadyHeld)
        .count();
    assert_eq!(accepted, 1);
    assert_eq!(already_held, 1);

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Artifact lifecycle
// ---------------------------------------------------------------------

async fn sealed_and_held_context(
    services: &Services,
    endpoint_id: EndpointId,
    job_id: JobId,
    job_step_id: JobStepId,
) -> bamep_domain::TransferContext {
    let context = sealed_single_chunk_context(services, endpoint_id, job_id, job_step_id).await;
    services
        .transfers
        .seal_manifest(context.transfer.id, 1, digest32(9))
        .await
        .unwrap();
    services
        .transfers
        .accept_verified_chunk(context.transfer.id, ChunkIndex(0), digest32(1))
        .await
        .unwrap();
    context
}

#[tokio::test]
async fn begin_verification_requires_sealed_manifest_and_every_chunk_held() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-artifact-01", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;
    services
        .transfers
        .seal_manifest(context.transfer.id, 1, digest32(9))
        .await
        .unwrap();

    // Chunk not yet held.
    let err = services
        .transfers
        .begin_artifact_verification(context.transfer.id)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::ArtifactTransition(_)));

    services
        .transfers
        .accept_verified_chunk(context.transfer.id, ChunkIndex(0), digest32(1))
        .await
        .unwrap();

    let artifact = services
        .transfers
        .begin_artifact_verification(context.transfer.id)
        .await
        .unwrap();
    assert_eq!(artifact.state, ArtifactState::PendingVerification);

    db.teardown().await;
}

#[tokio::test]
async fn missing_seal_prevents_pending_verification() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-artifact-02", now).await;
    let context = sealed_single_chunk_context(&services, endpoint_id, job_id, job_step_id).await;
    // Deliberately never sealed.

    let err = services
        .transfers
        .begin_artifact_verification(context.transfer.id)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::ArtifactTransition(_)));

    db.teardown().await;
}

#[tokio::test]
async fn full_digest_success_reaches_verified() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-artifact-03", now).await;
    let context = sealed_and_held_context(&services, endpoint_id, job_id, job_step_id).await;
    services
        .transfers
        .begin_artifact_verification(context.transfer.id)
        .await
        .unwrap();

    let artifact = services
        .transfers
        .complete_artifact_verification(context.transfer.id, true)
        .await
        .unwrap();
    assert_eq!(artifact.state, ArtifactState::Verified);

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.artifact.state, ArtifactState::Verified);

    db.teardown().await;
}

#[tokio::test]
async fn full_digest_failure_reaches_failed() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-artifact-04", now).await;
    let context = sealed_and_held_context(&services, endpoint_id, job_id, job_step_id).await;
    services
        .transfers
        .begin_artifact_verification(context.transfer.id)
        .await
        .unwrap();

    let artifact = services
        .transfers
        .complete_artifact_verification(context.transfer.id, false)
        .await
        .unwrap();
    assert_eq!(artifact.state, ArtifactState::Failed);

    db.teardown().await;
}

#[tokio::test]
async fn incomplete_to_failed_transition_persists() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-artifact-05", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    let artifact = services
        .transfers
        .fail_incomplete_artifact(context.transfer.id)
        .await
        .unwrap();
    assert_eq!(artifact.state, ArtifactState::Failed);

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.artifact.state, ArtifactState::Failed);

    db.teardown().await;
}

#[tokio::test]
async fn terminal_artifact_rejects_further_transitions() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-artifact-06", now).await;
    let context = sealed_and_held_context(&services, endpoint_id, job_id, job_step_id).await;
    services
        .transfers
        .begin_artifact_verification(context.transfer.id)
        .await
        .unwrap();
    services
        .transfers
        .complete_artifact_verification(context.transfer.id, true)
        .await
        .unwrap();

    let err = services
        .transfers
        .fail_incomplete_artifact(context.transfer.id)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::ArtifactTransition(_)));

    let err = services
        .transfers
        .complete_artifact_verification(context.transfer.id, false)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::ArtifactTransition(_)));

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.artifact.state,
        ArtifactState::Verified,
        "a terminal Artifact must never be rewritten by a later rejected transition"
    );

    db.teardown().await;
}

#[tokio::test]
async fn capture_consistency_remains_not_established_on_a_verified_artifact() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-artifact-07", now).await;
    let context = sealed_and_held_context(&services, endpoint_id, job_id, job_step_id).await;
    services
        .transfers
        .begin_artifact_verification(context.transfer.id)
        .await
        .unwrap();
    services
        .transfers
        .complete_artifact_verification(context.transfer.id, true)
        .await
        .unwrap();

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.artifact.state, ArtifactState::Verified);
    assert_eq!(
        reloaded.artifact.capture_consistency,
        CaptureConsistency::NotEstablished,
        "Verified must never imply capture_consistency == Established"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Transfer -> Attempt one-time binding
// ---------------------------------------------------------------------

#[tokio::test]
async fn binding_an_attempt_persists_and_survives_reload() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-bind-01", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;
    let attempt = insert_attempt(&db.pool, job_step_id).await;

    let bound = services
        .transfers
        .bind_attempt(context.transfer.id, attempt)
        .await
        .unwrap();
    assert_eq!(bound.attempt_id, Some(attempt.id));

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.transfer.attempt_id, Some(attempt.id));
    assert_eq!(
        reloaded.transfer.id, context.transfer.id,
        "binding must never change TransferId"
    );
    assert_eq!(
        reloaded.transfer.artifact_id, context.transfer.artifact_id,
        "binding must never change ArtifactId"
    );

    db.teardown().await;
}

#[tokio::test]
async fn rebinding_the_same_attempt_is_idempotent() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-bind-02", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;
    let attempt = insert_attempt(&db.pool, job_step_id).await;

    services
        .transfers
        .bind_attempt(context.transfer.id, attempt)
        .await
        .unwrap();
    let second = services
        .transfers
        .bind_attempt(context.transfer.id, attempt)
        .await
        .unwrap();
    assert_eq!(second.attempt_id, Some(attempt.id));

    db.teardown().await;
}

#[tokio::test]
async fn conflicting_rebind_to_a_different_attempt_is_rejected_and_rolled_back() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-bind-03", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;
    let first_attempt = insert_attempt(&db.pool, job_step_id).await;
    let second_attempt = insert_attempt(&db.pool, job_step_id).await;

    services
        .transfers
        .bind_attempt(context.transfer.id, first_attempt)
        .await
        .unwrap();

    let err = services
        .transfers
        .bind_attempt(context.transfer.id, second_attempt)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::TransferBinding(_)));

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.transfer.attempt_id,
        Some(first_attempt.id),
        "a conflicting rebind must never overwrite the original binding"
    );

    db.teardown().await;
}

#[tokio::test]
async fn binding_an_attempt_from_a_different_job_step_is_rejected() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) = workflow_context(&services, "xfer-bind-04", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;
    // A real, durably persisted JobStep — just not this Transfer's own —
    // so the FK on `attempts.job_step_id` is satisfied and the rejection
    // below is provably the Domain correlation check, not a foreign-key
    // failure.
    let other_job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let foreign_attempt = insert_attempt(&db.pool, other_job.steps[0].id).await;

    // The FK on transfers.attempt_id would itself reject this at commit if
    // it slipped past the Domain check; assert the Domain rejection fires
    // first, before any write is attempted.
    let err = services
        .transfers
        .bind_attempt(context.transfer.id, foreign_attempt)
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::TransferBinding(_)));

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.transfer.attempt_id, None);

    db.teardown().await;
}

#[tokio::test]
async fn two_concurrent_conflicting_attempt_bindings_serialize_and_only_one_wins() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-bind-race-01", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;
    let attempt_a = insert_attempt(&db.pool, job_step_id).await;
    let attempt_b = insert_attempt(&db.pool, job_step_id).await;

    let transfers_a =
        TransferService::new(Arc::new(PostgresTransferRepository::new(db.pool.clone())));
    let transfers_b =
        TransferService::new(Arc::new(PostgresTransferRepository::new(db.pool.clone())));
    let transfer_id = context.transfer.id;

    let (result_a, result_b) = tokio::join!(
        transfers_a.bind_attempt(transfer_id, attempt_a),
        transfers_b.bind_attempt(transfer_id, attempt_b),
    );

    let outcomes = [result_a, result_b];
    let successes = outcomes.iter().filter(|r| r.is_ok()).count();
    let conflicts = outcomes
        .iter()
        .filter(|r| matches!(r, Err(ApplicationError::TransferBinding(_))))
        .count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent binding attempt must win"
    );
    assert_eq!(conflicts, 1);

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Forced failure / atomicity
// ---------------------------------------------------------------------

#[tokio::test]
async fn forced_chunk_identity_failure_leaves_no_partial_state() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-atomic-01", now).await;
    let context = create_context(&services, endpoint_id, job_id, job_step_id).await;

    sqlx::query(
        "CREATE FUNCTION reject_chunk_identity_insert() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'forced chunk identity failure'; END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_chunk_identity_insert BEFORE INSERT ON chunk_identities \
         FOR EACH ROW EXECUTE FUNCTION reject_chunk_identity_insert()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let err = services
        .transfers
        .record_expected_chunk(context.transfer.id, ChunkIndex(0), 100, digest32(1))
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    sqlx::query("DROP TRIGGER reject_chunk_identity_insert ON chunk_identities")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION reject_chunk_identity_insert")
        .execute(&db.pool)
        .await
        .unwrap();

    let (reloaded, _held) = services
        .transfers
        .find_transfer_context(context.transfer.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.manifest.recorded_chunk_count(),
        0,
        "a forced failure must leave no partial chunk-identity row"
    );

    db.teardown().await;
}

#[tokio::test]
async fn forced_transfer_creation_failure_leaves_no_partial_transfer_or_artifact() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let (endpoint_id, job_id, job_step_id) =
        workflow_context(&services, "xfer-atomic-02", now).await;

    sqlx::query(
        "CREATE FUNCTION reject_transfer_insert() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'forced transfer insert failure'; END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_transfer_insert BEFORE INSERT ON transfers \
         FOR EACH ROW EXECUTE FUNCTION reject_transfer_insert()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let err = services
        .transfers
        .create_transfer_context(
            endpoint_id,
            job_id,
            job_step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            bamep_domain::ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    sqlx::query("DROP TRIGGER reject_transfer_insert ON transfers")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION reject_transfer_insert")
        .execute(&db.pool)
        .await
        .unwrap();

    let artifact_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        artifact_count, 0,
        "the Artifact row inserted before the failing Transfer insert must roll back too"
    );

    db.teardown().await;
}
