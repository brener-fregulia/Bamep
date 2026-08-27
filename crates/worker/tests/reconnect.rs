//! Real Unix Domain Socket tests for `bamep_worker::ipc::client::run_client_loop`
//! (Issue #37 "Worker reconnect"): the actual production reconnect loop
//! against a real kernel UDS, using a hand-rolled minimal `bamepd`-side
//! handshake responder built directly on `bamep-worker-protocol` (this
//! crate must not depend on `bamep-server`, so the fake server side cannot
//! reuse `bamep_server::adapters::worker_control_plane` — see
//! `crates/server/tests/worker_control_plane.rs` for the server-side
//! equivalent tests against the real `WorkerControlPlane`).
//!
//! Unix Domain Sockets are Unix-only; this whole file is a no-op elsewhere.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use bamep_worker::ipc::{authorization_channel, run_client_loop, AuthorityTracker, QueryError};
use bamep_worker_protocol::{
    receive, send, AuthorizationDecisionMessage, AuthorizationOperation, AuthorizationQueryMessage,
    HandshakeRejectedMessage, ProtocolVersion, ServerHelloMessage, WireTransferDirection,
    WorkerProtocolMessage,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;
use uuid::Uuid;

struct TempSocketPath(PathBuf);

impl TempSocketPath {
    fn fresh() -> Self {
        let dir =
            std::env::temp_dir().join(format!("bamep-worker-reconnect-tests-{}", Uuid::new_v4()));
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

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Accepts exactly one connection, completes the handshake as `bamepd`
/// would, and returns the accepted stream and the `worker_instance_id` the
/// real Worker client reported.
async fn fake_bamepd_accept_and_handshake(listener: &UnixListener) -> (UnixStream, Uuid) {
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
    (stream, hello.body.worker_instance_id)
}

#[tokio::test]
async fn reconnect_after_disconnect_uses_the_same_worker_instance_id_with_a_new_generation() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");

    let worker_instance_id = Uuid::new_v4();
    let (tracker, mut authority_rx) = AuthorityTracker::new();
    let (publisher, _authorization_client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        worker_instance_id,
        tracker,
        publisher,
    ));

    let (stream1, reported_id_1) = fake_bamepd_accept_and_handshake(&listener).await;
    assert_eq!(reported_id_1, worker_instance_id);
    timeout(TEST_TIMEOUT, authority_rx.wait_for(|s| s.is_available()))
        .await
        .expect("no timeout")
        .expect("watch channel open");
    let generation_1 = authority_rx.borrow().generation;

    // Disconnect: authority must become unavailable immediately, and the
    // client must reconnect on its own without any external restart.
    drop(stream1);
    timeout(TEST_TIMEOUT, authority_rx.wait_for(|s| !s.is_available()))
        .await
        .expect("no timeout")
        .expect("watch channel open");

    let (stream2, reported_id_2) = fake_bamepd_accept_and_handshake(&listener).await;
    assert_eq!(
        reported_id_2, worker_instance_id,
        "same Worker process must keep the same worker_instance_id across reconnect"
    );
    timeout(TEST_TIMEOUT, authority_rx.wait_for(|s| s.is_available()))
        .await
        .expect("no timeout")
        .expect("watch channel open");
    let generation_2 = authority_rx.borrow().generation;
    assert!(
        generation_2 > generation_1,
        "reconnect must start a new connection generation"
    );

    client_task.abort();
    drop(stream2);
}

#[tokio::test]
async fn a_rejected_handshake_never_becomes_available_and_the_client_keeps_retrying() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");

    let (tracker, authority_rx) = AuthorityTracker::new();
    let (publisher, _authorization_client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    for _ in 0..2 {
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
            &WorkerProtocolMessage::HandshakeRejected(
                HandshakeRejectedMessage::incompatible_version(hello.envelope.message_id),
            ),
        )
        .await
        .expect("send HandshakeRejected");

        assert!(!authority_rx.borrow().is_available());
    }

    client_task.abort();
}

