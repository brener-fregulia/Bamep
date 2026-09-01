//! Issue #19 checkpoint C4 — the integrated RF-005 interruption/resume +
//! fail-closed matrix.
//!
//! Every scenario reuses the one shared vertical harness
//! (`support::transfer_vertical`), so each still crosses real Agent Protocol
//! v1 WSS, real trusted-bootstrap pinning, real `TransferAuthorizationRequest`/
//! `Grant`, real Worker HTTPS, real AF_UNIX `bamep-worker-protocol` v1, and
//! real PostgreSQL. C1's committed deterministic hooks
//! (`TransferRunOptions::{interrupt_after_newly_held_chunks,
//! corrupt_transmitted_bytes_of_chunk}`, `InMemoryTransferSource::mutate_chunk`)
//! drive the adversarial inputs — never re-implemented Server-side.
//!
//! RF-005 is referenced in prose only; the Rust identities are behavioural.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::sync::Arc;

use bamep_agent_protocol::{
    AgentProtocolMessage, CancelAckMessage, CancelAckOutcome, StatusReportMessage,
};
use bamep_domain::{
    ActionId, Actor, ChunkSize, DigestAlgorithm, EndpointId, JobId, SourceProvenance,
    TransferDirection,
};
use bamep_server::adapters::postgres::{PostgresJobRepository, PostgresTransferRepository};
use bamep_server::application::{
    ActionEvidenceService, JobSchedulingService, JobService, TransferDispatchResult,
    TransferDispatchService, TransferService,
};
use bamep_server::ports::JobRepository;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_simulator::{
    AgentTransferAuthorization, DataPlaneClient, DataPlaneTransferDirection,
    InMemoryTransferSource, PutChunkOutcome, ResumeOutcome, SuspendReason, TransferActionResult,
    TransferOperation, TransferRunOptions, TransferRunOutcome,
};
use support::transfer_vertical::{
    run_transfer_streaming_progress, Vertical, CHUNK_SIZE, SOURCE_LEN,
};
use support::TestDatabase;

// =====================================================================
// helpers
// =====================================================================

fn sha256_wire(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

/// The RF-005 terminal `ActionResult` for `outcome`, sent over `session`.
async fn send_terminal(
    session: &mut support::transfer_vertical::AgentSession,
    outcome: &TransferActionResult,
    action_id: bamep_agent_protocol::ProtocolId,
) {
    session
        .send(AgentProtocolMessage::ActionResult(
            outcome.into_action_result(action_id),
        ))
        .await;
}

// =====================================================================
// §5 — interruption + legitimate resume, one continuous session
// =====================================================================

#[tokio::test]
async fn interruption_then_legitimate_resume_reaches_verified_with_the_same_identity() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-interrupt-resume").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;

    // --- run 1: durably hold one chunk, then C1's deterministic interrupt ---
    let (k1, g1) = session.obtain_grant(action_id, transfer_id).await;
    let auth1 = AgentTransferAuthorization::new(
        k1,
        g1.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g1.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 7);
    let run1 = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth1,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut session,
        action_id,
    )
    .await;
    let TransferRunOutcome::Suspended(s) = run1.outcome else {
        panic!("expected Suspended, got {:?}", run1.outcome);
    };
    assert_eq!(s.reason, SuspendReason::InterruptionHookFired);
    assert_eq!(s.transfer_id, transfer_id);
    assert_eq!(s.artifact_id, artifact_id);

    // Durable intermediate state: interruption alone never fabricates anything.
    assert_eq!(v.artifact_state().await, "Incomplete");
    assert_eq!(v.attempt_state().await, "InProgress");
    assert_eq!(v.held_chunk_indices().await, vec![0]);
    let recorded_digest_0 = v.recorded_chunk_digest(0).await.expect("chunk 0 recorded");
    let no_result_yet = v.event_count("JobSucceeded").await + v.event_count("JobFailed").await;
    assert_eq!(no_result_yet, 0);

    // --- resume: fresh grant, same accepted handle, same identity ---
    let (k2, g2) = session.obtain_grant(action_id, transfer_id).await;
    let auth2 = AgentTransferAuthorization::new(
        k2,
        g2.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g2.body.data_plane_base_url,
    );
    let run2 = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth2,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        action_id,
    )
    .await;
    let TransferRunOutcome::Completed(TransferActionResult::Verified { artifact_id: got }) =
        run2.outcome
    else {
        panic!("expected Completed(Verified), got {:?}", run2.outcome);
    };
    assert_eq!(got, artifact_id);
    // Progress resumed from the already-durable 4096 bytes, never regressing.
    assert_eq!(run2.progress_observed, vec![4096, 8192, 10_000]);

    send_terminal(
        &mut session,
        &TransferActionResult::Verified { artifact_id },
        action_id,
    )
    .await;
    session.close_and_join().await;

    // Same Transfer, same Artifact, same Attempt, same action_id; chunk 0 was
    // never re-recorded and its identity is unchanged.
    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.attempt_state().await, "Succeeded");
    assert_eq!(v.job_state().await, "Succeeded");
    assert_eq!(v.attempt_count_for_step().await, 1, "no new Attempt");
    assert_eq!(
        v.recorded_chunk_digest(0).await.unwrap(),
        recorded_digest_0,
        "the recorded chunk-0 identity was never rewritten"
    );
    assert_eq!(v.held_chunk_indices().await, vec![0, 1, 2]);

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §6 + §22 — real WSS interruption, reconnect, data-plane resume, then #28
// reconciliation completes the (durably Verified) transfer
// =====================================================================

