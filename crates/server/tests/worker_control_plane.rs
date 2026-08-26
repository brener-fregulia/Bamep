//! Real Unix Domain Socket tests for the `bamepd`-side Worker control plane
//! (Issue #37; `docs/specifications/m1-worker-data-plane-control-contract.md`).
//! Exercises `bamep_server::adapters::worker_control_plane::WorkerControlPlane`
//! and `bamep_server::runtime::worker_authority::WorkerAuthorityRegistry`
//! against a real kernel UDS, using `bamep-worker-protocol` directly as a
//! test-double Worker client — the real framing/message shapes, not an
//! in-process shortcut (`docs/development/testing.md` "Simulator": "exercise
//! the real external contracts ... rather than bypassing them through
//! in-process access").
//!
//! Unix Domain Sockets are Unix-only; this whole file is a no-op on other
//! platforms (`docs/development/testing.md` "Development environments":
//! "Linux is the reference environment for Bamep Server, Agent, Worker").

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bamep_server::adapters::worker_control_plane::{WorkerControlPlane, WorkerControlPlaneError};
use bamep_server::runtime::worker_authority::{WorkerAuthorityRegistry, WorkerControlState};
use bamep_worker_protocol::{
    receive, send, ProtocolErrorMessage, WorkerHelloMessage, WorkerProtocolMessage,
};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;

struct TempSocketPath(PathBuf);

impl TempSocketPath {
    fn fresh() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bamep-worker-control-plane-tests-{}",
            Uuid::new_v4()
        ));
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

async fn connect_and_send_hello(path: &Path) -> (UnixStream, Uuid, Uuid) {
    let mut stream = UnixStream::connect(path).await.expect("connect");
    let hello = WorkerHelloMessage::new(Uuid::new_v4());
    let sent_message_id = hello.envelope.message_id;
    let worker_instance_id = hello.body.worker_instance_id;
    send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello))
        .await
        .expect("send WorkerHello");
    (stream, sent_message_id, worker_instance_id)
}

async fn handshake(path: &Path) -> (UnixStream, Uuid) {
    let (mut stream, sent_message_id, worker_instance_id) = connect_and_send_hello(path).await;
    match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive response")
    {
        WorkerProtocolMessage::ServerHello(response) => {
            assert_eq!(response.body.in_reply_to, sent_message_id);
            assert!(response.body.compatible);
        }
        other => panic!("expected ServerHello, got {other:?}"),
    }
    (stream, worker_instance_id)
}

async fn wait_until<F: Fn(WorkerControlState) -> bool>(
    registry: &WorkerAuthorityRegistry,
    predicate: F,
) {
    timeout(TEST_TIMEOUT, async {
        loop {
            if predicate(registry.current()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition reached before timeout");
}

#[tokio::test]
async fn successful_handshake_makes_authority_available() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let (_stream, worker_instance_id) = handshake(&socket.0).await;

    wait_until(&registry, |state| state.is_available()).await;
    match registry.current() {
        WorkerControlState::Active {
            worker_instance_id: registered,
            ..
        } => {
            assert_eq!(registered, worker_instance_id)
        }
        WorkerControlState::NoConnection => panic!("expected an active connection"),
    }

    run_task.abort();
}

#[tokio::test]
async fn disconnect_invalidates_authority_immediately() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let (stream, _worker_instance_id) = handshake(&socket.0).await;
    wait_until(&registry, |state| state.is_available()).await;

    drop(stream);

    wait_until(&registry, |state| !state.is_available()).await;
    assert_eq!(registry.current(), WorkerControlState::NoConnection);

    run_task.abort();
}

#[tokio::test]
async fn reconnect_after_disconnect_completes_a_fresh_handshake_with_a_new_generation() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let (stream1, _id1) = handshake(&socket.0).await;
    wait_until(&registry, |state| state.is_available()).await;
    drop(stream1);
    wait_until(&registry, |state| !state.is_available()).await;

    let (_stream2, _id2) = handshake(&socket.0).await;
    wait_until(&registry, |state| state.is_available()).await;

    run_task.abort();
}

#[tokio::test]
async fn a_message_before_worker_hello_is_a_pre_handshake_violation() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let mut stream = UnixStream::connect(&socket.0).await.expect("connect");
    // Anything other than WorkerHello as the first message is a protocol
    // violation (`m1-worker-data-plane-control-contract.md` "Handshake":
    // "Every message sent before a successful handshake, other than
    // WorkerHello itself, is a protocol violation").
    send(
        &mut stream,
        &WorkerProtocolMessage::ProtocolError(ProtocolErrorMessage::new("client_test_violation")),
    )
    .await
    .expect("send");

    let response = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive response");
    match response {
        WorkerProtocolMessage::ProtocolError(body) => {
            assert_eq!(body.body.code, "pre_handshake_violation");
        }
        other => panic!("expected ProtocolError, got {other:?}"),
    }

    assert_eq!(registry.current(), WorkerControlState::NoConnection);
    run_task.abort();
}

