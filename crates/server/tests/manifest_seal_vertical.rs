//! Issue #39 Phase C2 — `ManifestSealRequest` atomic first durable commit.
//!
//! Covers:
//! - the full IPC vertical: a real Worker Protocol `ManifestSealRequest` frame
//!   over a real UDS -> `WorkerControlPlane` -> sender-constrained seal
//!   authorization, current-durable-state validation, `ChunkManifest::seal`
//!   and `begin_verification` in **one** PostgreSQL transaction -> Phase B
//!   `verification_handle` mint -> `ManifestSealDecision` (items 43, 58A);
//! - `incomplete_manifest` leaving no partial durable seal (item 42);
//! - conflicting reseal never rewriting the original sealed tuple (items 13,
//!   41);
//! - the identical-`(chunk_count, artifact_digest)` crash-recovery retry
//!   reaching `already_pending_verification` with the *same* durable
//!   `artifact_id` and a fresh `verification_handle`, no second transition
//!   (items 14, 15, 23, 49, 58C);
//! - a terminal Artifact seal being a generic `denied` (item 17);
//! - a post-commit `verification_handle` mint failure being recoverable
//!   (item 48);
//! - PostgreSQL-backed concurrent identical / conflicting seals (items 40,
//!   41).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use bamep_domain::{ArtifactState, Digest, DigestAlgorithm, TransferId};
use bamep_server::adapters::postgres::PostgresTransferRepository;
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::ManifestSealInput;
use bamep_server::ports::{ManifestSealCommit, TransferRepository};
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker_protocol::{
    receive, send, ManifestSealDecisionBody, ManifestSealOutcome, ManifestSealRejectionReason,
    ManifestSealRequestMessage, WireDigestAlgorithm, WorkerProtocolMessage,
};
use ed25519_dalek::SigningKey;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::timeout;

use support::{
    build_worker_control_services, dispatched_transfer_fixture, handshake, issue_capability,
    sign_proof, DispatchedTransfer, TempSocketPath, TestDatabase, WorkerControlServices,
    IPC_TEST_TIMEOUT as TEST_TIMEOUT,
};

const CHUNK_SIZE: u32 = 4096;

fn digest_wire(byte: u8) -> String {
    Digest::new(DigestAlgorithm::Sha256, vec![byte; 32])
        .unwrap()
        .to_wire_value()
}

/// Durably records + holds `chunk_index` straight through the real
/// `ChunkAcceptanceService` (no wire), so `begin_verification` sees it.
async fn hold_chunk(services: &WorkerControlServices, fixture: &DispatchedTransfer, index: u64) {
    let outcome = services
        .chunk_acceptance
        .commit_chunk_acceptance(
            fixture.transfer_id,
            index,
            digest_wire(index as u8),
            CHUNK_SIZE,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        bamep_server::ports::ChunkAcceptanceCommit::Committed,
        "held chunk {index} must commit"
    );
}

fn seal_request(
    signing_key: &SigningKey,
    token: &str,
    fixture: &DispatchedTransfer,
    chunk_count: u64,
    artifact_digest: &str,
) -> ManifestSealRequestMessage {
    let (proof_id, issued_at, signature) = sign_proof(
        signing_key,
        token,
        fixture,
        bamep_domain::AuthorizationOperation::SealManifest,
        None,
    );
    ManifestSealRequestMessage::new(
        token,
        fixture.transfer_id.0,
        proof_id,
        issued_at,
        signature,
        chunk_count,
        artifact_digest,
    )
}

async fn send_seal(
    stream: &mut tokio::net::UnixStream,
    request: ManifestSealRequestMessage,
) -> Option<ManifestSealDecisionBody> {
    let sent = request.envelope.message_id;
    send(stream, &WorkerProtocolMessage::ManifestSealRequest(request))
        .await
        .expect("send ManifestSealRequest");
    match timeout(Duration::from_millis(1200), receive(stream)).await {
        Ok(Ok(WorkerProtocolMessage::ManifestSealDecision(d))) => {
            assert_eq!(d.body.in_reply_to, sent);
            Some(d.body)
        }
        Ok(other) => panic!("expected ManifestSealDecision, got {other:?}"),
        Err(_) => None, // no response — fail-closed / mint failure
    }
}

