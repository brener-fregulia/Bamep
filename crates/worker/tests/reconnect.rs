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

use bamep_worker::ipc::{run_client_loop, AuthorityTracker};
use bamep_worker_protocol::{
    receive, send, HandshakeRejectedMessage, ServerHelloMessage, WorkerProtocolMessage,
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
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        worker_instance_id,
        tracker,
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
    let client_task = tokio::spawn(run_client_loop(
        socket.0.clone(),
        Duration::from_millis(15),
        Uuid::new_v4(),
        tracker,
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
