//! Outbound authenticated-session delivery directory (Runtime Service)
//! (`m0-stack-and-boundaries-baseline.md` "Runtime Services"; Issue #26
//! "Outbound authenticated session delivery").
//!
//! Deliberately separate from [`super::presence::PresenceRegistry`]: presence
//! answers "is at least one authenticated session alive for this Endpoint",
//! while this directory additionally owns *which exact session* currently
//! receives outbound `ActionDispatch` traffic and how to reach it. One
//! authenticated-session lifecycle registers/unregisters the same exact
//! `SessionId` in both registries, but neither collapses into the other.
//!
//! This directory never touches a WebSocket/TLS type directly — it only
//! holds a bounded [`tokio::sync::mpsc`] command channel per session.
//! `bamep_server::adapters::agent_gateway::AgentControlGateway`'s
//! authenticated-session task is the sole owner of the actual socket and the
//! only place that ever calls `.send()` on it; this directory only enqueues
//! [`OutboundCommand`]s onto that task's channel and awaits its completion
//! signal.
//!
//! Memory-only — never persisted, exactly like [`super::presence::PresenceRegistry`].

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use bamep_agent_protocol::{ActionDispatchMessage, AgentProtocolMessage, ProtocolId};
use bamep_domain::EndpointId;
use tokio::sync::{mpsc, oneshot};

use crate::ports::{AgentDispatchError, AgentDispatchPort};

/// One outbound instruction the authenticated-session task executes against
/// its own owned socket. `ack` carries only whether the local WebSocket
/// `sink.send` succeeded — never Agent receipt/execution
/// (`m0-job-lifecycle-and-scheduling.md`; Issue #26 "Outbound authenticated
/// session delivery").
pub enum OutboundCommand {
    Send {
        message: AgentProtocolMessage,
        ack: oneshot::Sender<Result<(), ()>>,
    },
}

pub type OutboundSender = mpsc::Sender<OutboundCommand>;
pub type OutboundReceiver = mpsc::Receiver<OutboundCommand>;

/// Bounded outbound command-queue capacity for one authenticated session.
/// #26's normal execution path sends at most one `ActionDispatch` per
/// Attempt; a small bound is sufficient and keeps a stalled Agent from
/// allowing unbounded memory growth.
const OUTBOUND_CHANNEL_CAPACITY: usize = 32;

/// Constructs the bounded outbound command channel one authenticated session
/// owns for the lifetime of its connection.
pub fn outbound_channel() -> (OutboundSender, OutboundReceiver) {
    mpsc::channel(OUTBOUND_CHANNEL_CAPACITY)
}

#[derive(Default)]
struct Inner {
    /// The most-recently-registered live `SessionId` per Endpoint — exactly
    /// the session `dispatch_action` selects
    /// (`m0-job-lifecycle-and-scheduling.md`; Issue #26: "choose the most
    /// recently registered authenticated session").
    most_recent_session: HashMap<EndpointId, ProtocolId>,
    channels: HashMap<ProtocolId, OutboundSender>,
}

/// The transient authenticated-session delivery directory. See module docs.
#[derive(Default)]
pub struct OutboundSessionDirectory {
    inner: Mutex<Inner>,
}

impl OutboundSessionDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `session_id`'s outbound sender for `endpoint_id`, and makes
    /// it the Endpoint's most-recently-registered live session. Idempotent
    /// for the exact same `(endpoint_id, session_id, sender)` triple;
    /// registering a second session for the same Endpoint simply replaces
    /// which session is currently selected — the older session's own
    /// channel entry remains until its own `unregister` call, so it can
    /// still be explicitly unregistered later, but it is never selected by
    /// [`AgentDispatchPort::dispatch_action`] again once superseded.
    pub fn register(
        &self,
        endpoint_id: EndpointId,
        session_id: ProtocolId,
        sender: OutboundSender,
    ) {
        let mut inner = self
            .inner
            .lock()
            .expect("outbound session directory lock poisoned");
        inner.channels.insert(session_id, sender);
        inner.most_recent_session.insert(endpoint_id, session_id);
    }

    /// Unregisters exactly `session_id`'s channel. The Endpoint's
    /// most-recent-session pointer is cleared only when it still points at
    /// this exact `session_id` — an older, already-superseded session's exit
    /// must never erase a newer session's registration
    /// (`m0-job-lifecycle-and-scheduling.md`; Issue #26: "no fallback send
    /// through another overlapping session after one send attempt").
    /// Idempotent: unregistering an unknown/already-removed `session_id` is
    /// a safe no-op.
    pub fn unregister(&self, endpoint_id: EndpointId, session_id: ProtocolId) {
        let mut inner = self
            .inner
            .lock()
            .expect("outbound session directory lock poisoned");
        inner.channels.remove(&session_id);
        if inner.most_recent_session.get(&endpoint_id) == Some(&session_id) {
            inner.most_recent_session.remove(&endpoint_id);
        }
    }
}

