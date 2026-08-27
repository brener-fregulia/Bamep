//! Issue #38 "PostgreSQL/Application" validation: `TransferAuthorizationService`
//! (`crates/server/src/application/mod.rs`) against the real
//! `PostgresTransferAuthorizationRepository` Adapter and a real PostgreSQL
//! instance (ADR-0013), composed on top of the same durable dispatch
//! fixtures `transfer_dispatch_commit.rs` already exercises (enrolled
//! Endpoint -> Job -> JobStep -> pre-dispatch Transfer -> committed
//! `Attempt{Dispatched}`).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::Arc;

use bamep_agent_protocol::ProtocolId;
use bamep_domain::{
    ActionEvidence, ArtifactId, ChunkSize, DigestAlgorithm, EndpointId, JobStepId,
    SourceProvenance, TransferDirection, TransferId,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository, PostgresTransferAuthorizationRepository,
    PostgresTransferRepository,
};
use bamep_server::application::{
    ActionEvidenceService, BootOrchestrationService, EnrollmentService, RedeemResult,
    TransferAuthorizationOutcome, TransferAuthorizationService, TransferDispatchResult,
    TransferDispatchService, TransferService, WorkerAuthorizationOutcome,
    WorkerAuthorizationQueryInput,
};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signer, SigningKey};
use sqlx::PgPool;
use support::TestDatabase;
use uuid::Uuid;

const DATA_PLANE_BASE_URL: &str = "https://server.example:8443";

struct Services {
    boot: BootOrchestrationService<PostgresBootContextRepository>,
    enrollment:
        Arc<EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>>,
    jobs: bamep_server::application::JobService<PostgresJobRepository>,
    scheduling: bamep_server::application::JobSchedulingService<PostgresJobRepository>,
    transfers: TransferService<PostgresTransferRepository>,
    dispatch: TransferDispatchService<PostgresJobRepository>,
    evidence: ActionEvidenceService,
    authorization: TransferAuthorizationService,
}

fn network_claims() -> Vec<ResourceClaim> {
    vec![ResourceClaim::new(ResourceKind::new("network"), 1)]
}

fn build_services(pool: PgPool) -> Services {
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let transfer_repo = Arc::new(PostgresTransferRepository::new(pool.clone()));
    let authorization_repo = Arc::new(PostgresTransferAuthorizationRepository::new(pool.clone()));
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        10,
    )]));

    Services {
        boot: BootOrchestrationService::new(boot_repo, chrono::Duration::minutes(5)),
        enrollment: Arc::new(EnrollmentService::new(endpoint_repo, redemption_repo)),
        jobs: bamep_server::application::JobService::new(Arc::clone(&job_repo)),
        scheduling: bamep_server::application::JobSchedulingService::new(Arc::clone(&job_repo)),
        transfers: TransferService::new(transfer_repo),
        dispatch: TransferDispatchService::new(Arc::clone(&job_repo), Arc::clone(&arbiter)),
        evidence: ActionEvidenceService::new(
            job_repo as Arc<dyn bamep_server::ports::JobRepository>,
            Arc::new(AttemptReservationRegistry::new()),
            arbiter,
        ),
        authorization: TransferAuthorizationService::new(
            authorization_repo,
            Arc::new(CapabilityStore::new()),
            Arc::new(ReplayCache::new()),
            DATA_PLANE_BASE_URL,
        ),
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
                label: "transfer-authorization-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    endpoint_id
}

struct DispatchedTransfer {
    endpoint_id: EndpointId,
    transfer_id: TransferId,
    artifact_id: ArtifactId,
    action_id: ProtocolId,
}

/// Full fixture through the exact durable state Issue #38 requires to be
/// authorization-eligible: enrolled+approved Endpoint, `Running` Job,
/// `PreconditionsSatisfied` JobStep, pre-dispatch Transfer, then a
/// committed `Attempt{Dispatched}` bound to that Transfer (Issue #40's own
/// commitment path, reused unmodified).
async fn dispatched_transfer_fixture(services: &Services, signal: &str) -> DispatchedTransfer {
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(services, signal, now).await;

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

    let result = services
        .dispatch
        .commit_transfer_dispatch(job.id, step_id, context.transfer.id, network_claims())
        .await
        .unwrap();
    let TransferDispatchResult::Committed { outcome, .. } = result else {
        panic!("expected a successful dispatch commitment");
    };

    let action_id = ProtocolId::from_uuid(outcome.attempt.action_id.0)
        .expect("a Domain ActionId is always a valid UUID v4");

    DispatchedTransfer {
        endpoint_id,
        transfer_id: context.transfer.id,
        artifact_id: context.transfer.artifact_id,
        action_id,
    }
}

