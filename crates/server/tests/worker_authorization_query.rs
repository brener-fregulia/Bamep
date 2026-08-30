//! Issue #38 "Worker UDS" validation: `AuthorizationQuery`/
//! `AuthorizationDecision` over a *real* Unix Domain Socket against the real
//! `WorkerControlPlane` (`bamepd`-side) and a real, PostgreSQL-backed
//! `TransferAuthorizationService` — the same real external framing/message
//! shapes a genuine Worker process uses, not an in-process shortcut
//! (`docs/development/testing.md`; `m1-worker-data-plane-control-contract.md`
//! "Validation": "Contract tests exercise the real framing/message shapes").
//!
//! `worker_control_plane.rs` already proves #37's handshake/generation/socket
//! semantics with a fake always-denying authorization repository; this file
//! is the complementary #38 proof that a real signed proof against real
//! durable state is correctly approved/denied end to end over the wire.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::runtime::transient_worker_operations::TransientOperationError;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker_protocol::{
    receive, send, AuthorizationDecisionOutcome, AuthorizationQueryMessage, WireDigestAlgorithm,
    WorkerProtocolMessage,
};
use ed25519_dalek::SigningKey;
use tokio::sync::watch;
use tokio::time::timeout;

use support::{
    build_artifact_verification_service, build_authorization_service,
    build_chunk_acceptance_service, build_manifest_seal_service, dispatched_transfer_fixture,
    handshake, issue_capability, sign_proof, DispatchedTransfer, TempSocketPath, TestDatabase,
    IPC_TEST_TIMEOUT as TEST_TIMEOUT,
};

/// A real, byte-identical `chunk_upload` `AuthorizationQuery` for
/// `chunk_index 0`, plus the exact `proof_id` wire value it signed.
fn signed_authorization_query(
    signing_key: &SigningKey,
    token: &str,
    fixture: &DispatchedTransfer,
) -> (AuthorizationQueryMessage, String) {
    let (proof_id_wire, issued_at, signature_wire) = sign_proof(
        signing_key,
        token,
        fixture,
        bamep_domain::AuthorizationOperation::ChunkUpload,
        Some(0),
    );
    let message = AuthorizationQueryMessage::new(
        token,
        fixture.transfer_id.0,
        0,
        proof_id_wire.clone(),
        issued_at,
        signature_wire,
    );
    (message, proof_id_wire)
}

#[tokio::test]
async fn a_real_signed_query_over_a_real_uds_socket_is_approved() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-approved-01").await;
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
    let (query, proof_id_wire) = signed_authorization_query(&signing_key, &token, &fixture);
    let sent_id = query.envelope.message_id;
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");

    let acceptance_handle = match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            assert_eq!(decision.body.in_reply_to, sent_id);
            assert_eq!(
                decision.body.decision,
                AuthorizationDecisionOutcome::Approved
            );
            assert_eq!(
                decision.body.digest_algorithm,
                Some(WireDigestAlgorithm::Sha256)
            );
            assert_eq!(decision.body.chunk_size, Some(4096));
            decision
                .body
                .acceptance_handle
                .expect("an approved decision must carry an acceptance_handle")
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    };

    // The handle exists in *this* generation's transient store, bound to the
    // exact operation identity (`m1-worker-data-plane-control-contract.md`
    // "Transient operation handles").
    let operations = registry
        .current_operations()
        .expect("the current generation must have published its transient store");
    assert!(acceptance_handle.starts_with("acc_"));
    let binding = operations
        .acceptance_binding(&acceptance_handle)
        .expect("the returned handle must be a live acceptance binding");
    assert_eq!(binding.transfer_id, fixture.transfer_id);
    assert_eq!(binding.chunk_index, 0);
    assert_eq!(binding.proof_id, proof_id_wire);
    assert_eq!(operations.live_count(), 1);

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

#[tokio::test]
async fn a_query_signed_by_the_wrong_key_over_a_real_uds_socket_is_denied() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-denied-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;
    let unrelated_key = SigningKey::from_bytes(&rand::random());

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
    let (query, _proof_id_wire) = signed_authorization_query(&unrelated_key, &token, &fixture);
    let sent_id = query.envelope.message_id;
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");

    match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            assert_eq!(decision.body.in_reply_to, sent_id);
            assert_eq!(decision.body.decision, AuthorizationDecisionOutcome::Denied);
            assert!(
                decision.body.acceptance_handle.is_none()
                    && decision.body.expected_chunk_digest.is_none()
                    && decision.body.digest_algorithm.is_none(),
                "a denial must never carry any further detail"
            );
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }

    // A denied authorization never mints a transient binding.
    if let Some(operations) = registry.current_operations() {
        assert_eq!(operations.live_count(), 0);
    }

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

