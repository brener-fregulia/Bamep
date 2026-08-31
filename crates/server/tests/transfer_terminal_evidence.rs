//! Issue #19 checkpoint C2 — Server terminal-result integration for
//! `bamep.m1.data-plane-transfer`.
//!
//! Real PostgreSQL. The owning action is classified from durable Server facts
//! (a bound `Transfer`, Issue #40), never from `ActionResult.detail`; CASE A
//! (`TRANSFER_VERIFIED`) commits workflow success only after independently
//! confirming a durable `Verified` Artifact; CASE B
//! (`ARTIFACT_VERIFICATION_FAILED`) requires a durable `Failed` Artifact and
//! performs no further Artifact transition; CASE C
//! (`CHUNK_VERIFICATION_FAILED` / `TRANSFER_ABANDONED`) drives
//! `Artifact Incomplete -> Failed` **atomically with** the terminal
//! Attempt/JobStep/Job transition
//! (`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle").

#![cfg(unix)]

mod support;

use std::sync::Arc;

use bamep_agent_protocol::{
    decode, encode, ActionResultMessage, ActionResultOutcome, AgentProtocolMessage,
    AuthRequestMessage, ProtocolId,
};
use bamep_domain::{ChunkIndex, EndpointId, TransferId};
use bamep_server::adapters::agent_gateway::{AgentControlGateway, HandshakeOutcome};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository, PostgresTransferRepository,
};
use bamep_server::application::{
    parse_transfer_result_detail, ActionEvidenceService, BootOrchestrationService,
    BootstrapEvidenceService, EnrollmentService, TransferActionClassification, TransferResultCode,
    TransferResultDetailError, TransferService, TransferTerminalEvidenceService,
    TransferTerminalOutcome,
};
use bamep_server::ports::JobRepository;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::TechnicalResourceArbiter;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use sqlx::{PgPool, Row};
use support::{dispatched_transfer_fixture, DispatchedTransfer, TestDatabase};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const DIGEST_A: [u8; 32] = [0xAB; 32];
const ARTIFACT_DIGEST: [u8; 32] = [0xCD; 32];

// ---------------------------------------------------------------------
// fixture + helpers
// ---------------------------------------------------------------------

/// The durable `bamep.m1.data-plane-transfer` state a terminal `ActionResult`
/// arrives against, plus the owning Job/JobStep/Attempt ids for assertions.
struct Fixture {
    base: DispatchedTransfer,
    job_id: uuid::Uuid,
    step_id: uuid::Uuid,
    attempt_id: uuid::Uuid,
}

impl Fixture {
    async fn create(pool: &PgPool, signal: &str) -> Self {
        let base = dispatched_transfer_fixture(pool, signal).await;
        let row = sqlx::query(
            "SELECT s.job_id, s.id AS step_id, a.id AS attempt_id \
             FROM attempts a JOIN job_steps s ON s.id = a.job_step_id \
             WHERE a.action_id = $1",
        )
        .bind(base.action_id.as_uuid())
        .fetch_one(pool)
        .await
        .unwrap();
        Self {
            job_id: row.get("job_id"),
            step_id: row.get("step_id"),
            attempt_id: row.get("attempt_id"),
            base,
        }
    }

    fn transfers(&self, pool: &PgPool) -> TransferService<PostgresTransferRepository> {
        TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())))
    }

    /// `Incomplete -> PendingVerification -> Verified` through the production
    /// #36/#39 Application/Adapter path (a sealed 1-chunk manifest, the chunk
    /// durably held, then an independently-computed digest match).
    async fn drive_artifact_verified(&self, pool: &PgPool) {
        self.drive_to_pending_verification(pool).await;
        self.transfers(pool)
            .complete_artifact_verification(TransferId(self.base.transfer_id.0), true)
            .await
            .unwrap();
    }

    /// `... -> PendingVerification -> Failed` (a digest mismatch) — the CASE B
    /// precondition, committed before the Agent's `ActionResult`.
    async fn drive_artifact_verification_failed(&self, pool: &PgPool) {
        self.drive_to_pending_verification(pool).await;
        self.transfers(pool)
            .complete_artifact_verification(TransferId(self.base.transfer_id.0), false)
            .await
            .unwrap();
    }

    async fn drive_to_pending_verification(&self, pool: &PgPool) {
        let transfers = self.transfers(pool);
        let transfer_id = TransferId(self.base.transfer_id.0);
        transfers
            .record_expected_chunk(transfer_id, ChunkIndex(0), 10, DIGEST_A.to_vec())
            .await
            .unwrap();
        transfers
            .accept_verified_chunk(transfer_id, ChunkIndex(0), DIGEST_A.to_vec())
            .await
            .unwrap();
        transfers
            .seal_manifest(transfer_id, 1, ARTIFACT_DIGEST.to_vec())
            .await
            .unwrap();
        transfers
            .begin_artifact_verification(transfer_id)
            .await
            .unwrap();
    }

    fn service(&self, pool: &PgPool) -> Arc<TransferTerminalEvidenceService> {
        build_transfer_terminal_service(pool.clone())
    }
}