fn generate_proof_key() -> (SigningKey, String) {
    let signing_key = SigningKey::from_bytes(&rand::random());
    let public = bamep_domain::ProofPublicKey::from_bytes(signing_key.verifying_key().to_bytes())
        .expect("a freshly generated Ed25519 key is always valid");
    (signing_key, public.to_wire_value())
}

fn unused_job_step_id() -> JobStepId {
    JobStepId(Uuid::new_v4())
}

// ---------------------------------------------------------------------
// Issuance (Agent WSS path)
// ---------------------------------------------------------------------

#[tokio::test]
async fn exact_valid_transfer_attempt_ownership_succeeds() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "auth-valid-01").await;
    let (_signing_key, public_key) = generate_proof_key();

    let outcome = services
        .authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            &public_key,
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        TransferAuthorizationOutcome::Granted { .. }
    ));
    db.teardown().await;
}

#[tokio::test]
async fn pre_dispatch_unbound_transfer_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let now = Utc::now();
    let endpoint_id = enrolled_endpoint(&services, "auth-predispatch-01", now).await;
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
    assert!(!context.transfer.is_attempt_bound());

    let (_signing_key, public_key) = generate_proof_key();
    // No real action_id exists yet — any presented correlation must fail.
    let bogus_action_id = ProtocolId::generate();
    let outcome = services
        .authorization
        .issue(
            endpoint_id,
            bogus_action_id,
            context.transfer.id,
            &public_key,
        )
        .await
        .unwrap();

    assert_eq!(outcome, TransferAuthorizationOutcome::Denied);
    db.teardown().await;
}

#[tokio::test]
async fn unknown_transfer_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (_signing_key, public_key) = generate_proof_key();

    let outcome = services
        .authorization
        .issue(
            EndpointId::new(),
            ProtocolId::generate(),
            TransferId::new(),
            &public_key,
        )
        .await
        .unwrap();

    assert_eq!(outcome, TransferAuthorizationOutcome::Denied);
    db.teardown().await;
}

#[tokio::test]
async fn wrong_action_id_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "auth-wrongaction-01").await;
    let (_signing_key, public_key) = generate_proof_key();

    let outcome = services
        .authorization
        .issue(
            fixture.endpoint_id,
            ProtocolId::generate(),
            fixture.transfer_id,
            &public_key,
        )
        .await
        .unwrap();

    assert_eq!(outcome, TransferAuthorizationOutcome::Denied);
    db.teardown().await;
}

#[tokio::test]
async fn wrong_endpoint_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "auth-wrongendpoint-01").await;
    let other_endpoint = enrolled_endpoint(&services, "auth-wrongendpoint-other", Utc::now()).await;
    let (_signing_key, public_key) = generate_proof_key();

    let outcome = services
        .authorization
        .issue(
            other_endpoint,
            fixture.action_id,
            fixture.transfer_id,
            &public_key,
        )
        .await
        .unwrap();

    assert_eq!(outcome, TransferAuthorizationOutcome::Denied);
    db.teardown().await;
}

#[tokio::test]
async fn a_malformed_proof_public_key_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "auth-malformedkey-01").await;

    let outcome = services
        .authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            "not-a-valid-key",
        )
        .await
        .unwrap();

    assert_eq!(outcome, TransferAuthorizationOutcome::Denied);
    db.teardown().await;
}

#[tokio::test]
async fn a_terminal_attempt_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "auth-terminal-01").await;

    services
        .evidence
        .apply(
            fixture.action_id,
            fixture.endpoint_id,
            ActionEvidence::ResultSucceeded,
        )
        .await
        .unwrap();

    let (_signing_key, public_key) = generate_proof_key();
    let outcome = services
        .authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            &public_key,
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        TransferAuthorizationOutcome::Denied,
        "a terminal (Succeeded) Attempt must never be authorization-eligible"
    );
    db.teardown().await;
}