/// Issue #38 "Disconnect/reconnect behavior": a fresh connection generation
/// after an earlier one disconnected mid-query must still correctly serve a
/// brand-new query — proving the per-connection request/response loop
/// (`WorkerControlPlane::handle_connection`) leaves no state behind that
/// could corrupt a later generation.
#[tokio::test]
async fn a_new_connection_after_a_prior_ones_mid_query_disconnect_still_works() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-reconnect-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        registry,
        Arc::clone(&authorization),
        build_chunk_acceptance_service(db.pool.clone()),
        build_manifest_seal_service(db.pool.clone()),
        build_artifact_verification_service(db.pool.clone()),
        shutdown_rx,
    ));

    // First generation: send a query, then disconnect immediately without
    // waiting for the reply.
    {
        let mut stream = handshake(&socket.0).await;
        let (query, _) = signed_authorization_query(&signing_key, &token, &fixture);
        send(
            &mut stream,
            &WorkerProtocolMessage::AuthorizationQuery(query),
        )
        .await
        .expect("send AuthorizationQuery");
        drop(stream);
    }

    // Second, independent generation: a fresh query must still be answered
    // correctly.
    let mut stream = handshake(&socket.0).await;
    let (query, _) = signed_authorization_query(&signing_key, &token, &fixture);
    let sent_id = query.envelope.message_id;
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");
    match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            assert_eq!(decision.body.in_reply_to, sent_id);
            assert_eq!(
                decision.body.decision,
                AuthorizationDecisionOutcome::Approved
            );
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

/// Issue #39 Phase B (`m1-worker-data-plane-control-contract.md` "Transient
/// operation handles" / "Failure semantics"): an `acceptance_handle` returned
/// on one generation carries no authority once that connection drops — the
/// store is gone from the registry, and a task still holding its `Arc` sees
/// every operation fail closed. A fresh generation starts with an empty
/// transient authority set.
#[tokio::test]
async fn an_acceptance_handle_loses_all_authority_when_its_generation_ends() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-phaseb-disc-01").await;
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
    let (query, _) = signed_authorization_query(&signing_key, &token, &fixture);
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");
    let handle = match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            decision.body.acceptance_handle.expect("approved handle")
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    };
    // Keep a strong reference to generation A's store, then drop the socket.
    let store_a = registry.current_operations().expect("generation A store");
    drop(stream);

    // Generation A ends; its store is no longer reachable through the
    // registry and every operation on the retained `Arc` fails closed.
    let mut waited = 0;
    while registry.current_operations().is_some() && waited < 50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
    }
    assert!(registry.current_operations().is_none());
    assert_eq!(
        store_a.consume_acceptance(&handle, fixture.transfer_id, 0),
        Err(TransientOperationError::StaleGeneration)
    );
    assert!(store_a.acceptance_binding(&handle).is_none());

    // A fresh generation starts empty; the old handle is unknown to it.
    let _stream_b = handshake(&socket.0).await;
    let mut waited = 0;
    while registry.current_operations().is_none() && waited < 50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
    }
    let store_b = registry.current_operations().expect("generation B store");
    assert!(store_b.acceptance_binding(&handle).is_none());
    assert_eq!(store_b.live_count(), 0);

    run_task.abort();
    db.teardown().await;
}

/// Issue #39 Phase B (`m1-worker-data-plane-control-contract.md` "Transient
/// operation handles"): when the generation's transient store is at capacity,
/// a fully-authorized `AuthorizationQuery` still fails closed — the Worker
/// receives the same generic denial, never an approved decision carrying an
/// unusable handle, and the already-live binding is never overwritten. The
/// saturation cause never appears on the wire.
#[tokio::test]
async fn a_saturated_transient_store_denies_an_otherwise_authorized_query() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-phaseb-sat-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    // Capacity 1: the first approved query fills the store.
    let registry = Arc::new(WorkerAuthorityRegistry::with_operations_capacity(1));
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

    let (query1, _) = signed_authorization_query(&signing_key, &token, &fixture);
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query1),
    )
    .await
    .expect("send first AuthorizationQuery");
    let first_handle = match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(d) => {
            assert_eq!(d.body.decision, AuthorizationDecisionOutcome::Approved);
            d.body.acceptance_handle.expect("first handle")
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    };

    // A second, independently valid query (fresh proof) — the store is now
    // full, so minting its acceptance binding fails closed.
    let (query2, _) = signed_authorization_query(&signing_key, &token, &fixture);
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query2),
    )
    .await
    .expect("send second AuthorizationQuery");
    match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(d) => {
            assert_eq!(
                d.body.decision,
                AuthorizationDecisionOutcome::Denied,
                "a saturated transient store fails the query closed"
            );
            assert!(d.body.acceptance_handle.is_none());
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }

    // The first live binding was never overwritten.
    let operations = registry.current_operations().expect("current store");
    assert!(operations.acceptance_binding(&first_handle).is_some());
    assert_eq!(operations.live_count(), 1);

    drop(stream);
    run_task.abort();
    db.teardown().await;
}