fn build_transfer_terminal_service(pool: PgPool) -> Arc<TransferTerminalEvidenceService> {
    Arc::new(TransferTerminalEvidenceService::new(
        Arc::new(PostgresJobRepository::new(pool)) as Arc<dyn JobRepository>,
        Arc::new(AttemptReservationRegistry::new()),
        Arc::new(TechnicalResourceArbiter::new([])),
    ))
}

fn detail(code: &str, artifact_id: uuid::Uuid) -> Map<String, Value> {
    json!({ "code": code, "artifact_id": artifact_id.to_string() })
        .as_object()
        .unwrap()
        .clone()
}

async fn artifact_state(pool: &PgPool, transfer_id: uuid::Uuid) -> String {
    sqlx::query_scalar(
        "SELECT ar.state::text FROM artifacts ar \
         JOIN transfers t ON t.artifact_id = ar.id WHERE t.id = $1",
    )
    .bind(transfer_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn attempt_state(pool: &PgPool, attempt_id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state::text FROM attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn job_step_state(pool: &PgPool, step_id: uuid::Uuid) -> (String, Option<String>) {
    let row = sqlx::query("SELECT state::text, failure_reason::text FROM job_steps WHERE id = $1")
        .bind(step_id)
        .fetch_one(pool)
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

async fn job_state(pool: &PgPool, job_id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state::text FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn event_count(pool: &PgPool, job_id: uuid::Uuid, event_type: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM domain_events WHERE job_id = $1 AND event_type::text = $2",
    )
    .bind(job_id)
    .bind(event_type)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------
// §26 — action-kind resolution from durable Server facts
// ---------------------------------------------------------------------

#[tokio::test]
async fn classify_resolves_a_bound_transfer_action_as_data_plane_transfer() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-classify-transfer").await;
    let svc = fixture.service(&db.pool);

    let kind = svc
        .classify(fixture.base.action_id, fixture.base.endpoint_id)
        .await
        .unwrap();
    assert_eq!(kind, TransferActionClassification::DataPlaneTransfer);

    // A detail payload cannot spoof the kind: an unknown action is Unknown
    // regardless of what an ActionResult would claim.
    let unknown = svc
        .classify(ProtocolId::generate(), fixture.base.endpoint_id)
        .await
        .unwrap();
    assert_eq!(unknown, TransferActionClassification::Unknown);

    // Foreign endpoint -> indistinguishable from unknown.
    let foreign = svc
        .classify(fixture.base.action_id, EndpointId::new())
        .await
        .unwrap();
    assert_eq!(foreign, TransferActionClassification::Unknown);

    db.teardown().await;
}

#[tokio::test]
async fn classify_resolves_an_attempt_with_no_bound_transfer_as_simulated_execution() {
    // An Attempt that exists for this Endpoint but has no bound Transfer is
    // the RF-004 `bamep.m1.simulated-execution` shape as far as the durable
    // discriminator is concerned. (Unbinding an existing transfer fixture's
    // Attempt is the smallest way to produce that row shape.)
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-classify-rf004").await;
    sqlx::query("UPDATE transfers SET attempt_id = NULL WHERE id = $1")
        .bind(fixture.base.transfer_id.0)
        .execute(&db.pool)
        .await
        .unwrap();
    let svc = fixture.service(&db.pool);

    assert_eq!(
        svc.classify(fixture.base.action_id, fixture.base.endpoint_id)
            .await
            .unwrap(),
        TransferActionClassification::SimulatedExecution
    );
    let repo = PostgresJobRepository::new(db.pool.clone());
    assert!(!repo
        .action_has_bound_transfer(bamep_domain::ActionId(fixture.base.action_id.as_uuid()))
        .await
        .unwrap());

    // A transfer-detail payload therefore cannot spoof the kind: `classify`
    // never reads `ActionResult.detail` (its signature carries none), and the
    // gateway routes this action_id to the RF-004 path, whose
    // `m1_result_detail_matches` check rejects a `TRANSFER_VERIFIED` code.
    assert!(!bamep_server::application::m1_result_detail_matches(
        ActionResultOutcome::Succeeded,
        &detail("TRANSFER_VERIFIED", fixture.base.artifact_id.0),
    ));

    db.teardown().await;
}

// ---------------------------------------------------------------------
// §27 / §5 — CASE A: TRANSFER_VERIFIED happy path
// ---------------------------------------------------------------------

#[tokio::test]
async fn transfer_verified_against_a_durably_verified_artifact_commits_workflow_success() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-verified-happy").await;
    fixture.drive_artifact_verified(&db.pool).await;
    let svc = fixture.service(&db.pool);

    let parsed = parse_transfer_result_detail(
        ActionResultOutcome::Succeeded,
        &detail("TRANSFER_VERIFIED", fixture.base.artifact_id.0),
    )
    .unwrap();
    let outcome = svc
        .apply(fixture.base.action_id, fixture.base.endpoint_id, parsed)
        .await
        .unwrap();
    assert_eq!(outcome, TransferTerminalOutcome::Consumed);

    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Verified"
    );
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt_id).await,
        "Succeeded"
    );
    assert_eq!(
        job_step_state(&db.pool, fixture.step_id).await.0,
        "Succeeded"
    );
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Succeeded");
    assert_eq!(
        event_count(&db.pool, fixture.job_id, "JobSucceeded").await,
        1
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// §24 / §28 — CASE A false success: Artifact not Verified
// ---------------------------------------------------------------------

#[tokio::test]
async fn transfer_verified_fails_closed_against_every_non_verified_artifact_state() {
    for (idx, state) in ["Incomplete", "PendingVerification", "Failed"]
        .iter()
        .enumerate()
    {
        let db = TestDatabase::setup().await;
        let fixture = Fixture::create(&db.pool, &format!("c2-false-success-{idx}")).await;
        match *state {
            "Incomplete" => {}
            "PendingVerification" => fixture.drive_to_pending_verification(&db.pool).await,
            "Failed" => fixture.drive_artifact_verification_failed(&db.pool).await,
            _ => unreachable!(),
        }
        let svc = fixture.service(&db.pool);

        let parsed = parse_transfer_result_detail(
            ActionResultOutcome::Succeeded,
            &detail("TRANSFER_VERIFIED", fixture.base.artifact_id.0),
        )
        .unwrap();
        let outcome = svc
            .apply(fixture.base.action_id, fixture.base.endpoint_id, parsed)
            .await
            .unwrap();
        assert_eq!(outcome, TransferTerminalOutcome::FailClosed, "{state}");

        assert_eq!(
            artifact_state(&db.pool, fixture.base.transfer_id.0).await,
            *state
        );
        assert_eq!(
            attempt_state(&db.pool, fixture.attempt_id).await,
            "InProgress",
            "{state}"
        );
        assert_eq!(
            job_step_state(&db.pool, fixture.step_id).await.0,
            "Dispatching",
            "{state}"
        );
        assert_eq!(
            job_state(&db.pool, fixture.job_id).await,
            "Running",
            "{state}"
        );
        assert_eq!(
            event_count(&db.pool, fixture.job_id, "JobSucceeded").await,
            0
        );

        db.teardown().await;
    }
}

// ---------------------------------------------------------------------
// §29 — wrong artifact_id: cross-transfer Artifact substitution fails closed
// ---------------------------------------------------------------------

#[tokio::test]
async fn transfer_verified_with_a_foreign_artifact_id_fails_closed() {
    let db = TestDatabase::setup().await;
    let bound = Fixture::create(&db.pool, "c2-wrong-artifact-bound").await;
    let other = Fixture::create(&db.pool, "c2-wrong-artifact-other").await;
    other.drive_artifact_verified(&db.pool).await; // a genuinely Verified Artifact B
    let svc = bound.service(&db.pool);

    let parsed = parse_transfer_result_detail(
        ActionResultOutcome::Succeeded,
        // claim B's artifact_id for bound's action
        &detail("TRANSFER_VERIFIED", other.base.artifact_id.0),
    )
    .unwrap();
    let outcome = svc
        .apply(bound.base.action_id, bound.base.endpoint_id, parsed)
        .await
        .unwrap();
    assert_eq!(outcome, TransferTerminalOutcome::FailClosed);

    assert_eq!(
        attempt_state(&db.pool, bound.attempt_id).await,
        "InProgress"
    );
    assert_eq!(job_state(&db.pool, bound.job_id).await, "Running");
    // Artifact B (Verified, owned by another transfer) is untouched.
    assert_eq!(
        artifact_state(&db.pool, other.base.transfer_id.0).await,
        "Verified"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// §30 — CASE B: ARTIFACT_VERIFICATION_FAILED
// ---------------------------------------------------------------------

#[tokio::test]
async fn artifact_verification_failed_against_a_durably_failed_artifact_fails_workflow() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-case-b").await;
    fixture.drive_artifact_verification_failed(&db.pool).await;
    let svc = fixture.service(&db.pool);

    let parsed = parse_transfer_result_detail(
        ActionResultOutcome::Failed,
        &detail("ARTIFACT_VERIFICATION_FAILED", fixture.base.artifact_id.0),
    )
    .unwrap();
    let outcome = svc
        .apply(fixture.base.action_id, fixture.base.endpoint_id, parsed)
        .await
        .unwrap();
    assert_eq!(outcome, TransferTerminalOutcome::Consumed);

    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Failed"
    );
    assert_eq!(attempt_state(&db.pool, fixture.attempt_id).await, "Failed");
    let (step_state, failure_reason) = job_step_state(&db.pool, fixture.step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(failure_reason.as_deref(), Some("ExecutionFailed"));
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Failed");
    assert_eq!(event_count(&db.pool, fixture.job_id, "JobFailed").await, 1);

    db.teardown().await;
}

#[tokio::test]
async fn artifact_verification_failed_against_a_non_failed_artifact_fails_closed() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-case-b-wrong-state").await;
    fixture.drive_artifact_verified(&db.pool).await; // Verified, not Failed
    let svc = fixture.service(&db.pool);

    let parsed = parse_transfer_result_detail(
        ActionResultOutcome::Failed,
        &detail("ARTIFACT_VERIFICATION_FAILED", fixture.base.artifact_id.0),
    )
    .unwrap();
    let outcome = svc
        .apply(fixture.base.action_id, fixture.base.endpoint_id, parsed)
        .await
        .unwrap();
    assert_eq!(outcome, TransferTerminalOutcome::FailClosed);
    // Terminal Artifact immutability: Verified is never rewritten to Failed.
    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Verified"
    );
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt_id).await,
        "InProgress"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// §31 / §32 — CASE C: CHUNK_VERIFICATION_FAILED / TRANSFER_ABANDONED atomicity
// ---------------------------------------------------------------------

#[tokio::test]
async fn case_c_drives_incomplete_to_failed_atomically_with_workflow_failure() {
    for (idx, code) in ["CHUNK_VERIFICATION_FAILED", "TRANSFER_ABANDONED"]
        .iter()
        .enumerate()
    {
        let db = TestDatabase::setup().await;
        let fixture = Fixture::create(&db.pool, &format!("c2-case-c-{idx}")).await;
        assert_eq!(
            artifact_state(&db.pool, fixture.base.transfer_id.0).await,
            "Incomplete"
        );
        let svc = fixture.service(&db.pool);

        let parsed = parse_transfer_result_detail(
            ActionResultOutcome::Failed,
            &detail(code, fixture.base.artifact_id.0),
        )
        .unwrap();
        let outcome = svc
            .apply(fixture.base.action_id, fixture.base.endpoint_id, parsed)
            .await
            .unwrap();
        assert_eq!(outcome, TransferTerminalOutcome::Consumed, "{code}");

        assert_eq!(
            artifact_state(&db.pool, fixture.base.transfer_id.0).await,
            "Failed",
            "{code}"
        );
        assert_eq!(
            attempt_state(&db.pool, fixture.attempt_id).await,
            "Failed",
            "{code}"
        );
        let (step_state, failure_reason) = job_step_state(&db.pool, fixture.step_id).await;
        assert_eq!(step_state, "Failed", "{code}");
        assert_eq!(failure_reason.as_deref(), Some("ExecutionFailed"), "{code}");
        assert_eq!(
            job_state(&db.pool, fixture.job_id).await,
            "Failed",
            "{code}"
        );
        assert_eq!(
            event_count(&db.pool, fixture.job_id, "JobStepFailed").await,
            1,
            "{code}"
        );
        assert_eq!(
            event_count(&db.pool, fixture.job_id, "JobFailed").await,
            1,
            "{code}"
        );

        db.teardown().await;
    }
}

#[tokio::test]
async fn case_c_transaction_failure_at_the_audit_write_leaves_no_partial_state() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-case-c-atomic-audit").await;
    let svc = fixture.service(&db.pool);

    // A trigger that aborts the terminal-audit INSERT — the last write in the
    // CASE C transaction, after both the Artifact and workflow mutations.
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

    let parsed = parse_transfer_result_detail(
        ActionResultOutcome::Failed,
        &detail("CHUNK_VERIFICATION_FAILED", fixture.base.artifact_id.0),
    )
    .unwrap();
    let err = svc
        .apply(fixture.base.action_id, fixture.base.endpoint_id, parsed)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        bamep_server::application::ApplicationError::Repository(_)
    ));

    // Neither side is durably committed.
    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Incomplete"
    );
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt_id).await,
        "InProgress"
    );
    assert_eq!(
        job_step_state(&db.pool, fixture.step_id).await.0,
        "Dispatching"
    );
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Running");
    assert_eq!(event_count(&db.pool, fixture.job_id, "JobFailed").await, 0);

    // An idempotent resend (trigger dropped) then completes it.
    sqlx::query("DROP TRIGGER reject_terminal_audit ON audit_records")
        .execute(&db.pool)
        .await
        .unwrap();
    let parsed = parse_transfer_result_detail(
        ActionResultOutcome::Failed,
        &detail("CHUNK_VERIFICATION_FAILED", fixture.base.artifact_id.0),
    )
    .unwrap();
    let outcome = svc
        .apply(fixture.base.action_id, fixture.base.endpoint_id, parsed)
        .await
        .unwrap();
    assert_eq!(outcome, TransferTerminalOutcome::Consumed);
    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Failed"
    );
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Failed");

    db.teardown().await;
}