#[tokio::test]
async fn a_revoked_credential_is_denied_at_issuance() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "auth-revoked-01").await;

    services
        .enrollment
        .revoke_credential(fixture.endpoint_id, Utc::now())
        .await
        .unwrap();

    let (_signing_key, public_key) = generate_proof_key();
    let outcome = services
        .authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            &public_key,
        )
        .await
        .unwrap();

    assert_eq!(outcome, TransferAuthorizationOutcome::Denied);
    db.teardown().await;
}

#[tokio::test]
async fn renewal_with_a_fresh_proof_key_succeeds_without_creating_a_new_attempt_or_transfer() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "auth-renewal-01").await;

    let (_first_key, first_public) = generate_proof_key();
    let first = services
        .authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            &first_public,
        )
        .await
        .unwrap();
    assert!(matches!(
        first,
        TransferAuthorizationOutcome::Granted { .. }
    ));

    let attempt_count_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE job_step_id != $1")
            .bind(unused_job_step_id().0)
            .fetch_one(&db.pool)
            .await
            .unwrap();

    // Renewal: same transfer_id/action_id, a genuinely different proof key —
    // simulating Agent restart (`m0-agent-protocol-contract.md` "Renewal and
    // restart").
    let (_second_key, second_public) = generate_proof_key();
    assert_ne!(first_public, second_public);
    let second = services
        .authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            &second_public,
        )
        .await
        .unwrap();
    assert!(matches!(
        second,
        TransferAuthorizationOutcome::Granted { .. }
    ));

    let attempt_count_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE job_step_id != $1")
            .bind(unused_job_step_id().0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        attempt_count_before, attempt_count_after,
        "renewal must never create a new Attempt"
    );

    let transfer_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transfers WHERE id = $1")
        .bind(fixture.transfer_id.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        transfer_count, 1,
        "renewal must never create a new Transfer"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Worker UDS decision path (`TransferAuthorizationService::decide`)
// ---------------------------------------------------------------------

/// Builds a real, byte-identical `AuthorizationQuery` input for `chunk_upload
/// operation_index 0`, signing the exact canonical transcript with
/// `signing_key` bound to `token`'s derived `capability_id`.
fn signed_query_input(
    signing_key: &SigningKey,
    token: &str,
    transfer_id: TransferId,
    artifact_id: ArtifactId,
    issued_at_millis: u64,
    proof_id: bamep_domain::ProofId,
) -> WorkerAuthorizationQueryInput {
    let capability_id = bamep_domain::CapabilityId::from_token_bytes(token.as_bytes());
    let fields = bamep_domain::ProofTranscriptFields {
        operation: bamep_domain::AuthorizationOperation::ResumeDiscovery,
        transfer_id,
        artifact_id,
        direction: TransferDirection::AgentToServer,
        chunk_index: None,
        proof_id,
        issued_at_millis,
    };
    let transcript = bamep_domain::build_proof_transcript(&capability_id, &fields);
    let signature = signing_key.sign(&transcript);
    let signature = bamep_domain::ProofSignature::from_bytes(signature.to_bytes());

    WorkerAuthorizationQueryInput {
        token: token.to_string(),
        operation: bamep_domain::AuthorizationOperation::ResumeDiscovery,
        transfer_id: transfer_id.0,
        artifact_id: artifact_id.0,
        direction: TransferDirection::AgentToServer,
        chunk_index: None,
        proof_id: proof_id.to_wire_value(),
        issued_at_millis,
        signature: signature.to_wire_value(),
    }
}

async fn issued_token(
    services: &Services,
    fixture: &DispatchedTransfer,
    signing_key: &SigningKey,
) -> String {
    let public =
        bamep_domain::ProofPublicKey::from_bytes(signing_key.verifying_key().to_bytes()).unwrap();
    let outcome = services
        .authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            &public.to_wire_value(),
        )
        .await
        .unwrap();
    let TransferAuthorizationOutcome::Granted { token, .. } = outcome else {
        panic!("expected Granted");
    };
    token
}

#[tokio::test]
async fn a_valid_signed_query_against_a_freshly_issued_capability_is_approved() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "decide-valid-01").await;
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issued_token(&services, &fixture, &signing_key).await;

    let now_millis = Utc::now().timestamp_millis() as u64;
    let input = signed_query_input(
        &signing_key,
        &token,
        fixture.transfer_id,
        fixture.artifact_id,
        now_millis,
        bamep_domain::ProofId::generate(),
    );

    let outcome = services.authorization.decide(input).await.unwrap();
    assert!(matches!(
        outcome,
        WorkerAuthorizationOutcome::Approved { .. }
    ));
    db.teardown().await;
}

