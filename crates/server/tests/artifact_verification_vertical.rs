//! Issue #39 Phase C2 — `ArtifactVerificationReport` independent verification
//! commit.
//!
//! Covers:
//! - the full IPC vertical: a real `ArtifactVerificationReport` frame over a
//!   real UDS -> `WorkerControlPlane` -> consume `verification_handle` ->
//!   reload durable sealed Artifact -> `bamepd` independently compares the
//!   Worker-reported digest against its **own durable** expected digest ->
//!   `PendingVerification -> Verified | Failed` in one transaction ->
//!   `ArtifactVerificationAck` (items 44, 45, 58B);
//! - a malformed reported digest failing closed (no `Ack`, Artifact stays
//!   `PendingVerification`) and the re-drive path recovering (item 46);
//! - a stale-generation `verification_handle` never mutating (item 47);
//! - a consumed `verification_handle` being single-use / exactly-once
//!   terminal (items 26, 34);
//! - an unknown `verification_handle` discarded with no response, never
//!   mapped to `Failed` (item 32).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use bamep_domain::{ArtifactState, Digest, DigestAlgorithm, TransferId};
use bamep_server::adapters::postgres::PostgresTransferRepository;
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::ports::TransferRepository;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker_protocol::{
    receive, send, ArtifactVerificationAckOutcome, ArtifactVerificationReportMessage,
    ManifestSealOutcome, ManifestSealRequestMessage, WireArtifactStatus, WorkerProtocolMessage,
};
use ed25519_dalek::SigningKey;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::timeout;

use support::{
    build_worker_control_services, dispatched_transfer_fixture, handshake, issue_capability,
    sign_proof, DispatchedTransfer, TempSocketPath, TestDatabase, WorkerControlServices,
};

const CHUNK_SIZE: u32 = 4096;

fn digest_wire(byte: u8) -> String {
    Digest::new(DigestAlgorithm::Sha256, vec![byte; 32])
        .unwrap()
        .to_wire_value()
}

struct Env {
    db: TestDatabase,
    fixture: DispatchedTransfer,
    signing_key: SigningKey,
    token: String,
    // Held for the test's lifetime so the shared capability store / replay
    // cache and the connection-generation registry outlive the running plane.
    _services: WorkerControlServices,
    _registry: Arc<WorkerAuthorityRegistry>,
    socket: TempSocketPath,
    run_task: tokio::task::JoinHandle<
        Result<(), bamep_server::adapters::worker_control_plane::WorkerControlPlaneError>,
    >,
    _shutdown_tx: watch::Sender<bool>,
    /// The authoritative durable sealed digest for `fixture`'s Artifact.
    expected_digest: String,
}

impl Env {
    /// Sets up a Transfer whose Artifact is durably `PendingVerification`
    /// (one held chunk, sealed with `expected_digest`), with the plane
    /// running.
    async fn start(signal: &str) -> Self {
        let db = TestDatabase::setup().await;
        let fixture = dispatched_transfer_fixture(&db.pool, signal).await;
        let services = build_worker_control_services(db.pool.clone());
        let signing_key = SigningKey::from_bytes(&rand::random());
        let token = issue_capability(&services.authorization, &fixture, &signing_key).await;
        let expected_digest = digest_wire(0xEE);

        services
            .chunk_acceptance
            .commit_chunk_acceptance(fixture.transfer_id, 0, digest_wire(0), CHUNK_SIZE)
            .await
            .unwrap();

        let socket = TempSocketPath::fresh();
        let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let run_task = tokio::spawn(plane.run(
            Arc::clone(&registry),
            Arc::clone(&services.authorization),
            Arc::clone(&services.chunk_acceptance),
            Arc::clone(&services.manifest_seal),
            Arc::clone(&services.artifact_verification),
            shutdown_rx,
        ));

        Self {
            db,
            fixture,
            signing_key,
            token,
            _services: services,
            _registry: registry,
            socket,
            run_task,
            _shutdown_tx: shutdown_tx,
            expected_digest,
        }
    }

    fn seal_message(&self) -> ManifestSealRequestMessage {
        let (proof_id, issued_at, signature) = sign_proof(
            &self.signing_key,
            &self.token,
            &self.fixture,
            bamep_domain::AuthorizationOperation::SealManifest,
            None,
        );
        ManifestSealRequestMessage::new(
            &self.token,
            self.fixture.transfer_id.0,
            proof_id,
            issued_at,
            signature,
            1,
            &self.expected_digest,
        )
    }

