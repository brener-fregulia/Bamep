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

use std::os::unix::fs::PermissionsExt;
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

/// Correction audit "Unknown top-level Worker message type": an unknown
/// `type` on an otherwise-parseable envelope, sent as the very first
/// message, must receive a stable `ProtocolError` — distinct from the
/// generic `pre_handshake_violation` case above — and must never let
/// authority become current
/// (`m1-worker-data-plane-control-contract.md`: "Unknown top-level type:
/// rejected with ProtocolError"). Uses real framed JSON over a real
/// connection, not a unit-level `serde_json::from_str` assertion.
#[tokio::test]
async fn an_unknown_top_level_message_type_receives_a_protocol_error_and_never_registers_a_generation(
) {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let mut stream = UnixStream::connect(&socket.0).await.expect("connect");
    let raw = format!(
        r#"{{"type":"TotallyBogusMessageType","protocol_version":"1","message_id":"{}"}}"#,
        Uuid::new_v4()
    );
    bamep_worker_protocol::write_frame(&mut stream, raw.as_bytes())
        .await
        .expect("write raw frame");

    let response = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive response");
    match response {
        WorkerProtocolMessage::ProtocolError(body) => {
            assert_eq!(body.body.code, "unknown_message_type");
        }
        other => panic!("expected ProtocolError, got {other:?}"),
    }

    assert_eq!(
        registry.current(),
        WorkerControlState::NoConnection,
        "authority must never become current for an unknown top-level message type"
    );

    // The connection must close/fail safely afterward: a further read
    // observes EOF/error rather than the connection staying open as if
    // handshake had succeeded.
    let after = timeout(TEST_TIMEOUT, receive(&mut stream)).await;
    if let Ok(Ok(other)) = after {
        panic!("expected the connection to close, got {other:?}");
    }

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

/// Server validation (correction audit "Strict handshake validation"): a
/// `WorkerHello` whose envelope `protocol_version` is not `"1"` is a
/// malformed-identity violation, distinct from a well-formed-but-
/// unsupported `worker_protocol_version` — it must never reach
/// `begin_generation`.
#[tokio::test]
async fn worker_hello_with_wrong_envelope_protocol_version_never_registers_a_generation() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let mut stream = UnixStream::connect(&socket.0).await.expect("connect");
    let mut hello = WorkerHelloMessage::new(Uuid::new_v4());
    hello.envelope.protocol_version = bamep_worker_protocol::ProtocolVersion::new("2");
    send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello))
        .await
        .expect("send");

    let response = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive response");
    match response {
        WorkerProtocolMessage::ProtocolError(body) => {
            assert_eq!(body.body.code, "malformed_handshake_identity");
        }
        other => panic!("expected ProtocolError, got {other:?}"),
    }
    assert_eq!(registry.current(), WorkerControlState::NoConnection);
    run_task.abort();
}

/// Server validation: a `WorkerHello` envelope `message_id` that is not a
/// UUID v4 must never reach `begin_generation`.
#[tokio::test]
async fn worker_hello_with_non_v4_message_id_never_registers_a_generation() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let mut stream = UnixStream::connect(&socket.0).await.expect("connect");
    let mut hello = WorkerHelloMessage::new(Uuid::new_v4());
    hello.envelope.message_id = Uuid::nil();
    send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello))
        .await
        .expect("send");

    let response = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive response");
    match response {
        WorkerProtocolMessage::ProtocolError(body) => {
            assert_eq!(body.body.code, "malformed_handshake_identity");
        }
        other => panic!("expected ProtocolError, got {other:?}"),
    }
    assert_eq!(registry.current(), WorkerControlState::NoConnection);
    run_task.abort();
}

