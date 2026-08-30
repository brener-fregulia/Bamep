//! Real Unix Domain Socket tests for the Worker control client's concurrent
//! multiplexing, correlation, generation-scoped follow-up tickets, bounded
//! request timeout, pending saturation, resume-discovery pagination, and
//! seal + verification (`bamep_worker::ipc::worker_control`; Issue #39 Phase
//! E1). A hand-rolled `bamepd`-side peer built directly on
//! `bamep-worker-protocol` — no `bamep-server` dependency.
//!
//! Connect/handshake/reconnect regression lives in `reconnect.rs`; the
//! cross-crate proof against the real `WorkerControlPlane` lives in
//! `crates/server/tests/worker_control_client_interop.rs`.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use bamep_worker::ipc::{
    worker_control, ArtifactVerification, AuthorizeChunkInput, ChunkAuthorization, ControlDriver,
    ControlError, ManifestSeal, ResumeDiscovery, ResumeDiscoveryInput, WorkerControlHandle,
};
use bamep_worker_protocol::{
    receive, send, ArtifactVerificationAckMessage, AuthorizationDecisionMessage,
    ChunkAcceptanceDecisionMessage, HeldChunk, ManifestSealDecisionMessage,
    ResumeDiscoveryPageMessage, SealedManifestFacts, ServerHelloMessage, WireArtifactStatus,
    WireDigestAlgorithm, WorkerProtocolMessage,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_millis(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

// ---------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------

struct TempSocketPath(PathBuf);

impl TempSocketPath {
    fn fresh() -> Self {
        let dir =
            std::env::temp_dir().join(format!("bamep-worker-control-tests-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
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

/// A minimal `bamepd`-side peer over one accepted connection.
struct FakePeer {
    stream: UnixStream,
}

impl FakePeer {
    async fn accept_and_handshake(listener: &UnixListener) -> Self {
        let (mut stream, _addr) = timeout(TEST_TIMEOUT, listener.accept())
            .await
            .expect("no timeout")
            .expect("accept");
        let hello = match timeout(TEST_TIMEOUT, receive(&mut stream))
            .await
            .expect("no timeout")
            .expect("receive WorkerHello")
        {
            WorkerProtocolMessage::WorkerHello(hello) => hello,
            other => panic!("expected WorkerHello, got {other:?}"),
        };
        send(
            &mut stream,
            &WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(hello.envelope.message_id)),
        )
        .await
        .expect("send ServerHello");
        Self { stream }
    }

    async fn recv(&mut self) -> WorkerProtocolMessage {
        timeout(TEST_TIMEOUT, receive(&mut self.stream))
            .await
            .expect("no timeout")
            .expect("receive frame")
    }

    async fn try_recv(&mut self, within: Duration) -> Option<WorkerProtocolMessage> {
        match timeout(within, receive(&mut self.stream)).await {
            Ok(result) => Some(result.expect("receive frame")),
            Err(_) => None,
        }
    }

    async fn send(&mut self, message: WorkerProtocolMessage) {
        send(&mut self.stream, &message).await.expect("send frame");
    }
}

fn spawn(
    socket: &TempSocketPath,
) -> (
    WorkerControlHandle,
    JoinHandle<()>,
    tokio::sync::watch::Sender<bool>,
) {
    spawn_with(worker_control(
        socket.0.clone(),
        RECONNECT_DELAY,
        REQUEST_TIMEOUT,
        Uuid::new_v4(),
    ))
}

fn spawn_with(
    (handle, driver): (WorkerControlHandle, ControlDriver),
) -> (
    WorkerControlHandle,
    JoinHandle<()>,
    tokio::sync::watch::Sender<bool>,
) {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(driver.run(async move {
        let mut rx = shutdown_rx;
        let _ = rx.wait_for(|stop| *stop).await;
    }));
    (handle, task, shutdown_tx)
}

async fn wait_ready(handle: &WorkerControlHandle) {
    let mut authority = handle.authority();
    timeout(TEST_TIMEOUT, authority.wait_for(|s| s.is_available()))
        .await
        .expect("no timeout")
        .expect("watch open");
}

async fn wait_unavailable(handle: &WorkerControlHandle) {
    let mut authority = handle.authority();
    timeout(TEST_TIMEOUT, authority.wait_for(|s| !s.is_available()))
        .await
        .expect("no timeout")
        .expect("watch open");
}

fn authorize_input(chunk_index: u64) -> AuthorizeChunkInput {
    AuthorizeChunkInput {
        token: "opaque-token".to_string(),
        transfer_id: Uuid::new_v4(),
        chunk_index,
        proof_id: format!("proof-{chunk_index}"),
        issued_at: 1_700_000_000_000,
        signature: "signature-value".to_string(),
    }
}

fn resume_input() -> ResumeDiscoveryInput {
    ResumeDiscoveryInput {
        token: "opaque-token".to_string(),
        transfer_id: Uuid::new_v4(),
        proof_id: "resume-proof".to_string(),
        issued_at: 1_700_000_000_000,
        signature: "signature-value".to_string(),
    }
}

// ---------------------------------------------------------------------
// §41 — concurrent, out-of-order responses each reach their own caller
// ---------------------------------------------------------------------

#[tokio::test]
async fn concurrent_responses_returned_in_reverse_order_each_reach_their_own_caller() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    const N: u64 = 20;
    let calls: Vec<JoinHandle<Result<ChunkAuthorization, ControlError>>> = (0..N)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move { handle.authorize_chunk(authorize_input(i)).await })
        })
        .collect();

    // Collect all N queries, then answer them in reverse arrival order with a
    // per-request-distinguishable chunk_size.
    let mut queries = Vec::new();
    for _ in 0..N {
        match peer.recv().await {
            WorkerProtocolMessage::AuthorizationQuery(q) => queries.push(q),
            other => panic!("expected AuthorizationQuery, got {other:?}"),
        }
    }
    for query in queries.into_iter().rev() {
        let chunk_index = query.body.chunk_index;
        peer.send(WorkerProtocolMessage::AuthorizationDecision(
            AuthorizationDecisionMessage::approved(
                query.envelope.message_id,
                WireDigestAlgorithm::Sha256,
                chunk_index as u32 + 1000,
                format!("acc-{chunk_index}"),
                None,
            ),
        ))
        .await;
    }

    for (i, call) in calls.into_iter().enumerate() {
        let result = timeout(TEST_TIMEOUT, call)
            .await
            .expect("no timeout")
            .expect("join")
            .expect("query ok");
        match result {
            ChunkAuthorization::Approved(approved) => {
                assert_eq!(
                    approved.chunk_size,
                    i as u32 + 1000,
                    "caller {i} received another caller's response"
                );
            }
            ChunkAuthorization::Denied => panic!("expected Approved for caller {i}"),
        }
    }

    task.abort();
    drop(peer);
}

// ---------------------------------------------------------------------
// §42 — a duplicate response and an unknown in_reply_to are discarded
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_duplicate_and_an_unknown_response_are_discarded_and_the_connection_survives() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    // Complete one request.
    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(0)).await }
    });
    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    let decision = AuthorizationDecisionMessage::approved(
        query.envelope.message_id,
        WireDigestAlgorithm::Sha256,
        4096,
        "acc-0",
        None,
    );
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        decision.clone(),
    ))
    .await;
    let first = timeout(TEST_TIMEOUT, call)
        .await
        .expect("no timeout")
        .expect("join")
        .expect("ok");
    assert!(matches!(first, ChunkAuthorization::Approved(_)));

    // Re-send the same response (now stale) and a random unknown in_reply_to.
    peer.send(WorkerProtocolMessage::AuthorizationDecision(decision))
        .await;
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::denied(Uuid::new_v4()),
    ))
    .await;

    // The connection must still work: a fresh request round-trips.
    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(1)).await }
    });
    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::denied(query.envelope.message_id),
    ))
    .await;
    let second = timeout(TEST_TIMEOUT, call)
        .await
        .expect("no timeout")
        .expect("join")
        .expect("ok");
    assert!(matches!(second, ChunkAuthorization::Denied));
    assert!(handle.is_ready(), "generation was never recycled");

    task.abort();
}

