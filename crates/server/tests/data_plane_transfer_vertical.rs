//! Issue #19 checkpoint C3 — the integrated RF-005 happy-path vertical.
//!
//! One deterministic successful M1 Agent -> Server capture, every boundary real:
//!
//! ```text
//! durable Job / JobStep / Transfer / Artifact (real PostgreSQL)
//!   -> #40 non-destructive commit_transfer_dispatch -> Attempt{Dispatched}
//!   -> ActionDispatchService.dispatch_transfer over the real
//!      OutboundSessionDirectory / AgentControlGateway session
//!   -> real loopback TCP -> pinned TLS 1.3 -> WebSocket -> Agent Protocol v1
//!        ActionDispatch
//!   -> committed C1 DataPlaneTransferAgent::accept -> ActionAck{Accepted}
//!   -> real WSS TransferAuthorizationRequest (ephemeral Ed25519 proof key)
//!   -> real Server TransferAuthorizationService -> TransferAuthorizationGrant{token, base_url}
//!   -> committed C1 DataPlaneTransferAgent::run:
//!        real hyper-1 HTTPS (exact leaf pin) GET resume / PUT chunks / POST seal
//!          -> real bamep_worker::data_plane::DataPlane (Worker TLS server)
//!            -> real Worker IPC client + real D1 staging + real D2 reconstruction
//!              -> real WorkerControlPlane over AF_UNIX (bamep-worker-protocol v1)
//!                -> real PostgreSQL-backed chunk acceptance / manifest seal /
//!                   Artifact verification -> durable Artifact Verified
//!   -> ActionProgress{bytes_processed} over the same WSS session
//!   -> ActionResult{Succeeded, TRANSFER_VERIFIED} over the same WSS session
//!   -> C2 TransferTerminalEvidenceService through the real AgentControlGateway
//!   -> Attempt Succeeded -> JobStep Succeeded -> Job Succeeded
//! ```
//!
//! The shared composition lives in `support::transfer_vertical`; the
//! interruption/resume + fail-closed matrix that reuses it is
//! `data_plane_transfer_failure_matrix.rs` (Issue #19 checkpoint C4).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use bamep_agent_protocol::AgentProtocolMessage;
use bamep_simulator::{
    AgentTransferAuthorization, DataPlaneTransferDirection, InMemoryTransferSource,
    TransferActionResult, TransferRunOptions, TransferRunOutcome,
};
use sqlx::Row;
use support::transfer_vertical::{run_transfer_streaming_progress, Vertical, SOURCE_LEN};
use support::TestDatabase;
use uuid::Uuid;

#[tokio::test]
async fn agent_to_server_transfer_happy_path_reaches_verified_artifact_and_succeeded_job() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "rf005-happy").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;

    // §9 — the seven-item destructive gate was never evaluated: this JobStep
    // carries no durable destructive-authorization snapshot, yet the transfer
    // dispatched through the #40 non-destructive path.
    let snap = sqlx::query(
        "SELECT authorized_inventory_revision_id, authorized_target_fingerprint \
         FROM job_steps WHERE id = $1",
    )
    .bind(v.fixture.step_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        snap.get::<Option<Uuid>, _>(0).is_none() && snap.get::<Option<String>, _>(1).is_none(),
        "the transfer path must dispatch without any destructive-only prerequisite"
    );

    // ---- one real authenticated WSS session; dispatch the RF-005 action ----
    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;

    let dispatch = session.expect_dispatch().await;
    assert_eq!(dispatch.body.action_id, v.fixture.action_id);
    assert_eq!(dispatch.body.action_type, "bamep.m1.data-plane-transfer");

    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("C1 accepts the RF-005 dispatch");
    assert_eq!(accepted.transfer_id(), transfer_id);
    assert_eq!(accepted.artifact_id(), artifact_id);
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;

    // ---- real WSS TransferAuthorizationRequest / Grant --------------------
    let (proof_key, grant) = session.obtain_grant(v.fixture.action_id, transfer_id).await;
    assert!(!grant.body.token.is_empty());
    assert_eq!(
        grant.body.data_plane_base_url, v.data_plane_base_url,
        "the grant points the Agent at the real Worker HTTPS origin"
    );
    let auth = AgentTransferAuthorization::new(
        proof_key,
        grant.body.token.clone(),
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        grant.body.data_plane_base_url.clone(),
    );

    // ---- C1 run: real HTTPS resume / upload / seal -> durable Verified ----
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 19);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        v.fixture.action_id,
    )
    .await;

    let TransferRunOutcome::Completed(TransferActionResult::Verified {
        artifact_id: verified,
    }) = run.outcome
    else {
        panic!("expected Completed(Verified), got {:?}", run.outcome);
    };
    assert_eq!(verified, artifact_id);
    assert_eq!(run.progress_observed, vec![0, 4096, 8192, 10_000]);

    // ---- §22 ordering: Verified is durable BEFORE workflow success --------
    // Artifact already `Verified` (Worker seal path, its own earlier
    // transaction); no `ActionResult` on the wire yet; workflow still Running.
    // With C2's CASE A gate this proves ActionResult{Succeeded} cannot commit
    // workflow success before `Verified`.
    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.attempt_state().await, "InProgress");
    assert_eq!(v.job_state().await, "Running");

    // ---- ActionResult{Succeeded, TRANSFER_VERIFIED} over the same session --
    session
        .send(AgentProtocolMessage::ActionResult(
            TransferActionResult::Verified {
                artifact_id: verified,
            }
            .into_action_result(v.fixture.action_id),
        ))
        .await;
    session.close_and_join().await;

    // ---- §21 final durable assertions ------------------------------------
    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.attempt_state().await, "Succeeded");
    assert_eq!(v.job_step_state().await, "Succeeded");
    assert_eq!(v.job_state().await, "Succeeded");
    assert_eq!(v.event_count("JobSucceeded").await, 1);
    assert_eq!(v.event_count("JobFailed").await, 0);
    assert_eq!(v.terminal_audit_count().await, 1);

    // Identity correlation held end to end.
    let b = sqlx::query(
        "SELECT t.artifact_id, t.attempt_id, a.action_id, s.job_id \
         FROM transfers t JOIN attempts a ON a.id = t.attempt_id \
         JOIN job_steps s ON s.id = a.job_step_id WHERE t.id = $1",
    )
    .bind(transfer_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(b.get::<Uuid, _>("artifact_id"), artifact_id);
    assert_eq!(b.get::<Uuid, _>("attempt_id"), v.fixture.attempt.id.0);
    assert_eq!(b.get::<Uuid, _>("action_id"), v.fixture.action_id.as_uuid());
    assert_eq!(b.get::<Uuid, _>("job_id"), v.fixture.job_id);

    // The reservation was released exactly once — full capacity again.
    assert!(v
        .arbiter
        .acquire(vec![
            bamep_server::runtime::resource_arbiter::ResourceClaim::new(
                bamep_server::runtime::resource_arbiter::ResourceKind::new("network"),
                10,
            )
        ])
        .is_ok());

    drop(v);
    db.teardown().await;
}
