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
    /// The `(SessionId, ActionId)` pair recorded when `ActionDispatch` was
    /// last resolved to a session for each Endpoint (Issue #28 second
    /// corrective pass "Attempt-scoped session correlation"). Deliberately
    /// Attempt-scoped, not merely Endpoint-scoped: an Endpoint admits only
    /// one active Attempt at a time, but that Attempt can reach a terminal
    /// state and be superseded by a *later* Attempt/action_id while the
    /// session that carried the earlier one is still live and only now
    /// disconnecting — a purely Endpoint-scoped fact would then wrongly read
    /// as "relevant" to the new Attempt. Recording the `action_id` alongside
    /// the session lets a disconnecting session's own check be compared
    /// against whichever Attempt is actually current at reconciliation time
    /// (`ReconciliationService::mark_endpoint_uncertain`), so a stale
    /// correlation from an already-terminal Attempt can never leak into a
    /// later one.
    ///
    /// Deliberately distinct from `live_sessions`' "currently selected"
    /// notion: that notion changes the instant a newer session registers,
    /// which races a concurrent reconnect against an older session's own
    /// disconnect handling. This map instead reflects a fact fixed at send
    /// time, so a disconnecting session can reliably tell whether *it* was
    /// the one actually used for this Endpoint's most recent `ActionDispatch`,
    /// immune to that race.
    ///
    /// Two writers. `ActionDispatch` transmission ([`Self::send`])
    /// unconditionally overwrites this map — it always corresponds to the
    /// literal creation of a fresh, never-before-seen `action_id` for the
    /// next Attempt, so it is always monotonically newer than whatever the
    /// map currently holds and must always win. `CancelAction`/`StatusQuery`
    /// transmission never writes here — neither proves which session owns an
    /// action's execution (a `CancelAction`/`StatusQuery` send can resolve to
    /// a *different* live session than the one the original `ActionDispatch`
    /// went through, and neither message's mere local-transport acceptance is
    /// authoritative evidence the Agent on that session even recognizes the
    /// action).
    ///
    /// [`Self::bind_dispatch_relevant_session`] is the second writer
    /// (Issue #28 fourth corrective pass "Late stale rebind ordering"):
    /// unlike `ActionDispatch`, it can legitimately fire for an `action_id`
    /// that is no longer current — an evidence-application continuation
    /// (`ActionAck{Accepted}`/`StatusReport{Accepted|Running}`) can resume
    /// from its own `.await` an arbitrary amount of time after a NEWER
    /// `ActionDispatch` for a different, later Attempt has already
    /// overwritten this map. That second writer is therefore
    /// compare-and-swap-like, never a blind overwrite — see its own docs.
    ///
    /// Cleared for an Endpoint only when its last live session unregisters —
    /// mirrors `live_sessions`' own cleanup, never persisted.
    dispatch_correlation: HashMap<EndpointId, (ProtocolId, ProtocolId)>,
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
                // recently dispatched through" fact is no longer meaningful;
                // a future session starts with a clean slate rather than
                // inheriting a stale binding from a since-departed session.
                inner.dispatch_correlation.remove(&endpoint_id);
            }
        }
    }

    /// Reports the `action_id` `session_id` actually carried `ActionDispatch`
    /// for on `endpoint_id`, if any — the transient, Runtime-only,
    /// Attempt-scoped correlation a disconnecting session uses to decide
    /// both *whether* it was dispatch-relevant for reconciliation purposes
    /// and, crucially, *for which action* (Issue #28 second corrective pass
    /// "Attempt-scoped session correlation": last-sent-session tracking alone
    /// is Endpoint-scoped and can leak a stale, already-terminal Attempt's
    /// relevance into a later Attempt dispatched through a different session
    /// — see this type's module/field docs). `None` when no `ActionDispatch`
    /// has ever resolved a session for this Endpoint, or the resolved
    /// session does not match `session_id`. Never durable, never used to
    /// fall back or resend.
    ///
    /// Returning the `action_id` itself (rather than a bare `bool`) is what
    /// lets the caller pass it on to
    /// `ReconciliationService::mark_endpoint_uncertain`, which only enters
    /// `AwaitingReconciliation` for the Attempt actually carrying this exact
    /// `action_id` — never merely "whatever Attempt happens to be current for
    /// this Endpoint right now".
    pub fn dispatch_relevant_action(
        &self,
        endpoint_id: EndpointId,
        session_id: ProtocolId,
    ) -> Option<ProtocolId> {
        let inner = self
            .inner
            .lock()
            .expect("outbound session directory lock poisoned");
        match inner.dispatch_correlation.get(&endpoint_id) {
            Some((sid, action_id)) if *sid == session_id => Some(*action_id),
            _ => None,
        }
    }

    /// Rebinds `endpoint_id`'s dispatch-relevant correlation to
    /// `(session_id, action_id)` — a second, deliberately narrow writer of
    /// the exact same [`Inner::dispatch_correlation`] fact [`Self::send`]
    /// itself writes for `ActionDispatch` (Issue #28 third corrective pass
    /// "Session-relevance transfer after authoritative non-terminal
    /// evidence"). This is not a new/generic concept — it is the same
    /// correlation, given a second legitimate way to become current.
    ///
    /// Why a second writer is correct rather than a layering violation: the
    /// wider Agent Protocol/evidence-application contract already treats
    /// "authenticated Endpoint + `action_id`" — never exact `SessionId`
    /// identity — as the correlation authority for evidence
    /// (`JobRepository::apply_action_evidence`/`apply_status_report`: locked
    /// and applied by `action_id` + `authenticated_endpoint_id` alone, with
    /// no session check). A session that supplies AUTHORITATIVE evidence
    /// restoring/maintaining an Attempt as `InProgress` —
    /// `StatusReport{Accepted|Running}` after reconciliation, or
    /// `ActionAck{Accepted}` on a session overlapping the one that carried
    /// the original `ActionDispatch` — is therefore just as legitimately
    /// "the session currently relevant to this action" as the one that sent
    /// the original dispatch. Without this transfer, losing the ORIGINAL
    /// dispatching session while a DIFFERENT, still-live session is the one
    /// actually current would either (a) wrongly reconcile an Attempt a live
    /// session remains responsible for (if the stale entry still names the
    /// departed session and happens to match by accident), or, the actual
    /// defect this method closes, (b) silently strand the Attempt
    /// `InProgress` forever once the ORIGINAL session eventually
    /// disconnects, because [`Self::dispatch_relevant_action`] no longer
    /// names it and the disconnect is therefore never even considered
    /// reconciliation-relevant.
    ///
    /// Callers MUST invoke this only after the Application/Repository layer
    /// has already durably accepted the evidence for this exact
    /// `(endpoint_id, action_id, session_id)` triple — never merely because
    /// untrusted wire input claims `Accepted`/`Running`. See
    /// `AgentControlGateway::handle_status_report`/`handle_action_ack`, the
    /// only callers.
    ///
    /// Compare-and-swap-like, NOT a blind overwrite (Issue #28 fourth
    /// corrective pass "Late stale rebind ordering"): the Gateway task that
    /// calls this resumes from its own evidence-application `.await` with no
    /// guarantee that this Endpoint's correlation hasn't since moved on to a
    /// genuinely later Attempt — e.g. session B awaits
    /// `ActionEvidenceService::apply` for action X; before B's task resumes,
    /// a different session supplies X's terminal evidence, the next ordered
    /// JobStep commits Attempt Y, and `ActionDispatch{Y}` transmits through
    /// session D, correctly overwriting the map to `(D, Y)` via
    /// [`Self::send`]. B's now-stale continuation must not then overwrite
    /// that with `(B, X)` — doing so would strand Y: D's later disconnect
    /// would find `dispatch_relevant_action(endpoint, D)` returning `None`
    /// (the map now wrongly names B) and never even consider reconciling Y.
    /// A legitimate later action always obtains its own correlation through
    /// its own `ActionDispatch`, so once the correlation has genuinely moved
    /// from X to Y, any asynchronous continuation still carrying X is stale
    /// with respect to session-loss tracking and must never move it
    /// backward — mirroring the durable no-regression rules already used
    /// elsewhere (newer authoritative lifecycle identity is never overwritten
    /// by delayed evidence for an earlier one).
    ///
    /// Three cases, decided by the CURRENT correlation's `action_id`:
    /// - no correlation exists for `endpoint_id` yet: bind unconditionally
    ///   (required for Server-restart reconciliation, where Runtime state
    ///   starts empty and `StatusReport{Accepted|Running}` must be able to
    ///   establish relevance from nothing);
    /// - the current correlation already names this exact `action_id`
    ///   (possibly through a different session): rebind/transfer to
    ///   `session_id`, exactly [`Self::bind_dispatch_relevant_session`]'s
    ///   original purpose;
    /// - the current correlation names a DIFFERENT `action_id`: reject —
    ///   [`BindOutcome::StaleActionIgnored`], no mutation. That different
    ///   `action_id` can only have gotten there through a genuinely later
    ///   `ActionDispatch`.
    pub fn bind_dispatch_relevant_session(
        &self,
        endpoint_id: EndpointId,
        session_id: ProtocolId,
        action_id: ProtocolId,
    ) -> BindOutcome {
        let mut inner = self
            .inner
            .lock()
            .expect("outbound session directory lock poisoned");
        if let Some((_, current_action_id)) = inner.dispatch_correlation.get(&endpoint_id) {
            if *current_action_id != action_id {
                return BindOutcome::StaleActionIgnored;
            }
        }
        inner
            .dispatch_correlation
            .insert(endpoint_id, (session_id, action_id));
        BindOutcome::Bound
    }
}