#[tokio::test]
async fn incompatible_protocol_version_is_rejected_and_never_registers_a_generation() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let mut stream = UnixStream::connect(&socket.0).await.expect("connect");
    let mut hello = WorkerHelloMessage::new(Uuid::new_v4());
    hello.body.worker_protocol_version = bamep_worker_protocol::ProtocolVersion::new("99");
    let sent_id = hello.envelope.message_id;
    send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello))
        .await
        .expect("send incompatible WorkerHello");

    let response = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive response");
    match response {
        WorkerProtocolMessage::HandshakeRejected(body) => {
            assert_eq!(body.body.in_reply_to, sent_id);
        }
        other => panic!("expected HandshakeRejected, got {other:?}"),
    }

    assert_eq!(registry.current(), WorkerControlState::NoConnection);
    run_task.abort();
}

#[tokio::test]
async fn an_overlapping_second_handshake_supersedes_the_first_and_its_later_disconnect_is_stale() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let (stream_a, id_a) = handshake(&socket.0).await;
    wait_until(&registry, |state| state.is_available()).await;
    let generation_a = registry.current();

    let (stream_b, id_b) = handshake(&socket.0).await;
    assert_ne!(id_a, id_b);

    // The second, later handshake must become current — never fan out to
    // two simultaneously authoritative connections
    // (`m1-worker-data-plane-control-contract.md` "One authoritative active
    // connection").
    wait_until(&registry, |state| state != generation_a).await;
    match registry.current() {
        WorkerControlState::Active {
            worker_instance_id, ..
        } => assert_eq!(worker_instance_id, id_b),
        WorkerControlState::NoConnection => panic!("expected active"),
    }

    // Connection A's belated disconnect must never clobber B's now-current
    // state.
    drop(stream_a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    match registry.current() {
        WorkerControlState::Active {
            worker_instance_id, ..
        } => assert_eq!(worker_instance_id, id_b),
        WorkerControlState::NoConnection => {
            panic!("stale disconnect must not clear the current generation")
        }
    }

    drop(stream_b);
    wait_until(&registry, |state| !state.is_available()).await;
    run_task.abort();
}

#[tokio::test]
async fn controlled_shutdown_removes_the_socket_file() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    assert!(socket.0.exists());
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(registry, shutdown_rx));

    shutdown_tx.send(true).expect("send shutdown");
    timeout(TEST_TIMEOUT, run_task)
        .await
        .expect("no timeout")
        .expect("task");

    assert!(!socket.0.exists());
}

#[tokio::test]
async fn a_stale_socket_left_by_an_unclean_shutdown_is_replaced() {
    let socket = TempSocketPath::fresh();
    let first = WorkerControlPlane::bind(&socket.0).expect("first bind");
    // Simulate an unclean shutdown: the listener is dropped without ever
    // running its own socket-file cleanup.
    drop(first);
    assert!(socket.0.exists());

    // A fresh bind must recognize the leftover path is actually a socket
    // and replace it, rather than refusing to start.
    let second =
        WorkerControlPlane::bind(&socket.0).expect("second bind replaces the stale socket");
    drop(second);
}

#[tokio::test]
async fn a_pre_existing_non_socket_path_is_never_blindly_deleted() {
    let socket = TempSocketPath::fresh();
    std::fs::create_dir_all(socket.0.parent().unwrap()).expect("create parent dir");
    std::fs::write(&socket.0, b"not a socket, an unrelated file").expect("write regular file");

    let err = match WorkerControlPlane::bind(&socket.0) {
        Ok(_) => panic!("must refuse to remove a non-socket path"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        WorkerControlPlaneError::RefusingToRemoveNonSocket { .. }
    ));

    // The unrelated file must still exist, untouched.
    let contents = std::fs::read(&socket.0).expect("file still exists");
    assert_eq!(contents, b"not a socket, an unrelated file");
}