#[tokio::test]
async fn case_c_transaction_failure_at_the_artifact_write_leaves_no_partial_state() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-case-c-atomic-artifact").await;
    let svc = fixture.service(&db.pool);

    // Abort the `Incomplete -> Failed` Artifact UPDATE — the first write in
    // the CASE C transaction.
    sqlx::query(
        "CREATE FUNCTION reject_artifact_fail() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF OLD.state = 'Incomplete' AND NEW.state = 'Failed' THEN \
         RAISE EXCEPTION 'forced artifact fail'; END IF; RETURN NEW; END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_artifact_fail BEFORE UPDATE ON artifacts \
         FOR EACH ROW EXECUTE FUNCTION reject_artifact_fail()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let parsed = parse_transfer_result_detail(
        ActionResultOutcome::Failed,
        &detail("TRANSFER_ABANDONED", fixture.base.artifact_id.0),
    )
    .unwrap();
    let err = svc
        .apply(fixture.base.action_id, fixture.base.endpoint_id, parsed)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        bamep_server::application::ApplicationError::Repository(_)
    ));

    // The workflow is not durably failed either.
    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Incomplete"
    );
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt_id).await,
        "InProgress"
    );
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Running");

    db.teardown().await;
}

// ---------------------------------------------------------------------
// §33 — CASE C duplicate is idempotent
// ---------------------------------------------------------------------