/// Accepts exactly one connection and returns the received `WorkerHello`,
/// without sending any response — the caller crafts the (possibly
/// intentionally malformed) response itself.
async fn accept_and_receive_hello(
    listener: &UnixListener,
) -> (UnixStream, bamep_worker_protocol::WorkerHelloMessage) {
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
    (stream, hello)
}

/// Worker validation (correction audit "Strict handshake validation"): a
/// `ServerHello` whose envelope `protocol_version` is not `"1"` must never
/// let Worker reach `Ready`, even though it otherwise correlates.
#[tokio::test]
async fn a_server_hello_with_wrong_envelope_protocol_version_never_becomes_available() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");
    let (tracker, authority_rx) = AuthorityTracker::new();
    let (publisher, _authorization_client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    let (mut stream, hello) = accept_and_receive_hello(&listener).await;
    let mut response = ServerHelloMessage::new(hello.envelope.message_id);
    response.envelope.protocol_version = ProtocolVersion::new("2");
    send(&mut stream, &WorkerProtocolMessage::ServerHello(response))
        .await
        .expect("send tampered ServerHello");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!authority_rx.borrow().is_available());
    client_task.abort();
}

/// Worker validation: a `ServerHello` with a well-formed envelope but the
/// wrong `server_protocol_version` must never let Worker reach `Ready`.
#[tokio::test]
async fn a_server_hello_with_wrong_server_protocol_version_never_becomes_available() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");
    let (tracker, authority_rx) = AuthorityTracker::new();
    let (publisher, _authorization_client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    let (mut stream, hello) = accept_and_receive_hello(&listener).await;
    let mut response = ServerHelloMessage::new(hello.envelope.message_id);
    response.body.server_protocol_version = ProtocolVersion::new("2");
    send(&mut stream, &WorkerProtocolMessage::ServerHello(response))
        .await
        .expect("send tampered ServerHello");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!authority_rx.borrow().is_available());
    client_task.abort();
}

/// Worker validation: a `ServerHello` envelope `message_id` that is not a
/// UUID v4 must never let Worker reach `Ready`.
#[tokio::test]
async fn a_server_hello_with_non_v4_message_id_never_becomes_available() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");
    let (tracker, authority_rx) = AuthorityTracker::new();
    let (publisher, _authorization_client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    let (mut stream, hello) = accept_and_receive_hello(&listener).await;
    let mut response = ServerHelloMessage::new(hello.envelope.message_id);
    response.envelope.message_id = Uuid::nil();
    send(&mut stream, &WorkerProtocolMessage::ServerHello(response))
        .await
        .expect("send tampered ServerHello");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!authority_rx.borrow().is_available());
    client_task.abort();
}

/// Worker validation: a well-formed `ServerHello` that correlates to a
/// *different* `WorkerHello` than the one Worker actually sent must never
/// let Worker reach `Ready`.
#[tokio::test]
async fn an_uncorrelated_server_hello_never_becomes_available() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");
    let (tracker, authority_rx) = AuthorityTracker::new();
    let (publisher, _authorization_client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    let (mut stream, _hello) = accept_and_receive_hello(&listener).await;
    let response = ServerHelloMessage::new(Uuid::new_v4());
    send(&mut stream, &WorkerProtocolMessage::ServerHello(response))
        .await
        .expect("send uncorrelated ServerHello");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!authority_rx.borrow().is_available());
    client_task.abort();
}

/// Worker validation: a `HandshakeRejected` with malformed envelope fields
/// must never be treated as a valid rejection that simply keeps retrying —
/// it must not crash or panic, and authority must remain unavailable.
#[tokio::test]
async fn a_malformed_handshake_rejected_never_becomes_available() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");
    let (tracker, authority_rx) = AuthorityTracker::new();
    let (publisher, _authorization_client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    let (mut stream, hello) = accept_and_receive_hello(&listener).await;
    let mut response = HandshakeRejectedMessage::incompatible_version(hello.envelope.message_id);
    response.envelope.message_id = Uuid::nil();
    send(
        &mut stream,
        &WorkerProtocolMessage::HandshakeRejected(response),
    )
    .await
    .expect("send malformed HandshakeRejected");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!authority_rx.borrow().is_available());
    client_task.abort();
}