    /// Drives one seal over `stream`, asserts the outcome, and returns the
    /// `verification_handle`.
    async fn seal(&self, stream: &mut tokio::net::UnixStream, expect_first: bool) -> String {
        let request = self.seal_message();
        let sent = request.envelope.message_id;
        send(stream, &WorkerProtocolMessage::ManifestSealRequest(request))
            .await
            .unwrap();
        let body = match timeout(Duration::from_millis(1200), receive(stream))
            .await
            .expect("no timeout")
            .expect("receive")
        {
            WorkerProtocolMessage::ManifestSealDecision(d) => {
                assert_eq!(d.body.in_reply_to, sent);
                d.body
            }
            other => panic!("expected ManifestSealDecision, got {other:?}"),
        };
        let expected = if expect_first {
            ManifestSealOutcome::Sealed
        } else {
            ManifestSealOutcome::AlreadyPendingVerification
        };
        assert_eq!(body.outcome, expected);
        body.verification_handle.expect("committed seal handle")
    }

    async fn finish(self) {
        self.run_task.abort();
        self.db.teardown().await;
    }
}

async fn send_report(
    stream: &mut tokio::net::UnixStream,
    verification_handle: &str,
    computed_artifact_digest: &str,
) -> Option<WireArtifactStatus> {
    let report =
        ArtifactVerificationReportMessage::new(verification_handle, computed_artifact_digest);
    let sent = report.envelope.message_id;
    send(
        stream,
        &WorkerProtocolMessage::ArtifactVerificationReport(report),
    )
    .await
    .unwrap();
    match timeout(Duration::from_millis(1200), receive(stream)).await {
        Ok(Ok(WorkerProtocolMessage::ArtifactVerificationAck(ack))) => {
            assert_eq!(ack.body.in_reply_to, sent);
            assert_eq!(ack.body.outcome, ArtifactVerificationAckOutcome::Committed);
            Some(ack.body.artifact_status)
        }
        Ok(other) => panic!("expected ArtifactVerificationAck, got {other:?}"),
        Err(_) => None,
    }
}

async fn artifact_state(pool: &PgPool, transfer_id: TransferId) -> ArtifactState {
    let repo = PostgresTransferRepository::new(pool.clone());
    let (ctx, _held) = repo
        .find_transfer_context(transfer_id)
        .await
        .unwrap()
        .unwrap();
    ctx.artifact.state
}