#[tokio::test]
async fn wss_disconnect_then_reconnect_resumes_the_data_plane_and_reconciliation_completes_it() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-wss-reconnect").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    // --- session 1: dispatch, ack, hold one chunk, then drop the socket ---
    let mut s1 = v.connect_agent().await;
    v.dispatch_transfer(&s1).await;
    let dispatch = s1.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    s1.send(AgentProtocolMessage::ActionAck(response.ack)).await;

    let (k1, g1) = s1.obtain_grant(action_id, transfer_id).await;
    let auth1 = AgentTransferAuthorization::new(
        k1,
        g1.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g1.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 11);
    let run1 = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth1,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut s1,
        action_id,
    )
    .await;
    assert!(matches!(run1.outcome, TransferRunOutcome::Suspended(_)));
    assert_eq!(v.held_chunk_indices().await, vec![0]);

    // A real transport interruption — no Close frame.
    s1.drop_ungracefully().await;

    // #28 moved the Attempt to AwaitingReconciliation because this session
    // carried the ActionDispatch — the action is NOT redispatched.
    assert_eq!(v.attempt_state().await, "AwaitingReconciliation");
    assert_eq!(v.attempt_count_for_step().await, 1);
    assert_eq!(v.artifact_state().await, "Incomplete");

    // --- session 2: reconnect. #28 issues a StatusQuery for the uncertain
    // Attempt; the Agent truthfully reports it is still Running, which returns
    // the Attempt to InProgress and lets the data plane resume. ---
    let mut s2 = v.connect_agent().await;
    let status_query = s2.recv().await;
    let AgentProtocolMessage::StatusQuery(q) = status_query else {
        panic!("expected StatusQuery, got {status_query:?}");
    };
    assert_eq!(q.body.action_id, action_id);
    s2.send(AgentProtocolMessage::StatusReport(
        StatusReportMessage::new(action_id, bamep_agent_protocol::KnownActionState::Running),
    ))
    .await;
    // Give the server loop a moment to apply the StatusReport before the next
    // authorization request checks the Attempt state.
    poll_until(|| async { v.attempt_state().await == "InProgress" }).await;

    let (k2, g2) = s2.obtain_grant(action_id, transfer_id).await;
    let auth2 = AgentTransferAuthorization::new(
        k2,
        g2.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g2.body.data_plane_base_url,
    );
    let run2 = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth2,
        &source,
        &TransferRunOptions::default(),
        &mut s2,
        action_id,
    )
    .await;
    assert!(matches!(
        run2.outcome,
        TransferRunOutcome::Completed(TransferActionResult::Verified { .. })
    ));
    // The already-held chunk was NOT retransmitted — progress resumed at 4096.
    assert_eq!(run2.progress_observed, vec![4096, 8192, 10_000]);

    // The terminal ActionResult now resolves the (InProgress) Attempt normally.
    send_terminal(
        &mut s2,
        &TransferActionResult::Verified { artifact_id },
        action_id,
    )
    .await;
    s2.close_and_join().await;

    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.attempt_state().await, "Succeeded");
    assert_eq!(v.job_state().await, "Succeeded");
    assert_eq!(v.attempt_count_for_step().await, 1, "no new Attempt");
    assert_eq!(v.held_chunk_indices().await, vec![0, 1, 2]);

    drop(v);
    db.teardown().await;
}

#[tokio::test]
async fn reconciled_transfer_cancellation_fails_the_incomplete_artifact_atomically() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-reconciled-cancelled").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut s1 = v.connect_agent().await;
    v.dispatch_transfer(&s1).await;
    let dispatch = s1.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    s1.send(AgentProtocolMessage::ActionAck(response.ack)).await;

    let (key, grant) = s1.obtain_grant(action_id, transfer_id).await;
    let authorization = AgentTransferAuthorization::new(
        key,
        grant.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        grant.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 53);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &authorization,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut s1,
        action_id,
    )
    .await;
    assert!(matches!(run.outcome, TransferRunOutcome::Suspended(_)));
    assert_eq!(v.artifact_state().await, "Incomplete");
    assert_eq!(v.held_chunk_indices().await, vec![0]);

    v.cancellation_service()
        .request(
            JobId(v.fixture.job_id),
            Actor::Operator {
                label: "reconciled-cancel-operator".into(),
            },
        )
        .await
        .unwrap();
    let AgentProtocolMessage::CancelAction(cancel) = s1.recv().await else {
        panic!("expected CancelAction");
    };
    assert_eq!(cancel.body.action_id, action_id);

    // Lose the session without CancelAck or ActionResult. The reconnect uses
    // #28 StatusQuery/StatusReport and is the sole terminal evidence path.
    s1.drop_ungracefully().await;
    assert_eq!(v.attempt_state().await, "AwaitingReconciliation");

    let mut s2 = v.connect_agent().await;
    let AgentProtocolMessage::StatusQuery(query) = s2.recv().await else {
        panic!("expected StatusQuery");
    };
    assert_eq!(query.body.action_id, action_id);
    s2.send(AgentProtocolMessage::StatusReport(
        StatusReportMessage::new(action_id, bamep_agent_protocol::KnownActionState::Cancelled),
    ))
    .await;
    poll_until(|| async { v.attempt_state().await == "Cancelled" }).await;

    // A matching duplicate and conflicting late success both cross the same
    // real WSS path. The first committed terminal outcome must remain final.
    s2.send(AgentProtocolMessage::StatusReport(
        StatusReportMessage::new(action_id, bamep_agent_protocol::KnownActionState::Cancelled),
    ))
    .await;
    s2.send(AgentProtocolMessage::StatusReport(
        StatusReportMessage::new(action_id, bamep_agent_protocol::KnownActionState::Succeeded),
    ))
    .await;
    s2.close_and_join().await;

    assert_eq!(v.artifact_state().await, "Failed");
    assert_eq!(v.attempt_state().await, "Cancelled");
    assert_eq!(v.job_step_state().await, "Cancelled");
    assert_eq!(v.job_state().await, "Cancelled");
    assert_eq!(v.held_chunk_indices().await, vec![0]);
    assert_eq!(v.event_count("JobCancelled").await, 1);
    assert_eq!(v.event_count("JobSucceeded").await, 0);
    assert_eq!(v.terminal_audit_count().await, 1);

    drop(v);
    db.teardown().await;
}

/// Polls `cond` until true or the test timeout elapses.
async fn poll_until<F, Fut>(mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if cond().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("poll_until: condition never became true");
}

// =====================================================================
// §22 — the data plane completes (Artifact durably Verified) but the terminal
// ActionResult is lost; #28 reconciliation completes the workflow without
// re-running the transfer and without rewriting the Verified Artifact
// =====================================================================

