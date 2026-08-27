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
use super::authorization_client::PendingQuery;

#[cfg(unix)]
mod imp {
    use bamep_worker_protocol::{receive, send, WorkerHelloMessage, WorkerProtocolMessage};
    use tokio::net::UnixStream;
    use tokio::sync::{mpsc, watch};

    use super::*;

    /// Bounded so a runaway caller cannot queue unbounded outstanding
    /// queries against one connection generation.
    const PENDING_QUERY_CHANNEL_CAPACITY: usize = 32;

    #[derive(Debug, thiserror::Error)]
    enum HandshakeError {
        #[error("transport error sending WorkerHello")]
        Send(#[from] bamep_worker_protocol::SendError),
        #[error("transport error awaiting bamepd's handshake response")]
        Receive(#[from] bamep_worker_protocol::ReceiveError),
        #[error("bamepd rejected the handshake as incompatible")]
        Rejected,
        #[error("bamepd's ServerHello failed normative envelope/field/correlation validation")]
        InvalidServerHello,
        #[error("bamepd's HandshakeRejected failed normative envelope/correlation validation")]
        InvalidHandshakeRejected,
        #[error("bamepd sent an unexpected message before the handshake completed")]
        UnexpectedMessage,
    }

    /// Every field this Worker requires from a received `ServerHello`/
    /// `HandshakeRejected` is validated here — envelope `protocol_version`/
    /// `message_id`, `server_protocol_version`, `compatible`, and
    /// `in_reply_to` correlation to the `WorkerHello` this Worker sent
    /// (`m1-worker-data-plane-control-contract.md` "Handshake"). Worker must
    /// never enter `Ready` on a malformed or uncorrelated response, even one
    /// that superficially resembles success.
    async fn perform_handshake(
        mut stream: UnixStream,
        worker_instance_id: Uuid,
    ) -> Result<UnixStream, HandshakeError> {
        let hello = WorkerHelloMessage::new(worker_instance_id);
        let sent_id = hello.envelope.message_id;
        send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello)).await?;

        match receive(&mut stream).await? {
            WorkerProtocolMessage::ServerHello(response) => {
                if !response.is_valid_reply_to(sent_id) {
                    return Err(HandshakeError::InvalidServerHello);
                }
                Ok(stream)
            }
            WorkerProtocolMessage::HandshakeRejected(response) => {
                if !response.is_valid_reply_to(sent_id) {
                    return Err(HandshakeError::InvalidHandshakeRejected);
                }
                Err(HandshakeError::Rejected)
            }
            _ => Err(HandshakeError::UnexpectedMessage),
        }
    }

    /// One connect+handshake+connection lifetime. Returns once authority is
    /// lost (connect failure, rejected/malformed handshake, or the
    /// connection ending) so the caller's reconnect loop can apply its
    /// bounded delay. `publisher` announces this generation's fresh
    /// request channel the instant the handshake succeeds, and clears it
    /// back to `None` before returning — no caller can ever observe a
    /// request channel for a generation that has already ended (Issue #38
    /// "Generation-scoped UDS request/response routing").
    async fn run_one_connection(
        uds_path: &std::path::Path,
        worker_instance_id: Uuid,
        tracker: &AuthorityTracker,
        publisher: &watch::Sender<Option<mpsc::Sender<PendingQuery>>>,
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
        let (request_tx, mut request_rx) = mpsc::channel(PENDING_QUERY_CHANNEL_CAPACITY);
        let _ = publisher.send(Some(request_tx));

        // This connection task is the sole owner of `stream`'s serialized
        // I/O for the rest of this generation (Issue #38 "Connection task
        // ownership"): every send and every matching receive happens here,
        // sequentially, one outstanding query at a time. `bamepd` never
        // sends anything unsolicited on this boundary
        // (`m1-worker-data-plane-control-contract.md`), so a frame observed
        // while no query is outstanding is itself already a protocol
        // violation/disconnect signal, exactly like Issue #37's original
        // idle-read behavior below.
        loop {
            tokio::select! {
                pending = request_rx.recv() => {
                    let Some(pending) = pending else {
                        // No caller can hold a `Sender` once `publisher`
                        // stops advertising one for this generation; treated
                        // defensively as connection end.
                        break;
                    };
                    let sent_id = pending.message.envelope.message_id;
                    if send(&mut stream, &WorkerProtocolMessage::AuthorizationQuery(pending.message))
                        .await
                        .is_err()
                    {
                        // `pending.reply` is dropped here, surfacing
                        // `QueryError::Disconnected` to the caller — never a
                        // fabricated decision.
                        break;
                    }
                    match receive(&mut stream).await {
                        Ok(WorkerProtocolMessage::AuthorizationDecision(decision))
                            if decision.is_reply_to(sent_id) =>
                        {
                            let _ = pending.reply.send(decision);
                        }
                        // Anything else — a stale/uncorrelated response, an
                        // unexpected message type, or a transport error —
                        // cannot be trusted as this query's answer. Drop
                        // `pending.reply` (surfacing `Disconnected`) and end
                        // this generation rather than guessing.
                        _ => break,
                    }
                }
                idle = receive(&mut stream) => {
                    let _ = idle;
                    break;
                }
            }
        }

        let _ = publisher.send(None);
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
        publisher: watch::Sender<Option<mpsc::Sender<PendingQuery>>>,
    ) -> std::convert::Infallible {
        loop {
            run_one_connection(&uds_path, worker_instance_id, &tracker, &publisher).await;
            tokio::time::sleep(reconnect_delay).await;
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use tokio::sync::{mpsc, watch};

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
        publisher: watch::Sender<Option<mpsc::Sender<PendingQuery>>>,
    ) -> std::convert::Infallible {
        tracker.set_disconnected();
        let _ = publisher.send(None);
        std::future::pending().await
    }
}

pub use imp::run_client_loop;
