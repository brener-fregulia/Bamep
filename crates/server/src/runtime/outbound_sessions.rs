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
use bamep_agent_protocol::{
    ActionDispatchMessage, AgentProtocolMessage, CancelActionMessage, ProtocolId,
    StatusQueryMessage,
};
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
    /// Every currently-live (registered, not yet unregistered) `SessionId`
    /// per Endpoint, oldest to newest — the last entry is exactly the session
    /// `dispatch_action` selects (`m0-job-lifecycle-and-scheduling.md`; Issue
    /// #26: "choose the most recently registered authenticated session").
    /// Kept as an ordered list, not a single pointer, so that when the
    /// currently-selected newest session unregisters, an older session that
    /// is still live becomes selectable again — this is not fallback after a
    /// send attempt, only correct selection before one ever begins (Issue
    /// #26 correction "Fix overlapping live-session selection").
    live_sessions: HashMap<EndpointId, Vec<ProtocolId>>,
    channels: HashMap<ProtocolId, OutboundSender>,
    /// The `SessionId` most recently resolved to actually carry outbound
    /// Agent Protocol traffic (`ActionDispatch`/`CancelAction`/`StatusQuery`)
    /// for each Endpoint (Issue #28 corrective pass "Session-loss
    /// reconciliation with overlapping sessions"). Deliberately distinct from
    /// `live_sessions`' "currently selected" notion: that notion changes the
    /// instant a newer session registers, which races a concurrent
    /// reconnect against an older session's own disconnect handling. This
    /// map instead reflects a fact fixed at send time, so a disconnecting
    /// session can reliably tell whether *it* was the one actually used for
    /// this Endpoint's most recent outbound send, immune to that race.
    /// Cleared for an Endpoint only when its last live session unregisters —
    /// mirrors `live_sessions`' own cleanup, never persisted.
    last_sent_session: HashMap<EndpointId, ProtocolId>,
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

    /// Registers `session_id`'s outbound sender for `endpoint_id`, and marks
    /// it the newest currently-live session — the one
    /// [`AgentDispatchPort::dispatch_action`] selects. Idempotent for the
    /// exact same `(endpoint_id, session_id, sender)` triple; registering a
    /// second session for the same Endpoint simply changes which session is
    /// currently selected — an older session's own channel entry remains
    /// until its own `unregister` call, so it can still be explicitly
    /// unregistered later. If that older session is still live when the
    /// newer one unregisters, it becomes selectable again (Issue #26
    /// correction "Fix overlapping live-session selection") — this is
    /// distinct from ever falling back to it *after* a send attempt has
    /// begun through the newer one, which never happens.
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
        let sessions = inner.live_sessions.entry(endpoint_id).or_default();
        if !sessions.contains(&session_id) {
            sessions.push(session_id);
        }
    }

    /// Unregisters exactly `session_id`'s channel and removes it from
    /// `endpoint_id`'s live-session list, wherever in that list it currently
    /// sits — an older, already-superseded session's exit must never erase a
    /// newer session's registration, and a newer session's exit must reveal
    /// whichever older session is still live, if any
    /// (`m0-job-lifecycle-and-scheduling.md`; Issue #26: "no fallback send
    /// through another overlapping session after one send attempt" — a rule
    /// about not retrying a send that already began, not about which
    /// still-live session is selectable before one does). Idempotent:
    /// unregistering an unknown/already-removed `session_id` is a safe
    /// no-op.
    pub fn unregister(&self, endpoint_id: EndpointId, session_id: ProtocolId) {
        let mut inner = self
            .inner
            .lock()
            .expect("outbound session directory lock poisoned");
        inner.channels.remove(&session_id);
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            inner.live_sessions.entry(endpoint_id)
        {
            entry.get_mut().retain(|s| *s != session_id);
            if entry.get().is_empty() {
                entry.remove();
                // No live session remains for this Endpoint — the "most
                // recently sent through" fact is no longer meaningful; a
                // future session starts with a clean slate rather than
                // inheriting a stale binding from a since-departed session.
                inner.last_sent_session.remove(&endpoint_id);
            }
        }
    }

    /// Reports whether `session_id` was the session most recently resolved
    /// to carry outbound Agent Protocol traffic for `endpoint_id` (Issue #28
    /// corrective pass) — the transient, Runtime-only, Endpoint-scoped
    /// correlation a disconnecting session uses to decide whether it was
    /// dispatch-relevant for reconciliation purposes. Endpoint-scoped, not
    /// Attempt-scoped: a Job admits only one active Attempt per Endpoint at
    /// a time, so this fact is equivalent and simpler; never durable, never
    /// used to fall back or resend. `false` when no outbound send has ever
    /// resolved a session for this Endpoint, or the resolved session does
    /// not match.
    pub fn is_dispatch_session(&self, endpoint_id: EndpointId, session_id: ProtocolId) -> bool {
        let inner = self
            .inner
            .lock()
            .expect("outbound session directory lock poisoned");
        inner.last_sent_session.get(&endpoint_id) == Some(&session_id)
    }
}

