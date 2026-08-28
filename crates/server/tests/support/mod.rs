//! Shared PostgreSQL Component/Integration test harness (`docs/development/testing.md`
//! "Test isolation": "isolated databases", "explicit setup and teardown").
//!
//! Every test gets its own uniquely named database, migrated through the
//! real Adapter's own `adapters::postgres::connect` entry point — never a
//! hand-rolled schema shortcut, so these tests exercise exactly the
//! migration path the running Server uses.
//!
//! Requires a real, reachable PostgreSQL instance (ADR-0013). The admin
//! connection used to create/drop each per-test database defaults to
//! `postgres://postgres@localhost:5432/postgres` — this machine's local,
//! trust-authenticated development instance — and can be overridden with
//! `BAMEP_TEST_PG_ADMIN_URL` for other environments. No production
//! credential is hardcoded; every database this harness creates or drops
//! carries the `bamep_wp1_test_` prefix it generates itself, so teardown
//! never touches a database this harness did not itself create.
//!
//! Shared across multiple `tests/*.rs` integration-test binaries, each
//! compiled separately: an item unused by one particular binary (e.g. a
//! schema-only test file that never advances a clock) is not dead code in
//! the module as a whole.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::Mutex;

use bamep_agent_protocol::ProtocolId;
use bamep_domain::{
    ArtifactId, ChunkSize, DigestAlgorithm, EndpointId, SourceProvenance, TransferDirection,
    TransferId,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository, PostgresTransferRepository,
};
use bamep_server::application::{
    ActionEvidenceService, BootOrchestrationService, Clock, EnrollmentService,
    JobSchedulingService, JobService, RedeemResult, TransferDispatchResult,
    TransferDispatchService, TransferService,
};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use sqlx::PgPool;
use uuid::Uuid;

pub const IPC_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub const DATA_PLANE_BASE_URL: &str = "https://server.example:8443";

/// A throwaway UDS pathname under a fresh owner-only temp directory; the
/// directory (and socket) are removed on drop.
pub struct TempSocketPath(pub std::path::PathBuf);

impl TempSocketPath {
    pub fn fresh() -> Self {
        let dir = std::env::temp_dir().join(format!("bamep-worker-ipc-tests-{}", Uuid::new_v4()));
        Self(dir.join("worker.sock"))
    }
}