#[async_trait]
impl AgentDispatchPort for OutboundSessionDirectory {
    async fn dispatch_action(
        &self,
        endpoint_id: EndpointId,
        dispatch: ActionDispatchMessage,
    ) -> Result<(), AgentDispatchError> {
        let sender = {
            let inner = self
                .inner
                .lock()
                .expect("outbound session directory lock poisoned");
            let session_id = inner
                .most_recent_session
                .get(&endpoint_id)
                .copied()
                .ok_or(AgentDispatchError::NoSession)?;
            inner
                .channels
                .get(&session_id)
                .cloned()
                .ok_or(AgentDispatchError::NoSession)?
        };

        let (ack_tx, ack_rx) = oneshot::channel();
        let command = OutboundCommand::Send {
            message: AgentProtocolMessage::ActionDispatch(dispatch),
            ack: ack_tx,
        };
        if sender.send(command).await.is_err() {
            return Err(AgentDispatchError::ChannelClosed);
        }
        match ack_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(())) => Err(AgentDispatchError::SendFailed),
            Err(_) => Err(AgentDispatchError::ChannelClosed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn endpoint() -> EndpointId {
        EndpointId::new()
    }

    fn dispatch() -> ActionDispatchMessage {
        ActionDispatchMessage::new(
            ProtocolId::generate(),
            "bamep.m1.simulated-execution",
            "1",
            serde_json::Map::new(),
        )
    }

    #[tokio::test]
    async fn dispatch_with_no_registered_session_returns_no_session() {
        let directory = OutboundSessionDirectory::new();
        let result = directory.dispatch_action(endpoint(), dispatch()).await;
        assert_eq!(result, Err(AgentDispatchError::NoSession));
    }

    #[tokio::test]
    async fn dispatch_selects_the_most_recently_registered_session_never_fanning_out() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let (tx_b, mut rx_b) = outbound_channel();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        directory.register(endpoint_id, session_b, tx_b);

        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });

        let OutboundCommand::Send { ack, .. } = rx_b
            .recv()
            .await
            .expect("session B (most recently registered) must receive the command");
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert!(
            rx_a.try_recv().is_err(),
            "session A must never receive a fanned-out copy"
        );
    }

    #[tokio::test]
    async fn unregistering_an_older_session_does_not_erase_a_newer_registration() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, _rx_a) = outbound_channel();
        let (tx_b, mut rx_b) = outbound_channel();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        directory.register(endpoint_id, session_b, tx_b);

        // Session A exits after being superseded by B.
        directory.unregister(endpoint_id, session_a);

        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_b.recv().await.expect("B must still receive");
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unregistering_the_current_session_leaves_the_endpoint_with_no_session() {
        let directory = OutboundSessionDirectory::new();
        let endpoint_id = endpoint();
        let (tx, _rx) = outbound_channel();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, tx);
        directory.unregister(endpoint_id, session_id);

        let result = directory.dispatch_action(endpoint_id, dispatch()).await;
        assert_eq!(result, Err(AgentDispatchError::NoSession));
    }

    #[tokio::test]
    async fn a_dropped_receiver_surfaces_as_channel_closed() {
        let directory = OutboundSessionDirectory::new();
        let endpoint_id = endpoint();
        let (tx, rx) = outbound_channel();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, tx);
        drop(rx);

        let result = directory.dispatch_action(endpoint_id, dispatch()).await;
        assert_eq!(result, Err(AgentDispatchError::ChannelClosed));
    }

    #[tokio::test]
    async fn a_local_send_failure_reported_through_the_ack_surfaces_as_send_failed() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx, mut rx) = outbound_channel();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, tx);

        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });
        let OutboundCommand::Send { ack, .. } = rx.recv().await.unwrap();
        ack.send(Err(())).unwrap();
        let result = send_task.await.unwrap();
        assert_eq!(result, Err(AgentDispatchError::SendFailed));
    }
}