async fn context(
    pool: &PgPool,
    transfer_id: TransferId,
) -> (bamep_domain::Artifact, bamep_domain::ChunkManifest) {
    let repo = PostgresTransferRepository::new(pool.clone());
    let (ctx, _held) = repo
        .find_transfer_context(transfer_id)
        .await
        .unwrap()
        .expect("transfer context");
    (ctx.artifact, ctx.manifest)
}

struct Env {
    db: TestDatabase,
    fixture: DispatchedTransfer,
    signing_key: SigningKey,
    token: String,
    services: WorkerControlServices,
    registry: Arc<WorkerAuthorityRegistry>,
    socket: TempSocketPath,
    run_task: tokio::task::JoinHandle<
        Result<(), bamep_server::adapters::worker_control_plane::WorkerControlPlaneError>,
    >,
    _shutdown_tx: watch::Sender<bool>,
}

impl Env {
    async fn start(signal: &str) -> Self {
        Self::start_with_capacity(signal, None).await
    }

    async fn start_with_capacity(signal: &str, operations_capacity: Option<usize>) -> Self {
        let db = TestDatabase::setup().await;
        let fixture = dispatched_transfer_fixture(&db.pool, signal).await;
        let services = build_worker_control_services(db.pool.clone());
        let signing_key = SigningKey::from_bytes(&rand::random());
        let token = issue_capability(&services.authorization, &fixture, &signing_key).await;

        let socket = TempSocketPath::fresh();
        let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
        let registry = Arc::new(match operations_capacity {
            Some(cap) => WorkerAuthorityRegistry::with_operations_capacity(cap),
            None => WorkerAuthorityRegistry::new(),
        });
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
            services,
            registry,
            socket,
            run_task,
            _shutdown_tx: shutdown_tx,
        }
    }

    fn seal_request(&self, chunk_count: u64, artifact_digest: &str) -> ManifestSealRequestMessage {
        seal_request(
            &self.signing_key,
            &self.token,
            &self.fixture,
            chunk_count,
            artifact_digest,
        )
    }

    async fn finish(self) {
        self.run_task.abort();
        self.db.teardown().await;
    }
}