#[tokio::test]
async fn a_verified_artifact_whose_action_result_was_lost_is_reconciled_not_fabricated() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-verified-lost").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut s1 = v.connect_agent().await;
    v.dispatch_transfer(&s1).await;
    let dispatch = s1.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    s1.send(AgentProtocolMessage::ActionAck(response.ack)).await;
    let (k, g) = s1.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 53);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions::default(),
        &mut s1,
        action_id,
    )
    .await;
    assert!(matches!(
        run.outcome,
        TransferRunOutcome::Completed(TransferActionResult::Verified { .. })
    ));
    assert_eq!(v.artifact_state().await, "Verified");

    // The Agent's terminal ActionResult never reaches the Server — the
    // connection is lost first.
    s1.drop_ungracefully().await;
    assert_eq!(v.attempt_state().await, "AwaitingReconciliation");
    let held_before = v.held_chunk_indices().await;

    // On reconnect the Server's StatusQuery is answered with the authoritative
    // Succeeded; workflow resolves; the data plane is NOT re-run and the
    // Verified Artifact is never touched.
    let mut s2 = v.connect_agent().await;
    let AgentProtocolMessage::StatusQuery(q) = s2.recv().await else {
        panic!("expected StatusQuery");
    };
    assert_eq!(q.body.action_id, action_id);
    s2.send(AgentProtocolMessage::StatusReport(
        StatusReportMessage::new(action_id, bamep_agent_protocol::KnownActionState::Succeeded),
    ))
    .await;
    s2.close_and_join().await;

    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.attempt_state().await, "Succeeded");
    assert_eq!(v.job_state().await, "Succeeded");
    assert_eq!(
        v.held_chunk_indices().await,
        held_before,
        "data plane not re-run"
    );
    assert_eq!(v.event_count("JobFailed").await, 0);

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §23 — the seal committed a durably Failed Artifact but the terminal
// ActionResult is lost; #28 reconciliation drives workflow failure without
// rewriting the Artifact
// =====================================================================

#[tokio::test]
async fn a_failed_artifact_whose_action_result_was_lost_is_reconciled_without_rewrite() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-failed-lost").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut s1 = v.connect_agent().await;
    v.dispatch_transfer(&s1).await;
    let dispatch = s1.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    s1.send(AgentProtocolMessage::ActionAck(response.ack)).await;
    let _ = accepted;
    let (k, g) = s1.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token.clone(),
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url.clone(),
    );
    // Raw HTTPS: upload every chunk honestly, then seal with a full-Artifact
    // digest that does not match the reconstructed bytes. bamepd's own
    // comparison drives `PendingVerification -> Failed` (a 200, never an error)
    // — the same shape C1 would map to ARTIFACT_VERIFICATION_FAILED. (C1's
    // `run` always declares an honest digest, so this durable-Failed
    // precondition is set through the raw client.)
    let client = DataPlaneClient::connect(&g.body.data_plane_base_url, v.identity.fingerprint)
        .expect("connect");
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 59);
    let bytes = source.as_bytes().to_vec();
    for (idx, chunk) in bytes.chunks(CHUNK_SIZE as usize).enumerate() {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let proof = auth
            .create_proof(TransferOperation::ChunkUpload, Some(idx as u64), now)
            .unwrap();
        let out = client
            .put_chunk(
                auth.token(),
                transfer_id,
                idx as u64,
                &sha256_wire(chunk),
                &proof,
                chunk.to_vec(),
            )
            .await
            .expect("transport ok");
        assert!(matches!(out, PutChunkOutcome::Accepted { .. }));
    }
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let seal_proof = auth
        .create_proof(TransferOperation::SealManifest, None, now)
        .unwrap();
    let sealed = client
        .seal(
            auth.token(),
            transfer_id,
            &seal_proof,
            bytes.chunks(CHUNK_SIZE as usize).count() as u64,
            &sha256_wire(b"deliberately not the real artifact digest"),
        )
        .await
        .expect("transport ok");
    assert!(
        matches!(
            sealed,
            bamep_simulator::SealOutcome::Completed {
                artifact_status: bamep_simulator::SealArtifactStatus::Failed,
                ..
            }
        ),
        "expected a durably Failed Artifact, got {sealed:?}"
    );
    assert_eq!(v.artifact_state().await, "Failed");

    // The terminal ActionResult is lost; the connection drops.
    s1.drop_ungracefully().await;
    assert_eq!(v.attempt_state().await, "AwaitingReconciliation");

    // On reconnect the authoritative StatusReport{Failed} drives workflow
    // failure; the already-Failed Artifact is never rewritten.
    let mut s2 = v.connect_agent().await;
    let AgentProtocolMessage::StatusQuery(q) = s2.recv().await else {
        panic!("expected StatusQuery");
    };
    assert_eq!(q.body.action_id, action_id);
    s2.send(AgentProtocolMessage::StatusReport(
        StatusReportMessage::new(action_id, bamep_agent_protocol::KnownActionState::Failed),
    ))
    .await;
    s2.close_and_join().await;

    assert_eq!(v.artifact_state().await, "Failed");
    assert_eq!(v.attempt_state().await, "Failed");
    assert_eq!(v.job_state().await, "Failed");
    assert_eq!(v.event_count("JobFailed").await, 1);
    assert_eq!(v.event_count("JobSucceeded").await, 0);

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §21 — StatusReport{Unknown} for an uncertain transfer Attempt never proves
// non-execution or success; it stays uncertain until real evidence arrives
// =====================================================================

#[tokio::test]
async fn status_report_unknown_never_proves_transfer_non_execution_or_success() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-status-unknown").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut s1 = v.connect_agent().await;
    v.dispatch_transfer(&s1).await;
    let dispatch = s1.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    s1.send(AgentProtocolMessage::ActionAck(response.ack)).await;
    let (k, g) = s1.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 61);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut s1,
        action_id,
    )
    .await;
    assert!(matches!(run.outcome, TransferRunOutcome::Suspended(_)));
    let held_before = v.held_chunk_indices().await;
    s1.drop_ungracefully().await;
    assert_eq!(v.attempt_state().await, "AwaitingReconciliation");

    let mut s2 = v.connect_agent().await;
    let AgentProtocolMessage::StatusQuery(_) = s2.recv().await else {
        panic!("expected StatusQuery");
    };
    s2.send(AgentProtocolMessage::StatusReport(
        StatusReportMessage::new(action_id, bamep_agent_protocol::KnownActionState::Unknown),
    ))
    .await;
    s2.close_and_join().await;

    // Unknown proved nothing: the Attempt stays uncertain, the Artifact stays
    // Incomplete, the durably-held chunk is untouched, and neither JobSucceeded
    // nor JobFailed was emitted.
    assert_eq!(v.attempt_state().await, "AwaitingReconciliation");
    assert_eq!(v.artifact_state().await, "Incomplete");
    assert_eq!(v.held_chunk_indices().await, held_before);
    assert_eq!(v.event_count("JobSucceeded").await, 0);
    assert_eq!(v.event_count("JobFailed").await, 0);

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §7 + §17 — corrupted transmission -> CHUNK_VERIFICATION_FAILED -> C2 CASE C
// =====================================================================

