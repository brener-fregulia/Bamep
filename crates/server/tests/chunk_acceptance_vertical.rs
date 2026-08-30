//! Issue #39 Phase C1 — `ChunkAcceptanceRequest` durable coordination.
//!
//! Covers:
//! - the full IPC vertical: a real Worker Protocol frame over a real UDS ->
//!   `WorkerControlPlane` -> Phase B `acceptance_handle` consume ->
//!   `ChunkAcceptanceService` -> PostgreSQL -> `ChunkAcceptanceDecision`
//!   (item 43);
//! - response-loss idempotency at the `bamepd` level (item 20);
//! - a foreign/stale `acceptance_handle` discarded with no response (item 7);
//! - PostgreSQL-backed durable first-writer / idempotency / conflict /
//!   terminal-state / size-bound / concurrency behavior driven through the
//!   real `ChunkAcceptanceService` and real transactions (item 42).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use bamep_domain::{ChunkIndex, Digest, DigestAlgorithm, TransferId};
use bamep_server::adapters::postgres::PostgresTransferRepository;
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::ChunkAcceptanceService;
use bamep_server::ports::{ChunkAcceptanceCommit, TransferRepository};
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker_protocol::{
    receive, send, ChunkAcceptanceOutcome, ChunkAcceptanceRejectionReason,
    ChunkAcceptanceRequestMessage, WorkerProtocolMessage,
};
use ed25519_dalek::SigningKey;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::timeout;

use support::{
    build_artifact_verification_service, build_authorization_service,
    build_chunk_acceptance_service, build_manifest_seal_service, dispatched_transfer_fixture,
    handshake, issue_capability, sign_proof, DispatchedTransfer, TempSocketPath, TestDatabase,
    IPC_TEST_TIMEOUT as TEST_TIMEOUT,
};

const CHUNK_SIZE: u32 = 4096;

fn digest_wire(byte: u8) -> String {
    Digest::new(DigestAlgorithm::Sha256, vec![byte; 32])
        .unwrap()
        .to_wire_value()
}

/// Drives one `chunk_upload` authorization over the wire and returns its
/// `acceptance_handle` and the exact digest the approval carries (if any).
async fn authorize_chunk(
    stream: &mut tokio::net::UnixStream,
    signing_key: &SigningKey,
    token: &str,
    fixture: &DispatchedTransfer,
    chunk_index: u64,
) -> (String, Option<String>) {
    let (proof_id, issued_at, signature) = sign_proof(
        signing_key,
        token,
        fixture,
        bamep_domain::AuthorizationOperation::ChunkUpload,
        Some(chunk_index),
    );
    let query = bamep_worker_protocol::AuthorizationQueryMessage::new(
        token,
        fixture.transfer_id.0,
        chunk_index,
        proof_id,
        issued_at,
        signature,
    );
    send(stream, &WorkerProtocolMessage::AuthorizationQuery(query))
        .await
        .expect("send AuthorizationQuery");
    match timeout(TEST_TIMEOUT, receive(stream))
        .await
        .expect("no timeout")
        .expect("receive")
    {
        WorkerProtocolMessage::AuthorizationDecision(d) => {
            assert_eq!(
                d.body.decision,
                bamep_worker_protocol::AuthorizationDecisionOutcome::Approved,
                "the fixture chunk_upload must be approved"
            );
            (
                d.body.acceptance_handle.expect("approved handle"),
                d.body.expected_chunk_digest,
            )
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }
}

async fn send_acceptance(
    stream: &mut tokio::net::UnixStream,
    handle: &str,
    fixture: &DispatchedTransfer,
    chunk_index: u64,
    digest: &str,
    size: u32,
) -> Option<(
    ChunkAcceptanceOutcome,
    Option<ChunkAcceptanceRejectionReason>,
)> {
    let request = ChunkAcceptanceRequestMessage::new(
        handle,
        fixture.transfer_id.0,
        chunk_index,
        digest,
        size,
    );
    let sent_id = request.envelope.message_id;
    send(
        stream,
        &WorkerProtocolMessage::ChunkAcceptanceRequest(request),
    )
    .await
    .expect("send ChunkAcceptanceRequest");
    match timeout(Duration::from_millis(800), receive(stream)).await {
        Ok(Ok(WorkerProtocolMessage::ChunkAcceptanceDecision(d))) => {
            assert_eq!(d.body.in_reply_to, sent_id);
            Some((d.body.outcome, d.body.reason))
        }
        Ok(other) => panic!("expected ChunkAcceptanceDecision, got {other:?}"),
        Err(_) => None, // no response — discarded / fail-closed
    }
}

#[tokio::test]
async fn a_verified_chunk_is_durably_committed_end_to_end_over_a_real_uds_socket() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c1-accept-commit").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        build_chunk_acceptance_service(db.pool.clone()),
        build_manifest_seal_service(db.pool.clone()),
        build_artifact_verification_service(db.pool.clone()),
        shutdown_rx,
    ));

    let mut stream = handshake(&socket.0).await;

    // chunk_index 0 not yet durable → no expected_chunk_digest.
    let (handle, expected) = authorize_chunk(&mut stream, &signing_key, &token, &fixture, 0).await;
    assert!(expected.is_none());

    let digest = digest_wire(0xAB);
    let outcome = send_acceptance(&mut stream, &handle, &fixture, 0, &digest, 2048).await;
    assert_eq!(outcome, Some((ChunkAcceptanceOutcome::Committed, None)));

    // The chunk identity is durably recorded AND held/verified.
    let transfers = PostgresTransferRepository::new(db.pool.clone());
    let (context, held) = transfers
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .expect("transfer context");
    assert!(held.contains(&ChunkIndex(0)));
    let expected_chunk = context.manifest.expected_chunk(ChunkIndex(0)).unwrap();
    assert_eq!(expected_chunk.size, 2048);
    assert_eq!(expected_chunk.digest.to_wire_value(), digest);

    // The single-use handle is gone.
    let store = registry.current_operations().expect("current store");
    assert!(store.acceptance_binding(&handle).is_none());

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

