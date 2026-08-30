//! Cross-crate interoperability proof (Issue #39 Phase E1): the **real**
//! `bamep_worker::ipc` control client against the **real**
//! `bamep_server::adapters::worker_control_plane::WorkerControlPlane`, over a
//! real Unix Domain Socket, backed by a real PostgreSQL-backed
//! `TransferAuthorizationService` — the same external framing/message shapes
//! a genuine Worker process uses (`docs/development/testing.md`;
//! `m1-worker-data-plane-control-contract.md` "Validation").
//!
//! `bamep-worker` is a **dev-dependency** of `bamep-server` for this file
//! only; the production dependency edge stays one-directional
//! (`bamep-worker` never depends on `bamep-server`). Behaviour beyond one
//! compatible request/response pair is covered by the worker crate's own
//! real-codec fake-peer tests (`crates/worker/tests/control_client.rs`).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker::ipc::{worker_control, AuthorizeChunkInput, ChunkAuthorization};
use ed25519_dalek::SigningKey;
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;

use support::{
    build_artifact_verification_service, build_authorization_service,
    build_chunk_acceptance_service, build_manifest_seal_service, dispatched_transfer_fixture,
    issue_capability, sign_proof, TempSocketPath, TestDatabase, IPC_TEST_TIMEOUT as TEST_TIMEOUT,
};

#[tokio::test]
async fn the_real_control_client_authorizes_a_real_signed_chunk_over_a_real_uds() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "e1-interop-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    // Real bamepd-side control plane.
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_plane_shutdown_tx, plane_shutdown_rx) = watch::channel(false);
    let plane_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        build_chunk_acceptance_service(db.pool.clone()),
        build_manifest_seal_service(db.pool.clone()),
        build_artifact_verification_service(db.pool.clone()),
        plane_shutdown_rx,
    ));

    // Real Worker-side control client.
    let (handle, driver) = worker_control(
        socket.0.clone(),
        Duration::from_millis(20),
        Duration::from_secs(4),
        Uuid::new_v4(),
    );
    let (client_shutdown_tx, client_shutdown_rx) = watch::channel(false);
    let driver_task = tokio::spawn(driver.run(async move {
        let mut rx = client_shutdown_rx;
        let _ = rx.wait_for(|stop| *stop).await;
    }));

    timeout(
        TEST_TIMEOUT,
        handle.authority().wait_for(|s| s.is_available()),
    )
    .await
    .expect("no timeout")
    .expect("watch open");

    // A real, byte-identical `chunk_upload` proof for chunk_index 0.
    let (proof_id, issued_at, signature) = sign_proof(
        &signing_key,
        &token,
        &fixture,
        bamep_domain::AuthorizationOperation::ChunkUpload,
        Some(0),
    );

    let outcome = timeout(
        TEST_TIMEOUT,
        handle.authorize_chunk(AuthorizeChunkInput {
            token: token.clone(),
            transfer_id: fixture.transfer_id.0,
            chunk_index: 0,
            proof_id: proof_id.clone(),
            issued_at,
            signature,
        }),
    )
    .await
    .expect("no timeout")
    .expect("control op ok");

    match outcome {
        ChunkAuthorization::Approved(approved) => {
            assert_eq!(
                approved.digest_algorithm,
                bamep_worker_protocol::WireDigestAlgorithm::Sha256
            );
            assert_eq!(approved.chunk_size, 4096);
        }
        ChunkAuthorization::Denied => panic!("a real signed proof against real state must approve"),
    }

    // A second denial path over the same live connection: a proof for a
    // different chunk_index than the client will send is rejected (the proof
    // signed chunk_index 0, the request claims 1) — proves denial round-trips
    // too, not just approval.
    let (proof_id_2, issued_at_2, signature_2) = sign_proof(
        &signing_key,
        &token,
        &fixture,
        bamep_domain::AuthorizationOperation::ChunkUpload,
        Some(0),
    );
    let denied = timeout(
        TEST_TIMEOUT,
        handle.authorize_chunk(AuthorizeChunkInput {
            token,
            transfer_id: fixture.transfer_id.0,
            chunk_index: 1,
            proof_id: proof_id_2,
            issued_at: issued_at_2,
            signature: signature_2,
        }),
    )
    .await
    .expect("no timeout")
    .expect("control op ok");
    assert!(
        matches!(denied, ChunkAuthorization::Denied),
        "a proof bound to a different chunk_index must be denied"
    );

    client_shutdown_tx.send(true).expect("stop client");
    timeout(TEST_TIMEOUT, driver_task)
        .await
        .expect("driver stops")
        .expect("driver join");
    plane_task.abort();
    db.teardown().await;
}