impl Drop for TempSocketPath {
    fn drop(&mut self) {
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

pub fn build_authorization_service(
    pool: PgPool,
) -> Arc<bamep_server::application::TransferAuthorizationService> {
    Arc::new(
        bamep_server::application::TransferAuthorizationService::new(
            Arc::new(
                bamep_server::adapters::postgres::PostgresTransferAuthorizationRepository::new(
                    pool,
                ),
            ),
            Arc::new(CapabilityStore::new()),
            Arc::new(ReplayCache::new()),
            DATA_PLANE_BASE_URL,
        ),
    )
}

pub fn build_chunk_acceptance_service(
    pool: PgPool,
) -> Arc<bamep_server::application::ChunkAcceptanceService> {
    Arc::new(bamep_server::application::ChunkAcceptanceService::new(
        Arc::new(PostgresTransferRepository::new(pool)),
    ))
}

/// Issues a real sender-constrained capability for `fixture` bound to
/// `signing_key`'s public key.
pub async fn issue_capability(
    authorization: &bamep_server::application::TransferAuthorizationService,
    fixture: &DispatchedTransfer,
    signing_key: &SigningKey,
) -> String {
    let public =
        bamep_domain::ProofPublicKey::from_bytes(signing_key.verifying_key().to_bytes()).unwrap();
    let outcome = authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            &public.to_wire_value(),
        )
        .await
        .unwrap();
    let bamep_server::application::TransferAuthorizationOutcome::Granted { token, .. } = outcome
    else {
        panic!("expected Granted");
    };
    token
}

/// Signs one real 137-byte per-request proof transcript and returns its wire
/// carrier triple `(proof_id, issued_at, signature)`. `operation`/`chunk_index`
/// select the exact transcript shape; `artifact_id`/`direction` are signed
/// (the layout is unchanged) but never carried on the v1 wire message.
pub fn sign_proof(
    signing_key: &SigningKey,
    token: &str,
    fixture: &DispatchedTransfer,
    operation: bamep_domain::AuthorizationOperation,
    chunk_index: Option<u64>,
) -> (String, u64, String) {
    let capability_id = bamep_domain::CapabilityId::from_token_bytes(token.as_bytes());
    let proof_id = bamep_domain::ProofId::generate();
    let issued_at_millis = Utc::now().timestamp_millis() as u64;
    let fields = bamep_domain::ProofTranscriptFields {
        operation,
        transfer_id: fixture.transfer_id,
        artifact_id: fixture.artifact_id,
        direction: TransferDirection::AgentToServer,
        chunk_index,
        proof_id,
        issued_at_millis,
    };
    let transcript = bamep_domain::build_proof_transcript(&capability_id, &fields);
    let signature =
        bamep_domain::ProofSignature::from_bytes(signing_key.sign(&transcript).to_bytes());
    (
        proof_id.to_wire_value(),
        issued_at_millis,
        signature.to_wire_value(),
    )
}

/// Completes the Worker handshake over a fresh connection and returns the
/// live stream.
pub async fn handshake(path: &std::path::Path) -> tokio::net::UnixStream {
    use bamep_worker_protocol::{receive, send, WorkerHelloMessage, WorkerProtocolMessage};
    let mut stream = tokio::net::UnixStream::connect(path)
        .await
        .expect("connect");
    let hello = WorkerHelloMessage::new(Uuid::new_v4());
    let sent_id = hello.envelope.message_id;
    send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello))
        .await
        .expect("send WorkerHello");
    match tokio::time::timeout(IPC_TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive ServerHello")
    {
        WorkerProtocolMessage::ServerHello(m) => {
            assert_eq!(m.body.in_reply_to, sent_id);
            assert!(m.body.compatible);
        }
        other => panic!("expected ServerHello, got {other:?}"),
    }
    stream
}

/// A durably dispatched Transfer whose owning Attempt has been advanced to
/// `InProgress` (`ActionAck{Accepted}` applied) — the exact durable
/// precondition `TransferAuthorizationService`/`ChunkAcceptanceService` need
/// (`m0-agent-protocol-contract.md` "Transfer authorization"). Shared by the
/// Worker-UDS authorization, chunk-acceptance, and resume-discovery
/// integration tests.
pub struct DispatchedTransfer {
    pub endpoint_id: EndpointId,
    pub transfer_id: TransferId,
    pub artifact_id: ArtifactId,
    pub action_id: ProtocolId,
}

/// Builds a [`DispatchedTransfer`] from scratch through the real Application
/// services (enrollment, workflow admission, transfer dispatch, action
/// evidence) against `pool`. `signal` must be unique per call within one
/// database.
pub async fn dispatched_transfer_fixture(pool: &PgPool, signal: &str) -> DispatchedTransfer {
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(pool.clone())),
        chrono::Duration::minutes(5),
    );
    let enrollment = EnrollmentService::new(
        Arc::new(PostgresEndpointRepository::new(pool.clone())),
        Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone())),
    );
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let jobs = JobService::new(Arc::clone(&job_repo));
    let scheduling = JobSchedulingService::new(Arc::clone(&job_repo));
    let transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        10,
    )]));
    let dispatch = TransferDispatchService::new(Arc::clone(&job_repo), Arc::clone(&arbiter));
    let evidence = ActionEvidenceService::new(
        Arc::clone(&job_repo) as Arc<dyn bamep_server::ports::JobRepository>,
        Arc::new(AttemptReservationRegistry::new()),
        arbiter,
    );

    let now = Utc::now();
    let boot_nonce = bamep_domain::BootNonce::generate().expect("OS CSPRNG must be available");
    let credential = boot
        .issue_enrollment_credential(signal, boot_nonce, now)
        .await
        .expect("issuance must succeed");
    let RedeemResult::Established { endpoint_id, .. } = enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("first contact must establish a session");
    };
    enrollment
        .approve_enrollment(
            endpoint_id,
            bamep_domain::Actor::Operator {
                label: "worker-data-plane-harness".into(),
            },
            now,
        )
        .await
        .unwrap();

    let job = jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;
    scheduling.admit(job.id).await.unwrap();
    scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();

    let context = transfers
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

    let result = dispatch
        .commit_transfer_dispatch(
            job.id,
            step_id,
            context.transfer.id,
            vec![ResourceClaim::new(ResourceKind::new("network"), 1)],
        )
        .await
        .unwrap();
    let TransferDispatchResult::Committed { outcome, .. } = result else {
        panic!("expected a successful dispatch commitment");
    };
    let action_id = ProtocolId::from_uuid(outcome.attempt.action_id.0)
        .expect("a Domain ActionId is always a valid UUID v4");

    evidence
        .apply(
            action_id,
            endpoint_id,
            bamep_domain::ActionEvidence::AckAccepted,
        )
        .await
        .expect("ActionAck{Accepted} advances the Attempt to InProgress");

    DispatchedTransfer {
        endpoint_id,
        transfer_id: context.transfer.id,
        artifact_id: context.transfer.artifact_id,
        action_id,
    }
}

