//! `bamepd`-side UDS control-plane Adapter (Issue #37): binds/listens on the
//! configured Unix Domain Socket, accepts Worker connections, performs the
//! handshake, and tracks connection-generation authority through
//! `crate::runtime::worker_authority::WorkerAuthorityRegistry`.
//!
//! Mirrors `agent_transport.rs`'s narrow transport-boundary role, but for
//! Issue #37 also owns the tiny per-connection session loop directly — there
//! is no business message catalog yet to warrant a separate gateway-
//! equivalent module (`m1-worker-data-plane-control-contract.md` "Out of
//! scope": "a general-purpose RPC/service framework beyond the messages
//! above").
//!
//! Unix Domain Sockets are Unix-only; the real implementation lives behind
//! `#[cfg(unix)]`, mirroring `bamep_worker::ipc::client`'s identical
//! boundary. On other platforms this module compiles to a stub that never
//! successfully binds — no fake TCP/localhost substitute is introduced.

use std::sync::Arc;

use crate::adapters::worker_runtime_ownership::RuntimeDirError;
use crate::runtime::worker_authority::WorkerAuthorityRegistry;

#[derive(Debug, thiserror::Error)]
pub enum WorkerControlPlaneError {
    #[error("failed to prepare the UDS socket directory at {path}")]
    PrepareDirectory {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("refusing to use unsafe pre-existing parent directory {path}: {reason}")]
    UnsafeParentDirectory { path: String, reason: String },
    #[error("refusing to remove non-socket path {path}: an unrelated file already exists there")]
    RefusingToRemoveNonSocket { path: String },
    #[error("refusing to replace {path}: a live Worker control-plane listener already owns it")]
    SocketAlreadyActive { path: String },
    #[error("failed to inspect existing path {path}")]
    InspectExisting {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove stale socket at {path}")]
    RemoveStaleSocket {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to bind UDS listener at {path}")]
    Bind {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to set socket permissions at {path}")]
    SetPermissions {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Worker control-plane listener failed persistently")]
    AcceptFailed {
        #[source]
        source: std::io::Error,
    },
    #[error("Unix Domain Sockets are not supported on this platform")]
    UnsupportedPlatform,
}

/// The trusted-runtime-directory validation this module's own `bind` still
/// performs as defense in depth (correction audit: "Filesystem metadata
/// checks are defense in depth") reuses
/// `worker_runtime_ownership::TrustedRuntimeDir` rather than duplicating
/// directory-trust logic. This maps its richer error classification back
/// onto the pre-existing variants above so every existing caller/test
/// keeps observing the same `WorkerControlPlaneError` shape.
impl From<RuntimeDirError> for WorkerControlPlaneError {
    fn from(err: RuntimeDirError) -> Self {
        match err {
            RuntimeDirError::RelativePath { path } => {
                WorkerControlPlaneError::UnsafeParentDirectory {
                    path,
                    reason: "path must be absolute for the trusted runtime-directory boundary"
                        .to_string(),
                }
            }
            RuntimeDirError::Symlink { path } => WorkerControlPlaneError::UnsafeParentDirectory {
                path,
                reason: "parent path is a symlink, not a real directory".to_string(),
            },
            RuntimeDirError::NotADirectory { path } => {
                WorkerControlPlaneError::UnsafeParentDirectory {
                    path,
                    reason: "parent path is not a directory".to_string(),
                }
            }
            RuntimeDirError::InsecureMode { path } => {
                WorkerControlPlaneError::UnsafeParentDirectory {
                    path,
                    reason: "parent directory grants group/other permissions; expected \
                             owner-only (0700)"
                        .to_string(),
                }
            }
            RuntimeDirError::WrongOwner {
                path,
                expected,
                actual,
            } => WorkerControlPlaneError::UnsafeParentDirectory {
                path,
                reason: format!(
                    "parent directory is owned by uid {actual}, not the effective uid \
                     {expected} running bamepd"
                ),
            },
            RuntimeDirError::UnsafeAncestor { path, reason } => {
                WorkerControlPlaneError::UnsafeParentDirectory { path, reason }
            }
            RuntimeDirError::Create { path, source } => {
                WorkerControlPlaneError::PrepareDirectory { path, source }
            }
            RuntimeDirError::Inspect { path, source } => {
                WorkerControlPlaneError::InspectExisting { path, source }
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use bamep_worker_protocol::{
        receive, send, DecodeError, HandshakeRejectedMessage, ProtocolErrorMessage, ReceiveError,
        ServerHelloMessage, WorkerProtocolMessage,
    };
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::watch;
    use tokio::task::JoinSet;
    use uuid::Uuid;

    use super::*;

    /// After this many consecutive `accept()` failures, the listener is
    /// treated as terminally failed rather than retried
    /// (correction audit "Accept loop failure"). Each failure is separated
    /// by [`ACCEPT_ERROR_BACKOFF`], so reaching this threshold takes at
    /// least `MAX_CONSECUTIVE_ACCEPT_ERRORS * ACCEPT_ERROR_BACKOFF` — never
    /// a tight loop.
    const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 10;
    /// Bounded, non-zero backoff applied between `accept()` failures so a
    /// persistently failing listener (for example, file-descriptor
    /// exhaustion) never busy-spins.
    const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

    #[derive(Debug, thiserror::Error)]
    enum HandshakeError {
        #[error(transparent)]
        Receive(#[from] ReceiveError),
        #[error(transparent)]
        Send(#[from] bamep_worker_protocol::SendError),
        #[error("first message on a new connection was not WorkerHello")]
        PreHandshakeViolation,
        #[error("Worker's protocol_version is incompatible")]
        IncompatibleVersion,
        #[error("WorkerHello envelope or identity fields failed normative validation")]
        MalformedIdentity,
        #[error("received an unrecognized top-level message type")]
        UnknownMessageType,
    }

    /// The exact Unix filesystem identity (device, inode) of the socket this
    /// instance bound, captured immediately after `bind()` succeeds
    /// (correction audit "Owned socket cleanup"). Compared without following
    /// symlinks at cleanup time so this instance only ever removes the exact
    /// socket it created — never a pathname some other process/object has
    /// since replaced.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct SocketIdentity {
        dev: u64,
        ino: u64,
    }

    impl SocketIdentity {
        fn of(metadata: &std::fs::Metadata) -> Self {
            Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
            }
        }
    }

    pub struct WorkerControlPlane {
        listener: UnixListener,
        socket_path: PathBuf,
        socket_identity: SocketIdentity,
    }

    impl WorkerControlPlane {
        /// Binds the UDS listener at `path`
        /// (`m1-worker-data-plane-control-contract.md` "UDS filesystem
        /// security"). The freshly bound socket is restricted to owner-only
        /// access (`0600`).
        ///
        /// Ownership/trust decisions made here (correction audit "Safe UDS
        /// path ownership"/"Trusted UDS parent directory"):
        ///
        /// - the parent directory must already be a trustworthy, owner-only
        ///   real directory — never a symlink, never group/other
        ///   writable/readable/executable — or must not exist yet, in which
        ///   case it is created fresh as owner-only `0700`; an existing
        ///   parent that fails this check is refused rather than blindly
        ///   `chmod`ed;
        /// - an existing pathname is removed only after confirming (a) it is
        ///   actually a socket, never an arbitrary pre-existing file/
        ///   directory/symlink, and (b) it is not a *live* socket — a
        ///   pathname a real connect attempt reaches is refused
        ///   (`SocketAlreadyActive`) rather than unlinked out from under a
        ///   running daemon; only a socket that presents the narrow, actual
        ///   evidence of staleness (`ECONNREFUSED`/`ENOENT` on connect) is
        ///   removed.
        pub fn bind(path: &Path) -> Result<Self, WorkerControlPlaneError> {
            // Directory-trust validation now lives in
            // `worker_runtime_ownership::TrustedRuntimeDir`, shared with the
            // primary ownership-lock mechanism `bamepd` acquires *before*
            // calling `bind` (correction audit "Solve the ownership model
            // once"). `bind` still performs this check itself so it remains
            // independently safe to call directly (as this module's own
            // tests do), but production startup order is: validate/create
            // this same directory, acquire the lifetime lock, only then
            // call `bind`.
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                crate::adapters::worker_runtime_ownership::TrustedRuntimeDir::validate_or_create(
                    parent,
                )?;
            }

            match std::fs::symlink_metadata(path) {
                Ok(metadata) => {
                    if metadata.file_type().is_socket() {
                        if socket_is_live(path)? {
                            return Err(WorkerControlPlaneError::SocketAlreadyActive {
                                path: path.display().to_string(),
                            });
                        }
                        std::fs::remove_file(path).map_err(|source| {
                            WorkerControlPlaneError::RemoveStaleSocket {
                                path: path.display().to_string(),
                                source,
                            }
                        })?;
                    } else {
                        return Err(WorkerControlPlaneError::RefusingToRemoveNonSocket {
                            path: path.display().to_string(),
                        });
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(WorkerControlPlaneError::InspectExisting {
                        path: path.display().to_string(),
                        source,
                    })
                }
            }

            let listener =
                UnixListener::bind(path).map_err(|source| WorkerControlPlaneError::Bind {
                    path: path.display().to_string(),
                    source,
                })?;
            set_permissions(path, 0o600).map_err(|source| {
                WorkerControlPlaneError::SetPermissions {
                    path: path.display().to_string(),
                    source,
                }
            })?;

            let bound_metadata = std::fs::symlink_metadata(path).map_err(|source| {
                WorkerControlPlaneError::InspectExisting {
                    path: path.display().to_string(),
                    source,
                }
            })?;

            Ok(Self {
                listener,
                socket_path: path.to_path_buf(),
                socket_identity: SocketIdentity::of(&bound_metadata),
            })
        }

        /// Runs the accept loop until `shutdown` becomes `true` or the
        /// listener fails terminally. Every accepted connection is handed to
        /// [`handle_connection`] and owned structurally in a [`JoinSet`] —
        /// never a detached `tokio::spawn` — so controlled shutdown can
        /// reap every one of them before this method returns (correction
        /// audit "Structured connection-task ownership"/"Connection
        /// shutdown").
        ///
        /// Returns `Ok(())` on controlled shutdown, or
        /// `Err(WorkerControlPlaneError::AcceptFailed)` if `accept()` fails
        /// persistently (correction audit "Accept loop failure") — the
        /// composition root must treat that as a fatal, fail-closed
        /// condition rather than assuming the control plane is still
        /// healthy.
        pub async fn run(
            self,
            registry: Arc<WorkerAuthorityRegistry>,
            mut shutdown: watch::Receiver<bool>,
        ) -> Result<(), WorkerControlPlaneError> {
            let mut tasks: JoinSet<()> = JoinSet::new();
            let mut consecutive_accept_errors: u32 = 0;

            let outcome = loop {
                tokio::select! {
                    accepted = self.listener.accept() => {
                        match accepted {
                            Ok((stream, _addr)) => {
                                consecutive_accept_errors = 0;
                                let registry = Arc::clone(&registry);
                                let conn_shutdown = shutdown.clone();
                                tasks.spawn(handle_connection(stream, registry, conn_shutdown));
                            }
                            Err(source) => {
                                consecutive_accept_errors += 1;
                                if accept_failure_is_terminal(consecutive_accept_errors) {
                                    break Err(WorkerControlPlaneError::AcceptFailed { source });
                                }
                                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                            }
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break Ok(());
                        }
                    }
                    // Drains completed connection-handler tasks *during*
                    // normal operation, not only at shutdown (correction
                    // audit "Drain completed JoinSet tasks during normal
                    // operation"): every handler still eventually completes
                    // on its own (disconnect, protocol violation, or
                    // controlled shutdown), so leaving finished `JoinHandle`s
                    // parked in `tasks` until this method's own shutdown path
                    // would let them accumulate for as long as the daemon
                    // keeps running. The `if !tasks.is_empty()` guard is
                    // required: `JoinSet::join_next()` on an empty set
                    // resolves immediately with `None`, which would
                    // otherwise make this branch spuriously ready on every
                    // loop iteration.
                    Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                        if let Err(join_err) = result {
                            if join_err.is_panic() {
                                eprintln!(
                                    "bamepd: Worker control-plane connection handler panicked: {join_err}"
                                );
                            }
                        }
                    }
                }
            };

            match &outcome {
                Ok(()) => {
                    // Controlled shutdown: `shutdown` is already `true`, so
                    // every owned handler observes it through its own
                    // `tokio::select!` and stops itself promptly. Wait for
                    // every one of them so none remains detached after this
                    // method returns.
                    while tasks.join_next().await.is_some() {}
                }
                Err(_) => {
                    // Terminal listener failure: nothing guarantees the
                    // caller has requested shutdown yet, so do not wait on a
                    // graceful stop condition that may never arrive. Abort
                    // every owned handler directly — `Drop` still runs
                    // during abort-triggered unwinding, so every begun
                    // connection generation is still invalidated
                    // (correction audit "Generation cleanup must be
                    // cancellation-safe").
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                }
            }

            self.cleanup_own_socket();
            outcome
        }

        /// Removes this instance's own socket pathname only if it is still
        /// the exact filesystem object this instance bound — same device,
        /// same inode, still a socket — inspected without following
        /// symlinks. If the pathname has been replaced by anything else
        /// since `bind()`, it is left untouched (correction audit "Owned
        /// socket cleanup").
        fn cleanup_own_socket(&self) {
            if let Ok(metadata) = std::fs::symlink_metadata(&self.socket_path) {
                if metadata.file_type().is_socket()
                    && SocketIdentity::of(&metadata) == self.socket_identity
                {
                    let _ = std::fs::remove_file(&self.socket_path);
                }
            }
        }
    }

    /// Whether `consecutive_failures` `accept()` errors in a row mean the
    /// listener must be treated as terminally failed. A small pure
    /// threshold function so the accept-error policy is unit-testable
    /// without a real listener (correction audit "Accept loop failure").
    fn accept_failure_is_terminal(consecutive_failures: u32) -> bool {
        consecutive_failures >= MAX_CONSECUTIVE_ACCEPT_ERRORS
    }

    /// A lifecycle guard whose `Drop` invalidates `generation` exactly once,
    /// regardless of how the owning task ends — normal return, early return,
    /// panic-triggered unwind, or `JoinSet`/`abort()`-triggered cancellation
    /// (correction audit "Generation cleanup must be cancellation-safe").
    /// `WorkerAuthorityRegistry::end_generation` is already a synchronous,
    /// idempotent no-op against a superseded generation, so no async work is
    /// needed in `Drop`.
    struct GenerationGuard {
        registry: Arc<WorkerAuthorityRegistry>,
        generation: crate::runtime::worker_authority::ConnectionGeneration,
    }

    impl Drop for GenerationGuard {
        fn drop(&mut self) {
            self.registry.end_generation(self.generation);
        }
    }

    /// One accepted connection's full lifetime: handshake, then block until
    /// disconnect/protocol-violation/controlled-shutdown, whichever comes
    /// first. Never remains detached after `WorkerControlPlane::run`
    /// returns — every blocking point here is raced against `shutdown`
    /// through `tokio::select!` (correction audit "Connection shutdown").
    async fn handle_connection(
        mut stream: UnixStream,
        registry: Arc<WorkerAuthorityRegistry>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let worker_instance_id = tokio::select! {
            result = handshake_as_server(&mut stream) => {
                match result {
                    Ok(id) => id,
                    Err(_) => return,
                }
            }
            _ = shutdown.changed() => return,
        };

        let generation = registry.begin_generation(worker_instance_id);
        let _guard = GenerationGuard {
            registry: Arc::clone(&registry),
            generation,
        };

        // Issue #37 defines no post-handshake business message `bamepd`
        // consumes yet; any received message is unexpected. Detecting
        // that, an I/O error, EOF, or controlled shutdown here is exactly
        // the fail-closed behavior (`m1-worker-data-plane-control-contract.md`
        // "IPC loss is fail-closed"). `_guard` invalidates this generation
        // on every exit path, including cancellation.
        tokio::select! {
            received = receive(&mut stream) => {
                match received {
                    Ok(_unexpected) => {
                        let _ = send(
                            &mut stream,
                            &WorkerProtocolMessage::ProtocolError(ProtocolErrorMessage::new(
                                "unexpected_message",
                            )),
                        )
                        .await;
                    }
                    // Unknown top-level `type`: the approved contract
                    // requires a stable `ProtocolError`, distinct from
                    // silently dropping the connection on decode failure
                    // (`m1-worker-data-plane-control-contract.md`: "Unknown
                    // top-level type: rejected with ProtocolError").
                    Err(ReceiveError::Decode(DecodeError::UnknownType(type_name))) => {
                        let _ = send(
                            &mut stream,
                            &WorkerProtocolMessage::ProtocolError(
                                ProtocolErrorMessage::new("unknown_message_type")
                                    .with_message(format!("unrecognized type {type_name:?}")),
                            ),
                        )
                        .await;
                    }
                    Err(_) => {}
                }
            }
            _ = shutdown.changed() => {}
        }
    }

    async fn handshake_as_server(stream: &mut UnixStream) -> Result<Uuid, HandshakeError> {
        let first = match receive(stream).await {
            Ok(message) => message,
            // Unknown top-level `type`: distinct from every other
            // malformed-frame case because the envelope was parseable
            // enough to name a `type` at all
            // (`m1-worker-data-plane-control-contract.md`: "Unknown
            // top-level type: rejected with ProtocolError"). Never reaches
            // `begin_generation`.
            Err(ReceiveError::Decode(DecodeError::UnknownType(type_name))) => {
                let _ = send(
                    stream,
                    &WorkerProtocolMessage::ProtocolError(
                        ProtocolErrorMessage::new("unknown_message_type")
                            .with_message(format!("unrecognized type {type_name:?}")),
                    ),
                )
                .await;
                return Err(HandshakeError::UnknownMessageType);
            }
            Err(err) => return Err(HandshakeError::Receive(err)),
        };
        let WorkerProtocolMessage::WorkerHello(hello) = first else {
            let _ = send(
                stream,
                &WorkerProtocolMessage::ProtocolError(ProtocolErrorMessage::new(
                    "pre_handshake_violation",
                )),
            )
            .await;
            return Err(HandshakeError::PreHandshakeViolation);
        };

        // Malformed identity shape (envelope `message_id`/`worker_instance_id`
        // not UUID v4, or envelope `protocol_version` not "1") is distinct
        // from a genuine, well-formed but unsupported
        // `worker_protocol_version` — the former can never be described by
        // the closed `HandshakeRejected` reason vocabulary, so it is a
        // `ProtocolError` instead (`m1-worker-data-plane-control-contract.md`
        // "Failure semantics": "Malformed frame/message"). Neither path ever
        // reaches `begin_generation`.
        if !hello.envelope.is_valid()
            || !bamep_worker_protocol::is_uuid_v4(&hello.body.worker_instance_id)
        {
            let _ = send(
                stream,
                &WorkerProtocolMessage::ProtocolError(
                    ProtocolErrorMessage::new("malformed_handshake_identity")
                        .with_in_reply_to(hello.envelope.message_id),
                ),
            )
            .await;
            return Err(HandshakeError::MalformedIdentity);
        }

        if !hello.body.worker_protocol_version.is_v1() {
            let _ = send(
                stream,
                &WorkerProtocolMessage::HandshakeRejected(
                    HandshakeRejectedMessage::incompatible_version(hello.envelope.message_id),
                ),
            )
            .await;
            return Err(HandshakeError::IncompatibleVersion);
        }

        send(
            stream,
            &WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(hello.envelope.message_id)),
        )
        .await?;

        Ok(hello.body.worker_instance_id)
    }

    /// Probes whether `path` is a *live* reachable UDS listener, distinct
    /// from a stale socket left by an unclean exit (correction audit "Safe
    /// UDS path ownership"). A successful connect is unambiguous evidence of
    /// liveness. `ECONNREFUSED` (the kernel accepted the bind association
    /// but no process currently holds the listener open) and `ENOENT` (the
    /// path vanished between the caller's existence check and this connect)
    /// are the only OS error conditions accepted as evidence of a stale
    /// endpoint. Every other error — most importantly a permission error,
    /// which proves nothing about liveness — fails closed with
    /// [`WorkerControlPlaneError::InspectExisting`] rather than being
    /// silently treated as "stale". `connect()` on `AF_UNIX` is
    /// non-blocking in the sense that it never performs a network-style
    /// handshake, so no explicit timeout is needed here.
    fn socket_is_live(path: &Path) -> Result<bool, WorkerControlPlaneError> {
        match StdUnixStream::connect(path) {
            Ok(_stream) => Ok(true),
            Err(err)
                if err.kind() == std::io::ErrorKind::ConnectionRefused
                    || err.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(false)
            }
            Err(source) => Err(WorkerControlPlaneError::InspectExisting {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    fn set_permissions(path: &Path, mode: u32) -> std::io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    #[cfg(test)]
    mod unit_tests {
        use super::*;

        /// Proves the exact `tokio::select!` arm shape `WorkerControlPlane::run`
        /// uses to drain completed connection-handler tasks
        /// (correction audit "Drain completed JoinSet tasks during normal
        /// operation"): every already-completed task is reaped without
        /// waiting for a shutdown signal, and the `if !tasks.is_empty()`
        /// guard stops the branch from firing once nothing remains.
        #[tokio::test]
        async fn join_set_completed_tasks_are_drained_without_waiting_for_shutdown() {
            let mut tasks: JoinSet<()> = JoinSet::new();
            for _ in 0..5 {
                tasks.spawn(async {});
            }
            // Give the spawned no-op tasks a real chance to complete before
            // the drain loop below starts observing them.
            tokio::time::sleep(Duration::from_millis(20)).await;

            let mut drained = 0;
            while !tasks.is_empty() {
                tokio::select! {
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {
                        drained += 1;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(5)) => break,
                }
            }

            assert_eq!(
                drained, 5,
                "every completed task must be reaped without waiting for shutdown"
            );
            assert!(tasks.is_empty());
        }

        #[test]
        fn fewer_than_the_threshold_is_not_terminal() {
            assert!(!accept_failure_is_terminal(
                MAX_CONSECUTIVE_ACCEPT_ERRORS - 1
            ));
        }

        #[test]
        fn reaching_the_threshold_is_terminal() {
            assert!(accept_failure_is_terminal(MAX_CONSECUTIVE_ACCEPT_ERRORS));
        }

        #[test]
        fn past_the_threshold_remains_terminal() {
            assert!(accept_failure_is_terminal(
                MAX_CONSECUTIVE_ACCEPT_ERRORS + 1
            ));
        }

        /// Correction audit "Generation cleanup must be cancellation-safe":
        /// `GenerationGuard::drop` must invalidate its generation even when
        /// its owning task is aborted mid-flight, never only on a normal
        /// return. This exercises `GenerationGuard` directly (a private
        /// type), since `WorkerControlPlane::run`'s own connection tasks are
        /// not otherwise externally abortable from a black-box integration
        /// test.
        #[tokio::test]
        async fn generation_guard_end_generation_runs_even_when_its_task_is_aborted() {
            let registry = Arc::new(WorkerAuthorityRegistry::new());
            let generation = registry.begin_generation(Uuid::new_v4());
            assert!(registry.is_current(generation));

            let guard_registry = Arc::clone(&registry);
            let handle = tokio::spawn(async move {
                let _guard = GenerationGuard {
                    registry: guard_registry,
                    generation,
                };
                // Blocks forever: the only way this task ends is abort().
                std::future::pending::<()>().await
            });

            // Let the task actually start and construct the guard before
            // aborting it.
            tokio::task::yield_now().await;
            handle.abort();
            let _ = handle.await;

            assert!(
                !registry.is_current(generation),
                "abort-triggered unwinding must still run GenerationGuard::drop"
            );
        }

        /// A stale (already-superseded) generation's guard must never clear
        /// a newer generation, mirroring
        /// `WorkerAuthorityRegistry`'s own equivalent unit test but through
        /// the `GenerationGuard` RAII path specifically.
        #[test]
        fn a_stale_generation_guards_drop_never_clobbers_a_newer_one() {
            let registry = Arc::new(WorkerAuthorityRegistry::new());
            let first = registry.begin_generation(Uuid::new_v4());
            let second = registry.begin_generation(Uuid::new_v4());

            {
                let _guard = GenerationGuard {
                    registry: Arc::clone(&registry),
                    generation: first,
                };
            }

            assert!(registry.is_current(second));
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;

    use tokio::sync::watch;

    use super::*;

    /// Unix Domain Sockets are not available on this platform. Linux is the
    /// Bamep Server/Worker reference/production environment
    /// (`docs/development/testing.md`); this stub only keeps the crate
    /// portable/compilable elsewhere and never successfully binds — no fake
    /// TCP/localhost IPC substitute is introduced.
    pub struct WorkerControlPlane;

    impl WorkerControlPlane {
        pub fn bind(_path: &Path) -> Result<Self, WorkerControlPlaneError> {
            Err(WorkerControlPlaneError::UnsupportedPlatform)
        }

        pub async fn run(
            self,
            _registry: Arc<WorkerAuthorityRegistry>,
            _shutdown: watch::Receiver<bool>,
        ) -> Result<(), WorkerControlPlaneError> {
            Ok(())
        }
    }
}

pub use imp::WorkerControlPlane;