// ---------------------------------------------------------------------
// §43 — a matching in_reply_to carrying the wrong response type fails closed
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_wrong_response_type_for_a_live_request_fails_closed_and_recycles_the_generation() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(0)).await }
    });
    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    // Correct in_reply_to, wrong message type.
    peer.send(WorkerProtocolMessage::ChunkAcceptanceDecision(
        ChunkAcceptanceDecisionMessage::committed(query.envelope.message_id),
    ))
    .await;

    let result = timeout(TEST_TIMEOUT, call)
        .await
        .expect("no timeout")
        .expect("join");
    assert!(
        matches!(result, Err(ControlError::CorrelationViolation)),
        "a wrong-typed response must fail closed, got {result:?}"
    );
    // The generation was recycled; the client drops to unavailable.
    wait_unavailable(&handle).await;

    task.abort();
    drop(peer);
}

// ---------------------------------------------------------------------
// §44 — disconnect fails every pending request
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_disconnect_fails_every_pending_request_and_the_client_reconnects() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let calls: Vec<_> = (0..5)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move { handle.authorize_chunk(authorize_input(i)).await })
        })
        .collect();
    for _ in 0..5 {
        assert!(matches!(
            peer.recv().await,
            WorkerProtocolMessage::AuthorizationQuery(_)
        ));
    }

    drop(peer); // close before responding to any

    for call in calls {
        let result = timeout(TEST_TIMEOUT, call)
            .await
            .expect("no timeout")
            .expect("join");
        assert!(
            matches!(result, Err(ControlError::ConnectionLost)),
            "every pending request must fail on disconnect, got {result:?}"
        );
    }

    // A fresh connection generation is established on its own.
    let _peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    task.abort();
}

