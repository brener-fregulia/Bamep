//! Worker's reconnecting UDS client loop (Issue #37 "Worker reconnect"):
//! connect, complete the handshake, then block until the connection ends,
//! updating [`AuthorityTracker`] at every step so authority is fail-closed
//! the instant the connection is lost — never on a delay, never inferred.
//!
//! Unix Domain Sockets are Unix-only; the real implementation lives behind
//! `#[cfg(unix)]`. On other platforms this module compiles to a stub that
//! never becomes available, per the WP's narrow non-Unix portability
//! boundary — no fake TCP/localhost substitute is introduced.

use std::path::PathBuf;
use std::time::Duration;

use uuid::Uuid;

use super::authority::AuthorityTracker;

#[cfg(unix)]
mod imp {
    use bamep_worker_protocol::{receive, send, WorkerHelloMessage, WorkerProtocolMessage};
    use tokio::net::UnixStream;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum HandshakeError {
        #[error("transport error sending WorkerHello")]
        Send(#[from] bamep_worker_protocol::SendError),
        #[error("transport error awaiting bamepd's handshake response")]
        Receive(#[from] bamep_worker_protocol::ReceiveError),
        #[error("bamepd rejected the handshake as incompatible")]
        Rejected,
        #[error("bamepd's handshake response did not correlate to this WorkerHello")]
        Uncorrelated,
        #[error("bamepd sent an unexpected message before the handshake completed")]
        UnexpectedMessage,
    }

    async fn perform_handshake(
        mut stream: UnixStream,
        worker_instance_id: Uuid,
    ) -> Result<UnixStream, HandshakeError> {
        let hello = WorkerHelloMessage::new(worker_instance_id);
        let sent_id = hello.envelope.message_id;
        send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello)).await?;

        match receive(&mut stream).await? {
            WorkerProtocolMessage::ServerHello(response) => {
                if response.body.in_reply_to != sent_id {
                    return Err(HandshakeError::Uncorrelated);
                }
                if !response.body.compatible {
                    return Err(HandshakeError::Rejected);
                }
                Ok(stream)
            }
            WorkerProtocolMessage::HandshakeRejected(response) => {
                if response.body.in_reply_to != sent_id {
                    return Err(HandshakeError::Uncorrelated);
                }
                Err(HandshakeError::Rejected)
            }
            _ => Err(HandshakeError::UnexpectedMessage),
        }
    }

    /// One connect+handshake+connection lifetime. Returns once authority is
    /// lost (connect failure, rejected/malformed handshake, or the
    /// connection ending) so the caller's reconnect loop can apply its
    /// bounded delay.
    async fn run_one_connection(
        uds_path: &std::path::Path,
        worker_instance_id: Uuid,
        tracker: &AuthorityTracker,
    ) {
        tracker.set_connecting();
        let stream = match UnixStream::connect(uds_path).await {
            Ok(stream) => stream,
            Err(_) => {
                tracker.set_disconnected();
                return;
            }
        };

        tracker.set_handshaking();
        let mut stream = match perform_handshake(stream, worker_instance_id).await {
            Ok(stream) => stream,
            Err(_) => {
                tracker.set_disconnected();
                return;
            }
        };

        tracker.set_ready();

        // Issue #37 defines no post-handshake business message Worker
        // consumes yet; block here so any disconnect (EOF, I/O error, or a
        // malformed/unexpected message from bamepd) is observed immediately
        // and authority becomes unavailable without delay
        // (`m1-worker-data-plane-control-contract.md` "IPC loss is
        // fail-closed").
        let _ = receive(&mut stream).await;
        tracker.set_disconnected();
    }

    /// Runs forever: connect, handshake, wait for disconnect, sleep the
    /// bounded reconnect delay, repeat. Never busy-spins — every iteration
    /// either blocks on I/O or sleeps.
    pub async fn run_client_loop(
        uds_path: PathBuf,
        reconnect_delay: Duration,
        worker_instance_id: Uuid,
        tracker: AuthorityTracker,
    ) -> std::convert::Infallible {
        loop {
            run_one_connection(&uds_path, worker_instance_id, &tracker).await;
            tokio::time::sleep(reconnect_delay).await;
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    /// Unix Domain Sockets are not available on this platform. Linux is the
    /// Bamep Worker reference/production environment
    /// (`docs/development/testing.md`); this stub only keeps the crate
    /// portable/compilable elsewhere and never becomes available — no fake
    /// TCP/localhost IPC substitute is introduced.
    pub async fn run_client_loop(
        _uds_path: PathBuf,
        _reconnect_delay: Duration,
        _worker_instance_id: Uuid,
        tracker: AuthorityTracker,
    ) -> std::convert::Infallible {
        tracker.set_disconnected();
        std::future::pending().await
    }
}

pub use imp::run_client_loop;
