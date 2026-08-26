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

use crate::runtime::worker_authority::WorkerAuthorityRegistry;

#[derive(Debug, thiserror::Error)]
pub enum WorkerControlPlaneError {
    #[error("failed to prepare the UDS socket directory at {path}")]
    PrepareDirectory {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("refusing to remove non-socket path {path}: an unrelated file already exists there")]
    RefusingToRemoveNonSocket { path: String },
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
    #[error("Unix Domain Sockets are not supported on this platform")]
    UnsupportedPlatform,
}

#[cfg(unix)]
mod imp {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use bamep_worker_protocol::{
        receive, send, HandshakeRejectedMessage, ProtocolErrorMessage, ReceiveError,
        ServerHelloMessage, WorkerProtocolMessage,
    };
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::watch;
    use uuid::Uuid;

    use super::*;

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
    }

    pub struct WorkerControlPlane {
        listener: UnixListener,
        socket_path: PathBuf,
    }

    impl WorkerControlPlane {
        /// Binds the UDS listener at `path`
        /// (`m1-worker-data-plane-control-contract.md` "UDS filesystem
        /// security"). Creates the parent directory if missing (owner-only
        /// `0700`). If a path already exists there, it is removed only
        /// after confirming it is actually a socket — never an arbitrary
        /// pre-existing file (Issue #37 "UDS filesystem security"). The
        /// freshly bound socket is restricted to owner-only access
        /// (`0600`).
        pub fn bind(path: &Path) -> Result<Self, WorkerControlPlaneError> {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    std::fs::create_dir_all(parent).map_err(|source| {
                        WorkerControlPlaneError::PrepareDirectory {
                            path: parent.display().to_string(),
                            source,
                        }
                    })?;
                    set_permissions(parent, 0o700).map_err(|source| {
                        WorkerControlPlaneError::PrepareDirectory {
                            path: parent.display().to_string(),
                            source,
                        }
                    })?;
                }
            }

            match std::fs::symlink_metadata(path) {
                Ok(metadata) => {
                    if metadata.file_type().is_socket() {
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

            Ok(Self {
                listener,
                socket_path: path.to_path_buf(),
            })
        }

        /// Runs the accept loop until `shutdown` becomes `true`: accepts
        /// each connection, hands it to [`handle_connection`], and spawns
        /// that as its own task so one slow/misbehaving connection never
        /// blocks accepting the next. Removes the socket file on return
        /// (controlled shutdown; Issue #37 "Controlled shutdown").
        pub async fn run(
            self,
            registry: Arc<WorkerAuthorityRegistry>,
            mut shutdown: watch::Receiver<bool>,
        ) {
            loop {
                tokio::select! {
                    accepted = self.listener.accept() => {
                        if let Ok((stream, _addr)) = accepted {
                            tokio::spawn(handle_connection(stream, Arc::clone(&registry)));
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }

    async fn handle_connection(mut stream: UnixStream, registry: Arc<WorkerAuthorityRegistry>) {
        let worker_instance_id = match handshake_as_server(&mut stream).await {
            Ok(id) => id,
            Err(_) => return,
        };

        let generation = registry.begin_generation(worker_instance_id);

        // Issue #37 defines no post-handshake business message `bamepd`
        // consumes yet; any received message is unexpected. Detecting
        // that, an I/O error, or EOF here and invalidating the generation
        // immediately is exactly the fail-closed behavior
        // (`m1-worker-data-plane-control-contract.md` "IPC loss is
        // fail-closed").
        if let Ok(_unexpected) = receive(&mut stream).await {
            let _ = send(
                &mut stream,
                &WorkerProtocolMessage::ProtocolError(ProtocolErrorMessage::new(
                    "unexpected_message",
                )),
            )
            .await;
        }

        registry.end_generation(generation);
    }

    async fn handshake_as_server(stream: &mut UnixStream) -> Result<Uuid, HandshakeError> {
        let first = receive(stream).await?;
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

    fn set_permissions(path: &Path, mode: u32) -> std::io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
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
        ) {
        }
    }
}

pub use imp::WorkerControlPlane;