fn sample_authorization_query() -> AuthorizationQueryMessage {
    AuthorizationQueryMessage::new(
        "opaque-token",
        AuthorizationOperation::ResumeDiscovery,
        Uuid::new_v4(),
        Uuid::new_v4(),
        WireTransferDirection::AgentToServer,
        None,
        "proof-id-value",
        1_700_000_000_000,
        "signature-value",
    )
}

/// Issue #38 "Worker UDS" runtime evidence: a real Worker process (the
/// actual `run_client_loop`) sends a real `AuthorizationQuery` over a real
/// kernel UDS, and `AuthorizationClient::query` correctly correlates the
/// fake `bamepd`'s `AuthorizationDecision` reply via `in_reply_to` back to
/// the exact caller awaiting it.
#[tokio::test]
async fn a_query_receives_the_fake_bamepds_decision_and_correlates_via_in_reply_to() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");
    let (tracker, mut authority_rx) = AuthorityTracker::new();
    let (publisher, client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    let (mut stream, _worker_instance_id) = fake_bamepd_accept_and_handshake(&listener).await;
    timeout(TEST_TIMEOUT, authority_rx.wait_for(|s| s.is_available()))
        .await
        .expect("no timeout")
        .expect("watch channel open");

    let query_client = client.clone();
    let query_handle =
        tokio::spawn(async move { query_client.query(sample_authorization_query()).await });

    let received = match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationQuery")
    {
        WorkerProtocolMessage::AuthorizationQuery(query) => query,
        other => panic!("expected AuthorizationQuery, got {other:?}"),
    };
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationDecision(AuthorizationDecisionMessage::denied(
            received.envelope.message_id,
        )),
    )
    .await
    .expect("send AuthorizationDecision");

    let decision = timeout(TEST_TIMEOUT, query_handle)
        .await
        .expect("no timeout")
        .expect("task join")
        .expect("query succeeds");
    assert_eq!(decision.body.in_reply_to, received.envelope.message_id);

    client_task.abort();
}

/// Issue #38 acceptance criterion: "in-flight query + disconnect => failed/
/// uncertain, never approved". The fake `bamepd` drops the connection after
/// receiving the query but before answering it; the caller must observe an
/// error, never a fabricated `Approved`.
#[tokio::test]
async fn a_query_in_flight_when_the_connection_drops_fails_closed() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");
    let (tracker, mut authority_rx) = AuthorityTracker::new();
    let (publisher, client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    let (mut stream, _worker_instance_id) = fake_bamepd_accept_and_handshake(&listener).await;
    timeout(TEST_TIMEOUT, authority_rx.wait_for(|s| s.is_available()))
        .await
        .expect("no timeout")
        .expect("watch channel open");

    let query_client = client.clone();
    let query_handle =
        tokio::spawn(async move { query_client.query(sample_authorization_query()).await });

    let _received = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationQuery");
    // Disconnect instead of answering.
    drop(stream);

    let result = timeout(TEST_TIMEOUT, query_handle)
        .await
        .expect("no timeout")
        .expect("task join");
    assert!(
        matches!(result, Err(QueryError::Disconnected)),
        "an in-flight query whose connection drops must fail closed, never fabricate approval; got {result:?}"
    );

    client_task.abort();
}

/// Worker validation: an uncorrelated `HandshakeRejected` (correct shape,
/// wrong `in_reply_to`) must never be treated as this Worker's own rejected
/// handshake.
#[tokio::test]
async fn an_uncorrelated_handshake_rejected_never_becomes_available() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind fake bamepd listener");
    let (tracker, authority_rx) = AuthorityTracker::new();
    let (publisher, _authorization_client) = authorization_channel();
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
        publisher,
    ));

    let (mut stream, _hello) = accept_and_receive_hello(&listener).await;
    let response = HandshakeRejectedMessage::incompatible_version(Uuid::new_v4());
    send(
        &mut stream,
        &WorkerProtocolMessage::HandshakeRejected(response),
    )
    .await
    .expect("send uncorrelated HandshakeRejected");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!authority_rx.borrow().is_available());
    client_task.abort();
}