// ---------------------------------------------------------------------
// §45 — reconnect does not replay any business request
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_reconnect_replays_no_business_request() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(0)).await }
    });
    assert!(matches!(
        peer.recv().await,
        WorkerProtocolMessage::AuthorizationQuery(_)
    ));
    drop(peer);
    assert!(matches!(
        timeout(TEST_TIMEOUT, call).await.unwrap().unwrap(),
        Err(ControlError::ConnectionLost)
    ));

    // New connection: the peer must see the handshake and then NOTHING until
    // a new explicit caller request appears.
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;
    assert!(
        peer.try_recv(Duration::from_millis(150)).await.is_none(),
        "no business request may be auto-replayed across a reconnect"
    );

    // An explicit new request does appear.
    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(9)).await }
    });
    match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => assert_eq!(q.body.chunk_index, 9),
        other => panic!("got {other:?}"),
    }
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::denied(Uuid::new_v4()), // unknown in_reply_to
    ))
    .await;
    // (that answer is stale/unknown; drop the call — the point is proven)
    call.abort();
    task.abort();
}

// ---------------------------------------------------------------------
// §46 — a stale follow-up ticket after reconnect sends nothing
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_stale_acceptance_ticket_after_reconnect_is_rejected_locally_and_sends_nothing() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    // Generation A: authorize a chunk and obtain the acceptance ticket.
    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(3)).await }
    });
    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::approved(
            query.envelope.message_id,
            WireDigestAlgorithm::Sha256,
            4096,
            "acc-A",
            None,
        ),
    ))
    .await;
    let ChunkAuthorization::Approved(approved) =
        timeout(TEST_TIMEOUT, call).await.unwrap().unwrap().unwrap()
    else {
        panic!("expected Approved");
    };

    // Disconnect, reconnect -> generation B.
    drop(peer);
    wait_unavailable(&handle).await;
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    // The generation-A ticket is rejected locally; nothing is written.
    let result = handle
        .commit_chunk(approved.acceptance_ticket, "digest-xyz".to_string(), 4096)
        .await;
    assert!(
        matches!(result, Err(ControlError::GenerationChanged)),
        "a stale acceptance ticket must be rejected locally, got {result:?}"
    );

    // Prove nothing was sent: the peer sees a fresh AuthorizationQuery next,
    // never a ChunkAcceptanceRequest.
    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(4)).await }
    });
    match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => assert_eq!(q.body.chunk_index, 4),
        WorkerProtocolMessage::ChunkAcceptanceRequest(_) => {
            panic!("a stale acceptance ticket must not send a ChunkAcceptanceRequest")
        }
        other => panic!("got {other:?}"),
    }
    call.abort();
    task.abort();
}

