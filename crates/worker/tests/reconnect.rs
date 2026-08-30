//! Real Unix Domain Socket tests for the Worker control client's
//! connect / handshake / reconnect / fail-closed lifecycle
//! (`bamep_worker::ipc::worker_control`; Issue #37 slice, completed by Issue
//! #39 Phase E1). A hand-rolled minimal `bamepd`-side peer built directly on
//! `bamep-worker-protocol` — this crate must not depend on `bamep-server`
//! (see `crates/server/tests/worker_control_client_interop.rs` for the
//! cross-crate proof against the real `WorkerControlPlane`).
//!
//! Concurrency, correlation, generation-scoped tickets, timeout, saturation,
//! resume aggregation, and seal + verification live in `control_client.rs`.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use bamep_worker::ipc::{
    worker_control, AuthorizeChunkInput, ChunkAuthorization, ControlError, WorkerControlHandle,
};
use bamep_worker_protocol::{
    receive, send, AuthorizationDecisionMessage, HandshakeRejectedMessage, ProtocolVersion,
    ServerHelloMessage, WireDigestAlgorithm, WorkerHelloMessage, WorkerProtocolMessage,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_millis(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

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

/// Spawns a control client against `socket` and waits until it is ready.
fn spawn_client(
    socket: &TempSocketPath,
    worker_instance_id: Uuid,
) -> (WorkerControlHandle, JoinHandle<()>) {
    let (handle, driver) = worker_control(
        socket.0.clone(),
        RECONNECT_DELAY,
        REQUEST_TIMEOUT,
        worker_instance_id,
    );
    let task = tokio::spawn(driver.run(std::future::pending::<()>()));
    (handle, task)
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

/// Accepts one connection and completes the handshake as `bamepd` would;
/// returns the stream and the `worker_instance_id` the client reported.
async fn accept_and_handshake(listener: &UnixListener) -> (UnixStream, Uuid) {
    let (mut stream, _addr) = timeout(TEST_TIMEOUT, listener.accept())
        .await
        .expect("no timeout")
        .expect("accept");
    let hello = expect_hello(&mut stream).await;
    send(
        &mut stream,
        &WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(hello.envelope.message_id)),
    )
    .await
    .expect("send ServerHello");
    (stream, hello.body.worker_instance_id)
}

/// Accepts one connection and returns the received `WorkerHello` without
/// responding — the caller crafts the (possibly malformed) reply itself.
async fn accept_and_receive_hello(listener: &UnixListener) -> (UnixStream, WorkerHelloMessage) {
    let (mut stream, _addr) = timeout(TEST_TIMEOUT, listener.accept())
        .await
        .expect("no timeout")
        .expect("accept");
    let hello = expect_hello(&mut stream).await;
    (stream, hello)
}

async fn expect_hello(stream: &mut UnixStream) -> WorkerHelloMessage {
    match timeout(TEST_TIMEOUT, receive(stream))
        .await
        .expect("no timeout")
        .expect("receive WorkerHello")
    {
        WorkerProtocolMessage::WorkerHello(hello) => hello,
        other => panic!("expected WorkerHello, got {other:?}"),
    }
}

fn sample_authorize_input() -> AuthorizeChunkInput {
    AuthorizeChunkInput {
        token: "opaque-token".to_string(),
        transfer_id: Uuid::new_v4(),
        chunk_index: 0,
        proof_id: "proof-id-value".to_string(),
        issued_at: 1_700_000_000_000,
        signature: "signature-value".to_string(),
    }
}

// ---------------------------------------------------------------------
// reconnect + worker_instance_id stability + local generation
// ---------------------------------------------------------------------

#[tokio::test]
async fn reconnect_keeps_the_worker_instance_id_and_advances_the_local_generation() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let worker_instance_id = Uuid::new_v4();
    let (handle, task) = spawn_client(&socket, worker_instance_id);

    let (stream1, reported_1) = accept_and_handshake(&listener).await;
    assert_eq!(reported_1, worker_instance_id);
    wait_ready(&handle).await;
    let generation_1 = handle.current_generation().expect("connected");

    drop(stream1);
    wait_unavailable(&handle).await;
    assert!(handle.current_generation().is_none());

    let (stream2, reported_2) = accept_and_handshake(&listener).await;
    assert_eq!(
        reported_2, worker_instance_id,
        "the same Worker process keeps its worker_instance_id across reconnect"
    );
    wait_ready(&handle).await;
    let generation_2 = handle.current_generation().expect("reconnected");
    assert!(
        generation_2.get() > generation_1.get(),
        "reconnect starts a new local connection generation"
    );

    task.abort();
    drop(stream2);
}

// ---------------------------------------------------------------------
// handshake validation — never becomes available on a bad ServerHello
// ---------------------------------------------------------------------

async fn assert_never_ready_after<F>(craft_response: F)
where
    F: FnOnce(Uuid) -> WorkerProtocolMessage,
{
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task) = spawn_client(&socket, Uuid::new_v4());

    let (mut stream, hello) = accept_and_receive_hello(&listener).await;
    send(&mut stream, &craft_response(hello.envelope.message_id))
        .await
        .expect("send crafted response");

    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!handle.is_ready());
    assert!(handle.current_generation().is_none());
    task.abort();
}