/// Server validation: a non-v4 `worker_instance_id` must never reach
/// `begin_generation`, even though `message_id`/`protocol_version` are
/// otherwise valid.
#[tokio::test]
async fn worker_hello_with_non_v4_worker_instance_id_never_registers_a_generation() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let mut stream = UnixStream::connect(&socket.0).await.expect("connect");
    let hello = WorkerHelloMessage::new(Uuid::nil());
    send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello))
        .await
        .expect("send");

    let response = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive response");
    match response {
        WorkerProtocolMessage::ProtocolError(body) => {
            assert_eq!(body.body.code, "malformed_handshake_identity");
        }
        other => panic!("expected ProtocolError, got {other:?}"),
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
    let result = timeout(TEST_TIMEOUT, run_task)
        .await
        .expect("no timeout")
        .expect("task");
    assert!(result.is_ok(), "controlled shutdown must return Ok(())");

    assert!(!socket.0.exists());
}

#[tokio::test]
async fn a_stale_socket_left_by_an_unclean_shutdown_is_replaced() {
    let socket = TempSocketPath::fresh();
    let first = WorkerControlPlane::bind(&socket.0).expect("first bind");
    // Simulate an unclean shutdown: the listener is dropped without ever
    // running its own socket-file cleanup. The kernel-level listen
    // association ends with the fd, so a subsequent connect attempt gets
    // `ECONNREFUSED` — the narrow evidence `WorkerControlPlane::bind`
    // accepts as proof of staleness (correction audit "Safe UDS path
    // ownership").
    drop(first);
    assert!(socket.0.exists());

    // A fresh bind must recognize the leftover path is actually a stale
    // socket and replace it, rather than refusing to start.
    let second =
        WorkerControlPlane::bind(&socket.0).expect("second bind replaces the stale socket");
    drop(second);
}