impl OutboundSessionDirectory {
    /// Shared send path for [`AgentDispatchPort::dispatch_action`] and
    /// [`AgentDispatchPort::cancel_action`]: selects the most-recently-
    /// registered live session for `endpoint_id` (no fan-out, no fallback
    /// after one send attempt) and enqueues `message` onto its outbound
    /// channel.
    async fn send(
        &self,
        endpoint_id: EndpointId,
        message: AgentProtocolMessage,
    ) -> Result<(), AgentDispatchError> {
        let sender = {
            let mut inner = self
                .inner
                .lock()
                .expect("outbound session directory lock poisoned");
            let session_id = inner
                .live_sessions
                .get(&endpoint_id)
                .and_then(|sessions| sessions.last())
                .copied()
                .ok_or(AgentDispatchError::NoSession)?;
            // Fixes the dispatch-relevant session for this Endpoint at the
            // moment of resolution — see `last_sent_session`'s docs. Recorded
            // regardless of whether the local send below ultimately
            // succeeds: resolving to `session_id` at all is itself the fact
            // that matters, mirroring `dispatch_action`'s own "the local
            // transport accepted the frame" trust boundary.
            inner.last_sent_session.insert(endpoint_id, session_id);
            inner
                .channels
                .get(&session_id)
                .cloned()
                .ok_or(AgentDispatchError::NoSession)?
        };

        let (ack_tx, ack_rx) = oneshot::channel();
        let command = OutboundCommand::Send {
            message,
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

#[async_trait]
impl AgentDispatchPort for OutboundSessionDirectory {
    async fn dispatch_action(
        &self,
        endpoint_id: EndpointId,
        dispatch: ActionDispatchMessage,
    ) -> Result<(), AgentDispatchError> {
        self.send(endpoint_id, AgentProtocolMessage::ActionDispatch(dispatch))
            .await
    }

    async fn cancel_action(
        &self,
        endpoint_id: EndpointId,
        cancel: CancelActionMessage,
    ) -> Result<(), AgentDispatchError> {
        self.send(endpoint_id, AgentProtocolMessage::CancelAction(cancel))
            .await
    }

    async fn status_query(
        &self,
        endpoint_id: EndpointId,
        query: StatusQueryMessage,
    ) -> Result<(), AgentDispatchError> {
        self.send(endpoint_id, AgentProtocolMessage::StatusQuery(query))
            .await
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
    async fn newer_session_unregistering_reveals_a_still_live_older_session() {
        // B is selected while A and B are both live; B disconnects before
        // any dispatch was attempted through it — A must become selectable
        // again. This is not fallback (no send through B was ever
        // attempted), only correct selection among currently-live sessions.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let (tx_b, _rx_b) = outbound_channel();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        directory.register(endpoint_id, session_b, tx_b);

        directory.unregister(endpoint_id, session_b);

        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a
            .recv()
            .await
            .expect("A (still live, now the newest remaining session) must receive the command");
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn older_session_unregistering_while_newer_remains_live_never_disturbs_the_newer() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, _rx_a) = outbound_channel();
        let (tx_b, mut rx_b) = outbound_channel();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        directory.register(endpoint_id, session_b, tx_b);

        directory.unregister(endpoint_id, session_a);

        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_b
            .recv()
            .await
            .expect("B must remain selected, undisturbed by A's unrelated exit");
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

    // -- Issue #28 corrective pass: is_dispatch_session -------------------

    #[tokio::test]
    async fn no_send_has_ever_happened_is_never_a_dispatch_session() {
        let directory = OutboundSessionDirectory::new();
        let endpoint_id = endpoint();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, outbound_channel().0);
        assert!(!directory.is_dispatch_session(endpoint_id, session_id));
    }

    #[tokio::test]
    async fn a_successful_send_binds_the_resolved_session_as_the_dispatch_session() {
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
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert!(directory.is_dispatch_session(endpoint_id, session_id));
    }

    #[tokio::test]
    async fn a_newer_session_registering_alone_never_changes_the_dispatch_session_binding() {
        // Registration alone — no send — must not disturb which session is
        // considered dispatch-relevant; only an actual resolved send does
        // (Issue #28 corrective pass "Session-loss reconciliation with
        // overlapping sessions": this is what makes a disconnecting older
        // session's own check immune to a concurrent newer registration).
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let (tx_b, _rx_b) = outbound_channel();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);

        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
        assert!(directory.is_dispatch_session(endpoint_id, session_a));

        // B registers afterward — becomes the newest *live* session, but
        // never actually had anything sent through it.
        directory.register(endpoint_id, session_b, tx_b);
        assert!(
            directory.is_dispatch_session(endpoint_id, session_a),
            "A must remain the dispatch session until something is actually sent through B"
        );
        assert!(!directory.is_dispatch_session(endpoint_id, session_b));
    }

    #[tokio::test]
    async fn a_send_through_the_newly_selected_session_rebinds_the_dispatch_session() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let (tx_b, mut rx_b) = outbound_channel();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        directory.register(endpoint_id, session_b, tx_b);
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_b.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert!(!directory.is_dispatch_session(endpoint_id, session_a));
        assert!(directory.is_dispatch_session(endpoint_id, session_b));
    }

    #[tokio::test]
    async fn the_last_live_session_unregistering_clears_the_dispatch_session_binding() {
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
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
        assert!(directory.is_dispatch_session(endpoint_id, session_id));

        directory.unregister(endpoint_id, session_id);

        // A brand-new session, even if (hypothetically) it reused the exact
        // same SessionId, must never inherit a stale binding.
        assert!(!directory.is_dispatch_session(endpoint_id, session_id));
    }

    #[tokio::test]
    async fn an_older_sessions_own_disconnect_check_is_unaffected_by_a_concurrently_registering_newer_session(
    ) {
        // Regression proof for the exact race the corrective pass closes:
        // Attempt X is dispatched through session A. A's own disconnect
        // handling must observe "I was the dispatch session" reliably even
        // when a brand-new session B registers for the same Endpoint in the
        // interim (a concurrent reconnect racing A's own cleanup) — because
        // B's mere registration never touches the binding, only an actual
        // resolved send would.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let session_a = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, dispatch()).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        // B races in and registers before A's disconnect check runs.
        let (tx_b, _rx_b) = outbound_channel();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_b, tx_b);

        // A's disconnect handling still correctly identifies itself as the
        // dispatch-relevant session for the Attempt it actually carried.
        assert!(directory.is_dispatch_session(endpoint_id, session_a));
    }
}