#[tokio::test]
async fn a_wrong_signature_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "decide-wrongsig-01").await;
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issued_token(&services, &fixture, &signing_key).await;

    let now_millis = Utc::now().timestamp_millis() as u64;
    // Sign with an unrelated key instead of the one bound to this capability.
    let unrelated_key = SigningKey::from_bytes(&rand::random());
    let input = signed_query_input(
        &unrelated_key,
        &token,
        fixture.transfer_id,
        fixture.artifact_id,
        now_millis,
        bamep_domain::ProofId::generate(),
    );

    let outcome = services.authorization.decide(input).await.unwrap();
    assert_eq!(outcome, WorkerAuthorizationOutcome::Denied);
    db.teardown().await;
}

#[tokio::test]
async fn an_exact_replay_of_the_same_proof_id_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "decide-replay-01").await;
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issued_token(&services, &fixture, &signing_key).await;

    let now_millis = Utc::now().timestamp_millis() as u64;
    let proof_id = bamep_domain::ProofId::generate();
    let first = signed_query_input(
        &signing_key,
        &token,
        fixture.transfer_id,
        fixture.artifact_id,
        now_millis,
        proof_id,
    );
    let second = signed_query_input(
        &signing_key,
        &token,
        fixture.transfer_id,
        fixture.artifact_id,
        now_millis,
        proof_id,
    );

    let first_outcome = services.authorization.decide(first).await.unwrap();
    assert!(matches!(
        first_outcome,
        WorkerAuthorizationOutcome::Approved { .. }
    ));
    let second_outcome = services.authorization.decide(second).await.unwrap();
    assert_eq!(
        second_outcome,
        WorkerAuthorizationOutcome::Denied,
        "the exact same proof_id must never be accepted twice"
    );
    db.teardown().await;
}

#[tokio::test]
async fn a_credential_revoked_after_issuance_denies_a_subsequent_decide() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "decide-revoked-01").await;
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issued_token(&services, &fixture, &signing_key).await;

    // The capability is already durably issued and still unexpired — only
    // the credential state changes.
    services
        .enrollment
        .revoke_credential(fixture.endpoint_id, Utc::now())
        .await
        .unwrap();

    let now_millis = Utc::now().timestamp_millis() as u64;
    let input = signed_query_input(
        &signing_key,
        &token,
        fixture.transfer_id,
        fixture.artifact_id,
        now_millis,
        bamep_domain::ProofId::generate(),
    );
    let outcome = services.authorization.decide(input).await.unwrap();
    assert_eq!(
        outcome,
        WorkerAuthorizationOutcome::Denied,
        "credential revocation must take effect immediately, per-request — never only at \
         issuance time"
    );
    db.teardown().await;
}

#[tokio::test]
async fn an_attempt_that_becomes_terminal_after_issuance_denies_a_subsequent_decide() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let fixture = dispatched_transfer_fixture(&services, "decide-terminal-01").await;
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issued_token(&services, &fixture, &signing_key).await;

    services
        .evidence
        .apply(
            fixture.action_id,
            fixture.endpoint_id,
            ActionEvidence::ResultFailed,
        )
        .await
        .unwrap();

    let now_millis = Utc::now().timestamp_millis() as u64;
    let input = signed_query_input(
        &signing_key,
        &token,
        fixture.transfer_id,
        fixture.artifact_id,
        now_millis,
        bamep_domain::ProofId::generate(),
    );
    let outcome = services.authorization.decide(input).await.unwrap();
    assert_eq!(
        outcome,
        WorkerAuthorizationOutcome::Denied,
        "current durable Attempt state must be re-checked on every decide, not only at \
         issuance"
    );
    db.teardown().await;
}

#[tokio::test]
async fn an_unknown_capability_token_is_denied() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let now_millis = Utc::now().timestamp_millis() as u64;
    let input = signed_query_input(
        &signing_key,
        "a-token-that-was-never-issued",
        TransferId::new(),
        ArtifactId::new(),
        now_millis,
        bamep_domain::ProofId::generate(),
    );
    let outcome = services.authorization.decide(input).await.unwrap();
    assert_eq!(outcome, WorkerAuthorizationOutcome::Denied);
    db.teardown().await;
}