#[tokio::test]
async fn corrupted_transmission_is_rejected_and_fails_the_incomplete_artifact_closed() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-corrupt").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (k, g) = session.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url,
    );

    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 13);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions {
            // Transmit corrupted bytes for chunk 1 while still declaring the
            // digest of the true source bytes — the Worker's independent hash
            // rejects it (409 DIGEST_MISMATCH).
            corrupt_transmitted_bytes_of_chunk: Some(1),
            ..Default::default()
        },
        &mut session,
        action_id,
    )
    .await;
    assert_eq!(
        run.outcome,
        TransferRunOutcome::Completed(TransferActionResult::ChunkVerificationFailed {
            artifact_id
        })
    );

    // Corrupt bytes never durably accepted; chunk 1 has no held identity.
    assert_eq!(v.held_chunk_indices().await, vec![0]);
    assert!(v.recorded_chunk_digest(1).await.is_none());
    assert_eq!(v.artifact_state().await, "Incomplete");

    // Terminal ActionResult crosses the real WSS; C2 atomically drives
    // Artifact Incomplete -> Failed + workflow failure.
    send_terminal(
        &mut session,
        &TransferActionResult::ChunkVerificationFailed { artifact_id },
        action_id,
    )
    .await;
    session.close_and_join().await;

    assert_eq!(v.artifact_state().await, "Failed");
    assert_eq!(v.attempt_state().await, "Failed");
    assert_eq!(v.job_step_state().await, "Failed");
    assert_eq!(
        v.job_step_failure_reason().await.as_deref(),
        Some("ExecutionFailed")
    );
    assert_eq!(v.job_state().await, "Failed");
    assert_eq!(v.event_count("JobFailed").await, 1);
    assert_eq!(v.event_count("JobSucceeded").await, 0);
    // The one chunk that *was* durably held keeps its identity — never rewritten.
    assert_eq!(v.held_chunk_indices().await, vec![0]);

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §8 + §13 — source mutation of an already-recorded chunk fails closed;
// the durably-bound source identity is never rewritten
// =====================================================================

#[tokio::test]
async fn source_mutation_of_a_recorded_chunk_fails_closed_without_rewriting_identity() {
    let db = TestDatabase::setup().await;
    let v =
        Vertical::start_with_provenance(&db, "c4-source-mutation", "captured-from-disk-7").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;

    // --- run 1: durably record chunk 0, then interrupt ---
    let mut source = InMemoryTransferSource::pattern(SOURCE_LEN, 17);
    let (k1, g1) = session.obtain_grant(action_id, transfer_id).await;
    let auth1 = AgentTransferAuthorization::new(
        k1,
        g1.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g1.body.data_plane_base_url,
    );
    let run1 = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth1,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut session,
        action_id,
    )
    .await;
    assert!(matches!(run1.outcome, TransferRunOutcome::Suspended(_)));
    let recorded_digest_0 = v.recorded_chunk_digest(0).await.expect("chunk 0 recorded");
    let bound_provenance = v.source_provenance().await;
    assert_eq!(bound_provenance, "captured-from-disk-7");

    // The deterministic source's chunk-0 bytes now change — source evidence
    // for this Transfer is no longer consistent with what was durably bound.
    source.mutate_chunk(0, CHUNK_SIZE);

    // --- run 2: resume, same identity, fresh authorization ---
    let (k2, g2) = session.obtain_grant(action_id, transfer_id).await;
    let auth2 = AgentTransferAuthorization::new(
        k2,
        g2.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g2.body.data_plane_base_url,
    );
    let run2 = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth2,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        action_id,
    )
    .await;
    assert_eq!(
        run2.outcome,
        TransferRunOutcome::Completed(TransferActionResult::ChunkVerificationFailed {
            artifact_id
        })
    );

    send_terminal(
        &mut session,
        &TransferActionResult::ChunkVerificationFailed { artifact_id },
        action_id,
    )
    .await;
    session.close_and_join().await;

    // The recorded chunk-0 identity — the durable per-chunk source binding —
    // was never rewritten; no replacement Transfer/Artifact; source provenance
    // unchanged; terminal Artifact Failed; workflow Failed; no partial success.
    assert_eq!(v.recorded_chunk_digest(0).await.unwrap(), recorded_digest_0);
    assert_eq!(v.source_provenance().await, bound_provenance);
    assert_eq!(v.artifact_state().await, "Failed");
    assert_eq!(v.attempt_state().await, "Failed");
    assert_eq!(v.job_state().await, "Failed");
    assert_eq!(v.attempt_count_for_step().await, 1);
    let transfer_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM transfers WHERE artifact_id = $1")
            .bind(artifact_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(transfer_count, 1, "no replacement Transfer");
    let _ = transfer_id;

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §9 + §11 — a capability/proof against the wrong immutable binding reaches
// the real Worker/bamepd authority and produces the non-enumerable 401
// =====================================================================

#[tokio::test]
async fn a_proof_against_the_wrong_chunk_binding_is_generically_denied_by_the_real_authority() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-wrong-binding").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (key, grant) = session.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        key,
        grant.body.token.clone(),
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        grant.body.data_plane_base_url.clone(),
    );
    let client = DataPlaneClient::connect(&grant.body.data_plane_base_url, v.identity.fingerprint)
        .expect("connect");
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let chunk = vec![0xAB; CHUNK_SIZE as usize];
    let digest = sha256_wire(&chunk);

    // A proof correctly signed for chunk_index 0, replayed against chunk_index 3.
    let proof_for_0 = auth
        .create_proof(TransferOperation::ChunkUpload, Some(0), now)
        .unwrap();
    let out = client
        .put_chunk(
            auth.token(),
            transfer_id,
            3,
            &digest,
            &proof_for_0,
            chunk.clone(),
        )
        .await
        .expect("transport ok");
    assert_eq!(
        out,
        PutChunkOutcome::AuthorizationDenied,
        "the wrong-chunk binding is the single non-enumerable 401"
    );

    // A resume-discovery proof used for a chunk upload — wrong operation binding.
    let resume_proof = auth
        .create_proof(TransferOperation::ResumeDiscovery, None, now)
        .unwrap();
    let out = client
        .put_chunk(auth.token(), transfer_id, 0, &digest, &resume_proof, chunk)
        .await
        .expect("transport ok");
    assert_eq!(out, PutChunkOutcome::AuthorizationDenied);

    // Nothing became durably accepted; no terminal success.
    assert_eq!(v.held_chunk_indices().await, Vec::<i32>::new());
    assert_eq!(v.artifact_state().await, "Incomplete");
    assert_eq!(v.attempt_state().await, "InProgress");

    session.close_and_join().await;
    drop(v);
    db.teardown().await;
}