#[tokio::test]
async fn a_still_live_socket_is_never_unlinked_or_replaced() {
    let socket = TempSocketPath::fresh();
    let first = WorkerControlPlane::bind(&socket.0).expect("first bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    // Keep the first control plane genuinely running (a live listener),
    // never dropped before the second bind attempt below.
    let first_run_task = tokio::spawn(first.run(registry, shutdown_rx));

    let err = WorkerControlPlane::bind(&socket.0)
        .err()
        .expect("must refuse to replace a live socket");
    assert!(matches!(
        err,
        WorkerControlPlaneError::SocketAlreadyActive { .. }
    ));

    // The first control plane must still be exactly the one serving that
    // path — a second `bamepd` must never unlink the first live daemon's
    // pathname.
    let (_stream, _worker_instance_id) = handshake(&socket.0).await;

    first_run_task.abort();
}

#[tokio::test]
async fn a_pre_existing_non_socket_path_is_never_blindly_deleted() {
    let socket = TempSocketPath::fresh();
    std::fs::create_dir_all(socket.0.parent().unwrap()).expect("create parent dir");
    std::fs::set_permissions(
        socket.0.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("set trusted parent dir permissions");
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

#[tokio::test]
async fn a_symlink_at_the_socket_path_is_never_followed_or_removed() {
    let socket = TempSocketPath::fresh();
    std::fs::create_dir_all(socket.0.parent().unwrap()).expect("create parent dir");
    std::fs::set_permissions(
        socket.0.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("set trusted parent dir permissions");

    // A real socket exists at a different path...
    let real_socket_path = socket.0.parent().unwrap().join("real.sock");
    let real_listener =
        std::os::unix::net::UnixListener::bind(&real_socket_path).expect("bind real socket");

    // ...and the configured path is a symlink pointing at it.
    std::os::unix::fs::symlink(&real_socket_path, &socket.0).expect("create symlink");

    let err = match WorkerControlPlane::bind(&socket.0) {
        Ok(_) => panic!("must refuse to follow a symlink at the socket path"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        WorkerControlPlaneError::RefusingToRemoveNonSocket { .. }
    ));

    // Neither the symlink nor the real target socket was touched.
    assert!(std::fs::symlink_metadata(&socket.0)
        .expect("symlink still exists")
        .file_type()
        .is_symlink());
    assert!(real_socket_path.exists());
    drop(real_listener);
}

#[tokio::test]
async fn an_unsafe_pre_existing_parent_directory_is_rejected() {
    let socket = TempSocketPath::fresh();
    std::fs::create_dir_all(socket.0.parent().unwrap()).expect("create parent dir");
    // World-writable: exactly the kind of shared/untrustworthy directory
    // this policy must reject rather than silently `chmod`.
    std::fs::set_permissions(
        socket.0.parent().unwrap(),
        std::fs::Permissions::from_mode(0o777),
    )
    .expect("relax parent dir permissions");

    let err = match WorkerControlPlane::bind(&socket.0) {
        Ok(_) => panic!("must refuse an unsafe pre-existing parent directory"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        WorkerControlPlaneError::UnsafeParentDirectory { .. }
    ));
}

#[tokio::test]
async fn a_secure_pre_existing_parent_directory_works() {
    let socket = TempSocketPath::fresh();
    std::fs::create_dir_all(socket.0.parent().unwrap()).expect("create parent dir");
    std::fs::set_permissions(
        socket.0.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("set trusted parent dir permissions");

    let plane = WorkerControlPlane::bind(&socket.0)
        .expect("bind must succeed against an already-secure parent directory");
    drop(plane);
}

#[tokio::test]
async fn controlled_shutdown_does_not_remove_a_pathname_replaced_after_bind() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Simulate the pathname being replaced by an unrelated object after
    // this instance bound it — controlled shutdown must never remove this
    // replacement (correction audit "Owned socket cleanup").
    std::fs::remove_file(&socket.0).expect("remove original socket file");
    std::fs::write(&socket.0, b"unrelated replacement content").expect("write replacement file");

    let run_task = tokio::spawn(plane.run(registry, shutdown_rx));
    shutdown_tx.send(true).expect("send shutdown");
    let result = timeout(TEST_TIMEOUT, run_task)
        .await
        .expect("no timeout")
        .expect("task");
    assert!(result.is_ok());

    let contents = std::fs::read(&socket.0).expect("replacement file must still exist");
    assert_eq!(contents, b"unrelated replacement content");
}

#[tokio::test]
async fn controlled_shutdown_disconnects_an_active_connection_before_returning() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    let (mut stream, _worker_instance_id) = handshake(&socket.0).await;
    wait_until(&registry, |state| state.is_available()).await;

    shutdown_tx.send(true).expect("send shutdown");

    // `run()` must not return while the active connection handler is still
    // live: by the time the task completes, the peer must already observe
    // its side of the connection closed (EOF) — the handler was actually
    // stopped and reaped, not merely detached (correction audit "Connection
    // shutdown").
    let eof = timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout");
    assert!(
        eof.is_err(),
        "the server must close the connection on shutdown"
    );

    let result = timeout(TEST_TIMEOUT, run_task)
        .await
        .expect("no timeout")
        .expect("task");
    assert!(result.is_ok());

    // The connection generation must have been invalidated by the same
    // shutdown, never left dangling as "available" with no live peer.
    assert_eq!(registry.current(), WorkerControlState::NoConnection);
}

/// Correction audit "Drain completed JoinSet tasks during normal
/// operation": many sequential connect/handshake/disconnect cycles must not
/// degrade the accept loop's responsiveness during normal operation (before
/// any shutdown is requested). If completed handlers were only reaped at
/// shutdown, this would still *pass* in wall-clock terms for a small cycle
/// count, but the point of this test is combined with
/// `join_set_completed_tasks_are_drained_without_waiting_for_shutdown` in
/// `worker_control_plane.rs`'s own unit tests, which proves the exact
/// `tokio::select!` drain mechanism directly — this test additionally
/// proves the real listener stays healthy and promptly responsive across
/// many cycles.
#[tokio::test]
async fn repeated_connect_disconnect_cycles_keep_the_listener_promptly_responsive() {
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(Arc::clone(&registry), shutdown_rx));

    for _ in 0..200 {
        let (stream, _worker_instance_id) = handshake(&socket.0).await;
        drop(stream);
    }

    // A fresh connection after 200 prior cycles must still handshake
    // promptly, well within the same timeout every other test in this file
    // uses for a single cycle.
    let (_stream, _worker_instance_id) = timeout(TEST_TIMEOUT, handshake(&socket.0)).await.expect(
        "the listener must remain promptly responsive after many connect/disconnect cycles",
    );
    wait_until(&registry, |state| state.is_available()).await;

    run_task.abort();
}