#[tokio::test]
async fn case_c_matching_duplicate_is_an_idempotent_no_op() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-case-c-dup").await;
    let svc = fixture.service(&db.pool);

    let make = || {
        parse_transfer_result_detail(
            ActionResultOutcome::Failed,
            &detail("CHUNK_VERIFICATION_FAILED", fixture.base.artifact_id.0),
        )
        .unwrap()
    };

    assert_eq!(
        svc.apply(fixture.base.action_id, fixture.base.endpoint_id, make())
            .await
            .unwrap(),
        TransferTerminalOutcome::Consumed
    );
    let step_events_after_first = event_count(&db.pool, fixture.job_id, "JobStepFailed").await;

    // Exact same terminal evidence again -> no second state change, no
    // overwrite, no rejection just because the Artifact is already Failed.
    assert_eq!(
        svc.apply(fixture.base.action_id, fixture.base.endpoint_id, make())
            .await
            .unwrap(),
        TransferTerminalOutcome::Consumed
    );
    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Failed"
    );
    assert_eq!(attempt_state(&db.pool, fixture.attempt_id).await, "Failed");
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Failed");
    assert_eq!(
        event_count(&db.pool, fixture.job_id, "JobStepFailed").await,
        step_events_after_first,
        "a matching duplicate must emit no second event"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// §34 / §18 — conflicting late terminal evidence never overwrites
// ---------------------------------------------------------------------

#[tokio::test]
async fn conflicting_transfer_verified_after_committed_case_c_failure_never_overwrites() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-conflict").await;
    let svc = fixture.service(&db.pool);

    svc.apply(
        fixture.base.action_id,
        fixture.base.endpoint_id,
        parse_transfer_result_detail(
            ActionResultOutcome::Failed,
            &detail("TRANSFER_ABANDONED", fixture.base.artifact_id.0),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Failed");

    // A late, conflicting success — the first committed terminal outcome wins.
    let outcome = svc
        .apply(
            fixture.base.action_id,
            fixture.base.endpoint_id,
            parse_transfer_result_detail(
                ActionResultOutcome::Succeeded,
                &detail("TRANSFER_VERIFIED", fixture.base.artifact_id.0),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, TransferTerminalOutcome::Consumed); // ignored, not applied
    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Failed"
    );
    assert_eq!(attempt_state(&db.pool, fixture.attempt_id).await, "Failed");
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Failed");
    assert_eq!(
        event_count(&db.pool, fixture.job_id, "JobSucceeded").await,
        0
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// §35 — terminal Artifact immutability
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_verified_artifact_is_never_driven_to_failed_by_a_case_c_result() {
    for (idx, code) in ["CHUNK_VERIFICATION_FAILED", "TRANSFER_ABANDONED"]
        .iter()
        .enumerate()
    {
        let db = TestDatabase::setup().await;
        let fixture = Fixture::create(&db.pool, &format!("c2-immutable-{idx}")).await;
        fixture.drive_artifact_verified(&db.pool).await;
        let svc = fixture.service(&db.pool);

        let outcome = svc
            .apply(
                fixture.base.action_id,
                fixture.base.endpoint_id,
                parse_transfer_result_detail(
                    ActionResultOutcome::Failed,
                    &detail(code, fixture.base.artifact_id.0),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outcome, TransferTerminalOutcome::FailClosed, "{code}");
        assert_eq!(
            artifact_state(&db.pool, fixture.base.transfer_id.0).await,
            "Verified",
            "{code}"
        );
        assert_eq!(
            attempt_state(&db.pool, fixture.attempt_id).await,
            "InProgress",
            "{code}"
        );

        db.teardown().await;
    }
}

// ---------------------------------------------------------------------
// §19 / §38 — CASE C while the Job is Cancelling composes atomically
// ---------------------------------------------------------------------

#[tokio::test]
async fn transfer_abandoned_while_cancelling_ends_the_job_cancelled_and_fails_the_artifact() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-abandon-while-cancelling").await;

    // Drive the Job to `Cancelling` directly (Issue #27's authoritative
    // transition), leaving the Attempt InProgress — the exact
    // "cancellation races active work" window.
    sqlx::query("UPDATE jobs SET state = 'Cancelling' WHERE id = $1")
        .bind(fixture.job_id)
        .execute(&db.pool)
        .await
        .unwrap();

    let svc = fixture.service(&db.pool);
    let outcome = svc
        .apply(
            fixture.base.action_id,
            fixture.base.endpoint_id,
            parse_transfer_result_detail(
                ActionResultOutcome::Failed,
                &detail("TRANSFER_ABANDONED", fixture.base.artifact_id.0),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, TransferTerminalOutcome::Consumed);

    // #27 composition (inherited via `apply_action_evidence`): Job -> Cancelled,
    // JobStep result preserved as Failed, plus the Artifact Incomplete -> Failed
    // in the same transaction.
    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Failed"
    );
    assert_eq!(attempt_state(&db.pool, fixture.attempt_id).await, "Failed");
    assert_eq!(job_step_state(&db.pool, fixture.step_id).await.0, "Failed");
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Cancelled");
    assert_eq!(
        event_count(&db.pool, fixture.job_id, "JobCancelled").await,
        1
    );
    assert_eq!(event_count(&db.pool, fixture.job_id, "JobFailed").await, 0);

    db.teardown().await;
}

// ---------------------------------------------------------------------
// §36 — malformed / unknown / mismatched RF-005 detail
// ---------------------------------------------------------------------

#[test]
fn transfer_result_detail_shape_validation_is_exact() {
    use ActionResultOutcome as O;
    let good = uuid::Uuid::new_v4();

    // unknown code
    assert_eq!(
        parse_transfer_result_detail(O::Failed, &detail("SOMETHING_NEW", good)).unwrap_err(),
        TransferResultDetailError::UnknownCode
    );
    // wrong outcome/code pair
    assert_eq!(
        parse_transfer_result_detail(O::Succeeded, &detail("CHUNK_VERIFICATION_FAILED", good))
            .unwrap_err(),
        TransferResultDetailError::OutcomeCodeMismatch
    );
    assert_eq!(
        parse_transfer_result_detail(O::Failed, &detail("TRANSFER_VERIFIED", good)).unwrap_err(),
        TransferResultDetailError::OutcomeCodeMismatch
    );
    // missing / non-string code
    assert_eq!(
        parse_transfer_result_detail(
            O::Succeeded,
            json!({ "artifact_id": good.to_string() })
                .as_object()
                .unwrap()
        )
        .unwrap_err(),
        TransferResultDetailError::MissingCode
    );
    // missing artifact_id
    assert_eq!(
        parse_transfer_result_detail(
            O::Succeeded,
            json!({ "code": "TRANSFER_VERIFIED" }).as_object().unwrap()
        )
        .unwrap_err(),
        TransferResultDetailError::MissingArtifactId
    );
    // malformed uuid (non-canonical / not hyphenated)
    assert_eq!(
        parse_transfer_result_detail(
            O::Succeeded,
            json!({ "code": "TRANSFER_VERIFIED", "artifact_id": good.simple().to_string() })
                .as_object()
                .unwrap()
        )
        .unwrap_err(),
        TransferResultDetailError::MalformedArtifactId
    );
    assert_eq!(
        parse_transfer_result_detail(
            O::Succeeded,
            json!({ "code": "TRANSFER_VERIFIED", "artifact_id": "not-a-uuid" })
                .as_object()
                .unwrap()
        )
        .unwrap_err(),
        TransferResultDetailError::MalformedArtifactId
    );

    // each valid pairing parses
    for (o, code, expect) in [
        (
            O::Succeeded,
            "TRANSFER_VERIFIED",
            TransferResultCode::TransferVerified,
        ),
        (
            O::Failed,
            "ARTIFACT_VERIFICATION_FAILED",
            TransferResultCode::ArtifactVerificationFailed,
        ),
        (
            O::Failed,
            "CHUNK_VERIFICATION_FAILED",
            TransferResultCode::ChunkVerificationFailed,
        ),
        (
            O::Failed,
            "TRANSFER_ABANDONED",
            TransferResultCode::TransferAbandoned,
        ),
    ] {
        let parsed = parse_transfer_result_detail(o, &detail(code, good)).unwrap();
        assert_eq!(parsed.code, expect);
        assert_eq!(parsed.artifact_id, good);
    }
    // extra keys tolerated (forward compatibility)
    let with_extra =
        json!({ "code": "TRANSFER_VERIFIED", "artifact_id": good.to_string(), "x": 1 })
            .as_object()
            .unwrap()
            .clone();
    assert!(parse_transfer_result_detail(O::Succeeded, &with_extra).is_ok());
}

// ---------------------------------------------------------------------
// §21 / §27 / §36 — production gateway inbound path
// ---------------------------------------------------------------------

type GwEnrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type Gw = AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;

async fn websocket_pair() -> (
    WebSocketStream<tokio::io::DuplexStream>,
    WebSocketStream<tokio::io::DuplexStream>,
) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server =
        tokio::spawn(async move { tokio_tungstenite::accept_async(server_io).await.unwrap() });
    let (client_ws, _resp) = tokio_tungstenite::client_async("ws://bamep-c2/", client_io)
        .await
        .unwrap();
    (client_ws, server.await.unwrap())
}

async fn send(ws: &mut WebSocketStream<tokio::io::DuplexStream>, wire: String) {
    ws.send(Message::text(wire)).await.unwrap();
}

async fn recv(ws: &mut WebSocketStream<tokio::io::DuplexStream>) -> AgentProtocolMessage {
    decode(
        ws.next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap()
            .as_str(),
    )
    .unwrap()
}

/// Builds a gateway wired exactly as production would be for the transfer
/// terminal-result path, plus a fresh credential to authenticate the session
/// as the fixture's Endpoint.
async fn gateway_for(
    pool: &PgPool,
    fixture: &Fixture,
    signal: &str,
    seed: u8,
) -> (Arc<Gw>, String) {
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let enrollment: Arc<GwEnrollment> = Arc::new(EnrollmentService::new(
        endpoint_repo.clone(),
        redemption_repo,
    ));
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone())) as Arc<dyn JobRepository>;
    let action_evidence = Arc::new(ActionEvidenceService::new(
        Arc::clone(&job_repo),
        Arc::new(AttemptReservationRegistry::new()),
        Arc::new(TechnicalResourceArbiter::new([])),
    ));
    let transfer_terminal = build_transfer_terminal_service(pool.clone());

    let signer = bamep_trusted_bootstrap::fixture::FixtureAssertionSigner::from_seed([seed; 32]);
    let gateway = Arc::new(
        Gw::new(Arc::clone(&enrollment))
            .with_bootstrap_evidence_service(Arc::new(BootstrapEvidenceService::new(
                endpoint_repo,
                bamep_trusted_bootstrap::AcceptedSiteKeys::single(signer.public_key()),
            )))
            .with_action_evidence_service(action_evidence)
            .with_transfer_terminal_evidence_service(transfer_terminal),
    );

    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(pool.clone())),
        chrono::Duration::minutes(5),
    );
    let credential = boot
        .issue_enrollment_credential(
            signal,
            bamep_domain::BootNonce::generate().unwrap(),
            Utc::now(),
        )
        .await
        .unwrap();
    let _ = fixture; // fixture only needs to have already created the Endpoint via `signal`
    (gateway, credential.to_wire_value())
}