#[tokio::test]
async fn a_rejected_handshake_never_becomes_available() {
    assert_never_ready_after(|id| {
        WorkerProtocolMessage::HandshakeRejected(HandshakeRejectedMessage::incompatible_version(id))
    })
    .await;
}

#[tokio::test]
async fn a_server_hello_with_wrong_envelope_protocol_version_never_becomes_available() {
    assert_never_ready_after(|id| {
        let mut response = ServerHelloMessage::new(id);
        response.envelope.protocol_version = ProtocolVersion::new("2");
        WorkerProtocolMessage::ServerHello(response)
    })
    .await;
}

#[tokio::test]
async fn a_server_hello_with_wrong_server_protocol_version_never_becomes_available() {
    assert_never_ready_after(|id| {
        let mut response = ServerHelloMessage::new(id);
        response.body.server_protocol_version = ProtocolVersion::new("2");
        WorkerProtocolMessage::ServerHello(response)
    })
    .await;
}

#[tokio::test]
async fn a_server_hello_with_a_non_v4_message_id_never_becomes_available() {
    assert_never_ready_after(|id| {
        let mut response = ServerHelloMessage::new(id);
        response.envelope.message_id = Uuid::nil();
        WorkerProtocolMessage::ServerHello(response)
    })
    .await;
}

#[tokio::test]
async fn an_uncorrelated_server_hello_never_becomes_available() {
    assert_never_ready_after(|_id| {
        WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(Uuid::new_v4()))
    })
    .await;
}

#[tokio::test]
async fn a_malformed_handshake_rejected_never_becomes_available() {
    assert_never_ready_after(|id| {
        let mut response = HandshakeRejectedMessage::incompatible_version(id);
        response.envelope.message_id = Uuid::nil();
        WorkerProtocolMessage::HandshakeRejected(response)
    })
    .await;
}

#[tokio::test]
async fn an_uncorrelated_handshake_rejected_never_becomes_available() {
    assert_never_ready_after(|_id| {
        WorkerProtocolMessage::HandshakeRejected(HandshakeRejectedMessage::incompatible_version(
            Uuid::new_v4(),
        ))
    })
    .await;
}

// ---------------------------------------------------------------------
// no request before handshake
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_request_before_the_handshake_completes_is_never_written() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task) = spawn_client(&socket, Uuid::new_v4());

    // Accept the connection but do NOT send ServerHello yet.
    let (mut stream, _hello) = accept_and_receive_hello(&listener).await;

    // The client is connected + handshaking, not ready. A business request
    // fails closed and is never written.
    assert!(matches!(
        handle.authorize_chunk(sample_authorize_input()).await,
        Err(ControlError::NotConnected)
    ));

    // Nothing beyond WorkerHello was written: a follow-up receive must not
    // yield an AuthorizationQuery.
    assert!(
        timeout(Duration::from_millis(120), receive(&mut stream))
            .await
            .is_err(),
        "no frame should follow WorkerHello before ServerHello"
    );

    task.abort();
}

// ---------------------------------------------------------------------
// authorize_chunk round trip + in-flight disconnect
// ---------------------------------------------------------------------

#[tokio::test]
async fn authorize_chunk_correlates_the_decision_via_in_reply_to() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task) = spawn_client(&socket, Uuid::new_v4());

    let (mut stream, _id) = accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(sample_authorize_input()).await }
    });

    let query = match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive")
    {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("expected AuthorizationQuery, got {other:?}"),
    };
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationDecision(AuthorizationDecisionMessage::approved(
            query.envelope.message_id,
            WireDigestAlgorithm::Sha256,
            4096,
            "acc-handle-1",
            None,
        )),
    )
    .await
    .expect("send decision");

    let result = timeout(TEST_TIMEOUT, call)
        .await
        .expect("no timeout")
        .expect("join")
        .expect("query ok");
    match result {
        ChunkAuthorization::Approved(approved) => {
            assert_eq!(approved.chunk_size, 4096);
            assert_eq!(approved.digest_algorithm, WireDigestAlgorithm::Sha256);
        }
        ChunkAuthorization::Denied => panic!("expected Approved"),
    }

    task.abort();
    drop(stream);
}

#[tokio::test]
async fn an_in_flight_request_whose_connection_drops_fails_closed() {
    let socket = TempSocketPath::fresh();
    let listener = UnixListener::bind(&socket.0).expect("bind");
    let (handle, task) = spawn_client(&socket, Uuid::new_v4());

    let (mut stream, _id) = accept_and_handshake(&listener).await;
    wait_ready(&handle).await;

    let call = tokio::spawn({
        let handle = handle.clone();
        async move { handle.authorize_chunk(sample_authorize_input()).await }
    });

    let _query = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive query");
    drop(stream); // disconnect without answering

    let result = timeout(TEST_TIMEOUT, call)
        .await
        .expect("no timeout")
        .expect("join");
    assert!(
        matches!(result, Err(ControlError::ConnectionLost)),
        "an in-flight request whose connection drops must fail closed, got {result:?}"
    );

    task.abort();
}
