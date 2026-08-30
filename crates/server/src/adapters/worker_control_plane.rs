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
        receive, send, ArtifactVerificationAckMessage, ArtifactVerificationReportMessage,
        AuthorizationDecisionMessage, AuthorizationQueryMessage, ChunkAcceptanceDecisionMessage,
        ChunkAcceptanceRejectionReason, ChunkAcceptanceRequestMessage, DecodeError,
        HandshakeRejectedMessage, HeldChunk, ManifestSealDecisionMessage,
        ManifestSealRejectionReason, ManifestSealRequestMessage, ProtocolErrorMessage,
        ReceiveError, ResumeDiscoveryContinueMessage, ResumeDiscoveryPageMessage,
        ResumeDiscoveryQueryMessage, SealedManifestFacts, ServerHelloMessage, WireArtifactStatus,
        WireDigestAlgorithm, WorkerProtocolMessage,
    };
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::watch;
    use tokio::task::JoinSet;
    use uuid::Uuid;

    use crate::application::{
        ArtifactVerificationInput, ArtifactVerificationService, ChunkAcceptanceService,
        ManifestSealInput, ManifestSealService, ResumeAuthorizationOutcome,
        TransferAuthorizationService, WorkerAuthorizationOutcome, WorkerAuthorizationQueryInput,
    };
    use crate::ports::{ArtifactVerificationCommit, ChunkAcceptanceCommit, ManifestSealCommit};
    use crate::runtime::transient_worker_operations::{
        AcceptanceBinding, HeldChunkEntry, ResumeCursorBinding, ResumeCursorState, ResumeSnapshot,
        TransientWorkerOperationStore, VerificationBinding,
    };

    use super::*;

    /// The bounded number of `held_chunks` entries one `ResumeDiscoveryPage`
    /// frame carries (`m1-worker-data-plane-control-contract.md`
    /// "Resume-manifest pagination": "`bamepd` chooses a bounded page size
    /// such that every `ResumeDiscoveryPage` frame ... stays safely within
    /// 1 MiB"). Each entry encodes to at most ~82 UTF-8 bytes
    /// (`{"chunk_index":<=10 digits,"digest":"<43 chars>"}` plus a comma);
    /// `8192 * 82` ≈ 672 KiB leaves the first page's manifest-level fields,
    /// envelope, and cursor comfortable headroom under
    /// [`bamep_worker_protocol::MAX_FRAME_PAYLOAD_BYTES`] (proven by
    /// `resume_page_frame_stays_within_the_1_mib_limit`). The universal frame
    /// limit is never raised to fit a page.
    pub(crate) const RESUME_PAGE_MAX_HELD_CHUNKS: usize = 8192;

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
        resume_page_size: usize,
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
                resume_page_size: RESUME_PAGE_MAX_HELD_CHUNKS,
            })
        }

        /// Overrides the `held_chunks`-per-`ResumeDiscoveryPage` bound
        /// (default [`RESUME_PAGE_MAX_HELD_CHUNKS`], clamped to at least 1).
        /// The page size is `bamepd`'s implementation choice
        /// (`m1-worker-data-plane-control-contract.md` "Resume-manifest
        /// pagination"); this builder lets a test force multi-page pagination
        /// with a small held-chunk set. Never used to *raise* the frame
        /// limit.
        pub fn with_resume_page_size(mut self, page_size: usize) -> Self {
            self.resume_page_size = page_size.max(1);
            self
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
        #[allow(clippy::too_many_arguments)]
        pub async fn run(
            self,
            registry: Arc<WorkerAuthorityRegistry>,
            transfer_authorization: Arc<TransferAuthorizationService>,
            chunk_acceptance: Arc<ChunkAcceptanceService>,
            manifest_seal: Arc<ManifestSealService>,
            artifact_verification: Arc<ArtifactVerificationService>,
            mut shutdown: watch::Receiver<bool>,
        ) -> Result<(), WorkerControlPlaneError> {
            let resume_page_size = self.resume_page_size;
            let mut tasks: JoinSet<()> = JoinSet::new();
            let mut consecutive_accept_errors: u32 = 0;

            let outcome = loop {
                tokio::select! {
                    accepted = self.listener.accept() => {
                        match accepted {
                            Ok((stream, _addr)) => {
                                consecutive_accept_errors = 0;
                                let registry = Arc::clone(&registry);
                                let transfer_authorization = Arc::clone(&transfer_authorization);
                                let chunk_acceptance = Arc::clone(&chunk_acceptance);
                                let manifest_seal = Arc::clone(&manifest_seal);
                                let artifact_verification = Arc::clone(&artifact_verification);
                                let conn_shutdown = shutdown.clone();
                                tasks.spawn(handle_connection(
                                    stream,
                                    registry,
                                    transfer_authorization,
                                    chunk_acceptance,
                                    manifest_seal,
                                    artifact_verification,
                                    resume_page_size,
                                    conn_shutdown,
                                ));
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
    #[allow(clippy::too_many_arguments)]
    async fn handle_connection(
        mut stream: UnixStream,
        registry: Arc<WorkerAuthorityRegistry>,
        transfer_authorization: Arc<TransferAuthorizationService>,
        chunk_acceptance: Arc<ChunkAcceptanceService>,
        manifest_seal: Arc<ManifestSealService>,
        artifact_verification: Arc<ArtifactVerificationService>,
        resume_page_size: usize,
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

        // This generation's transient Worker-operation state (Issue #39
        // Phase B): a fresh empty store, owned here for exactly this
        // connection's lifetime and dropped when this function returns. It is
        // additionally published to `registry` as a non-owning `Weak` so the
        // authoritative generation's store is reachable through the same
        // registry `bamepd` already shares, and self-checks generation
        // currency on every operation so a superseded generation whose task
        // still holds this `Arc` can no longer mint or consume anything.
        let operations = Arc::new(TransientWorkerOperationStore::new(
            generation,
            Arc::clone(&registry),
            registry.operations_capacity(),
        ));
        registry.set_current_operations(generation, Arc::downgrade(&operations));

        // Issue #38 extends this loop from #37's single-shot receive into a
        // genuine per-connection request/response loop: `AuthorizationQuery`
        // is answered in place, sequentially — this connection task is the
        // sole owner of `stream`'s serialized I/O for its whole lifetime
        // (`m1-worker-data-plane-control-contract.md` "Authority": Worker
        // only requests a decision; `bamepd` alone decides). Every other
        // received message, an I/O error, EOF, or controlled shutdown ends
        // the loop — exactly the fail-closed behavior
        // (`m1-worker-data-plane-control-contract.md` "IPC loss is
        // fail-closed"). `_guard` invalidates this generation on every exit
        // path, including cancellation.
        loop {
            tokio::select! {
                received = receive(&mut stream) => {
                    match received {
                        Ok(WorkerProtocolMessage::AuthorizationQuery(query)) => {
                            let response = decide_authorization_query(
                                &operations,
                                &transfer_authorization,
                                &query,
                            )
                            .await;
                            if send(&mut stream, &WorkerProtocolMessage::AuthorizationDecision(response))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        // A `ChunkAcceptanceRequest` with an invalid transient
                        // `acceptance_handle`, or a contract-violating one, is
                        // discarded with no response — the Worker fails the
                        // HTTP request closed
                        // (`m1-worker-data-plane-control-contract.md` "Stale
                        // response / unknown correlation"; Issue #39 Phase C1
                        // items 7–8). Only a durable outcome produces a
                        // `ChunkAcceptanceDecision`.
                        Ok(WorkerProtocolMessage::ChunkAcceptanceRequest(request)) => {
                            if let Some(decision) = handle_chunk_acceptance(
                                &operations,
                                &chunk_acceptance,
                                &request,
                            )
                            .await
                            {
                                if send(
                                    &mut stream,
                                    &WorkerProtocolMessage::ChunkAcceptanceDecision(decision),
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                        }
                        Ok(WorkerProtocolMessage::ResumeDiscoveryQuery(query)) => {
                            let page = handle_resume_discovery_query(
                                &operations,
                                &transfer_authorization,
                                resume_page_size,
                                &query,
                            )
                            .await;
                            if send(&mut stream, &WorkerProtocolMessage::ResumeDiscoveryPage(page))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(WorkerProtocolMessage::ResumeDiscoveryContinue(cont)) => {
                            let page = handle_resume_discovery_continue(
                                &operations,
                                resume_page_size,
                                &cont,
                            );
                            if send(&mut stream, &WorkerProtocolMessage::ResumeDiscoveryPage(page))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        // A `ManifestSealRequest` whose durable seal fails
                        // closed (structural violation, internal invariant
                        // violation) or whose post-commit `verification_handle`
                        // mint fails is discarded with no response — the
                        // Worker fails the HTTP request closed and an
                        // idempotent seal retry re-drives it (Issue #39 Phase
                        // C2 items 8, 16, 21). Every other outcome produces a
                        // `ManifestSealDecision`.
                        Ok(WorkerProtocolMessage::ManifestSealRequest(request)) => {
                            if let Some(decision) = handle_manifest_seal(
                                &operations,
                                &manifest_seal,
                                &request,
                            )
                            .await
                            {
                                if send(
                                    &mut stream,
                                    &WorkerProtocolMessage::ManifestSealDecision(decision),
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                        }
                        // An `ArtifactVerificationReport` with an invalid/
                        // stale/consumed/wrong-kind `verification_handle`, a
                        // malformed reported digest, or a durable/binding
                        // mismatch is discarded with no response — never
                        // mapped to `Failed` (Issue #39 Phase C2 items 26, 27,
                        // 32, 33). Only a committed
                        // `Verified`/`Failed` transition produces an
                        // `ArtifactVerificationAck`.
                        Ok(WorkerProtocolMessage::ArtifactVerificationReport(report)) => {
                            if let Some(ack) = handle_artifact_verification(
                                &operations,
                                &artifact_verification,
                                &report,
                            )
                            .await
                            {
                                if send(
                                    &mut stream,
                                    &WorkerProtocolMessage::ArtifactVerificationAck(ack),
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                        }
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
                        Err(_) => break,
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    }

    fn to_wire_digest_algorithm(algorithm: bamep_domain::DigestAlgorithm) -> WireDigestAlgorithm {
        match algorithm {
            bamep_domain::DigestAlgorithm::Sha256 => WireDigestAlgorithm::Sha256,
        }
    }

    /// Converts one received `AuthorizationQuery` into the Application-layer
    /// input, delegates the authoritative decision to
    /// [`TransferAuthorizationService::decide`], mints the transient
    /// `acceptance_handle` into this generation's
    /// [`TransientWorkerOperationStore`], and converts the result back into
    /// the exact wire `AuthorizationDecision` — this Adapter never makes the
    /// authorization decision itself (ADR-0018).
    async fn decide_authorization_query(
        operations: &TransientWorkerOperationStore,
        transfer_authorization: &TransferAuthorizationService,
        query: &AuthorizationQueryMessage,
    ) -> AuthorizationDecisionMessage {
        // An `AuthorizationQuery` always implies `operation = chunk_upload`
        // and always carries a `chunk_index` (the message type is the
        // operation — `m1-worker-data-plane-control-contract.md` "Operations,
        // HTTP mapping, and transcript inputs").
        let input = WorkerAuthorizationQueryInput {
            token: query.body.token.clone(),
            operation: bamep_domain::AuthorizationOperation::ChunkUpload,
            transfer_id: query.body.transfer_id,
            chunk_index: Some(query.body.chunk_index),
            proof_id: query.body.proof_id.clone(),
            issued_at_millis: query.body.issued_at,
            signature: query.body.signature.clone(),
        };
        let request_id = query.envelope.message_id;
        match transfer_authorization.decide(input).await {
            Ok(WorkerAuthorizationOutcome::Approved {
                digest_algorithm,
                chunk_size,
                expected_chunk_digest,
            }) => {
                // The proof has already been accepted — and its `proof_id`
                // recorded in the replay cache — inside `decide`. If minting
                // the transient acceptance binding now fails closed (the
                // store is saturated, the freshly minted opaque id collided
                // with a live binding, or this connection generation is no
                // longer current), the Worker must NOT receive an approved
                // decision carrying an unusable handle
                // (`m1-worker-data-plane-control-contract.md` "Transient
                // operation handles"). We deliberately do not roll back the
                // consumed replay record: a retry with the *same* proof
                // legitimately fails replay, and the Agent mints a fresh
                // proof per "Idempotent retry is not proof reuse". The
                // failure cause is never surfaced to the Worker — it maps to
                // the same generic non-enumerable denial as any other.
                match operations.mint_acceptance(AcceptanceBinding {
                    transfer_id: bamep_domain::TransferId(query.body.transfer_id),
                    chunk_index: query.body.chunk_index,
                    proof_id: query.body.proof_id.clone(),
                }) {
                    Ok(acceptance_handle) => AuthorizationDecisionMessage::approved(
                        request_id,
                        to_wire_digest_algorithm(digest_algorithm),
                        chunk_size,
                        acceptance_handle,
                        expected_chunk_digest,
                    ),
                    Err(_) => AuthorizationDecisionMessage::denied(request_id),
                }
            }
            Ok(WorkerAuthorizationOutcome::Denied) => {
                AuthorizationDecisionMessage::denied(request_id)
            }
            // A genuine Repository/backend failure fails closed identically
            // to an ordinary denial — Worker must never observe a more
            // specific outcome than the generic non-enumerable shape
            // (`m1-worker-data-plane-control-contract.md` "Security and
            // logging").
            Err(_) => AuthorizationDecisionMessage::denied(request_id),
        }
    }

    /// One received `ChunkAcceptanceRequest`. Returns `Some(decision)` only
    /// when a durable outcome exists to report; `None` means the message is
    /// discarded with no response (invalid/stale/contract-violating
    /// follow-up) and the Worker's HTTP request fails closed
    /// (`m1-worker-data-plane-control-contract.md` "Verified-chunk durable
    /// acceptance", "Stale response / unknown correlation"; Issue #39 Phase
    /// C1).
    async fn handle_chunk_acceptance(
        operations: &TransientWorkerOperationStore,
        chunk_acceptance: &ChunkAcceptanceService,
        request: &ChunkAcceptanceRequestMessage,
    ) -> Option<ChunkAcceptanceDecisionMessage> {
        let in_reply_to = request.envelope.message_id;
        let transfer_id = bamep_domain::TransferId(request.body.transfer_id);

        // Consuming the `acceptance_handle` is THE generation-scoped
        // authorization-correlation linearization point (`m1` §2). A stale
        // (prior-generation), unknown, already-consumed, wrong-kind, or
        // transfer/chunk-mismatched handle is discarded — never mapped to an
        // enumerable `rejected` reason (Issue #39 Phase C1 item 7), never a
        // durable mutation.
        let binding = match operations.consume_acceptance(
            &request.body.acceptance_handle,
            transfer_id,
            request.body.chunk_index,
        ) {
            Ok(binding) => binding,
            Err(_) => {
                eprintln!(
                    "bamepd: discarded a ChunkAcceptanceRequest presenting an invalid or stale \
                     transient acceptance handle (no response, no durable mutation)"
                );
                return None;
            }
        };
        // `binding.proof_id` is internal acceptance metadata only — never
        // echoed to the Worker, never logged (`m1` §2; "Security and
        // logging"). The successful consume is single-use: it stays consumed
        // regardless of the durable outcome below (item 8).
        debug_assert_eq!(binding.transfer_id, transfer_id);
        debug_assert_eq!(binding.chunk_index, request.body.chunk_index);

        match chunk_acceptance
            .commit_chunk_acceptance(
                transfer_id,
                request.body.chunk_index,
                request.body.digest.clone(),
                request.body.size,
            )
            .await
        {
            Ok(ChunkAcceptanceCommit::Committed) => {
                Some(ChunkAcceptanceDecisionMessage::committed(in_reply_to))
            }
            Ok(ChunkAcceptanceCommit::AlreadyCommitted) => Some(
                ChunkAcceptanceDecisionMessage::already_committed(in_reply_to),
            ),
            Ok(ChunkAcceptanceCommit::RejectedConflict) => {
                Some(ChunkAcceptanceDecisionMessage::rejected(
                    in_reply_to,
                    ChunkAcceptanceRejectionReason::ChunkIdentityConflict,
                ))
            }
            Ok(ChunkAcceptanceCommit::RejectedNotContinuable) => {
                Some(ChunkAcceptanceDecisionMessage::rejected(
                    in_reply_to,
                    ChunkAcceptanceRejectionReason::TransferNotContinuable,
                ))
            }
            Ok(ChunkAcceptanceCommit::FailClosed) => {
                eprintln!(
                    "bamepd: discarded a contract-violating ChunkAcceptanceRequest (no durable \
                     mutation, no response)"
                );
                None
            }
            Err(_) => {
                // Internal persistence failure: nothing was durably
                // committed. No response — a fresh retry (fresh proof, fresh
                // handle) recovers idempotently via `already_committed`
                // (item 8).
                eprintln!(
                    "bamepd: a ChunkAcceptanceRequest durable commit failed internally; no \
                     response sent"
                );
                None
            }
        }
    }

    /// One received `ManifestSealRequest`. Returns `Some(decision)` for every
    /// authoritative outcome (`sealed`, `already_pending_verification`,
    /// `rejected`, `denied`); `None` means the request is discarded with no
    /// response — a structural violation, an internal invariant violation
    /// (Issue #39 Phase C2 items 8, 16), or a post-commit
    /// `verification_handle` mint failure (item 21). The
    /// `verification_handle` is minted **only after** the durable seal
    /// transaction has returned a committed outcome (item 20); a mint failure
    /// never emits a success decision with an unusable handle, and the
    /// durable `PendingVerification` commit is **not** rolled back — a fresh
    /// seal retry reaches `already_pending_verification` and mints a fresh
    /// handle.
    async fn handle_manifest_seal(
        operations: &TransientWorkerOperationStore,
        manifest_seal: &ManifestSealService,
        request: &ManifestSealRequestMessage,
    ) -> Option<ManifestSealDecisionMessage> {
        let in_reply_to = request.envelope.message_id;

        let commit = match manifest_seal
            .commit_manifest_seal(ManifestSealInput {
                token: request.body.token.clone(),
                transfer_id: request.body.transfer_id,
                proof_id: request.body.proof_id.clone(),
                issued_at_millis: request.body.issued_at,
                signature: request.body.signature.clone(),
                chunk_count: request.body.chunk_count,
                artifact_digest: request.body.artifact_digest.clone(),
            })
            .await
        {
            Ok(commit) => commit,
            Err(_) => {
                // Internal persistence failure: nothing was durably
                // committed. No response — a fresh retry recovers.
                eprintln!(
                    "bamepd: a ManifestSealRequest durable commit failed internally; no response \
                     sent"
                );
                return None;
            }
        };

        let (outcome_is_sealed, facts) = match commit {
            ManifestSealCommit::Sealed(facts) => (true, facts),
            ManifestSealCommit::AlreadyPending(facts) => (false, facts),
            ManifestSealCommit::RejectedIncomplete => {
                return Some(ManifestSealDecisionMessage::rejected(
                    in_reply_to,
                    ManifestSealRejectionReason::IncompleteManifest,
                ));
            }
            ManifestSealCommit::RejectedAlreadySealed => {
                return Some(ManifestSealDecisionMessage::rejected(
                    in_reply_to,
                    ManifestSealRejectionReason::ManifestAlreadySealed,
                ));
            }
            ManifestSealCommit::Denied => {
                return Some(ManifestSealDecisionMessage::denied(in_reply_to));
            }
            ManifestSealCommit::FailClosed => {
                eprintln!(
                    "bamepd: discarded a contract-violating or internally inconsistent \
                     ManifestSealRequest (no response)"
                );
                return None;
            }
        };

        // The durable `Incomplete -> PendingVerification` commit already
        // exists; only now mint the generation-scoped `verification_handle`
        // bound to the authoritative durable sealed identity (item 20). A mint
        // failure (store saturated, opaque-id collision, superseded
        // generation) must not yield a success decision carrying an unusable
        // handle — no response, fail closed, the Saturated/IdCollision/
        // StaleGeneration cause never surfaced (item 21).
        let verification_handle = match operations.mint_verification(VerificationBinding {
            transfer_id: bamep_domain::TransferId(request.body.transfer_id),
            artifact_id: facts.artifact_id,
            chunk_count: facts.chunk_count,
            expected_artifact_digest: facts.expected_artifact_digest.clone(),
            // The exact authorized `ManifestSealRequest` proof instance whose
            // durable result caused this handle to be minted — internal
            // operation-instance correlation metadata only, never echoed to
            // the Worker (`ManifestSealDecision` carries no `proof_id`), never
            // logged, never re-authorized on the follow-up (Issue #39 Phase C2
            // Correction B items 7, 12).
            proof_id: request.body.proof_id.clone(),
        }) {
            Ok(handle) => handle,
            Err(_) => {
                eprintln!(
                    "bamepd: a ManifestSealRequest committed durably but the verification_handle \
                     mint failed closed; no response sent (a fresh seal retry recovers)"
                );
                return None;
            }
        };

        let sealed_facts = SealedManifestFacts {
            verification_handle,
            artifact_id: facts.artifact_id.0,
            digest_algorithm: to_wire_digest_algorithm(facts.digest_algorithm),
            chunk_size: facts.chunk_size,
            chunk_count: facts.chunk_count,
            expected_artifact_digest: facts.expected_artifact_digest,
        };
        Some(if outcome_is_sealed {
            ManifestSealDecisionMessage::sealed(in_reply_to, sealed_facts)
        } else {
            ManifestSealDecisionMessage::already_pending_verification(in_reply_to, sealed_facts)
        })
    }

    /// One received `ArtifactVerificationReport`. Returns `Some(ack)` only
    /// when the durable `PendingVerification -> Verified | Failed` transition
    /// actually committed (both statuses are `committed` acks the Worker maps
    /// to HTTP `200`); `None` means the request is discarded with no
    /// response — an invalid/stale/consumed/wrong-kind `verification_handle`
    /// (item 32), a malformed reported digest, a durable/binding mismatch, or
    /// an internal persistence failure (items 26, 27, 33, 50). The consumed
    /// handle stays consumed regardless (item 26); a later logical retry
    /// starts from a fresh seal.
    async fn handle_artifact_verification(
        operations: &TransientWorkerOperationStore,
        artifact_verification: &ArtifactVerificationService,
        report: &ArtifactVerificationReportMessage,
    ) -> Option<ArtifactVerificationAckMessage> {
        let in_reply_to = report.envelope.message_id;

        // Consuming the `verification_handle` is the generation-scoped
        // linearization point. Unknown / stale-generation / already-consumed /
        // wrong-kind all fail closed with no response and no durable mutation
        // (item 32). The binding it returns is the authoritative correlation
        // target — the wire carries no Transfer/Artifact fields.
        let binding = match operations.consume_verification(&report.body.verification_handle) {
            Ok(binding) => binding,
            Err(_) => {
                eprintln!(
                    "bamepd: discarded an ArtifactVerificationReport presenting an invalid or \
                     stale verification_handle (no response, no durable mutation)"
                );
                return None;
            }
        };

        match artifact_verification
            .commit_artifact_verification(ArtifactVerificationInput {
                transfer_id: binding.transfer_id.0,
                artifact_id: binding.artifact_id.0,
                chunk_count: binding.chunk_count,
                // The consumed binding's full sealed identity is revalidated
                // against durable PostgreSQL state, including this last field
                // (Issue #39 Phase C2 Correction A). `binding.proof_id` is
                // internal correlation metadata only and is deliberately not
                // forwarded — the verification commit re-runs no proof
                // authorization.
                bound_expected_artifact_digest: binding.expected_artifact_digest.clone(),
                computed_artifact_digest: report.body.computed_artifact_digest.clone(),
            })
            .await
        {
            Ok(ArtifactVerificationCommit::Committed { verified }) => {
                let status = if verified {
                    WireArtifactStatus::Verified
                } else {
                    WireArtifactStatus::Failed
                };
                Some(ArtifactVerificationAckMessage::committed(
                    in_reply_to,
                    status,
                ))
            }
            Ok(ArtifactVerificationCommit::FailClosed) => {
                eprintln!(
                    "bamepd: discarded an ArtifactVerificationReport that could not be committed \
                     (malformed digest, or durable/binding mismatch); no response, no mutation"
                );
                None
            }
            Err(_) => {
                eprintln!(
                    "bamepd: an ArtifactVerificationReport durable commit failed internally; no \
                     response sent"
                );
                None
            }
        }
    }

    fn held_chunk_wire(entry: &HeldChunkEntry) -> HeldChunk {
        HeldChunk {
            chunk_index: entry.chunk_index,
            digest: entry.digest.clone(),
        }
    }

    /// One received `ResumeDiscoveryQuery`: authorize `resume_discovery` with
    /// the identical discipline as `AuthorizationQuery`, then return the first
    /// page of the consistent authorization-time durable snapshot. When the
    /// held-chunk set exceeds `page_size` the snapshot is materialized into
    /// this generation's process-local state and a `resume_cursor` is minted;
    /// otherwise the whole set ships in one page with no cursor
    /// (`m1-worker-data-plane-control-contract.md` "Resume-discovery
    /// authorization and first page", "Resume-manifest pagination"; Issue #39
    /// Phase C1).
    async fn handle_resume_discovery_query(
        operations: &TransientWorkerOperationStore,
        transfer_authorization: &TransferAuthorizationService,
        page_size: usize,
        query: &ResumeDiscoveryQueryMessage,
    ) -> ResumeDiscoveryPageMessage {
        let in_reply_to = query.envelope.message_id;
        let input = WorkerAuthorizationQueryInput {
            token: query.body.token.clone(),
            operation: bamep_domain::AuthorizationOperation::ResumeDiscovery,
            transfer_id: query.body.transfer_id,
            chunk_index: None,
            proof_id: query.body.proof_id.clone(),
            issued_at_millis: query.body.issued_at,
            signature: query.body.signature.clone(),
        };

        let snapshot = match transfer_authorization
            .authorize_resume_discovery(input)
            .await
        {
            Ok(ResumeAuthorizationOutcome::Approved(snapshot)) => snapshot,
            // A denied authorization and an internal backend failure fail
            // closed identically — the Worker never observes a more specific
            // outcome than `denied` (`m1` "Security and logging").
            Ok(ResumeAuthorizationOutcome::Denied) | Err(_) => {
                return ResumeDiscoveryPageMessage::denied(in_reply_to);
            }
        };

        let transfer_id = bamep_domain::TransferId(query.body.transfer_id);
        let algorithm = to_wire_digest_algorithm(snapshot.digest_algorithm);
        let held: Vec<HeldChunkEntry> = snapshot
            .held
            .iter()
            .map(|h| HeldChunkEntry {
                chunk_index: h.chunk_index,
                digest: h.digest_wire.clone(),
            })
            .collect();

        let page_end = page_size.min(held.len());
        let first_slice: Vec<HeldChunk> = held[..page_end].iter().map(held_chunk_wire).collect();
        let more_remain = page_end < held.len();

        if !more_remain {
            // The entire held-chunk set fits one page: no snapshot
            // registration, no cursor.
            return ResumeDiscoveryPageMessage::first_page(
                in_reply_to,
                transfer_id.0,
                snapshot.sealed,
                algorithm,
                snapshot.chunk_size,
                snapshot.expected_chunk_count,
                first_slice,
                None,
            );
        }

        let next_chunk_index = held[page_end - 1].chunk_index + 1;
        let stored = ResumeSnapshot {
            transfer_id,
            sealed: snapshot.sealed,
            digest_algorithm: snapshot.digest_algorithm,
            chunk_size: snapshot.chunk_size,
            expected_chunk_count: snapshot.expected_chunk_count,
            held,
        };

        // First cursor mint failure (snapshot registry saturated, opaque-id
        // collision, or a superseded generation) fails closed as the generic
        // `denied` — the Saturated/IdCollision cause is never exposed, and the
        // consumed replay state is NOT rolled back (Issue #39 Phase C1 item
        // 31); a fresh logical retry mints a fresh proof/snapshot/cursor.
        let snapshot_id = match operations.register_resume_snapshot(stored) {
            Ok(id) => id,
            Err(_) => return ResumeDiscoveryPageMessage::denied(in_reply_to),
        };
        let cursor = match operations.mint_resume_cursor(ResumeCursorBinding {
            transfer_id,
            // The authorizing `ResumeDiscoveryQuery` proof instance — success
            // above implies the authorization service already accepted it
            // structurally and cryptographically, so the Adapter re-parses
            // nothing. Every successor cursor preserves this exact value;
            // `ResumeDiscoveryContinue` carries no proof (Issue #39 Phase C2
            // Correction B items 8, 13).
            proof_id: query.body.proof_id.clone(),
            state: ResumeCursorState {
                snapshot_id,
                next_chunk_index,
            },
        }) {
            Ok(cursor) => cursor,
            Err(_) => {
                operations.drop_resume_snapshot(snapshot_id);
                return ResumeDiscoveryPageMessage::denied(in_reply_to);
            }
        };

        ResumeDiscoveryPageMessage::first_page(
            in_reply_to,
            transfer_id.0,
            snapshot.sealed,
            algorithm,
            snapshot.chunk_size,
            snapshot.expected_chunk_count,
            first_slice,
            Some(cursor),
        )
    }

    /// One received `ResumeDiscoveryContinue`: its authority is the
    /// current-generation `resume_cursor` from the already-authorized
    /// `ResumeDiscoveryQuery` — no fresh proof. A stale, wrong-generation,
    /// unknown, or already-consumed cursor returns `denied` and the Worker
    /// discards the aggregate (`m1-worker-data-plane-control-contract.md`
    /// "Resume-discovery pagination"; Issue #39 Phase C1 item 33). Never a
    /// durable mutation.
    fn handle_resume_discovery_continue(
        operations: &TransientWorkerOperationStore,
        page_size: usize,
        cont: &ResumeDiscoveryContinueMessage,
    ) -> ResumeDiscoveryPageMessage {
        let in_reply_to = cont.envelope.message_id;
        let cursor = cont.body.resume_cursor.as_str();

        let Some(binding) = operations.resume_cursor_binding(cursor) else {
            return ResumeDiscoveryPageMessage::denied(in_reply_to);
        };
        let ResumeCursorState {
            snapshot_id,
            next_chunk_index,
        } = binding.state;
        let Some(snapshot) = operations.resume_snapshot(snapshot_id) else {
            return ResumeDiscoveryPageMessage::denied(in_reply_to);
        };

        // The immutable snapshot's `held` is ascending by `chunk_index`; this
        // page is exactly the next ascending slice with no gap and no repeat.
        let start = snapshot
            .held
            .partition_point(|entry| entry.chunk_index < next_chunk_index);
        let end = (start + page_size).min(snapshot.held.len());
        let slice: Vec<HeldChunk> = snapshot.held[start..end]
            .iter()
            .map(held_chunk_wire)
            .collect();
        let more_remain = end < snapshot.held.len();

        if more_remain {
            let new_next = snapshot.held[end - 1].chunk_index + 1;
            // Advance atomically (Phase B `advance_resume_cursor`: the
            // successor is minted before the current cursor is removed, so a
            // collision leaves the current cursor intact — no gap, no
            // duplicate). The page is only constructed *after* the successor
            // materializes, so the response never carries a cursor that
            // failed to mint (item 35).
            match operations.advance_resume_cursor(
                cursor,
                binding.transfer_id,
                Some(ResumeCursorState {
                    snapshot_id,
                    next_chunk_index: new_next,
                }),
            ) {
                Ok(Some(next_cursor)) => ResumeDiscoveryPageMessage::continuation_page(
                    in_reply_to,
                    slice,
                    Some(next_cursor),
                ),
                Ok(None) | Err(_) => {
                    operations.drop_resume_snapshot(snapshot_id);
                    ResumeDiscoveryPageMessage::denied(in_reply_to)
                }
            }
        } else {
            // Final page: consume the current cursor, mint no successor, and
            // release the snapshot (item 36).
            match operations.advance_resume_cursor(cursor, binding.transfer_id, None) {
                Ok(None) => {
                    operations.drop_resume_snapshot(snapshot_id);
                    ResumeDiscoveryPageMessage::continuation_page(in_reply_to, slice, None)
                }
                _ => {
                    operations.drop_resume_snapshot(snapshot_id);
                    ResumeDiscoveryPageMessage::denied(in_reply_to)
                }
            }
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

        /// Issue #39 Phase C1 item 39: the largest `ResumeDiscoveryPage` the
        /// C1 pagination logic can produce — a full first page of
        /// [`RESUME_PAGE_MAX_HELD_CHUNKS`] worst-case `HeldChunk` entries
        /// (10-digit `chunk_index`, 43-char `digest`), `sealed` so
        /// `expected_chunk_count` is also carried, plus an opaque
        /// `resume_cursor` — must encode strictly below the universal 1 MiB
        /// frame limit through the *real* Worker Protocol codec, never by
        /// prose arithmetic. The limit is never raised to fit a page.
        #[test]
        fn resume_page_frame_stays_within_the_1_mib_limit() {
            let held: Vec<HeldChunk> = (0..RESUME_PAGE_MAX_HELD_CHUNKS)
                .map(|_| HeldChunk {
                    chunk_index: u64::from(u32::MAX),
                    digest: "A".repeat(43),
                })
                .collect();
            let page = ResumeDiscoveryPageMessage::first_page(
                Uuid::new_v4(),
                Uuid::new_v4(),
                true,
                WireDigestAlgorithm::Sha256,
                u32::MAX,
                Some(u64::from(u32::MAX)),
                held,
                Some(format!("res_{}", "f".repeat(32))),
            );
            let encoded =
                bamep_worker_protocol::encode(&WorkerProtocolMessage::ResumeDiscoveryPage(page))
                    .expect("encode");
            assert!(
                encoded.len() < bamep_worker_protocol::MAX_FRAME_PAYLOAD_BYTES as usize,
                "worst-case resume page is {} bytes, must be < 1 MiB ({})",
                encoded.len(),
                bamep_worker_protocol::MAX_FRAME_PAYLOAD_BYTES
            );
            // And with comfortable headroom (>= 10%) so a future minor field
            // addition does not silently cross the limit.
            assert!(
                encoded.len() < (bamep_worker_protocol::MAX_FRAME_PAYLOAD_BYTES as usize) * 9 / 10,
                "worst-case resume page {} bytes leaves < 10% headroom under 1 MiB",
                encoded.len()
            );
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

        pub fn with_resume_page_size(self, _page_size: usize) -> Self {
            self
        }

        #[allow(clippy::too_many_arguments)]
        pub async fn run(
            self,
            _registry: Arc<WorkerAuthorityRegistry>,
            _transfer_authorization: Arc<crate::application::TransferAuthorizationService>,
            _chunk_acceptance: Arc<crate::application::ChunkAcceptanceService>,
            _manifest_seal: Arc<crate::application::ManifestSealService>,
            _artifact_verification: Arc<crate::application::ArtifactVerificationService>,
            _shutdown: watch::Receiver<bool>,
        ) -> Result<(), WorkerControlPlaneError> {
            Ok(())
        }
    }
}

pub use imp::WorkerControlPlane;