/// Deterministic, test-controllable [`Clock`]: `EnrollmentService::redeem`
/// reads it at decision time (inside the Adapter's lock), so a test
/// controls exactly what "now" the Domain sees by calling `set`/`advance`
/// — including, unlike a plain fixed timestamp, *while* another task is
/// concurrently blocked on the same PostgreSQL lock (`docs/development/testing.md`
/// "Test isolation": deterministic fixtures).
pub struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self(Mutex::new(now))
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.0.lock().unwrap() = now;
    }

    pub fn advance(&self, delta: Duration) {
        let mut guard = self.0.lock().unwrap();
        *guard += delta;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

fn admin_url() -> String {
    std::env::var("BAMEP_TEST_PG_ADMIN_URL")
        .unwrap_or_else(|_| "postgres://postgres@localhost:5432/postgres".to_string())
}

fn with_database(url: &str, db_name: &str) -> String {
    let (prefix, _existing_db) = url
        .rsplit_once('/')
        .expect("admin URL must include a database path segment");
    format!("{prefix}/{db_name}")
}

pub struct TestDatabase {
    pub pool: PgPool,
    pub db_url: String,
    name: String,
    admin_url: String,
}

impl TestDatabase {
    /// Creates a uniquely named, `bamep_wp1_test_`-prefixed database and
    /// applies the embedded migration baseline via the real Adapter connect path.
    pub async fn setup() -> Self {
        let admin_url = admin_url();
        let name = format!("bamep_wp1_test_{}", Uuid::new_v4().simple());

        let admin_pool = PgPool::connect(&admin_url).await.unwrap_or_else(|e| {
            panic!(
                "connect to PostgreSQL admin database at {admin_url} \
                 (override with BAMEP_TEST_PG_ADMIN_URL): {e}"
            )
        });
        // `name` is generated by this harness (UUID-based), never external
        // input — `AssertSqlSafe` documents that audit, it does not bypass
        // it (sqlx::query normally requires a `&'static str` literal).
        sqlx::query(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
            .execute(&admin_pool)
            .await
            .expect("create per-test database");
        admin_pool.close().await;

        let db_url = with_database(&admin_url, &name);
        let pool = bamep_server::adapters::postgres::connect(&db_url)
            .await
            .expect("connect to and migrate the fresh per-test database");

        Self {
            pool,
            db_url,
            name,
            admin_url,
        }
    }

    /// Closes this test's pool and drops its database. Rust has no async
    /// `Drop`, so every test must call this explicitly; a test that panics
    /// before reaching it leaves behind a `bamep_wp1_test_*`-prefixed
    /// database only — trivially identifiable, never a production or
    /// owner-owned database, and safe to reap separately.
    pub async fn teardown(self) {
        self.pool.close().await;
        let admin_pool = PgPool::connect(&self.admin_url)
            .await
            .expect("connect to PostgreSQL admin database for teardown");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"DROP DATABASE IF EXISTS "{}" WITH (FORCE)"#,
            self.name
        )))
        .execute(&admin_pool)
        .await
        .expect("drop per-test database");
        admin_pool.close().await;
    }
}