#[tokio::test]
async fn a_stale_verification_ticket_after_reconnect_is_rejected_locally_and_sends_nothing() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .seal_manifest(bamep_worker::ipc::ManifestSealInput {
                    token: "t".to_string(),
                    transfer_id: Uuid::new_v4(),
                    proof_id: "p".to_string(),
                    issued_at: 1,
                    signature: "s".to_string(),
                    chunk_count: 2,
                    artifact_digest: "declared-digest".to_string(),
                })
                .await
        }
    });
    let seal_req = match peer.recv().await {
        WorkerProtocolMessage::ManifestSealRequest(r) => r,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ManifestSealDecision(
        ManifestSealDecisionMessage::sealed(
            seal_req.envelope.message_id,
            SealedManifestFacts {
                verification_handle: "ver-A".to_string(),
                artifact_id: Uuid::new_v4(),
                digest_algorithm: WireDigestAlgorithm::Sha256,
                chunk_size: 4096,
                chunk_count: 2,
                expected_artifact_digest: "durable-digest".to_string(),
            },
        ),
    ))
    .await;
    let ManifestSeal::Sealed(success) =
        timeout(TEST_TIMEOUT, call).await.unwrap().unwrap().unwrap()
    else {
        panic!("expected Sealed");
    };

    drop(peer);
    wait_unavailable(&handle).await;
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let result = handle
        .report_artifact_verification(success.verification_ticket, "computed-digest".to_string())
        .await;
    assert!(matches!(result, Err(ControlError::GenerationChanged)));

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(1)).await }
    });
    match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(_) => {}
        WorkerProtocolMessage::ArtifactVerificationReport(_) => {
            panic!("a stale verification ticket must not send an ArtifactVerificationReport")
        }
        other => panic!("got {other:?}"),
    }
    call.abort();
    task.abort();
}

// ---------------------------------------------------------------------
// §47 — timeout, and the late answer is discarded
// ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_control_request_times_out_and_a_later_answer_for_it_is_discarded() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, driver) = worker_control(
        socket.0.clone(),
        RECONNECT_DELAY,
        Duration::from_millis(500),
        Uuid::new_v4(),
    );
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(driver.run(async move {
        let mut rx = shutdown_rx;
        let _ = rx.wait_for(|s| *s).await;
    }));
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(0)).await }
    });
    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    // Do not answer. Paused-time auto-advance drives the dispatcher's
    // deadline sweep.
    let result = timeout(TEST_TIMEOUT, call)
        .await
        .expect("no timeout")
        .expect("join");
    assert!(
        matches!(result, Err(ControlError::Timeout { .. })),
        "an unanswered request must time out, got {result:?}"
    );

    // The late answer is discarded (unknown in_reply_to now); the connection
    // still serves a fresh request.
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::denied(query.envelope.message_id),
    ))
    .await;
    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(1)).await }
    });
    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::denied(query.envelope.message_id),
    ))
    .await;
    assert!(matches!(
        timeout(TEST_TIMEOUT, call).await.unwrap().unwrap().unwrap(),
        ChunkAuthorization::Denied
    ));

    task.abort();
}

// ---------------------------------------------------------------------
// §48 — pending saturation fails new requests without evicting live ones
// ---------------------------------------------------------------------

#[tokio::test]
async fn pending_saturation_fails_new_requests_without_evicting_live_ones() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, driver) = worker_control(
        socket.0.clone(),
        RECONNECT_DELAY,
        REQUEST_TIMEOUT,
        Uuid::new_v4(),
    );
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(driver.with_pending_capacity(2).run(async move {
        let mut rx = shutdown_rx;
        let _ = rx.wait_for(|s| *s).await;
    }));
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    // Two live requests the peer receives but never answers.
    let live: Vec<_> = (0..2)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move { handle.authorize_chunk(authorize_input(i)).await })
        })
        .collect();
    let mut first_id = None;
    for _ in 0..2 {
        match peer.recv().await {
            WorkerProtocolMessage::AuthorizationQuery(q) => {
                first_id.get_or_insert(q.envelope.message_id);
            }
            other => panic!("got {other:?}"),
        }
    }

    // A third request fails closed — no live waiter evicted.
    assert!(matches!(
        handle.authorize_chunk(authorize_input(2)).await,
        Err(ControlError::Saturated)
    ));

    // Answer one live request; capacity frees up.
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::denied(first_id.unwrap()),
    ))
    .await;
    let mut done = 0;
    for call in live {
        if let Ok(joined) = timeout(TEST_TIMEOUT, call).await {
            if joined.unwrap().is_ok() {
                done += 1;
            }
        }
        if done == 1 {
            break;
        }
    }
    assert_eq!(done, 1, "the answered live request completed");

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(authorize_input(3)).await }
    });
    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::denied(query.envelope.message_id),
    ))
    .await;
    assert!(timeout(TEST_TIMEOUT, call).await.unwrap().unwrap().is_ok());

    task.abort();
}