#[tokio::test]
async fn a_same_digest_different_size_follow_up_fails_closed_over_a_real_uds_socket() {
    // Issue #39 Phase C1 item 3: an identical-digest follow-up reporting a
    // size that contradicts the durable expected identity has no enumerable
    // `rejected` reason. `bamepd` sends no `ChunkAcceptanceDecision`, rewrites
    // nothing durable, and the single-use handle still stays consumed.
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c1-accept-size-contradiction").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        build_chunk_acceptance_service(db.pool.clone()),
        build_manifest_seal_service(db.pool.clone()),
        build_artifact_verification_service(db.pool.clone()),
        shutdown_rx,
    ));

    let mut stream = handshake(&socket.0).await;
    let digest = digest_wire(0xC3);

    // Durably commit chunk 0 at the full chunk size.
    let (handle1, _) = authorize_chunk(&mut stream, &signing_key, &token, &fixture, 0).await;
    let first = send_acceptance(&mut stream, &handle1, &fixture, 0, &digest, CHUNK_SIZE).await;
    assert_eq!(first, Some((ChunkAcceptanceOutcome::Committed, None)));

    // Same digest, contradicting size -> no response at all.
    let (handle2, expected) = authorize_chunk(&mut stream, &signing_key, &token, &fixture, 0).await;
    assert_eq!(expected.as_deref(), Some(digest.as_str()));
    let second = send_acceptance(&mut stream, &handle2, &fixture, 0, &digest, CHUNK_SIZE - 1).await;
    assert_eq!(
        second, None,
        "no ChunkAcceptanceDecision on a fail-closed follow-up"
    );

    // Durable digest / size / held state is exactly the first-writer's.
    let transfers = PostgresTransferRepository::new(db.pool.clone());
    let (context, held) = transfers
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        held.iter().copied().collect::<Vec<_>>(),
        vec![ChunkIndex(0)]
    );
    let expected_chunk = context.manifest.expected_chunk(ChunkIndex(0)).unwrap();
    assert_eq!(expected_chunk.size, CHUNK_SIZE);
    assert_eq!(expected_chunk.digest.to_wire_value(), digest);

    // The single-use handle is consumed regardless of the fail-closed outcome
    // (item 8); another logical retry needs a fresh proof and handle.
    let store = registry.current_operations().expect("current store");
    assert!(store.acceptance_binding(&handle2).is_none());

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