// =====================================================================
// §10 — a replayed proof is rejected by the real ReplayCache
// =====================================================================

#[tokio::test]
async fn a_replayed_transfer_proof_is_denied_by_the_real_replay_cache() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-replay").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (key, grant) = session.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        key,
        grant.body.token.clone(),
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        grant.body.data_plane_base_url.clone(),
    );
    let client = DataPlaneClient::connect(&grant.body.data_plane_base_url, v.identity.fingerprint)
        .expect("connect");
    let now = chrono::Utc::now().timestamp_millis() as u64;

    // One resume-discovery proof, used twice verbatim (test-only reuse — C1's
    // production path always mints a fresh proof_id; this narrow seam does not
    // weaken that).
    let proof = auth
        .create_proof(TransferOperation::ResumeDiscovery, None, now)
        .unwrap();
    let first = client
        .discover_resume(auth.token(), transfer_id, &proof)
        .await
        .expect("transport ok");
    assert!(matches!(first, ResumeOutcome::Approved(_)));

    let second = client
        .discover_resume(auth.token(), transfer_id, &proof)
        .await
        .expect("transport ok");
    assert_eq!(
        second,
        ResumeOutcome::AuthorizationDenied,
        "the exact proof_id is single-use"
    );

    assert_eq!(v.held_chunk_indices().await, Vec::<i32>::new());
    assert_eq!(v.artifact_state().await, "Incomplete");

    session.close_and_join().await;
    drop(v);
    db.teardown().await;
}

// =====================================================================
// §15 + §16 — Worker runtime restart mid-transfer cannot fabricate success;
// durable held state survives; a fresh-authorization resume finishes
// =====================================================================

#[tokio::test]
async fn worker_runtime_restart_mid_transfer_cannot_fabricate_success_then_resumes_cleanly() {
    let db = TestDatabase::setup().await;
    let mut v = Vertical::start(&db, "c4-worker-restart").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;

    let (k1, g1) = session.obtain_grant(action_id, transfer_id).await;
    let auth1 = AgentTransferAuthorization::new(
        k1,
        g1.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g1.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 23);
    let run1 = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth1,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut session,
        action_id,
    )
    .await;
    assert!(matches!(run1.outcome, TransferRunOutcome::Suspended(_)));
    assert_eq!(v.held_chunk_indices().await, vec![0]);
    let digest_0 = v.recorded_chunk_digest(0).await.unwrap();

    // --- real runtime interruption: HTTPS listener + IPC + control plane ---
    v.restart_worker().await;

    // The restart itself fabricated nothing: Artifact still Incomplete, no
    // ActionResult, durable held state intact.
    assert_eq!(v.artifact_state().await, "Incomplete");
    assert_eq!(v.attempt_state().await, "InProgress");
    assert_eq!(v.held_chunk_indices().await, vec![0]);
    assert_eq!(v.recorded_chunk_digest(0).await.unwrap(), digest_0);
    assert_eq!(v.event_count("JobSucceeded").await, 0);

    // --- resume against the restarted Worker with fresh authorization ---
    let (k2, g2) = session.obtain_grant(action_id, transfer_id).await;
    let auth2 = AgentTransferAuthorization::new(
        k2,
        g2.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g2.body.data_plane_base_url,
    );
    let run2 = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth2,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        action_id,
    )
    .await;
    assert!(matches!(
        run2.outcome,
        TransferRunOutcome::Completed(TransferActionResult::Verified { .. })
    ));
    assert_eq!(run2.progress_observed, vec![4096, 8192, 10_000]);

    send_terminal(
        &mut session,
        &TransferActionResult::Verified { artifact_id },
        action_id,
    )
    .await;
    session.close_and_join().await;

    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.attempt_state().await, "Succeeded");
    assert_eq!(v.job_state().await, "Succeeded");
    assert_eq!(v.held_chunk_indices().await, vec![0, 1, 2]);
    assert_eq!(v.recorded_chunk_digest(0).await.unwrap(), digest_0);

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §18 / §19 / §20A — transfer cancelled before completion: the truthful
// CancelAck{Cancelled} atomically fails the Incomplete Artifact
// =====================================================================