async fn authenticated_session(
    gateway: &Arc<Gw>,
    credential_wire: &str,
) -> (
    WebSocketStream<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>>,
) {
    let (mut client_ws, mut server_ws) = websocket_pair().await;
    send(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(AuthRequestMessage::new(
            credential_wire,
        )))
        .unwrap(),
    )
    .await;
    let HandshakeOutcome::Established(session) = gateway.handshake(&mut server_ws).await.unwrap()
    else {
        panic!("expected Established");
    };
    let _ = recv(&mut client_ws).await; // SessionEstablished
    let server_gateway = Arc::clone(gateway);
    let task = tokio::spawn(async move {
        server_gateway
            .run_authenticated_session(
                &mut server_ws,
                session,
                bamep_trusted_bootstrap::ServerCertFingerprint::from_sha256_digest([7; 32]),
            )
            .await
    });
    (client_ws, task)
}

#[tokio::test]
async fn gateway_consumes_a_case_c_action_result_end_to_end() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-gw-case-c").await;
    let (gateway, credential) = gateway_for(&db.pool, &fixture, "c2-gw-case-c", 0x21).await;
    let (mut client_ws, task) = authenticated_session(&gateway, &credential).await;

    let result = ActionResultMessage::new(
        fixture.base.action_id,
        ActionResultOutcome::Failed,
        detail("CHUNK_VERIFICATION_FAILED", fixture.base.artifact_id.0),
    );
    send(
        &mut client_ws,
        encode(&AgentProtocolMessage::ActionResult(result)).unwrap(),
    )
    .await;

    client_ws.close(None).await.unwrap();
    task.await.unwrap().unwrap();

    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Failed"
    );
    assert_eq!(attempt_state(&db.pool, fixture.attempt_id).await, "Failed");
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Failed");

    db.teardown().await;
}