/// The outcome of [`OutboundSessionDirectory::bind_dispatch_relevant_session`]
/// — see that method's docs for the exact compare-and-swap-like semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// The correlation was absent or already named this exact `action_id`;
    /// it now names `(session_id, action_id)`.
    Bound,
    /// The correlation already named a DIFFERENT, necessarily newer
    /// `action_id` — the bind was ignored; nothing was mutated.
    StaleActionIgnored,
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
            // Fixes the dispatch-relevant (session, action_id) correlation
            // for this Endpoint at the moment of resolution — see
            // `dispatch_correlation`'s docs. Recorded regardless of whether
            // the local send below ultimately succeeds: resolving to
            // `session_id` at all is itself the fact that matters, mirroring
            // `dispatch_action`'s own "the local transport accepted the
            // frame" trust boundary.
            //
            // Deliberately only for `ActionDispatch`: `CancelAction`/
            // `StatusQuery` transmission is never recorded here (Issue #28
            // second corrective pass "secondary problem" — see
            // `dispatch_correlation`'s docs for why).
            if let AgentProtocolMessage::ActionDispatch(ref dispatch) = message {
                inner
                    .dispatch_correlation
                    .insert(endpoint_id, (session_id, dispatch.body.action_id));
            }
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

    // -- Issue #28 second corrective pass: dispatch_relevant_action -------

    fn cancel(action_id: ProtocolId) -> CancelActionMessage {
        CancelActionMessage::new(action_id)
    }

    fn status_query(action_id: ProtocolId) -> StatusQueryMessage {
        StatusQueryMessage::new(action_id)
    }

    #[tokio::test]
    async fn no_send_has_ever_happened_is_never_a_dispatch_relevant_action() {
        let directory = OutboundSessionDirectory::new();
        let endpoint_id = endpoint();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, outbound_channel().0);
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            None
        );
    }

    #[tokio::test]
    async fn a_successful_action_dispatch_send_binds_its_exact_action_id_to_the_resolved_session() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx, mut rx) = outbound_channel();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, tx);
        let message = dispatch();
        let action_id = message.body.action_id;

        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message).await }
        });
        let OutboundCommand::Send { ack, .. } = rx.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            Some(action_id)
        );
    }

    #[tokio::test]
    async fn a_newer_session_registering_alone_never_changes_the_dispatch_correlation() {
        // Registration alone — no send — must not disturb which session is
        // considered dispatch-relevant; only an actual resolved ActionDispatch
        // send does (Issue #28 corrective passes: this is what makes a
        // disconnecting older session's own check immune to a concurrent
        // newer registration).
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let (tx_b, _rx_b) = outbound_channel();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        let message = dispatch();
        let action_id = message.body.action_id;

        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            Some(action_id)
        );

        // B registers afterward — becomes the newest *live* session, but
        // never actually had anything sent through it.
        directory.register(endpoint_id, session_b, tx_b);
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            Some(action_id),
            "A must remain the dispatch-relevant session until something is actually sent through B"
        );
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            None
        );
    }

    #[tokio::test]
    async fn a_send_through_the_newly_selected_session_rebinds_the_dispatch_correlation() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let (tx_b, mut rx_b) = outbound_channel();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        let message_a = dispatch();
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message_a).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        directory.register(endpoint_id, session_b, tx_b);
        let message_b = dispatch();
        let action_id_b = message_b.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message_b).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_b.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            None
        );
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            Some(action_id_b)
        );
    }

    #[tokio::test]
    async fn the_last_live_session_unregistering_clears_the_dispatch_correlation() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx, mut rx) = outbound_channel();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, tx);
        let message = dispatch();
        let action_id = message.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message).await }
        });
        let OutboundCommand::Send { ack, .. } = rx.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            Some(action_id)
        );

        directory.unregister(endpoint_id, session_id);

        // A brand-new session, even if (hypothetically) it reused the exact
        // same SessionId, must never inherit a stale binding.
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            None
        );
    }

    #[tokio::test]
    async fn an_older_sessions_own_disconnect_check_is_unaffected_by_a_concurrently_registering_newer_session(
    ) {
        // Regression proof for the exact race the first corrective pass
        // closes: Attempt X is dispatched through session A. A's own
        // disconnect handling must observe "I carried action X" reliably
        // even when a brand-new session B registers for the same Endpoint in
        // the interim (a concurrent reconnect racing A's own cleanup) —
        // because B's mere registration never touches the correlation, only
        // an actual resolved ActionDispatch send would.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let session_a = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        let message = dispatch();
        let action_id = message.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        // B races in and registers before A's disconnect check runs.
        let (tx_b, _rx_b) = outbound_channel();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_b, tx_b);

        // A's disconnect handling still correctly identifies itself as the
        // session that carried this exact action_id.
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            Some(action_id)
        );
    }

    // -- Issue #28 second corrective pass: cross-Attempt stale correlation
    // and the CancelAction/StatusQuery "secondary problem" -----------------

    #[tokio::test]
    async fn a_later_action_dispatch_through_the_same_session_supersedes_an_earlier_terminal_ones_correlation(
    ) {
        // Session A dispatches Attempt 1's action, which later reaches a
        // terminal state; the SAME still-live session A then dispatches
        // Attempt 2's action (the next JobStep). A's own disconnect check
        // must observe Attempt 2's action_id, never a stale binding to
        // Attempt 1 — Runtime-level proof that a terminal prior action's
        // correlation is never reused for a later one, even without an
        // intervening session change.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx, mut rx) = outbound_channel();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, tx);

        let attempt_1 = dispatch();
        let action_id_1 = attempt_1.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, attempt_1).await }
        });
        let OutboundCommand::Send { ack, .. } = rx.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            Some(action_id_1)
        );

        // Attempt 1 reaches a terminal state (irrelevant to this directory —
        // it never tracks Attempt state); the next JobStep's Attempt 2
        // dispatches through the same still-live session.
        let attempt_2 = dispatch();
        let action_id_2 = attempt_2.body.action_id;
        assert_ne!(action_id_1, action_id_2);
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, attempt_2).await }
        });
        let OutboundCommand::Send { ack, .. } = rx.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            Some(action_id_2),
            "the stale Attempt 1 correlation must never be observable once Attempt 2 dispatches"
        );
    }

    #[tokio::test]
    async fn transmitting_cancel_action_never_establishes_or_disturbs_dispatch_correlation() {
        // Secondary problem: CancelAction transmission proves nothing about
        // which session owns the action's execution and must never be
        // treated as equivalent to ActionDispatch routing.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx, mut rx) = outbound_channel();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, tx);

        let action_id = ProtocolId::generate();
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move {
                directory
                    .cancel_action(endpoint_id, cancel(action_id))
                    .await
            }
        });
        let OutboundCommand::Send { ack, .. } = rx.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            None,
            "CancelAction transmission alone must never establish dispatch correlation"
        );
    }

    #[tokio::test]
    async fn transmitting_status_query_never_establishes_or_disturbs_dispatch_correlation() {
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx, mut rx) = outbound_channel();
        let session_id = ProtocolId::generate();
        directory.register(endpoint_id, session_id, tx);

        let action_id = ProtocolId::generate();
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move {
                directory
                    .status_query(endpoint_id, status_query(action_id))
                    .await
            }
        });
        let OutboundCommand::Send { ack, .. } = rx.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            None,
            "StatusQuery transmission alone must never establish dispatch correlation"
        );
    }

    #[tokio::test]
    async fn a_later_status_query_through_another_session_never_erases_the_original_dispatch_correlation(
    ) {
        // Session A dispatches ActionDispatch; a later StatusQuery for the
        // same action_id resolves to session B (the currently-selected live
        // session at that moment). A's own correlation to the original
        // action must remain intact — StatusQuery transmission through B
        // must never overwrite or erase it.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let session_a = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        let message = dispatch();
        let action_id = message.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        let (tx_b, mut rx_b) = outbound_channel();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_b, tx_b);
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move {
                directory
                    .status_query(endpoint_id, status_query(action_id))
                    .await
            }
        });
        let OutboundCommand::Send { ack, .. } = rx_b.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            Some(action_id),
            "A's original dispatch correlation must survive an unrelated StatusQuery send"
        );
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            None,
            "StatusQuery transmission through B must never make B look dispatch-relevant"
        );
    }

    // -- Issue #28 third corrective pass: bind_dispatch_relevant_session --

    #[tokio::test]
    async fn rebinding_transfers_dispatch_relevance_to_the_reporting_session_for_the_same_action() {
        // Session A dispatches action X; session B later supplies
        // authoritative non-terminal evidence for the SAME action_id and is
        // rebound. A must no longer be considered dispatch-relevant for X —
        // only B is, from this point on.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let session_a = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        let message = dispatch();
        let action_id = message.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            Some(action_id)
        );

        let session_b = ProtocolId::generate();
        let outcome = directory.bind_dispatch_relevant_session(endpoint_id, session_b, action_id);

        assert_eq!(
            outcome,
            BindOutcome::Bound,
            "rebinding the same action_id to a different session must be allowed"
        );
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            None,
            "A must no longer be dispatch-relevant once relevance transfers to B"
        );
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            Some(action_id),
            "B must become dispatch-relevant for the exact action_id it reported on"
        );
    }

    #[tokio::test]
    async fn a_rebind_for_a_stale_action_can_never_affect_a_different_current_action() {
        // Mirrors the fbcbb37 safety property for the new writer: even if a
        // rebind call somehow named a superseded action_id (X, now
        // terminal), it must never be confused with a later, unrelated
        // action_id (Y) dispatched afterward through a different session —
        // the map holds one fact per Endpoint, and only the most recent
        // write (dispatch OR rebind) is ever observable.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let session_a = ProtocolId::generate();
        let session_b = ProtocolId::generate();
        let action_x = ProtocolId::generate();
        let action_y = ProtocolId::generate();

        // A stale rebind for a superseded action X, naming session A.
        directory.bind_dispatch_relevant_session(endpoint_id, session_a, action_x);
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            Some(action_x)
        );

        // A genuinely later action Y dispatches through session B.
        let (tx_b, mut rx_b) = outbound_channel();
        directory.register(endpoint_id, session_b, tx_b);
        let message_y = ActionDispatchMessage::new(
            action_y,
            "bamep.m1.simulated-execution",
            "1",
            serde_json::Map::new(),
        );
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message_y).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_b.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_a),
            None,
            "the stale rebind to action X must never survive a later action Y's real dispatch"
        );
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            Some(action_y)
        );
    }

    #[tokio::test]
    async fn losing_the_last_live_session_still_clears_the_correlation_after_a_rebind() {
        // The rebind path must compose with the existing last-live-session
        // cleanup in `unregister` — a rebound session unregistering as the
        // Endpoint's last live session must still clear the correlation,
        // exactly like a plain dispatch would.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();
        let (tx_a, mut rx_a) = outbound_channel();
        let session_a = ProtocolId::generate();
        directory.register(endpoint_id, session_a, tx_a);
        let message = dispatch();
        let action_id = message.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_a.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();

        let (tx_b, _rx_b) = outbound_channel();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_b, tx_b);
        directory.bind_dispatch_relevant_session(endpoint_id, session_b, action_id);
        // B is now dispatch-relevant; A no longer is.
        directory.unregister(endpoint_id, session_a);
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            Some(action_id),
            "A's unrelated unregister must not disturb B's rebound relevance"
        );

        directory.unregister(endpoint_id, session_b);
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            None,
            "B unregistering as the last live session must clear the correlation"
        );
    }

    // -- Issue #28 fourth corrective pass: late stale rebind ordering ------

    #[tokio::test]
    async fn a_delayed_stale_rebind_for_a_superseded_action_can_never_overwrite_a_newer_real_dispatch(
    ) {
        // The missing direction from
        // `a_rebind_for_a_stale_action_can_never_affect_a_different_current_action`:
        // there, a stale bind happens FIRST and a real dispatch for a later
        // action correctly overwrites it. Here, the real dispatch for the
        // later action Y happens FIRST, and the stale evidence-application
        // continuation for the EARLIER action X only calls
        // `bind_dispatch_relevant_session` afterward — exactly the race the
        // concrete defect describes: session B awaits
        // `ActionEvidenceService::apply` for X; before B's Gateway task
        // resumes, X reaches a terminal state elsewhere, the next ordered
        // JobStep commits Attempt Y, and `ActionDispatch{Y}` transmits
        // through session D. B's now-stale continuation must not then
        // overwrite `(D, Y)` with `(B, X)`.
        let directory = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = endpoint();

        // 1. Correlation for action X is established through session B.
        let (tx_b, mut rx_b) = outbound_channel();
        let session_b = ProtocolId::generate();
        directory.register(endpoint_id, session_b, tx_b);
        let message_x = dispatch();
        let action_x = message_x.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message_x).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_b.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            Some(action_x)
        );

        // 2. A genuinely newer action Y — the next ordered JobStep's Attempt
        // — dispatches through session D, exactly like the real Gateway
        // path (`FinalDispatchService` commit followed by
        // `ActionDispatchService::dispatch`).
        let (tx_d, mut rx_d) = outbound_channel();
        let session_d = ProtocolId::generate();
        directory.register(endpoint_id, session_d, tx_d);
        let message_y = dispatch();
        let action_y = message_y.body.action_id;
        let send_task = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.dispatch_action(endpoint_id, message_y).await }
        });
        let OutboundCommand::Send { ack, .. } = rx_d.recv().await.unwrap();
        ack.send(Ok(())).unwrap();
        send_task.await.unwrap().unwrap();
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_d),
            Some(action_y)
        );

        // 3. Session B's own, now-stale evidence-application continuation
        // for X — begun before Y ever existed — finally resumes and invokes
        // the rebind.
        let outcome = directory.bind_dispatch_relevant_session(endpoint_id, session_b, action_x);

        // 4. Rejected; the correlation remains exactly (session_d, action_y).
        assert_eq!(outcome, BindOutcome::StaleActionIgnored);
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_d),
            Some(action_y),
            "the newer real ActionDispatch correlation for Y must survive the stale rebind for X"
        );
        // 5. Session B must never become relevant again for the superseded
        // action X.
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_b),
            None,
            "session B must not regain relevance for the superseded action X"
        );
    }

    #[tokio::test]
    async fn binding_from_no_existing_correlation_is_allowed_and_returns_bound() {
        // Server-restart case: Runtime state starts empty — no
        // `ActionDispatch` has ever been recorded in this process for this
        // Endpoint — yet an accepted `StatusReport{Accepted|Running}` must
        // still be able to establish relevance from nothing once a session
        // (re-)reports on a `Dispatched`/`InProgress` Attempt the restarted
        // Server never itself dispatched.
        let directory = OutboundSessionDirectory::new();
        let endpoint_id = endpoint();
        let session_id = ProtocolId::generate();
        let action_id = ProtocolId::generate();

        let outcome = directory.bind_dispatch_relevant_session(endpoint_id, session_id, action_id);

        assert_eq!(outcome, BindOutcome::Bound);
        assert_eq!(
            directory.dispatch_relevant_action(endpoint_id, session_id),
            Some(action_id)
        );
    }
}