#[tokio::test]
async fn a_lost_decision_is_recovered_idempotently_by_a_fresh_proof_and_handle() {
    // Issue #39 Phase C1 item 20.
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c1-accept-lost").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        build_chunk_acceptance_service(db.pool.clone()),
        build_manifest_seal_service(db.pool.clone()),
        build_artifact_verification_service(db.pool.clone()),
        shutdown_rx,
    ));

    let digest = digest_wire(0x5C);
    let mut stream = handshake(&socket.0).await;

    // 1..4: authorize + commit chunk 3.
    let (handle1, _) = authorize_chunk(&mut stream, &signing_key, &token, &fixture, 3).await;
    let first = send_acceptance(&mut stream, &handle1, &fixture, 3, &digest, CHUNK_SIZE).await;
    assert_eq!(first, Some((ChunkAcceptanceOutcome::Committed, None)));

    // 5: the returned decision is "lost" — the Worker simply retries the
    // whole logical operation with a fresh proof.
    // 6..9: authorize the same chunk again — a fresh proof, fresh handle, and
    // now the approval carries the already-recorded expected digest.
    let (handle2, expected) = authorize_chunk(&mut stream, &signing_key, &token, &fixture, 3).await;
    assert_eq!(expected.as_deref(), Some(digest.as_str()));
    assert_ne!(handle1, handle2);

    // 10: the matching acceptance converges on already_committed — no second
    // semantic commit.
    let second = send_acceptance(&mut stream, &handle2, &fixture, 3, &digest, CHUNK_SIZE).await;
    assert_eq!(
        second,
        Some((ChunkAcceptanceOutcome::AlreadyCommitted, None))
    );

    let transfers = PostgresTransferRepository::new(db.pool.clone());
    let (context, held) = transfers
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(held.iter().filter(|i| **i == ChunkIndex(3)).count(), 1);
    assert_eq!(
        context.manifest.expected_chunk(ChunkIndex(3)).unwrap().size,
        CHUNK_SIZE
    );

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

#[tokio::test]
async fn a_foreign_acceptance_handle_is_discarded_with_no_response() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c1-accept-foreign").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        build_chunk_acceptance_service(db.pool.clone()),
        build_manifest_seal_service(db.pool.clone()),
        build_artifact_verification_service(db.pool.clone()),
        shutdown_rx,
    ));

    let mut stream = handshake(&socket.0).await;
    // Never authorized — a fabricated handle.
    let outcome = send_acceptance(
        &mut stream,
        "acc_deadbeefdeadbeefdeadbeefdeadbeef",
        &fixture,
        0,
        &digest_wire(1),
        1024,
    )
    .await;
    assert_eq!(
        outcome, None,
        "an unknown handle is discarded with no response"
    );

    // Nothing durable happened.
    let transfers = PostgresTransferRepository::new(db.pool.clone());
    let (_ctx, held) = transfers
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .unwrap();
    assert!(held.is_empty());

    // A wrong transfer_id / chunk_index on an otherwise real handle is also
    // discarded.
    let (handle, _) = authorize_chunk(&mut stream, &signing_key, &token, &fixture, 0).await;
    let mismatched =
        ChunkAcceptanceRequestMessage::new(handle, TransferId::new().0, 0, digest_wire(2), 1024);
    send(
        &mut stream,
        &WorkerProtocolMessage::ChunkAcceptanceRequest(mismatched),
    )
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_millis(600), receive(&mut stream))
            .await
            .is_err(),
        "a transfer/chunk-mismatched handle is discarded with no response"
    );

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

// --- PostgreSQL-backed durable behavior through the real service (item 42) ---

async fn service(pool: &PgPool) -> ChunkAcceptanceService {
    ChunkAcceptanceService::new(Arc::new(PostgresTransferRepository::new(pool.clone())))
}