#[tokio::test]
async fn transfer_cancelled_before_completion_fails_the_incomplete_artifact_atomically() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-cancel-before").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;

    // Durably hold one chunk, then C1 interrupts (transfer still Incomplete).
    let (k, g) = session.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 29);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut session,
        action_id,
    )
    .await;
    assert!(matches!(run.outcome, TransferRunOutcome::Suspended(_)));
    assert_eq!(v.artifact_state().await, "Incomplete");
    assert_eq!(v.held_chunk_indices().await, vec![0]);

    // --- operator cancellation over the real outbound path ---
    let cancellation = v.cancellation_service();
    cancellation
        .request(
            JobId(v.fixture.job_id),
            Actor::Operator {
                label: "c4-operator".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(v.job_state().await, "Cancelling");

    // The Agent receives CancelAction and answers truthfully — no ActionResult.
    let AgentProtocolMessage::CancelAction(cancel) = session.recv().await else {
        panic!("expected CancelAction");
    };
    assert_eq!(cancel.body.action_id, action_id);
    session
        .send(AgentProtocolMessage::CancelAck(CancelAckMessage::new(
            action_id,
            CancelAckOutcome::Cancelled,
        )))
        .await;
    session.close_and_join().await;

    // Atomic: Artifact Incomplete -> Failed committed with the #27 terminal
    // transition; the durably-held chunk is NOT rolled back.
    assert_eq!(v.artifact_state().await, "Failed");
    assert_eq!(v.attempt_state().await, "Cancelled");
    assert_eq!(v.job_step_state().await, "Cancelled");
    assert_eq!(v.job_state().await, "Cancelled");
    assert_eq!(v.event_count("JobCancelled").await, 1);
    assert_eq!(v.event_count("JobFailed").await, 0);
    assert_eq!(v.event_count("JobSucceeded").await, 0);
    assert_eq!(
        v.held_chunk_indices().await,
        vec![0],
        "held chunk not rolled back"
    );
    assert_eq!(v.terminal_audit_count().await, 1);
    let audit_detail: String = sqlx::query_scalar(
        "SELECT detail FROM audit_records WHERE attempt_id = $1 AND detail LIKE '%terminal state%'",
    )
    .bind(v.fixture.attempt.id.0)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(audit_detail.contains("artifact Incomplete -> Failed"));
    let _ = transfer_id;

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §20B — cancellation race after the Artifact is already Verified:
// Verified is never rewritten to Failed
// =====================================================================

#[tokio::test]
async fn cancellation_after_a_verified_artifact_never_rewrites_it() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-cancel-after-verified").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (k, g) = session.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 31);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        action_id,
    )
    .await;
    assert!(matches!(
        run.outcome,
        TransferRunOutcome::Completed(TransferActionResult::Verified { .. })
    ));
    assert_eq!(v.artifact_state().await, "Verified");

    // Operator cancels *after* the Artifact is durably Verified but before the
    // terminal ActionResult is consumed — a genuine race.
    v.cancellation_service()
        .request(
            JobId(v.fixture.job_id),
            Actor::Operator {
                label: "c4-late-operator".into(),
            },
        )
        .await
        .unwrap();
    let AgentProtocolMessage::CancelAction(_) = session.recv().await else {
        panic!("expected CancelAction");
    };
    // The Agent aborts before its ActionResult and reports Cancelled. The
    // transfer-cancel composition's guard sees the bound Artifact is already
    // `Verified` (not `Incomplete`) and never rewrites it.
    session
        .send(AgentProtocolMessage::CancelAck(CancelAckMessage::new(
            action_id,
            CancelAckOutcome::Cancelled,
        )))
        .await;
    session.close_and_join().await;

    // Verified is never rewritten to Failed; the #27 terminal transition
    // applies unchanged; no JobFailed for a Verified capture.
    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.attempt_state().await, "Cancelled");
    assert_eq!(v.job_step_state().await, "Cancelled");
    assert_eq!(v.job_state().await, "Cancelled");
    assert_eq!(v.event_count("JobCancelled").await, 1);
    assert_eq!(v.event_count("JobFailed").await, 0);
    // The terminal audit records the cancellation with no artifact transition.
    let audit_detail: String = sqlx::query_scalar(
        "SELECT detail FROM audit_records WHERE attempt_id = $1 AND detail LIKE '%terminal state%'",
    )
    .bind(v.fixture.attempt.id.0)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(!audit_detail.contains("Incomplete -> Failed"));
    let _ = transfer_id;

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §20C — a matching duplicate transfer CancelAck{Cancelled} is idempotent
// =====================================================================

#[tokio::test]
async fn a_matching_duplicate_transfer_cancel_ack_is_idempotent() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-cancel-dup").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (k, g) = session.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 37);
    let _ = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut session,
        action_id,
    )
    .await;

    v.cancellation_service()
        .request(
            JobId(v.fixture.job_id),
            Actor::Operator {
                label: "c4-dup-operator".into(),
            },
        )
        .await
        .unwrap();
    let AgentProtocolMessage::CancelAction(_) = session.recv().await else {
        panic!("expected CancelAction");
    };
    let ack = CancelAckMessage::new(action_id, CancelAckOutcome::Cancelled);
    session
        .send(AgentProtocolMessage::CancelAck(ack.clone()))
        .await;
    // The exact same evidence again, under a fresh message_id.
    session
        .send(AgentProtocolMessage::CancelAck(ack.with_fresh_message_id()))
        .await;
    session.close_and_join().await;

    assert_eq!(v.artifact_state().await, "Failed");
    assert_eq!(v.attempt_state().await, "Cancelled");
    assert_eq!(v.job_state().await, "Cancelled");
    assert_eq!(v.event_count("JobCancelled").await, 1, "no duplicate event");
    assert_eq!(v.terminal_audit_count().await, 1, "no duplicate audit");
    let _ = transfer_id;

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §12 — a Verified transfer Artifact keeps capture_consistency NotEstablished;
// the integrity result is not rewritten because of it
// =====================================================================

#[tokio::test]
async fn a_verified_transfer_artifact_keeps_capture_consistency_not_established() {
    let db = TestDatabase::setup().await;
    let v = Vertical::start(&db, "c4-capture-consistency").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (k, g) = session.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 41);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        action_id,
    )
    .await;
    assert!(matches!(
        run.outcome,
        TransferRunOutcome::Completed(TransferActionResult::Verified { .. })
    ));
    send_terminal(
        &mut session,
        &TransferActionResult::Verified { artifact_id },
        action_id,
    )
    .await;
    session.close_and_join().await;

    // Cryptographic integrity and capture consistency are independent
    // (`m0-data-plane-and-storage-contracts.md` "Capture-consistency fact"):
    // the Artifact is Verified, yet capture_consistency stays NotEstablished
    // (M1 has no mechanism that positively confirms it — it is never the
    // default), and the integrity result was NOT downgraded because of that.
    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.artifact_capture_consistency().await, "NotEstablished");
    assert_eq!(v.job_state().await, "Succeeded");

    // Domain-level independence: setting capture_consistency does not touch
    // `state`, and vice versa.
    let art = bamep_domain::Artifact {
        id: bamep_domain::ArtifactId(artifact_id),
        state: bamep_domain::ArtifactState::Verified,
        capture_consistency: bamep_domain::CaptureConsistency::NotEstablished,
    };
    let established =
        bamep_domain::set_capture_consistency(&art, bamep_domain::CaptureConsistency::Established);
    assert_eq!(established.state, bamep_domain::ArtifactState::Verified);
    assert_eq!(
        established.capture_consistency,
        bamep_domain::CaptureConsistency::Established
    );

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §14 — a transfer's source identity is independent of any later destructive
// target identity: differing is not, by itself, a rejection
// =====================================================================