#[tokio::test]
async fn first_valid_seal_atomically_reaches_pending_verification_over_a_real_uds() {
    // Issue #39 Phase C2 items 43, 58A, 19, 39.
    let env = Env::start("c2-seal-happy").await;
    hold_chunk(&env.services, &env.fixture, 0).await;
    hold_chunk(&env.services, &env.fixture, 1).await;
    let artifact_digest = digest_wire(0x99);

    let mut stream = handshake(&env.socket.0).await;
    let request = env.seal_request(2, &artifact_digest);
    let seal_proof_id = request.body.proof_id.clone();
    let body = send_seal(&mut stream, request)
        .await
        .expect("a valid seal must produce a decision");

    assert_eq!(body.outcome, ManifestSealOutcome::Sealed);
    assert!(body.reason.is_none());
    // The wire ManifestSealDecision exposes no proof_id anywhere (Correction
    // B item 16) — it is internal operation-instance correlation metadata.
    let wire = serde_json::to_string(&body).unwrap();
    assert!(!wire.contains("proof_id"));
    assert!(!wire.contains(&seal_proof_id));
    // Authoritative durable success facts, not echoed request values.
    assert_eq!(body.artifact_id, Some(env.fixture.artifact_id.0));
    assert_eq!(body.digest_algorithm, Some(WireDigestAlgorithm::Sha256));
    assert_eq!(body.chunk_size, Some(CHUNK_SIZE));
    assert_eq!(body.chunk_count, Some(2));
    assert_eq!(
        body.expected_artifact_digest.as_deref(),
        Some(artifact_digest.as_str())
    );
    let verification_handle = body
        .verification_handle
        .expect("a committed seal carries a verification_handle");
    assert!(verification_handle.starts_with("ver_"));

    // The verification handle is bound to the authoritative durable identity.
    let operations = env.registry.current_operations().expect("current store");
    let binding = operations
        .verification_binding(&verification_handle)
        .expect("live verification binding");
    assert_eq!(binding.transfer_id, env.fixture.transfer_id);
    assert_eq!(binding.artifact_id, env.fixture.artifact_id);
    assert_eq!(binding.chunk_count, 2);
    assert_eq!(binding.expected_artifact_digest, artifact_digest);
    // The binding retains the exact authorizing ManifestSealRequest proof_id
    // (Correction B items 7, 16) and never renders it in Debug.
    assert_eq!(binding.proof_id, seal_proof_id);
    assert!(!format!("{binding:?}").contains(&seal_proof_id));
    assert!(format!("{binding:?}").contains("REDACTED"));

    // Durable state: manifest sealed with exactly this tuple, Artifact
    // PendingVerification — both committed atomically.
    let (artifact, manifest) = context(&env.db.pool, env.fixture.transfer_id).await;
    assert_eq!(artifact.state, ArtifactState::PendingVerification);
    assert!(manifest.sealed);
    assert_eq!(manifest.chunk_count, Some(2));
    assert_eq!(
        manifest.artifact_digest.as_ref().map(|d| d.to_wire_value()),
        Some(artifact_digest.clone())
    );

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn an_incomplete_manifest_seal_leaves_no_partial_durable_seal() {
    // Issue #39 Phase C2 items 12, 37, 42.
    let env = Env::start("c2-seal-incomplete").await;
    // Only chunk 0 is durably held; the seal declares chunk_count = 2.
    hold_chunk(&env.services, &env.fixture, 0).await;

    let mut stream = handshake(&env.socket.0).await;
    let body = send_seal(&mut stream, env.seal_request(2, &digest_wire(0x11)))
        .await
        .expect("an incomplete seal produces a rejected decision");

    assert_eq!(body.outcome, ManifestSealOutcome::Rejected);
    assert_eq!(
        body.reason,
        Some(ManifestSealRejectionReason::IncompleteManifest)
    );
    assert!(body.verification_handle.is_none());

    // No partial durable seal: manifest unsealed, Artifact still Incomplete.
    let (artifact, manifest) = context(&env.db.pool, env.fixture.transfer_id).await;
    assert_eq!(artifact.state, ArtifactState::Incomplete);
    assert!(!manifest.sealed);
    assert_eq!(manifest.chunk_count, None);

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_seal_signed_by_the_wrong_key_is_a_generic_denial_with_no_durable_effect() {
    // Issue #39 Phase C2 items 5, 6: a request that fails authorization is a
    // generic non-enumerable `denied` — no `rejected` reason, no
    // verification_handle, no durable seal, and (Issue #38 invariant) it
    // consumes no proof replay state, so the correct key still works after.
    let env = Env::start("c2-seal-wrong-key").await;
    hold_chunk(&env.services, &env.fixture, 0).await;
    let artifact_digest = digest_wire(0x33);

    let mut stream = handshake(&env.socket.0).await;

    let wrong_key = SigningKey::from_bytes(&rand::random());
    let denied = send_seal(
        &mut stream,
        seal_request(&wrong_key, &env.token, &env.fixture, 1, &artifact_digest),
    )
    .await
    .expect("a denied seal still produces a decision");
    assert_eq!(denied.outcome, ManifestSealOutcome::Denied);
    assert!(denied.reason.is_none());
    assert!(denied.verification_handle.is_none());

    let (artifact, manifest) = context(&env.db.pool, env.fixture.transfer_id).await;
    assert_eq!(artifact.state, ArtifactState::Incomplete);
    assert!(!manifest.sealed);

    // The correctly-signed seal still succeeds (no replay state was spent).
    let sealed = send_seal(&mut stream, env.seal_request(1, &artifact_digest))
        .await
        .unwrap();
    assert_eq!(sealed.outcome, ManifestSealOutcome::Sealed);

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_seal_against_a_terminal_owning_attempt_is_denied() {
    // Issue #39 Phase C2 item 17 + m1 "Seal-manifest first durable commit":
    // an owning Attempt that is no longer `InProgress` is an authorization
    // `denied`, never `transfer_not_continuable` (seal has no such reason).
    let env = Env::start("c2-seal-terminal-attempt").await;
    hold_chunk(&env.services, &env.fixture, 0).await;

    sqlx::query(
        "UPDATE attempts SET state = 'Failed' WHERE id = \
         (SELECT attempt_id FROM transfers WHERE id = $1)",
    )
    .bind(env.fixture.transfer_id.0)
    .execute(&env.db.pool)
    .await
    .unwrap();

    let mut stream = handshake(&env.socket.0).await;
    let denied = send_seal(&mut stream, env.seal_request(1, &digest_wire(0x44)))
        .await
        .expect("a terminal-Attempt seal still produces a decision");
    assert_eq!(denied.outcome, ManifestSealOutcome::Denied);
    assert!(denied.verification_handle.is_none());

    let (artifact, manifest) = context(&env.db.pool, env.fixture.transfer_id).await;
    assert_eq!(artifact.state, ArtifactState::Incomplete);
    assert!(!manifest.sealed);

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_conflicting_reseal_is_rejected_and_never_rewrites_the_sealed_tuple() {
    // Issue #39 Phase C2 item 13.
    let env = Env::start("c2-seal-conflict").await;
    hold_chunk(&env.services, &env.fixture, 0).await;
    let original_digest = digest_wire(0xA1);

    let mut stream = handshake(&env.socket.0).await;
    let first = send_seal(&mut stream, env.seal_request(1, &original_digest))
        .await
        .unwrap();
    assert_eq!(first.outcome, ManifestSealOutcome::Sealed);

    // A second seal with a different artifact_digest.
    let conflicting = send_seal(&mut stream, env.seal_request(1, &digest_wire(0xB2)))
        .await
        .unwrap();
    assert_eq!(conflicting.outcome, ManifestSealOutcome::Rejected);
    assert_eq!(
        conflicting.reason,
        Some(ManifestSealRejectionReason::ManifestAlreadySealed)
    );
    assert!(conflicting.verification_handle.is_none());

    // The original sealed tuple is intact; the Artifact stays
    // PendingVerification (the winner's transition), never regressed.
    let (artifact, manifest) = context(&env.db.pool, env.fixture.transfer_id).await;
    assert_eq!(artifact.state, ArtifactState::PendingVerification);
    assert_eq!(
        manifest.artifact_digest.as_ref().map(|d| d.to_wire_value()),
        Some(original_digest)
    );

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn an_identical_seal_retry_after_reconnect_is_already_pending_with_the_same_artifact_id() {
    // Issue #39 Phase C2 items 14, 15, 23, 49, 58C: first seal committed, the
    // decision is "lost", the Worker reconnects on a fresh generation, signs a
    // fresh proof, and re-sends the identical seal.
    let env = Env::start("c2-seal-retry").await;
    hold_chunk(&env.services, &env.fixture, 0).await;
    hold_chunk(&env.services, &env.fixture, 1).await;
    let artifact_digest = digest_wire(0x5C);

    let mut stream = handshake(&env.socket.0).await;
    let first = send_seal(&mut stream, env.seal_request(2, &artifact_digest))
        .await
        .unwrap();
    assert_eq!(first.outcome, ManifestSealOutcome::Sealed);
    let first_artifact_id = first.artifact_id.unwrap();
    let first_handle = first.verification_handle.unwrap();

    // The decision is lost; the Worker drops the connection and reconnects.
    drop(stream);
    let mut stream = handshake(&env.socket.0).await;

    let retry = send_seal(&mut stream, env.seal_request(2, &artifact_digest))
        .await
        .expect("the idempotent retry produces a decision");
    assert_eq!(
        retry.outcome,
        ManifestSealOutcome::AlreadyPendingVerification
    );
    assert_eq!(retry.artifact_id, Some(first_artifact_id));
    assert_eq!(retry.chunk_count, Some(2));
    assert_eq!(
        retry.expected_artifact_digest.as_deref(),
        Some(artifact_digest.as_str())
    );
    let retry_handle = retry
        .verification_handle
        .expect("a fresh verification_handle");
    assert_ne!(retry_handle, first_handle);

    // No second durable transition: the Artifact is still PendingVerification
    // and the manifest tuple is unchanged.
    let (artifact, manifest) = context(&env.db.pool, env.fixture.transfer_id).await;
    assert_eq!(artifact.state, ArtifactState::PendingVerification);
    assert_eq!(manifest.chunk_count, Some(2));
    assert_eq!(
        manifest.artifact_digest.as_ref().map(|d| d.to_wire_value()),
        Some(artifact_digest)
    );

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_seal_against_a_terminal_artifact_is_a_generic_denial() {
    // Issue #39 Phase C2 item 17: a `Verified` (terminal) Artifact seal is
    // `denied`, never `manifest_already_sealed`.
    let env = Env::start("c2-seal-terminal").await;
    hold_chunk(&env.services, &env.fixture, 0).await;
    let artifact_digest = digest_wire(0x77);

    let mut stream = handshake(&env.socket.0).await;
    let sealed = send_seal(&mut stream, env.seal_request(1, &artifact_digest))
        .await
        .unwrap();
    assert_eq!(sealed.outcome, ManifestSealOutcome::Sealed);

    // Drive the Artifact terminal (`Verified`) directly at the row level.
    sqlx::query("UPDATE artifacts SET state = 'Verified' WHERE id = $1")
        .bind(env.fixture.artifact_id.0)
        .execute(&env.db.pool)
        .await
        .unwrap();

    // Reconnect + fresh proof + identical seal.
    drop(stream);
    let mut stream = handshake(&env.socket.0).await;
    let denied = send_seal(&mut stream, env.seal_request(1, &artifact_digest))
        .await
        .expect("a terminal-Artifact seal still produces a decision");
    assert_eq!(denied.outcome, ManifestSealOutcome::Denied);
    assert!(denied.reason.is_none());
    assert!(denied.verification_handle.is_none());

    drop(stream);
    env.finish().await;
}

#[tokio::test]
async fn a_post_commit_verification_handle_mint_failure_is_recoverable() {
    // Issue #39 Phase C2 items 21, 48: the durable seal commits, but the
    // `verification_handle` mint fails closed (transient store saturated) — no
    // success decision with an unusable handle, the durable
    // `PendingVerification` commit is NOT rolled back, and a fresh seal retry
    // reaches `already_pending_verification` + a fresh handle.
    let env = Env::start_with_capacity("c2-seal-mint-fail", Some(1)).await;
    hold_chunk(&env.services, &env.fixture, 0).await;
    let artifact_digest = digest_wire(0x42);

    let mut stream = handshake(&env.socket.0).await;

    // Saturate this generation's transient store: one authorized chunk_upload
    // mints one acceptance handle, filling the capacity-1 store.
    let (proof_id, issued_at, signature) = sign_proof(
        &env.signing_key,
        &env.token,
        &env.fixture,
        bamep_domain::AuthorizationOperation::ChunkUpload,
        Some(9),
    );
    let q = bamep_worker_protocol::AuthorizationQueryMessage::new(
        &env.token,
        env.fixture.transfer_id.0,
        9,
        proof_id,
        issued_at,
        signature,
    );
    send(&mut stream, &WorkerProtocolMessage::AuthorizationQuery(q))
        .await
        .unwrap();
    match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .unwrap()
        .unwrap()
    {
        WorkerProtocolMessage::AuthorizationDecision(d) => {
            assert_eq!(
                d.body.decision,
                bamep_worker_protocol::AuthorizationDecisionOutcome::Approved
            );
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }
    assert_eq!(
        env.registry.current_operations().unwrap().live_count(),
        1,
        "the transient store is now saturated"
    );

    // The seal commits durably, but the verification_handle mint fails closed
    // -> no response at all.
    let lost = send_seal(&mut stream, env.seal_request(1, &artifact_digest)).await;
    assert!(
        lost.is_none(),
        "a post-commit mint failure must not emit a success decision"
    );

    // The durable PendingVerification commit stands.
    let (artifact, manifest) = context(&env.db.pool, env.fixture.transfer_id).await;
    assert_eq!(artifact.state, ArtifactState::PendingVerification);
    assert!(manifest.sealed);

    // Reconnect on a fresh generation (transient store cleared) + fresh proof:
    // the identical seal now reaches already_pending_verification with a fresh
    // handle.
    drop(stream);
    let mut stream = handshake(&env.socket.0).await;
    let recovered = send_seal(&mut stream, env.seal_request(1, &artifact_digest))
        .await
        .expect("the recovery seal produces a decision");
    assert_eq!(
        recovered.outcome,
        ManifestSealOutcome::AlreadyPendingVerification
    );
    assert!(recovered
        .verification_handle
        .as_deref()
        .is_some_and(|h| h.starts_with("ver_")));

    drop(stream);
    env.finish().await;
}

// -- PostgreSQL-backed concurrency, driven through the real service -------

#[tokio::test]
async fn concurrent_identical_seals_converge_on_one_first_commit() {
    // Issue #39 Phase C2 item 40.
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c2-seal-race-same").await;
    let services = build_worker_control_services(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&services.authorization, &fixture, &signing_key).await;
    services
        .chunk_acceptance
        .commit_chunk_acceptance(fixture.transfer_id, 0, digest_wire(0), CHUNK_SIZE)
        .await
        .unwrap();
    let artifact_digest = digest_wire(0xC0);

    let make_input = || {
        let (proof_id, issued_at, signature) = sign_proof(
            &signing_key,
            &token,
            &fixture,
            bamep_domain::AuthorizationOperation::SealManifest,
            None,
        );
        ManifestSealInput {
            token: token.clone(),
            transfer_id: fixture.transfer_id.0,
            proof_id,
            issued_at_millis: issued_at,
            signature,
            chunk_count: 1,
            artifact_digest: artifact_digest.clone(),
        }
    };

    let (ra, rb) = tokio::join!(
        services.manifest_seal.commit_manifest_seal(make_input()),
        services.manifest_seal.commit_manifest_seal(make_input()),
    );
    let outcomes = [ra.unwrap(), rb.unwrap()];
    let sealed = outcomes
        .iter()
        .filter(|o| matches!(o, ManifestSealCommit::Sealed(_)))
        .count();
    let already = outcomes
        .iter()
        .filter(|o| matches!(o, ManifestSealCommit::AlreadyPending(_)))
        .count();
    assert_eq!(sealed, 1, "exactly one first-writer seal");
    assert_eq!(
        already, 1,
        "the other converges on already_pending_verification"
    );

    let repo = PostgresTransferRepository::new(db.pool.clone());
    let (ctx, _held) = repo
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ctx.artifact.state, ArtifactState::PendingVerification);
    assert_eq!(ctx.manifest.chunk_count, Some(1));

    db.teardown().await;
}

#[tokio::test]
async fn concurrent_conflicting_seals_leave_exactly_one_sealed_identity() {
    // Issue #39 Phase C2 item 41.
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c2-seal-race-diff").await;
    let services = build_worker_control_services(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&services.authorization, &fixture, &signing_key).await;
    services
        .chunk_acceptance
        .commit_chunk_acceptance(fixture.transfer_id, 0, digest_wire(0), CHUNK_SIZE)
        .await
        .unwrap();
    let digest_a = digest_wire(0xA1);
    let digest_b = digest_wire(0xB2);

    let input = |artifact_digest: &str| {
        let (proof_id, issued_at, signature) = sign_proof(
            &signing_key,
            &token,
            &fixture,
            bamep_domain::AuthorizationOperation::SealManifest,
            None,
        );
        ManifestSealInput {
            token: token.clone(),
            transfer_id: fixture.transfer_id.0,
            proof_id,
            issued_at_millis: issued_at,
            signature,
            chunk_count: 1,
            artifact_digest: artifact_digest.to_string(),
        }
    };

    let (ra, rb) = tokio::join!(
        services
            .manifest_seal
            .commit_manifest_seal(input(&digest_a)),
        services
            .manifest_seal
            .commit_manifest_seal(input(&digest_b)),
    );
    let outcomes = [ra.unwrap(), rb.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, ManifestSealCommit::Sealed(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, ManifestSealCommit::RejectedAlreadySealed))
            .count(),
        1
    );

    // Whichever digest won is immutable — the loser never overwrote it.
    let repo = PostgresTransferRepository::new(db.pool.clone());
    let (ctx, _held) = repo
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .unwrap();
    let durable = ctx
        .manifest
        .artifact_digest
        .as_ref()
        .unwrap()
        .to_wire_value();
    assert!(durable == digest_a || durable == digest_b);
    assert_eq!(ctx.artifact.state, ArtifactState::PendingVerification);

    db.teardown().await;
}