#[tokio::test]
async fn new_then_identical_then_conflicting_and_size_bounds() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c1-accept-service").await;
    let svc = service(&db.pool).await;
    let d = digest_wire(0x11);

    // new -> committed
    assert_eq!(
        svc.commit_chunk_acceptance(fixture.transfer_id, 0, d.clone(), 4096)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::Committed
    );
    // identical -> already_committed
    assert_eq!(
        svc.commit_chunk_acceptance(fixture.transfer_id, 0, d.clone(), 4096)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::AlreadyCommitted
    );
    // same index, different digest -> conflict, never overwritten. This is
    // the *only* condition that yields the closed `chunk_identity_conflict`
    // public reason.
    assert_eq!(
        svc.commit_chunk_acceptance(fixture.transfer_id, 0, digest_wire(0x22), 4096)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::RejectedConflict
    );
    // same digest but a size contradicting the durable record -> fail closed,
    // never a silent size rewrite. No *different* digest exists, so this must
    // not surface `chunk_identity_conflict` (Issue #39 Phase C1 item 3).
    assert_eq!(
        svc.commit_chunk_acceptance(fixture.transfer_id, 0, d.clone(), 4095)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::FailClosed
    );
    // size outside the manifest bound -> fail closed (not an enumerable reason)
    assert_eq!(
        svc.commit_chunk_acceptance(fixture.transfer_id, 1, digest_wire(0x33), 0)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::FailClosed
    );
    assert_eq!(
        svc.commit_chunk_acceptance(fixture.transfer_id, 1, digest_wire(0x33), CHUNK_SIZE + 1)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::FailClosed
    );
    // non-canonical digest -> fail closed
    assert_eq!(
        svc.commit_chunk_acceptance(fixture.transfer_id, 1, "not-canonical".into(), 10)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::FailClosed
    );

    // The original identity is exactly as first written.
    let transfers = PostgresTransferRepository::new(db.pool.clone());
    let (context, held) = transfers
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        held.iter().copied().collect::<Vec<_>>(),
        vec![ChunkIndex(0)]
    );
    let ec = context.manifest.expected_chunk(ChunkIndex(0)).unwrap();
    assert_eq!(ec.size, 4096);
    assert_eq!(ec.digest.to_wire_value(), d);

    db.teardown().await;
}

#[tokio::test]
async fn a_terminal_owning_attempt_makes_the_chunk_not_continuable() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c1-accept-terminal").await;

    // Drive the owning Attempt terminal directly at the row level (a
    // reconciliation outcome this WP does not itself implement).
    sqlx::query("UPDATE attempts SET state = 'Failed' WHERE id = (SELECT attempt_id FROM transfers WHERE id = $1)")
        .bind(fixture.transfer_id.0)
        .execute(&db.pool)
        .await
        .unwrap();

    let svc = service(&db.pool).await;
    assert_eq!(
        svc.commit_chunk_acceptance(fixture.transfer_id, 0, digest_wire(1), 4096)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::RejectedNotContinuable
    );

    let transfers = PostgresTransferRepository::new(db.pool.clone());
    let (_c, held) = transfers
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        held.is_empty(),
        "no durable mutation on a not-continuable reject"
    );
    db.teardown().await;
}

#[tokio::test]
async fn concurrent_same_index_same_digest_commits_exactly_once() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c1-accept-race-same").await;
    let d = digest_wire(0x77);

    let a = service(&db.pool).await;
    let b = service(&db.pool).await;
    let (ra, rb) = tokio::join!(
        a.commit_chunk_acceptance(fixture.transfer_id, 4, d.clone(), 4096),
        b.commit_chunk_acceptance(fixture.transfer_id, 4, d.clone(), 4096),
    );
    let outcomes = [ra.unwrap(), rb.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == ChunkAcceptanceCommit::Committed)
            .count(),
        1,
        "exactly one first-writer commit"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == ChunkAcceptanceCommit::AlreadyCommitted)
            .count(),
        1
    );
    db.teardown().await;
}

#[tokio::test]
async fn concurrent_same_index_different_digest_preserves_the_first_writer() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "c1-accept-race-diff").await;

    let a = service(&db.pool).await;
    let b = service(&db.pool).await;
    let da = digest_wire(0xA1);
    let db_ = digest_wire(0xB2);
    let (ra, rb) = tokio::join!(
        a.commit_chunk_acceptance(fixture.transfer_id, 2, da.clone(), 4096),
        b.commit_chunk_acceptance(fixture.transfer_id, 2, db_.clone(), 4096),
    );
    let outcomes = [ra.unwrap(), rb.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == ChunkAcceptanceCommit::Committed)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == ChunkAcceptanceCommit::RejectedConflict)
            .count(),
        1
    );

    // Whichever digest won, it is immutable — the loser never overwrote it.
    let transfers = PostgresTransferRepository::new(db.pool.clone());
    let (context, _held) = transfers
        .find_transfer_context(fixture.transfer_id)
        .await
        .unwrap()
        .unwrap();
    let durable = context
        .manifest
        .expected_chunk(ChunkIndex(2))
        .unwrap()
        .digest
        .to_wire_value();
    assert!(durable == da || durable == db_);
    // And it stays that way under a further conflicting attempt.
    let further = service(&db.pool).await;
    let other = if durable == da { db_ } else { da };
    assert_eq!(
        further
            .commit_chunk_acceptance(fixture.transfer_id, 2, other, 4096)
            .await
            .unwrap(),
        ChunkAcceptanceCommit::RejectedConflict
    );
    db.teardown().await;
}