#[tokio::test]
async fn a_transfer_source_identity_is_independent_of_any_destructive_target_identity() {
    let db = TestDatabase::setup().await;
    // A concrete capture source that is deliberately unlike any "current disk"
    // fingerprint a later destructive step would revalidate.
    let v =
        Vertical::start_with_provenance(&db, "c4-source-vs-target", "backup-of-replaced-disk-OLD")
            .await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (k, g) = session.obtain_grant(action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 43);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        action_id,
    )
    .await;
    assert!(matches!(
        run.outcome,
        TransferRunOutcome::Completed(TransferActionResult::Verified { .. })
    ));
    send_terminal(
        &mut session,
        &TransferActionResult::Verified { artifact_id },
        action_id,
    )
    .await;
    session.close_and_join().await;

    // The transfer completed Verified with a source provenance that names a
    // now-replaced disk — the transfer path never compares source provenance
    // to any target identity (the #40 `TransferDispatchInputs` carry no target
    // field at all; `m0-data-plane-and-storage-contracts.md` "Artifact
    // provenance and target identity": "Source identity and destructive target
    // identity are independent").
    assert_eq!(v.artifact_state().await, "Verified");
    assert_eq!(v.source_provenance().await, "backup-of-replaced-disk-OLD");
    assert_eq!(v.job_state().await, "Succeeded");
    let _ = transfer_id;

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §24 — non-destructive dispatch: destructive-only prerequisites may be
// absent (positive), a genuine generic precondition still blocks (negative)
// =====================================================================