// ---------------------------------------------------------------------
// §49 — resume discovery aggregates every page in order
// ---------------------------------------------------------------------

fn held(index: u64) -> HeldChunk {
    HeldChunk {
        chunk_index: index,
        digest: format!("digest-{index:03}"),
    }
}

#[tokio::test]
async fn discover_resume_aggregates_every_page_in_order_with_no_cursor_escaping() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.discover_resume(resume_input()).await }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::first_page(
            query.envelope.message_id,
            query.body.transfer_id,
            true,
            WireDigestAlgorithm::Sha256,
            4096,
            Some(5),
            vec![held(0), held(1)],
            Some("cursor-A".to_string()),
        ),
    ))
    .await;

    let cont_a = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryContinue(c) => c,
        other => panic!("got {other:?}"),
    };
    assert_eq!(cont_a.body.resume_cursor, "cursor-A");
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::continuation_page(
            cont_a.envelope.message_id,
            vec![held(2), held(3)],
            Some("cursor-B".to_string()),
        ),
    ))
    .await;

    let cont_b = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryContinue(c) => c,
        other => panic!("got {other:?}"),
    };
    assert_eq!(cont_b.body.resume_cursor, "cursor-B");
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::continuation_page(
            cont_b.envelope.message_id,
            vec![held(4)],
            None,
        ),
    ))
    .await;

    let result = timeout(TEST_TIMEOUT, call).await.unwrap().unwrap().unwrap();
    match result {
        ResumeDiscovery::Approved(aggregate) => {
            assert!(aggregate.sealed);
            assert_eq!(aggregate.expected_chunk_count, Some(5));
            assert_eq!(aggregate.chunk_size, 4096);
            let indices: Vec<u64> = aggregate
                .held_chunks
                .iter()
                .map(|c| c.chunk_index)
                .collect();
            assert_eq!(indices, vec![0, 1, 2, 3, 4]);
            // strictly ascending, no repeats
            let set: BTreeSet<u64> = indices.iter().copied().collect();
            assert_eq!(set.len(), indices.len());
        }
        ResumeDiscovery::Denied => panic!("expected Approved"),
    }

    task.abort();
    drop(peer);
}

// ---------------------------------------------------------------------
// §50 — a denied continuation discards the partial aggregate
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_denied_resume_continuation_discards_the_partial_aggregate() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.discover_resume(resume_input()).await }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::first_page(
            query.envelope.message_id,
            query.body.transfer_id,
            false,
            WireDigestAlgorithm::Sha256,
            4096,
            None,
            vec![held(0), held(1)],
            Some("cursor-A".to_string()),
        ),
    ))
    .await;

    let cont = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryContinue(c) => c,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::denied(cont.envelope.message_id),
    ))
    .await;

    let result = timeout(TEST_TIMEOUT, call).await.unwrap().unwrap();
    assert!(
        matches!(result, Err(ControlError::ResumePageUnavailable)),
        "a denied continuation must fail closed, not expose the partial aggregate; got {result:?}"
    );

    task.abort();
    drop(peer);
}

// ---------------------------------------------------------------------
// §51 — resume generation loss discards the partial, replays no cursor
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_disconnect_during_resume_pagination_discards_the_partial_and_replays_no_cursor() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.discover_resume(resume_input()).await }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::first_page(
            query.envelope.message_id,
            query.body.transfer_id,
            false,
            WireDigestAlgorithm::Sha256,
            4096,
            None,
            vec![held(0), held(1)],
            Some("cursor-A".to_string()),
        ),
    ))
    .await;
    // Drop before the continuation can be answered.
    drop(peer);

    let result = timeout(TEST_TIMEOUT, call).await.unwrap().unwrap();
    assert!(
        matches!(
            result,
            Err(ControlError::ConnectionLost)
                | Err(ControlError::NotConnected)
                | Err(ControlError::GenerationChanged)
        ),
        "resume pagination loss must fail closed, got {result:?}"
    );

    // The reconnected peer sees a handshake and then nothing — no replayed
    // ResumeDiscoveryContinue.
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;
    assert!(
        peer.try_recv(Duration::from_millis(150)).await.is_none(),
        "no ResumeDiscoveryContinue may be replayed across a reconnect"
    );

    task.abort();
}