#[tokio::test]
async fn a_matching_computed_digest_commits_verified_over_a_real_uds() {
    // Issue #39 Phase C2 items 28, 30, 44, 58B.
    let env = Env::start("c2-verify-match").await;
    let mut stream = handshake(&env.socket.0).await;
    let handle = env.seal(&mut stream, true).await;

    let status = send_report(&mut stream, &handle, &env.expected_digest)
        .await
        .expect("a committed verification produces an Ack");
    assert_eq!(status, WireArtifactStatus::Verified);
    assert_eq!(
        artifact_state(&env.db.pool, env.fixture.transfer_id).await,
        ArtifactState::Verified
    );

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_mismatching_computed_digest_commits_failed_over_a_real_uds() {
    // Issue #39 Phase C2 items 30, 45: a valid SHA-256 digest that does not
    // match the durable expected digest is an authoritative *completed*
    // verification result (`Failed`), not a protocol rejection.
    let env = Env::start("c2-verify-mismatch").await;
    let mut stream = handshake(&env.socket.0).await;
    let handle = env.seal(&mut stream, true).await;

    let status = send_report(&mut stream, &handle, &digest_wire(0x01))
        .await
        .expect("a mismatch is still a committed Ack");
    assert_eq!(status, WireArtifactStatus::Failed);
    assert_eq!(
        artifact_state(&env.db.pool, env.fixture.transfer_id).await,
        ArtifactState::Failed
    );

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_malformed_computed_digest_fails_closed_and_the_seal_retry_re_drives_verification() {
    // Issue #39 Phase C2 items 27, 35, 46: a malformed reported digest is NOT
    // a completed `Failed` verification — no `Ack`, the handle stays consumed,
    // the Artifact stays `PendingVerification`, and a fresh seal retry mints a
    // fresh handle to re-drive verification.
    let env = Env::start("c2-verify-malformed").await;
    let mut stream = handshake(&env.socket.0).await;
    let handle = env.seal(&mut stream, true).await;

    let no_ack = send_report(&mut stream, &handle, "not-a-canonical-digest").await;
    assert!(
        no_ack.is_none(),
        "a malformed digest must not produce an Ack"
    );
    assert_eq!(
        artifact_state(&env.db.pool, env.fixture.transfer_id).await,
        ArtifactState::PendingVerification,
        "the Artifact must remain PendingVerification"
    );

    // The consumed handle is not resurrected — a re-drive needs a fresh seal.
    let stale = send_report(&mut stream, &handle, &env.expected_digest).await;
    assert!(stale.is_none(), "the consumed handle stays consumed");

    // Fresh proof + identical seal -> already_pending_verification + fresh
    // handle -> verification completes.
    let fresh_handle = env.seal(&mut stream, false).await;
    assert_ne!(fresh_handle, handle);
    let status = send_report(&mut stream, &fresh_handle, &env.expected_digest)
        .await
        .expect("the re-driven verification commits");
    assert_eq!(status, WireArtifactStatus::Verified);
    assert_eq!(
        artifact_state(&env.db.pool, env.fixture.transfer_id).await,
        ArtifactState::Verified
    );

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_consumed_verification_handle_is_single_use_and_exactly_once_terminal() {
    // Issue #39 Phase C2 items 26, 34.
    let env = Env::start("c2-verify-single-use").await;
    let mut stream = handshake(&env.socket.0).await;
    let handle = env.seal(&mut stream, true).await;

    let status = send_report(&mut stream, &handle, &env.expected_digest)
        .await
        .expect("the first report commits");
    assert_eq!(status, WireArtifactStatus::Verified);

    // A duplicate report on the same handle: no Ack, no further transition.
    let duplicate = send_report(&mut stream, &handle, &digest_wire(0x02)).await;
    assert!(
        duplicate.is_none(),
        "a duplicate handle use produces no Ack"
    );
    assert_eq!(
        artifact_state(&env.db.pool, env.fixture.transfer_id).await,
        ArtifactState::Verified,
        "Verified is terminal — never transitioned again"
    );

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn an_unknown_verification_handle_is_discarded_with_no_response() {
    // Issue #39 Phase C2 item 32: never mapped to `Failed`.
    let env = Env::start("c2-verify-unknown").await;
    let mut stream = handshake(&env.socket.0).await;
    let _handle = env.seal(&mut stream, true).await;

    let no_ack = send_report(
        &mut stream,
        "ver_deadbeefdeadbeefdeadbeefdeadbeef",
        &env.expected_digest,
    )
    .await;
    assert!(no_ack.is_none());
    assert_eq!(
        artifact_state(&env.db.pool, env.fixture.transfer_id).await,
        ArtifactState::PendingVerification
    );

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_stale_generation_verification_handle_never_mutates() {
    // Issue #39 Phase C2 item 47: seal under generation A, obtain a handle,
    // supersede A, present the old handle on generation B -> no Ack, no
    // mutation. Then a fresh seal on B completes normally.
    let env = Env::start("c2-verify-stale-gen").await;
    let mut stream_a = handshake(&env.socket.0).await;
    let handle_a = env.seal(&mut stream_a, true).await;

    // Generation B: a fresh connection supersedes A.
    let mut stream_b = handshake(&env.socket.0).await;

    let no_ack = send_report(&mut stream_b, &handle_a, &env.expected_digest).await;
    assert!(
        no_ack.is_none(),
        "a prior-generation handle is honoured on no later generation"
    );
    assert_eq!(
        artifact_state(&env.db.pool, env.fixture.transfer_id).await,
        ArtifactState::PendingVerification
    );

    // A fresh seal on generation B + its fresh handle completes.
    let handle_b = env.seal(&mut stream_b, false).await;
    let status = send_report(&mut stream_b, &handle_b, &env.expected_digest)
        .await
        .expect("the fresh-generation verification commits");
    assert_eq!(status, WireArtifactStatus::Verified);

    drop(stream_a);
    drop(stream_b);
    env.finish().await;
}