#[tokio::test]
async fn gateway_answers_a_malformed_transfer_result_with_a_protocol_error() {
    let db = TestDatabase::setup().await;
    let fixture = Fixture::create(&db.pool, "c2-gw-malformed").await;
    let (gateway, credential) = gateway_for(&db.pool, &fixture, "c2-gw-malformed", 0x22).await;
    let (mut client_ws, task) = authenticated_session(&gateway, &credential).await;

    // Well-formed envelope, correct correlation, but an unknown detail code.
    let result = ActionResultMessage::new(
        fixture.base.action_id,
        ActionResultOutcome::Failed,
        detail("SOMETHING_ELSE", fixture.base.artifact_id.0),
    );
    let message_id = result.envelope.message_id;
    send(
        &mut client_ws,
        encode(&AgentProtocolMessage::ActionResult(result)).unwrap(),
    )
    .await;

    let response = recv(&mut client_ws).await;
    let AgentProtocolMessage::ProtocolError(err) = response else {
        panic!("expected ProtocolError, got {response:?}");
    };
    assert_eq!(err.envelope.correlation_id, Some(message_id));

    client_ws.close(None).await.unwrap();
    task.await.unwrap().unwrap();

    // No durable terminal mutation.
    assert_eq!(
        artifact_state(&db.pool, fixture.base.transfer_id.0).await,
        "Incomplete"
    );
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt_id).await,
        "InProgress"
    );
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Running");

    db.teardown().await;
}