// ---------------------------------------------------------------------
// §52 — seal + verification ticket; the verdict comes only from bamepd
// ---------------------------------------------------------------------

#[tokio::test]
async fn seal_then_report_verification_uses_the_generation_ticket_and_returns_bamepds_verdict() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, _shutdown) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let artifact_id = Uuid::new_v4();
    let call = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .seal_manifest(bamep_worker::ipc::ManifestSealInput {
                    token: "t".to_string(),
                    transfer_id: Uuid::new_v4(),
                    proof_id: "p".to_string(),
                    issued_at: 1,
                    signature: "s".to_string(),
                    chunk_count: 3,
                    artifact_digest: "declared".to_string(),
                })
                .await
        }
    });
    let seal_req = match peer.recv().await {
        WorkerProtocolMessage::ManifestSealRequest(r) => r,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ManifestSealDecision(
        ManifestSealDecisionMessage::sealed(
            seal_req.envelope.message_id,
            SealedManifestFacts {
                verification_handle: "vh-1".to_string(),
                artifact_id,
                digest_algorithm: WireDigestAlgorithm::Sha256,
                chunk_size: 4096,
                chunk_count: 3,
                expected_artifact_digest: "durable-expected".to_string(),
            },
        ),
    ))
    .await;

    let ManifestSeal::Sealed(success) =
        timeout(TEST_TIMEOUT, call).await.unwrap().unwrap().unwrap()
    else {
        panic!("expected Sealed");
    };
    assert_eq!(success.artifact_id, artifact_id);
    assert_eq!(success.chunk_count, 3);
    assert_eq!(success.expected_artifact_digest, "durable-expected");

    // Report on the SAME generation.
    let call = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .report_artifact_verification(
                    success.verification_ticket,
                    "worker-computed-digest".to_string(),
                )
                .await
        }
    });
    let report = match peer.recv().await {
        WorkerProtocolMessage::ArtifactVerificationReport(r) => r,
        other => panic!("got {other:?}"),
    };
    assert_eq!(report.body.verification_handle, "vh-1");
    assert_eq!(
        report.body.computed_artifact_digest,
        "worker-computed-digest"
    );

    // bamepd says Failed; the Worker must return exactly that, not derive it.
    peer.send(WorkerProtocolMessage::ArtifactVerificationAck(
        ArtifactVerificationAckMessage::committed(
            report.envelope.message_id,
            WireArtifactStatus::Failed,
        ),
    ))
    .await;
    let verdict = timeout(TEST_TIMEOUT, call).await.unwrap().unwrap().unwrap();
    assert_eq!(verdict, ArtifactVerification::Failed);

    task.abort();
    drop(peer);
}

// ---------------------------------------------------------------------
// §54 — shutdown fails pending callers and stops reconnecting
// ---------------------------------------------------------------------

#[tokio::test]
async fn shutdown_fails_pending_callers_and_stops_reconnecting() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task, shutdown_tx) = spawn(&socket);
    let mut peer = FakePeer::accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let calls: Vec<_> = (0..3)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move { handle.authorize_chunk(authorize_input(i)).await })
        })
        .collect();
    for _ in 0..3 {
        assert!(matches!(
            peer.recv().await,
            WorkerProtocolMessage::AuthorizationQuery(_)
        ));
    }

    shutdown_tx.send(true).expect("signal shutdown");

    for call in calls {
        let result = timeout(TEST_TIMEOUT, call)
            .await
            .expect("no timeout")
            .expect("join");
        assert!(
            result.is_err(),
            "a pending caller must fail on shutdown, got {result:?}"
        );
    }

    // The driver task completes; no reconnect is attempted.
    timeout(TEST_TIMEOUT, task)
        .await
        .expect("driver stops promptly")
        .expect("driver join");
    drop(peer);
    assert!(
        timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "no reconnect after shutdown"
    );
    assert!(!handle.is_ready());
}