#[tokio::test]
async fn non_destructive_transfer_dispatch_needs_no_destructive_gate_but_generic_preconditions_apply(
) {
    let db = TestDatabase::setup().await;
    let pool = db.pool.clone();
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        4,
    )]));

    // --- positive: a valid Agent -> Server transfer dispatches with NO
    // destructive-authorization snapshot on the JobStep ---
    let v = support::transfer_vertical::dispatched_fixture(
        &pool,
        &job_repo,
        &arbiter,
        "c4-nd-positive",
        "disk-0",
    )
    .await;
    let snap = sqlx::query(
        "SELECT authorized_inventory_revision_id, authorized_target_fingerprint \
         FROM job_steps WHERE id = $1",
    )
    .bind(v.step_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    use sqlx::Row;
    assert!(
        snap.get::<Option<uuid::Uuid>, _>(0).is_none()
            && snap.get::<Option<String>, _>(1).is_none()
    );
    assert_eq!(v.attempt.state, bamep_domain::AttemptState::Dispatched);

    // --- negative: a genuine generic workflow precondition failure blocks
    // dispatch — a JobStep whose Job was never admitted (Job still Pending) ---
    let jobs = JobService::new(Arc::clone(&job_repo));
    let scheduling = JobSchedulingService::new(Arc::clone(&job_repo));
    let transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
    let dispatch = TransferDispatchService::new(Arc::clone(&job_repo), Arc::clone(&arbiter));

    let job = jobs.create_workflow(v.endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;
    // Deliberately skip `scheduling.admit(...)` — the Job stays Pending, so the
    // generic workflow precondition "Job is Running" is not satisfied.
    let step_state_before: String =
        sqlx::query_scalar("SELECT state::text FROM job_steps WHERE id = $1")
            .bind(step_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    let ctx = transfers
        .create_transfer_context(
            v.endpoint_id,
            job.id,
            step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(CHUNK_SIZE).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap();
    let result = dispatch
        .commit_transfer_dispatch(
            job.id,
            step_id,
            ctx.transfer.id,
            vec![ResourceClaim::new(ResourceKind::new("network"), 1)],
        )
        .await
        .unwrap();
    assert!(
        matches!(result, TransferDispatchResult::Rejected(_)),
        "a non-admitted (Pending) Job must not dispatch a transfer, got {result:?}"
    );
    // Nothing durable changed for the blocked step.
    let step_state: String = sqlx::query_scalar("SELECT state::text FROM job_steps WHERE id = $1")
        .bind(step_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(step_state, step_state_before);
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE job_step_id = $1")
        .bind(step_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
    let _ = scheduling;

    db.teardown().await;
}

// =====================================================================
// §26 — the transfer CancelAck composition does not change generic
// (simulated-execution) cancellation behaviour
// =====================================================================

#[tokio::test]
async fn simulated_execution_cancellation_is_unchanged_by_the_transfer_composition() {
    let db = TestDatabase::setup().await;
    let pool = db.pool.clone();

    // Build a dispatched *simulated-execution* Attempt (no bound Transfer) via
    // the shared #26/#27 fixture, then drive operator cancellation +
    // CancelAck{Cancelled} through the generic path and assert the byte-for-byte
    // #27 outcome (Attempt/JobStep/Job Cancelled, one JobCancelled, one audit,
    // no Artifact involved).
    let fx = support::dispatched_transfer_fixture(&pool, "c4-simexec-cancel").await;
    // `dispatched_transfer_fixture` binds a Transfer; to get a *non-transfer*
    // action, detach it (mirrors `transfer_terminal_evidence.rs`'s §26 test).
    sqlx::query("UPDATE transfers SET attempt_id = NULL WHERE attempt_id IS NOT NULL")
        .execute(&pool)
        .await
        .unwrap();

    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone())) as Arc<dyn JobRepository>;
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let arbiter = Arc::new(TechnicalResourceArbiter::new([]));
    let outbound =
        Arc::new(bamep_server::runtime::outbound_sessions::OutboundSessionDirectory::new());
    let cancellation = bamep_server::application::CancellationService::new(
        Arc::clone(&job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
        Arc::clone(&outbound) as Arc<dyn bamep_server::ports::AgentDispatchPort>,
    );

    let job_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT s.job_id FROM attempts a JOIN job_steps s ON s.id = a.job_step_id WHERE a.action_id = $1",
    )
    .bind(fx.action_id.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();

    // Request cancellation directly (no live session — send just fails, which
    // `request` tolerates; the durable Cancelling transition is what matters).
    let _ = cancellation
        .request(
            JobId(job_id),
            Actor::Operator {
                label: "c4-simexec-op".into(),
            },
        )
        .await
        .unwrap();

    let evidence_service = ActionEvidenceService::new(
        Arc::clone(&job_repo),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    );
    let _ = &evidence_service;

    cancellation
        .apply_cancel_ack(
            fx.action_id,
            fx.endpoint_id,
            bamep_domain::CancelAckEvidence::Cancelled,
        )
        .await
        .unwrap();

    let attempt_state: String =
        sqlx::query_scalar("SELECT state::text FROM attempts WHERE action_id = $1")
            .bind(fx.action_id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempt_state, "Cancelled");
    let job_state: String = sqlx::query_scalar("SELECT state::text FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(job_state, "Cancelled");
    let cancelled_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM domain_events WHERE job_id = $1 AND event_type::text = 'JobCancelled'",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cancelled_events, 1);

    db.teardown().await;
}

// =====================================================================
// §30 / §31 — server-startup reconciliation + transient-authorization
// invalidation on a bamepd restart
// =====================================================================

#[tokio::test]
async fn bamepd_restart_marks_in_flight_transfer_uncertain_and_invalidates_old_capabilities() {
    let db = TestDatabase::setup().await;
    let mut v = Vertical::start(&db, "c4-bamepd-restart").await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    let action_id = v.fixture.action_id;

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (k, g) = session.obtain_grant(action_id, transfer_id).await;
    let old_token = g.body.token.clone();
    let old_auth = AgentTransferAuthorization::new(
        k,
        g.body.token,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        g.body.data_plane_base_url.clone(),
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 47);
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &old_auth,
        &source,
        &TransferRunOptions {
            interrupt_after_newly_held_chunks: Some(1),
            ..Default::default()
        },
        &mut session,
        action_id,
    )
    .await;
    assert!(matches!(run.outcome, TransferRunOutcome::Suspended(_)));
    assert_eq!(v.held_chunk_indices().await, vec![0]);
    session.drop_ungracefully().await;

    // --- bamepd restart: durable PostgreSQL state kept; transient
    // capability/replay authority replaced; then startup reconciliation ---
    v.restart_bamepd_transient_authority().await;
    v.reconciliation_service()
        .reconcile_on_startup()
        .await
        .unwrap();

    // An in-flight (InProgress / lost-session) transfer Attempt is now
    // AwaitingReconciliation; durable Transfer/Artifact/chunk state is intact;
    // no fabricated success.
    assert!(matches!(
        v.attempt_state().await.as_str(),
        "AwaitingReconciliation"
    ));
    assert_eq!(v.artifact_state().await, "Incomplete");
    assert_eq!(v.held_chunk_indices().await, vec![0]);
    assert_eq!(v.event_count("JobSucceeded").await, 0);

    // The pre-restart capability no longer resolves — fail closed with the
    // single non-enumerable 401.
    let client =
        DataPlaneClient::connect(&v.data_plane_base_url, v.identity.fingerprint).expect("connect");
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let stale_proof = old_auth
        .create_proof(TransferOperation::ResumeDiscovery, None, now)
        .unwrap();
    let out = client
        .discover_resume(&old_token, transfer_id, &stale_proof)
        .await
        .expect("transport ok");
    assert_eq!(out, ResumeOutcome::AuthorizationDenied);

    drop(v);
    db.teardown().await;
}

// =====================================================================
// §29 — lock-order concurrency regression: #40 commit_transfer_dispatch and
// C2 apply_transfer_terminal_evidence never deadlock for one Attempt
// =====================================================================

#[tokio::test]
async fn commit_transfer_dispatch_and_terminal_evidence_do_not_deadlock_for_one_attempt() {
    // The two operations lock shared tables in a different order
    // (`jobs -> job_steps -> attempts -> transfers` vs
    // `transfers -> artifacts -> attempts -> job_steps -> jobs`). The linear
    // transfer workflow makes them temporally disjoint for one Attempt:
    // `commit_transfer_dispatch` runs exactly once and fully commits before an
    // `ActionAck` can move the Attempt to `InProgress`, which is itself
    // required before a terminal `ActionResult` is valid. This test drives
    // them back to back under real PostgreSQL as fast as possible, many times,
    // and asserts neither ever raises a deadlock (SQLSTATE 40P01) and each
    // resolves safely.
    let db = TestDatabase::setup().await;
    let pool = db.pool.clone();
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        64,
    )]));

    for i in 0..24u32 {
        let fx = support::transfer_vertical::dispatched_fixture(
            &pool,
            &job_repo,
            &arbiter,
            &format!("c4-lockorder-{i}"),
            "disk-0",
        )
        .await;

        // Move the Attempt InProgress (ActionAck), then immediately consume a
        // terminal ARTIFACT_VERIFICATION_FAILED — the two heaviest evidence
        // transactions, one right after the dispatch commit.
        let reservations = Arc::new(AttemptReservationRegistry::new());
        let evidence = ActionEvidenceService::new(
            Arc::clone(&job_repo) as Arc<dyn JobRepository>,
            Arc::clone(&reservations),
            Arc::clone(&arbiter),
        );
        evidence
            .apply(
                fx.action_id,
                fx.endpoint_id,
                bamep_domain::ActionEvidence::AckAccepted,
            )
            .await
            .expect("ack advances to InProgress with no deadlock");

        // Drive the bound Artifact to Failed via the production seal path, then
        // consume the matching terminal ActionResult through C2.
        let transfers =
            TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
        let tid = fx.transfer.id;
        transfers
            .record_expected_chunk(tid, bamep_domain::ChunkIndex(0), 10, vec![0xAB; 32])
            .await
            .unwrap();
        transfers
            .accept_verified_chunk(tid, bamep_domain::ChunkIndex(0), vec![0xAB; 32])
            .await
            .unwrap();
        transfers
            .seal_manifest(tid, 1, vec![0xCD; 32])
            .await
            .unwrap();
        transfers.begin_artifact_verification(tid).await.unwrap();
        transfers
            .complete_artifact_verification(tid, false)
            .await
            .unwrap();

        let terminal = bamep_server::application::TransferTerminalEvidenceService::new(
            Arc::clone(&job_repo) as Arc<dyn JobRepository>,
            Arc::clone(&reservations),
            Arc::clone(&arbiter),
        );
        let detail = bamep_server::application::parse_transfer_result_detail(
            bamep_agent_protocol::ActionResultOutcome::Failed,
            serde_json::json!({
                "code": "ARTIFACT_VERIFICATION_FAILED",
                "artifact_id": fx.transfer.artifact_id.0.to_string(),
            })
            .as_object()
            .unwrap(),
        )
        .unwrap();
        let outcome = terminal
            .apply(fx.action_id, fx.endpoint_id, detail)
            .await
            .expect("terminal evidence commits with no deadlock");
        assert_eq!(
            outcome,
            bamep_server::application::TransferTerminalOutcome::Consumed
        );

        let job_state: String = sqlx::query_scalar("SELECT state::text FROM jobs WHERE id = $1")
            .bind(fx.job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(job_state, "Failed");
        let _ = (
            EndpointId::new(),
            ActionId(uuid::Uuid::nil()),
            JobId(uuid::Uuid::nil()),
        );
    }

    db.teardown().await;
}
