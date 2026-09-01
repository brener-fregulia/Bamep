//! Agent Control Gateway: Agent Protocol v1 handshake semantics over an
//! already-established WebSocket, plus the authenticated post-session
//! message loop (`docs/specifications/m0-agent-protocol-contract.md`
//! "Transport and handshake", "Runtime credential issuance and rotation").
//!
//! Boundary (Issue #17 handshake checkpoint, extended by Issue #18's
//! post-session `InventoryReport`, and by Issue #26's typed-action
//! dispatch/evidence traffic): `agent_transport` owns TCP/TLS/WebSocket
//! establishment; this module owns only what happens on the Agent Protocol
//! JSON stream once that WebSocket already exists —
//! `AuthRequest` -> `EnrollmentService::redeem` -> `SessionEstablished`/
//! `AuthError`. It never touches TLS/fingerprint verification (already
//! complete by the time [`AgentControlGateway::handshake`] is called), never
//! contains SQL, and never re-derives a Domain/Application decision — every
//! accept/reject decision is exactly the one `EnrollmentService::redeem`/
//! `ActionEvidenceService::apply` already made.
//!
//! After authentication this module drives the session loop, delegating
//! `BootstrapEvidence` verification, `InventoryReport` recording, and
//! `ActionAck`/`ActionResult` evidence application to their respective
//! Application services; treats `ActionProgress` as transient advisory
//! metadata only; and is the sole serialized owner of this session's
//! WebSocket writes — both for inbound-message responses and for outbound
//! `ActionDispatch` traffic enqueued through
//! `crate::runtime::outbound_sessions::OutboundSessionDirectory` (Issue #26
//! "Outbound authenticated session delivery").

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use bamep_agent_protocol::{
    decode, encode, ActionAckMessage, ActionAckOutcome, ActionProgressMessage, ActionResultMessage,
    ActionResultOutcome, AgentProtocolMessage, AuthErrorMessage, AuthRequestMessage,
    CancelAckMessage, CancelAckOutcome, KnownActionState, MessageTimestamp, ProtocolErrorMessage,
    ProtocolId, SessionEstablishedMessage, StatusReportMessage, TransferAuthorizationDeniedMessage,
    TransferAuthorizationGrantMessage, TransferAuthorizationRequestMessage,
};
use bamep_domain::{ActionEvidence, CancelAckEvidence, EndpointId};
use bamep_trusted_bootstrap::ServerCertFingerprint;

use crate::application::{
    parse_transfer_result_detail, ActionEvidenceService, ApplicationError,
    BootstrapEvidenceService, CancellationService, EnrollmentService, RedeemResult,
    TransferActionClassification, TransferAuthorizationOutcome, TransferAuthorizationService,
    TransferCancelAckOutcome, TransferStatusReportOutcome, TransferTerminalEvidenceService,
    TransferTerminalOutcome,
};
use crate::ports::{
    ApplyActionEvidenceResult, ApplyReconciliationResult, CredentialRedemptionRepository,
    EndpointRepository,
};
use crate::runtime::outbound_sessions::{
    outbound_channel, OutboundCommand, OutboundSessionDirectory,
};
use crate::runtime::presence::PresenceRegistry;

/// Agent Protocol v1 currently defines no richer closed `AuthError` reason
/// taxonomy (`m0-agent-protocol-contract.md` "Runtime credential issuance and
/// rotation": "a generic authentication rejection is sufficient"). Every
/// handshake rejection in this checkpoint — malformed message, wrong-phase
/// message, incompatible `protocol_version`, or a rejected credential — uses
/// this single value. Callers must never encode parser/serde/Application
/// detail into a richer reason.
const GENERIC_AUTH_ERROR_REASON: &str = "rejected";
pub const GENERIC_PROTOCOL_ERROR_CODE: &str = "GENERIC";
pub const GENERIC_PROTOCOL_ERROR_MESSAGE: &str = "protocol violation";
/// `TransferAuthorizationDenied.reason`'s single closed V1 value
/// (`m0-agent-protocol-contract.md` "Renewal and restart"): every internal
/// denial cause collapses into this one generic value.
pub const GENERIC_TRANSFER_AUTHORIZATION_DENIED_REASON: &str = "denied";

/// The result of a successful Agent Protocol v1 handshake. `session_id` is a
/// fresh, transient value — not persisted to PostgreSQL in WP1 (no session
/// repository/table exists).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedSession {
    pub endpoint_id: EndpointId,
    pub session_id: ProtocolId,
}

/// Reliably unregisters exactly one `(endpoint_id, session_id)` from both the
/// Runtime Presence Registry and the [`OutboundSessionDirectory`] on drop —
/// a panic-safety backstop for an unwinding panic, which no explicit code
/// path can run. The normal `Ok(())` return and every `?` early-return
/// instead unregister eagerly, synchronously, immediately once the message
/// loop ends (Issue #28 corrective pass "Session-loss reconciliation with
/// overlapping sessions": presence/outbound readiness must drop before any
/// asynchronous reconciliation work runs) — this guard's own `drop` then
/// finds both registries already clear and is a harmless idempotent no-op.
/// A single combined guard, not two separate ones, so the required shutdown
/// order (Issue #26 correction "Make presence mean outbound-ready for this
/// session": presence removed first, outbound delivery removed second) is
/// enforced structurally by this one `Drop` impl rather than by relying on
/// two guards being declared in the right order — the registries themselves
/// remain fully separate: this type owns no registry logic of its own, it
/// only sequences two existing `unregister` calls. Constructed only by
/// [`AgentControlGateway::run_authenticated_session`], after both
/// registrations have already happened (in the required outbound-then-
/// presence startup order).
struct SessionLifecycleGuard {
    presence: Arc<PresenceRegistry>,
    outbound_sessions: Arc<OutboundSessionDirectory>,
    endpoint_id: EndpointId,
    session_id: ProtocolId,
}

impl Drop for SessionLifecycleGuard {
    fn drop(&mut self) {
        self.presence.unregister(self.endpoint_id, self.session_id);
        self.outbound_sessions
            .unregister(self.endpoint_id, self.session_id);
    }
}

/// Distinguishes an expected Agent Protocol/authentication rejection from a
/// genuine Server/transport failure ([`AgentGatewayError`]). A rejection is
/// terminal for the handshake attempt that produced it: the caller drops the
/// connection rather than accepting a further `AuthRequest` as a silent retry.
#[derive(Debug)]
pub enum HandshakeOutcome {
    Established(AuthenticatedSession),
    Rejected,
}

/// Genuine Gateway/transport/Application processing failures — never an
/// expected protocol/authentication rejection, which is
/// [`HandshakeOutcome::Rejected`] instead.
#[derive(Debug, thiserror::Error)]
pub enum AgentGatewayError {
    #[error("failed to receive a WebSocket frame during the handshake")]
    Receive(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("failed to send a WebSocket frame during the handshake")]
    Send(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("the connection closed before the handshake completed")]
    ConnectionClosed,
    #[error("authenticated session requires a configured BootstrapEvidenceService")]
    BootstrapEvidenceServiceNotConfigured,
    #[error(
        "authenticated session received InventoryReport without a configured InventoryService"
    )]
    InventoryServiceNotConfigured,
    #[error(
        "authenticated session received ActionAck/ActionResult without a configured \
         ActionEvidenceService"
    )]
    ActionEvidenceServiceNotConfigured,
    #[error("authenticated session received CancelAck without a configured CancellationService")]
    CancellationServiceNotConfigured,
    #[error(
        "authenticated session received StatusReport without a configured ReconciliationService"
    )]
    ReconciliationServiceNotConfigured,
    #[error(
        "authenticated session received TransferAuthorizationRequest without a configured \
         TransferAuthorizationService"
    )]
    TransferAuthorizationServiceNotConfigured,
    #[error(transparent)]
    Application(#[from] ApplicationError),
}

/// Bound shared by every helper method below that writes to a session's
/// WebSocket sink half. Generic over the sink type itself, rather than a
/// concrete `SplitSink<WebSocketStream<S>, Message>` alias, because
/// `run_authenticated_session` splits a *borrowed* `&mut WebSocketStream<S>`
/// (its caller retains ownership of the connection) — `.split()` on a
/// mutable reference produces `SplitSink<&mut WebSocketStream<S>, Message>`,
/// a different concrete type from splitting an owned `WebSocketStream<S>`.
trait MessageSink:
    futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin
{
}
impl<T> MessageSink for T where
    T: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin
{
}

/// Holds the real [`EnrollmentService`] and drives one Agent Protocol v1
/// handshake per call. `Arc`-shared so one Gateway/`EnrollmentService` pair
/// serves many concurrent connections, consistent with `EnrollmentService`
/// itself already wrapping its repositories in `Arc`. Stateless across calls
/// beyond that shared reference: nothing about a rejected or completed
/// handshake is retained here.
pub struct AgentControlGateway<R: EndpointRepository, C: CredentialRedemptionRepository> {
    enrollment: Arc<EnrollmentService<R, C>>,
    bootstrap_evidence: Option<Arc<BootstrapEvidenceService<R>>>,
    inventory: Option<Arc<crate::application::InventoryService>>,
    action_evidence: Option<Arc<ActionEvidenceService>>,
    /// Consumes terminal `bamep.m1.data-plane-transfer` `ActionResult`
    /// evidence against durable Transfer/Artifact facts (Issue #19 checkpoint
    /// C2). When configured, `handle_action_result` classifies the owning
    /// action from durable Server facts and routes a transfer result here;
    /// the RF-004 `bamep.m1.simulated-execution` path stays on
    /// `action_evidence`, unchanged.
    transfer_terminal_evidence: Option<Arc<TransferTerminalEvidenceService>>,
    /// Applies inbound `CancelAck` evidence (Issue #27 "CancelAck
    /// handling"). Deliberately never used for the operator/internal
    /// cancellation-request control path — that path
    /// (`CancellationService::request`) must remain structurally separate
    /// from this inbound Agent Protocol message loop.
    cancellation: Option<Arc<CancellationService>>,
    /// Applies inbound `StatusReport` evidence and drives connection-loss/
    /// session-start reconciliation triggers (Issue #28 "Reconcile
    /// interrupted Attempts safely"). Deliberately never used for the
    /// explicit operator/internal Indeterminate-closure control path — that
    /// path (`ReconciliationService::close_indeterminate`) must remain
    /// structurally separate from this inbound Agent Protocol message loop,
    /// mirroring `cancellation`'s identical separation requirement.
    reconciliation: Option<Arc<crate::application::ReconciliationService>>,
    /// Serves `TransferAuthorizationRequest` (Issue #38 "Agent WSS
    /// integration"). Deliberately not routed through any other service —
    /// this is the sole Agent Protocol entry point into sender-constrained
    /// transfer authorization.
    transfer_authorization: Option<Arc<TransferAuthorizationService>>,
    /// The Runtime Presence Registry this Gateway's authenticated sessions
    /// register with/unregister from (`m0-stack-and-boundaries-baseline.md`
    /// "Runtime Presence Registry"). Owned by the Gateway by default so every
    /// `AgentControlGateway::new` caller gets working presence tracking
    /// without opting in; [`Self::with_presence_registry`] lets a caller
    /// share one `PresenceRegistry` instance across multiple Runtime
    /// Services (e.g. a future scheduler that must observe the same
    /// presence facts).
    presence: Arc<PresenceRegistry>,
    /// The transient outbound authenticated-session delivery directory
    /// (Issue #26 "Outbound authenticated session delivery"). Deliberately
    /// separate from `presence`: `OutboundSessionDirectory` additionally
    /// tracks *which exact session* currently receives outbound
    /// `ActionDispatch` traffic and how to reach it.
    outbound_sessions: Arc<OutboundSessionDirectory>,
}

impl<R: EndpointRepository, C: CredentialRedemptionRepository> AgentControlGateway<R, C> {
    pub fn new(enrollment: Arc<EnrollmentService<R, C>>) -> Self {
        Self {
            enrollment,
            bootstrap_evidence: None,
            inventory: None,
            action_evidence: None,
            transfer_terminal_evidence: None,
            cancellation: None,
            reconciliation: None,
            transfer_authorization: None,
            presence: Arc::new(PresenceRegistry::new()),
            outbound_sessions: Arc::new(OutboundSessionDirectory::new()),
        }
    }

    pub fn with_inventory_service(
        mut self,
        service: Arc<crate::application::InventoryService>,
    ) -> Self {
        self.inventory = Some(service);
        self
    }

    pub fn with_bootstrap_evidence_service(
        mut self,
        service: Arc<BootstrapEvidenceService<R>>,
    ) -> Self {
        self.bootstrap_evidence = Some(service);
        self
    }

    pub fn with_action_evidence_service(mut self, service: Arc<ActionEvidenceService>) -> Self {
        self.action_evidence = Some(service);
        self
    }

    pub fn with_transfer_terminal_evidence_service(
        mut self,
        service: Arc<TransferTerminalEvidenceService>,
    ) -> Self {
        self.transfer_terminal_evidence = Some(service);
        self
    }

    pub fn with_cancellation_service(mut self, service: Arc<CancellationService>) -> Self {
        self.cancellation = Some(service);
        self
    }

    pub fn with_reconciliation_service(
        mut self,
        service: Arc<crate::application::ReconciliationService>,
    ) -> Self {
        self.reconciliation = Some(service);
        self
    }

    pub fn with_transfer_authorization_service(
        mut self,
        service: Arc<TransferAuthorizationService>,
    ) -> Self {
        self.transfer_authorization = Some(service);
        self
    }

    pub fn with_presence_registry(mut self, presence: Arc<PresenceRegistry>) -> Self {
        self.presence = presence;
        self
    }

    pub fn with_outbound_session_directory(
        mut self,
        outbound_sessions: Arc<OutboundSessionDirectory>,
    ) -> Self {
        self.outbound_sessions = outbound_sessions;
        self
    }

    /// The shared [`PresenceRegistry`] this Gateway's authenticated sessions
    /// register with.
    pub fn presence(&self) -> Arc<PresenceRegistry> {
        Arc::clone(&self.presence)
    }

    /// The shared [`OutboundSessionDirectory`] this Gateway's authenticated
    /// sessions register with — the Port implementation
    /// `ActionDispatchService` sends `ActionDispatch` traffic through.
    pub fn outbound_sessions(&self) -> Arc<OutboundSessionDirectory> {
        Arc::clone(&self.outbound_sessions)
    }

    /// Runs the authenticated post-handshake phase on the same WebSocket.
    /// Evidence rejection is deliberately silent and non-terminal.
    ///
    /// Splits `websocket` into its read/write halves so one `tokio::select!`
    /// loop can serve inbound frames and outbound
    /// `crate::runtime::outbound_sessions::OutboundCommand`s from the same
    /// task without a double-mutable-borrow conflict — this task remains the
    /// sole serialized owner of every write to this session's socket,
    /// including outbound `ActionDispatch` traffic (Issue #26 "Outbound
    /// authenticated session delivery": "a recommended minimal design is:
    /// one bounded outbound channel per authenticated session; Gateway's
    /// authenticated-session task owns the socket").
    pub async fn run_authenticated_session<S>(
        &self,
        websocket: &mut WebSocketStream<S>,
        session: AuthenticatedSession,
        connection_fingerprint: ServerCertFingerprint,
    ) -> Result<(), AgentGatewayError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // Fail fast, before any registration below, when this checkpoint's
        // required configuration is missing — re-checked (and actually used)
        // by `run_message_loop` once the message loop begins.
        self.bootstrap_evidence
            .as_ref()
            .ok_or(AgentGatewayError::BootstrapEvidenceServiceNotConfigured)?;

        // Registration happens only here — after the caller has already
        // confirmed `HandshakeOutcome::Established` and every configuration
        // precondition above has passed — never for a rejected/failed
        // authentication. `SessionLifecycleGuard` reliably unregisters this
        // exact `SessionId` from both registries, in the required order, on
        // every exit path below: normal Close/disconnect (`Ok(())`), a
        // genuine Gateway error (`?`), and an unwinding panic alike.
        //
        // Ordering is deliberate (Issue #26 correction "Make presence mean
        // outbound-ready for this session"): outbound delivery registers
        // FIRST, presence SECOND, so #25's final-dispatch gate can never
        // observe presence for a session that is not yet outbound-ready.
        // `SessionLifecycleGuard::drop` unregisters presence first and
        // outbound delivery second — no new final-dispatch gate can pass
        // based on a session whose outbound delivery has already
        // disappeared.
        let (outbound_tx, mut outbound_rx) = outbound_channel();
        self.outbound_sessions
            .register(session.endpoint_id, session.session_id, outbound_tx);
        self.presence
            .register(session.endpoint_id, session.session_id);
        let _lifecycle_guard = SessionLifecycleGuard {
            presence: Arc::clone(&self.presence),
            outbound_sessions: Arc::clone(&self.outbound_sessions),
            endpoint_id: session.endpoint_id,
            session_id: session.session_id,
        };

        let (mut write, mut read) = websocket.split();

        // Reconciliation (Issue #28 "Reconciliation"): now that this session
        // is outbound-ready, issue `StatusQuery` for any Attempt this
        // Endpoint left `AwaitingReconciliation` — whether from an earlier
        // disconnect or from Server restart. Spawned, not awaited here: the
        // outbound `StatusQuery` send enqueues onto this same session's
        // outbound channel and awaits an ack that only the message loop
        // below (via `outbound_rx.recv()`) ever fulfills — awaiting it
        // inline, before that loop starts, would deadlock this task against
        // itself. Best-effort either way: a missing service or a local send
        // failure must never prevent this session from proceeding to the
        // normal message loop.
        if let Some(reconciliation) = &self.reconciliation {
            let reconciliation = Arc::clone(reconciliation);
            let endpoint_id = session.endpoint_id;
            tokio::spawn(async move {
                let _ = reconciliation.reconcile_on_session_start(endpoint_id).await;
            });
        }

        let result = self
            .run_message_loop(
                &mut write,
                &mut read,
                &mut outbound_rx,
                session,
                connection_fingerprint,
            )
            .await;

        // Issue #28 corrective pass "Session-loss reconciliation with
        // overlapping sessions": the moment this exact session's message
        // loop ends — normal disconnect/Close (`Ok(())`) or a genuine
        // Gateway error (`Err`) — it must stop advertising presence/
        // outbound readiness for this Endpoint BEFORE any asynchronous
        // reconciliation work (which awaits PostgreSQL) can run. Otherwise a
        // concurrent final-dispatch could still observe this Endpoint as
        // outbound-ready and durably create a new Attempt during that
        // window even though this session's message loop is already gone.
        //
        // `dispatch_relevant_action` is captured first, before either
        // registry mutates: it reports the exact `action_id` this session's
        // outbound `ActionDispatch` traffic actually carried, if any, immune
        // to a concurrent reconnect racing this cleanup (unlike "currently
        // selected live session", which changes the instant a new session
        // registers — see that method's docs). An unrelated older/
        // superseded session's disconnect must never move an Attempt that a
        // different, still-live session remains responsible for.
        //
        // Capturing the `action_id` itself — not merely a boolean — is what
        // closes the Issue #28 second corrective pass's cross-Attempt race:
        // this exact session may have carried an EARLIER Attempt that has
        // since reached a terminal state and been superseded by a new one
        // dispatched through a different (or the same) session while this
        // one's own message loop was still shutting down.
        // `mark_endpoint_uncertain` below only enters `AwaitingReconciliation`
        // for the Attempt that still carries this exact `action_id` — never
        // "whatever Attempt happens to be current for this Endpoint right
        // now" — so a stale correlation from an already-terminal Attempt can
        // never leak into a later one.
        let dispatched_action = self
            .outbound_sessions
            .dispatch_relevant_action(session.endpoint_id, session.session_id);
        self.presence
            .unregister(session.endpoint_id, session.session_id);
        self.outbound_sessions
            .unregister(session.endpoint_id, session.session_id);

        // Connection loss (Issue #28 "Connection loss"): only when this
        // session actually carried an `ActionDispatch`. Best-effort and
        // never overrides the loop's own result; an unwinding panic is not
        // covered (no async Drop exists to run this), but durable Attempt
        // state is never corrupted by skipping it — a later reconciliation
        // trigger (the next session start, or a Server-restart sweep) still
        // recovers it. If another authenticated session for the Endpoint
        // remains live (already registered, or one that raced this cleanup
        // and is now selected), it is awaited directly here — not spawned —
        // since it enqueues onto a *different*, already-running session
        // task's outbound channel, never this one's own now-torn-down loop.
        if let Some(action_id) = dispatched_action {
            if let Some(reconciliation) = &self.reconciliation {
                let _ = reconciliation
                    .mark_endpoint_uncertain(session.endpoint_id, action_id)
                    .await;
                let _ = reconciliation
                    .reconcile_on_session_start(session.endpoint_id)
                    .await;
            }
        }

        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_message_loop<S, W>(
        &self,
        write: &mut W,
        read: &mut futures_util::stream::SplitStream<&mut WebSocketStream<S>>,
        outbound_rx: &mut crate::runtime::outbound_sessions::OutboundReceiver,
        session: AuthenticatedSession,
        connection_fingerprint: ServerCertFingerprint,
    ) -> Result<(), AgentGatewayError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        W: MessageSink,
    {
        let bootstrap_evidence = self
            .bootstrap_evidence
            .as_ref()
            .ok_or(AgentGatewayError::BootstrapEvidenceServiceNotConfigured)?;

        loop {
            tokio::select! {
                frame = read.next() => {
                    let Some(frame) = frame else { return Ok(()); };
                    let frame = frame.map_err(AgentGatewayError::Receive)?;
                    match frame {
                        Message::Close(_) => return Ok(()),
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                        Message::Binary(_) => self.send_protocol_error(write, None).await?,
                        Message::Text(text) => {
                            let Ok(message) = decode(text.as_str()) else {
                                self.send_protocol_error(write, None).await?;
                                continue;
                            };
                            let id = message.envelope().message_id;
                            if !message.envelope().protocol_version.is_v1() {
                                self.send_protocol_error(write, Some(id)).await?;
                                continue;
                            }
                            match message {
                                AgentProtocolMessage::BootstrapEvidence(evidence) => {
                                    let _ = bootstrap_evidence
                                        .verify_and_establish(
                                            session.endpoint_id,
                                            &evidence,
                                            connection_fingerprint,
                                        )
                                        .await?;
                                }
                                AgentProtocolMessage::InventoryReport(report) => {
                                    let service = self
                                        .inventory
                                        .as_ref()
                                        .ok_or(AgentGatewayError::InventoryServiceNotConfigured)?;
                                    let _ = service.record(session.endpoint_id, report).await?;
                                }
                                AgentProtocolMessage::ActionAck(ack) => {
                                    self.handle_action_ack(
                                        write,
                                        session.endpoint_id,
                                        session.session_id,
                                        ack,
                                    )
                                    .await?;
                                }
                                AgentProtocolMessage::ActionResult(result) => {
                                    self.handle_action_result(write, session.endpoint_id, result)
                                        .await?;
                                }
                                AgentProtocolMessage::ActionProgress(progress) => {
                                    self.handle_action_progress(
                                        write,
                                        session.endpoint_id,
                                        id,
                                        progress,
                                    )
                                    .await?;
                                }
                                AgentProtocolMessage::CancelAck(ack) => {
                                    self.handle_cancel_ack(write, session.endpoint_id, ack)
                                        .await?;
                                }
                                AgentProtocolMessage::StatusReport(report) => {
                                    self.handle_status_report(
                                        write,
                                        session.endpoint_id,
                                        session.session_id,
                                        report,
                                    )
                                    .await?;
                                }
                                AgentProtocolMessage::TransferAuthorizationRequest(request) => {
                                    self.handle_transfer_authorization_request(
                                        write,
                                        session.endpoint_id,
                                        request,
                                    )
                                    .await?;
                                }
                                AgentProtocolMessage::ProtocolError(_) => {}
                                AgentProtocolMessage::AuthRequest(_)
                                | AgentProtocolMessage::SessionEstablished(_)
                                | AgentProtocolMessage::AuthError(_)
                                | AgentProtocolMessage::TransferAuthorizationGrant(_)
                                | AgentProtocolMessage::TransferAuthorizationDenied(_)
                                | AgentProtocolMessage::ActionDispatch(_)
                                | AgentProtocolMessage::CancelAction(_)
                                | AgentProtocolMessage::StatusQuery(_) => {
                                    // `ActionDispatch`/`CancelAction`/
                                    // `StatusQuery` are Server -> Agent
                                    // only; the Agent must never send any of
                                    // them — in particular, the Agent must
                                    // never be able to initiate Job
                                    // cancellation or decide reconciliation
                                    // on its own.
                                    self.send_protocol_error(write, Some(id)).await?;
                                }
                            }
                        }
                    }
                }
                Some(cmd) = outbound_rx.recv() => {
                    let OutboundCommand::Send { message, ack } = cmd;
                    let wire = encode(&message)
                        .expect("a well-formed outbound message always encodes");
                    let result = write.send(Message::text(wire)).await;
                    let _ = ack.send(result.map(|_| ()).map_err(|_| ()));
                }
            }
        }
    }

    /// Validates the protocol-wide `correlation_id == action_id` rule, then
    /// applies the evidence through [`ActionEvidenceService`]. Unknown/
    /// foreign `action_id` is deliberately silent and non-terminal — the
    /// Server never confirms or denies whether a foreign/unknown `action_id`
    /// exists (`m0-agent-protocol-contract.md`; Issue #26 "Authenticated
    /// Endpoint correlation").
    async fn handle_action_ack<W: MessageSink>(
        &self,
        write: &mut W,
        endpoint_id: EndpointId,
        session_id: ProtocolId,
        ack: ActionAckMessage,
    ) -> Result<(), AgentGatewayError> {
        let message_id = ack.envelope.message_id;
        if ack.envelope.correlation_id != Some(ack.body.action_id) {
            return self.send_protocol_error(write, Some(message_id)).await;
        }
        let service = self
            .action_evidence
            .as_ref()
            .ok_or(AgentGatewayError::ActionEvidenceServiceNotConfigured)?;
        let evidence = match ack.body.outcome {
            ActionAckOutcome::Accepted => ActionEvidence::AckAccepted,
            ActionAckOutcome::Rejected => ActionEvidence::AckRejected,
        };
        match service
            .apply(ack.body.action_id, endpoint_id, evidence)
            .await
        {
            Ok(result) => {
                // Issue #28 third corrective pass "Session-relevance transfer
                // after authoritative non-terminal evidence": `terminal:
                // false` on `Applied` is only ever produced by
                // `AckAccepted`'s `Dispatched -> InProgress` transition
                // (`bamep_domain::action_evidence` module docs) — i.e. this
                // durably-accepted evidence just confirmed THIS authenticated
                // session currently participates in the Agent's knowledge of
                // this action, whether or not it is the exact session
                // `ActionDispatch` originally flowed through. Rebinding only
                // AFTER the Repository has actually committed this — never
                // merely because the wire claimed `Accepted`.
                if let ApplyActionEvidenceResult::Applied(applied) = &result {
                    if !applied.terminal {
                        // The return value (Issue #28 fourth corrective pass
                        // "Late stale rebind ordering") is intentionally
                        // ignored here: `StaleActionIgnored` means this
                        // continuation resumed after the Endpoint's
                        // correlation already genuinely moved on to a later
                        // `action_id` via a newer `ActionDispatch` — exactly
                        // the safe no-op this compare-and-swap-like method
                        // exists to produce, requiring no further Gateway
                        // action.
                        let _ = self.outbound_sessions.bind_dispatch_relevant_session(
                            endpoint_id,
                            session_id,
                            ack.body.action_id,
                        );
                    }
                }
                Ok(())
            }
            Err(ApplicationError::UnknownAction) => Ok(()),
            Err(e) => Err(AgentGatewayError::Application(e)),
        }
    }

    /// Mirrors [`Self::handle_action_ack`] for `ActionResult`.
    ///
    /// `outcome: Cancelled` is deliberately never routed to either evidence
    /// service — Issue #26/#19 C2 handle only `Succeeded`/`Failed`;
    /// `Cancelled` action-specific handling belongs to Issue #27.
    ///
    /// The owning action is classified from **durable Server facts**, never
    /// from `ActionResult.detail` (Issue #19 §8/§9): a
    /// `bamep.m1.data-plane-transfer` result goes through
    /// [`TransferTerminalEvidenceService`] (RF-005 closed detail vocabulary +
    /// durable Artifact-truth gate + atomic CASE C `Incomplete -> Failed`);
    /// the RF-004 `bamep.m1.simulated-execution` path stays exactly on
    /// [`ActionEvidenceService`] with the unchanged
    /// [`crate::application::m1_result_detail_matches`] check.
    async fn handle_action_result<W: MessageSink>(
        &self,
        write: &mut W,
        endpoint_id: EndpointId,
        result: ActionResultMessage,
    ) -> Result<(), AgentGatewayError> {
        let message_id = result.envelope.message_id;
        if result.envelope.correlation_id != Some(result.body.action_id) {
            return self.send_protocol_error(write, Some(message_id)).await;
        }
        if result.body.outcome == ActionResultOutcome::Cancelled {
            return Ok(());
        }

        if let Some(transfer_terminal) = self.transfer_terminal_evidence.as_ref() {
            match transfer_terminal
                .classify(result.body.action_id, endpoint_id)
                .await
            {
                Ok(TransferActionClassification::Unknown) => {
                    // Non-enumeration: an unknown/foreign action is silently
                    // dropped, exactly like `ActionEvidenceService`.
                    return Ok(());
                }
                Ok(TransferActionClassification::DataPlaneTransfer) => {
                    let parsed = match parse_transfer_result_detail(
                        result.body.outcome,
                        &result.body.detail,
                    ) {
                        Ok(parsed) => parsed,
                        // Malformed/unknown/mismatched RF-005 detail: generic
                        // ProtocolError, no durable terminal mutation.
                        Err(_) => {
                            return self.send_protocol_error(write, Some(message_id)).await;
                        }
                    };
                    return match transfer_terminal
                        .apply(result.body.action_id, endpoint_id, parsed)
                        .await
                    {
                        Ok(TransferTerminalOutcome::Consumed) => Ok(()),
                        Ok(TransferTerminalOutcome::FailClosed) => {
                            self.send_protocol_error(write, Some(message_id)).await
                        }
                        Err(ApplicationError::UnknownAction) => Ok(()),
                        Err(e) => Err(AgentGatewayError::Application(e)),
                    };
                }
                Ok(TransferActionClassification::SimulatedExecution) => { /* RF-004 below */ }
                Err(ApplicationError::UnknownAction) => return Ok(()),
                Err(e) => return Err(AgentGatewayError::Application(e)),
            }
        }

        // RF-004 `bamep.m1.simulated-execution` path — unchanged.
        let evidence = match result.body.outcome {
            ActionResultOutcome::Succeeded => ActionEvidence::ResultSucceeded,
            ActionResultOutcome::Failed => ActionEvidence::ResultFailed,
            ActionResultOutcome::Cancelled => return Ok(()),
        };
        if !crate::application::m1_result_detail_matches(result.body.outcome, &result.body.detail) {
            return self.send_protocol_error(write, Some(message_id)).await;
        }
        let service = self
            .action_evidence
            .as_ref()
            .ok_or(AgentGatewayError::ActionEvidenceServiceNotConfigured)?;
        match service
            .apply(result.body.action_id, endpoint_id, evidence)
            .await
        {
            Ok(_) => Ok(()),
            Err(ApplicationError::UnknownAction) => Ok(()),
            Err(e) => Err(AgentGatewayError::Application(e)),
        }
    }

    /// Validates the protocol-wide `correlation_id == action_id` rule, then
    /// applies the evidence.
    ///
    /// The owning action is classified from durable Server facts, never from
    /// the wire (Issue #19 §9): a `bamep.m1.data-plane-transfer` `CancelAck`
    /// goes through [`TransferTerminalEvidenceService::apply_cancel_ack`]
    /// (Issue #27's `bamep_domain::apply_cancel_ack` decision, unchanged, plus
    /// the atomic `Incomplete -> Failed` Artifact leg when the CancelAck drives
    /// an authoritative terminal `Cancelled` for a still-`Incomplete` Artifact
    /// — Issue #19 checkpoint C4). The RF-004 `bamep.m1.simulated-execution`
    /// path — and any action with no bound `Transfer` — stays exactly on
    /// [`CancellationService`] (Issue #27 "CancelAck handling"). Unknown/
    /// foreign `action_id` is deliberately silent and non-terminal, mirroring
    /// [`Self::handle_action_ack`].
    async fn handle_cancel_ack<W: MessageSink>(
        &self,
        write: &mut W,
        endpoint_id: EndpointId,
        ack: CancelAckMessage,
    ) -> Result<(), AgentGatewayError> {
        let message_id = ack.envelope.message_id;
        if ack.envelope.correlation_id != Some(ack.body.action_id) {
            return self.send_protocol_error(write, Some(message_id)).await;
        }
        let evidence = match ack.body.outcome {
            CancelAckOutcome::Cancelled => CancelAckEvidence::Cancelled,
            CancelAckOutcome::AlreadyCompleted => CancelAckEvidence::AlreadyCompleted,
            CancelAckOutcome::CannotCancel => CancelAckEvidence::CannotCancel,
            CancelAckOutcome::Unknown => CancelAckEvidence::Unknown,
        };

        if let Some(transfer_terminal) = self.transfer_terminal_evidence.as_ref() {
            match transfer_terminal
                .classify(ack.body.action_id, endpoint_id)
                .await
            {
                Ok(TransferActionClassification::Unknown) => return Ok(()),
                Ok(TransferActionClassification::DataPlaneTransfer) => {
                    return match transfer_terminal
                        .apply_cancel_ack(ack.body.action_id, endpoint_id, evidence)
                        .await
                    {
                        Ok(TransferCancelAckOutcome::Consumed) => Ok(()),
                        // `classify` said DataPlaneTransfer under a plain read;
                        // an intervening change is possible but vanishingly
                        // rare — fall through to the generic path.
                        Ok(TransferCancelAckOutcome::NotTransferAction) => {
                            self.apply_generic_cancel_ack(ack.body.action_id, endpoint_id, evidence)
                                .await
                        }
                        Err(ApplicationError::UnknownAction) => Ok(()),
                        Err(e) => Err(AgentGatewayError::Application(e)),
                    };
                }
                Ok(TransferActionClassification::SimulatedExecution) => { /* generic below */ }
                Err(ApplicationError::UnknownAction) => return Ok(()),
                Err(e) => return Err(AgentGatewayError::Application(e)),
            }
        }

        self.apply_generic_cancel_ack(ack.body.action_id, endpoint_id, evidence)
            .await
    }

    /// The unchanged Issue #27 `CancelAck` path — [`CancellationService`].
    async fn apply_generic_cancel_ack(
        &self,
        action_id: ProtocolId,
        endpoint_id: EndpointId,
        evidence: CancelAckEvidence,
    ) -> Result<(), AgentGatewayError> {
        let service = self
            .cancellation
            .as_ref()
            .ok_or(AgentGatewayError::CancellationServiceNotConfigured)?;
        match service
            .apply_cancel_ack(action_id, endpoint_id, evidence)
            .await
        {
            Ok(_) => Ok(()),
            Err(ApplicationError::UnknownAction) => Ok(()),
            Err(e) => Err(AgentGatewayError::Application(e)),
        }
    }

    /// Validates the protocol-wide `correlation_id == action_id` rule, then
    /// applies the evidence through [`crate::application::ReconciliationService`]
    /// (Issue #28 "Gateway": inbound `StatusReport` evidence application).
    /// Unknown/foreign `action_id` is deliberately silent and non-terminal,
    /// mirroring [`Self::handle_action_ack`]/[`Self::handle_cancel_ack`].
    async fn handle_status_report<W: MessageSink>(
        &self,
        write: &mut W,
        endpoint_id: EndpointId,
        session_id: ProtocolId,
        report: StatusReportMessage,
    ) -> Result<(), AgentGatewayError> {
        let message_id = report.envelope.message_id;
        if report.envelope.correlation_id != Some(report.body.action_id) {
            return self.send_protocol_error(write, Some(message_id)).await;
        }

        // Only the authoritative terminal Cancelled outcome has an
        // additional transfer Artifact effect. Classify from durable Server
        // facts, never from the StatusReport payload, and reuse C2/C4's
        // transfer transaction. Every other status remains on #28's generic
        // reconciliation path below.
        if report.body.known_state == KnownActionState::Cancelled {
            if let Some(transfer_terminal) = self.transfer_terminal_evidence.as_ref() {
                match transfer_terminal
                    .classify(report.body.action_id, endpoint_id)
                    .await
                {
                    Ok(TransferActionClassification::Unknown) => return Ok(()),
                    Ok(TransferActionClassification::DataPlaneTransfer) => {
                        return match transfer_terminal
                            .apply_status_report_cancelled(report.body.action_id, endpoint_id)
                            .await
                        {
                            Ok(TransferStatusReportOutcome::Consumed) => Ok(()),
                            Ok(TransferStatusReportOutcome::NotTransferAction) => {
                                self.apply_generic_status_report(
                                    report.body.action_id,
                                    endpoint_id,
                                    bamep_domain::StatusReportEvidence::Cancelled,
                                )
                                .await
                            }
                            Err(ApplicationError::UnknownAction) => Ok(()),
                            Err(e) => Err(AgentGatewayError::Application(e)),
                        };
                    }
                    Ok(TransferActionClassification::SimulatedExecution) => { /* generic below */ }
                    Err(ApplicationError::UnknownAction) => return Ok(()),
                    Err(e) => return Err(AgentGatewayError::Application(e)),
                }
            }
        }

        let service = self
            .reconciliation
            .as_ref()
            .ok_or(AgentGatewayError::ReconciliationServiceNotConfigured)?;
        let evidence = match report.body.known_state {
            KnownActionState::Accepted => bamep_domain::StatusReportEvidence::Accepted,
            KnownActionState::Running => bamep_domain::StatusReportEvidence::Running,
            KnownActionState::Succeeded => bamep_domain::StatusReportEvidence::Succeeded,
            KnownActionState::Failed => bamep_domain::StatusReportEvidence::Failed,
            KnownActionState::Cancelled => bamep_domain::StatusReportEvidence::Cancelled,
            KnownActionState::Unknown => bamep_domain::StatusReportEvidence::Unknown,
        };
        match service
            .apply_status_report(report.body.action_id, endpoint_id, evidence)
            .await
        {
            Ok(result) => {
                // Issue #28 third corrective pass "Session-relevance transfer
                // after authoritative non-terminal evidence": `terminal:
                // false` on `Applied` is only ever produced by the
                // `Accepted`/`Running` `AwaitingReconciliation -> InProgress`
                // recovery (`bamep_domain::reconciliation` module docs) —
                // never by `Unknown` (always `NoOp`) or any terminal outcome.
                // This authenticated, correctly action_id-correlated session
                // just supplied the durably-accepted authoritative knowledge
                // that keeps the Attempt executing, whether or not it is the
                // exact session `ActionDispatch` originally flowed through —
                // it must become the session subsequent connection-loss
                // reconciliation depends on. Rebinding only AFTER the
                // Repository has actually committed this — never merely
                // because untrusted wire input claimed `Running`.
                if let ApplyReconciliationResult::Applied(applied) = &result {
                    if !applied.terminal {
                        // Return value intentionally ignored — see the
                        // identical note in `handle_action_ack`.
                        let _ = self.outbound_sessions.bind_dispatch_relevant_session(
                            endpoint_id,
                            session_id,
                            report.body.action_id,
                        );
                    }
                }
                Ok(())
            }
            Err(ApplicationError::UnknownAction) => Ok(()),
            Err(e) => Err(AgentGatewayError::Application(e)),
        }
    }

    async fn apply_generic_status_report(
        &self,
        action_id: ProtocolId,
        endpoint_id: EndpointId,
        evidence: bamep_domain::StatusReportEvidence,
    ) -> Result<(), AgentGatewayError> {
        let service = self
            .reconciliation
            .as_ref()
            .ok_or(AgentGatewayError::ReconciliationServiceNotConfigured)?;
        match service
            .apply_status_report(action_id, endpoint_id, evidence)
            .await
        {
            Ok(_) => Ok(()),
            Err(ApplicationError::UnknownAction) => Ok(()),
            Err(e) => Err(AgentGatewayError::Application(e)),
        }
    }

    /// Serves `TransferAuthorizationRequest`
    /// (`m0-agent-protocol-contract.md` "Transfer authorization"; Issue #38
    /// "Agent authorization request handling"). `endpoint_id` comes only
    /// from the already-authenticated session — the request body's
    /// `transfer_id` is the only Endpoint-adjacent value it may claim, and
    /// `TransferAuthorizationService::issue` independently re-verifies that
    /// the durable Transfer actually belongs to this exact `endpoint_id`
    /// before granting anything.
    ///
    /// Correlation handling (Issue #38 final correction; `m0-agent-protocol-
    /// contract.md` "Correlation": every `TransferAuthorizationRequest`/
    /// `Grant`/`Denied` MUST carry `correlation_id` equal to the owning
    /// data-plane action's `action_id`):
    ///
    /// - **no `correlation_id`** — the request is not a semantically valid
    ///   authorization request at all; it is a protocol/phase violation
    ///   answered with the generic `ProtocolError`, correlated to the
    ///   request's own `message_id`, never a wire-invalid uncorrelated
    ///   `TransferAuthorizationDenied`;
    /// - **a syntactically present but known-wrong `correlation_id`** (the
    ///   Transfer belongs to this Endpoint and its owning Attempt exists and
    ///   is current, but the presented value is not that Attempt's own
    ///   `action_id`) — also a generic `ProtocolError`, correlated to the
    ///   request's `message_id`. Emitting `TransferAuthorizationDenied`
    ///   correlated to the presented value here would itself violate the
    ///   same wire rule (a `Denied` message MUST carry the owning
    ///   `action_id`), and substituting the durable owning `action_id` in
    ///   would unnecessarily reveal it — so this case is a protocol
    ///   violation, not a denial, and the durable owning `action_id` is never
    ///   sent;
    /// - **the correct `correlation_id`** — the normal decision runs; on
    ///   semantic denial the response echoes exactly that `correlation_id`
    ///   (which is, by construction, the owning `action_id`).
    async fn handle_transfer_authorization_request<W: MessageSink>(
        &self,
        write: &mut W,
        endpoint_id: EndpointId,
        request: TransferAuthorizationRequestMessage,
    ) -> Result<(), AgentGatewayError> {
        let service = self
            .transfer_authorization
            .as_ref()
            .ok_or(AgentGatewayError::TransferAuthorizationServiceNotConfigured)?;

        let transfer_id = request.body.transfer_id;
        let Some(correlation_id) = request.envelope.correlation_id else {
            return self
                .send_protocol_error(write, Some(request.envelope.message_id))
                .await;
        };

        let outcome = service
            .issue(
                endpoint_id,
                correlation_id,
                bamep_domain::TransferId(transfer_id.as_uuid()),
                &request.body.proof_public_key,
            )
            .await?;

        match outcome {
            TransferAuthorizationOutcome::Granted {
                token,
                expires_at,
                data_plane_base_url,
            } => {
                let grant = TransferAuthorizationGrantMessage::new(
                    correlation_id,
                    transfer_id,
                    token,
                    MessageTimestamp::from_datetime(expires_at),
                    data_plane_base_url,
                );
                let wire = encode(&AgentProtocolMessage::TransferAuthorizationGrant(grant))
                    .expect("a well-formed TransferAuthorizationGrant always encodes");
                write
                    .send(Message::text(wire))
                    .await
                    .map_err(AgentGatewayError::Send)
            }
            TransferAuthorizationOutcome::Denied => {
                self.send_transfer_authorization_denied(write, transfer_id, correlation_id)
                    .await
            }
            TransferAuthorizationOutcome::ProtocolViolation => {
                self.send_protocol_error(write, Some(request.envelope.message_id))
                    .await
            }
        }
    }

    /// `reason` is intentionally the single generic constant
    /// (`m0-agent-protocol-contract.md` "Renewal and restart": "V1 may use
    /// one closed generic value and must not distinguish unknown transfer,
    /// wrong Endpoint, terminal transfer, or other internal denial causes").
    /// `correlation_id` is the exact value the client presented on its
    /// request — always present (a request without one is handled as a
    /// `ProtocolError` upstream).
    async fn send_transfer_authorization_denied<W: MessageSink>(
        &self,
        write: &mut W,
        transfer_id: ProtocolId,
        correlation_id: ProtocolId,
    ) -> Result<(), AgentGatewayError> {
        let denied = TransferAuthorizationDeniedMessage::new(
            correlation_id,
            transfer_id,
            GENERIC_TRANSFER_AUTHORIZATION_DENIED_REASON,
        );
        let wire = encode(&AgentProtocolMessage::TransferAuthorizationDenied(denied))
            .expect("a well-formed TransferAuthorizationDenied always encodes");
        write
            .send(Message::text(wire))
            .await
            .map_err(AgentGatewayError::Send)
    }

    /// Transient/advisory only — never persisted, never mutates Attempt/
    /// JobStep/Job lifecycle state (`m0-agent-protocol-contract.md`
    /// "ActionProgress fields"). Still enforced for two correlation rules:
    /// self-correlation (`correlation_id == action_id`) and — Issue #26
    /// "Correlate ActionProgress to the authenticated Endpoint" — that
    /// `action_id` actually belongs to an Attempt whose Job targets this
    /// authenticated Endpoint. Both violations respond identically with a
    /// generic `ProtocolError`; this method never distinguishes a
    /// self-correlation violation from an unknown/foreign `action_id`, and
    /// never creates a PostgreSQL progress row (there is none to create).
    async fn handle_action_progress<W: MessageSink>(
        &self,
        write: &mut W,
        endpoint_id: EndpointId,
        message_id: ProtocolId,
        progress: ActionProgressMessage,
    ) -> Result<(), AgentGatewayError> {
        if progress.envelope.correlation_id != Some(progress.body.action_id) {
            return self.send_protocol_error(write, Some(message_id)).await;
        }
        let service = self
            .action_evidence
            .as_ref()
            .ok_or(AgentGatewayError::ActionEvidenceServiceNotConfigured)?;
        let belongs = service
            .action_belongs_to_endpoint(progress.body.action_id, endpoint_id)
            .await?;
        if !belongs {
            return self.send_protocol_error(write, Some(message_id)).await;
        }
        Ok(())
    }

    async fn send_protocol_error<W: MessageSink>(
        &self,
        write: &mut W,
        correlation_id: Option<ProtocolId>,
    ) -> Result<(), AgentGatewayError> {
        let mut error =
            ProtocolErrorMessage::new(GENERIC_PROTOCOL_ERROR_CODE, GENERIC_PROTOCOL_ERROR_MESSAGE);
        if let Some(id) = correlation_id {
            error = error.with_correlation_id(id);
        }
        let wire = encode(&AgentProtocolMessage::ProtocolError(error))
            .expect("a well-formed ProtocolError always encodes");
        write
            .send(Message::text(wire))
            .await
            .map_err(AgentGatewayError::Send)
    }

    /// Drives the Agent Protocol v1 handshake phase on an already-established
    /// WebSocket. Borrows `websocket` rather than consuming it: on
    /// [`HandshakeOutcome::Established`], the caller retains the same open
    /// connection for the next checkpoint (`BootstrapEvidence`) — this method
    /// never closes it and never reads past `SessionEstablished`.
    pub async fn handshake<S>(
        &self,
        websocket: &mut WebSocketStream<S>,
    ) -> Result<HandshakeOutcome, AgentGatewayError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            let frame = websocket
                .next()
                .await
                .ok_or(AgentGatewayError::ConnectionClosed)?
                .map_err(AgentGatewayError::Receive)?;

            match frame {
                Message::Text(text) => return self.handle_text(websocket, text.as_str()).await,
                // Agent Protocol v1 is UTF-8 JSON in TEXT frames only
                // (`m0-agent-protocol-contract.md` "Wire encoding") — a
                // binary payload during the handshake is rejected outright,
                // never decoded.
                Message::Binary(_) => return self.reject(websocket, None).await,
                // A Close frame before `AuthRequest` means the handshake did
                // not complete — a genuine processing outcome, not an
                // expected authentication rejection.
                Message::Close(_) => return Err(AgentGatewayError::ConnectionClosed),
                // Control frames tungstenite already handles at the protocol
                // level (e.g. auto-queuing a Pong reply) without becoming
                // Agent Protocol messages; `Frame` is never returned by a
                // read per tungstenite's own contract.
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            }
        }
    }

    async fn handle_text<S>(
        &self,
        websocket: &mut WebSocketStream<S>,
        text: &str,
    ) -> Result<HandshakeOutcome, AgentGatewayError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // Malformed JSON and an unknown top-level `type` are both a
        // `DecodeError` here (`bamep_agent_protocol::codec`): neither yields
        // a trustworthy `message_id`, so no partial/unsafe extraction is
        // attempted — the response omits `correlation_id` entirely.
        let Ok(message) = decode(text) else {
            return self.reject(websocket, None).await;
        };

        let AgentProtocolMessage::AuthRequest(auth_request) = message else {
            // Known message, wrong phase (e.g. `BootstrapEvidence`, `InventoryReport`,
            // `SessionEstablished`, or `AuthError` sent by the Agent before
            // authentication). Decoding succeeded, so this message_id is
            // trustworthy and is used for correlation.
            let message_id = message.envelope().message_id;
            return self.reject(websocket, Some(message_id)).await;
        };

        self.handle_auth_request(websocket, auth_request).await
    }

    async fn handle_auth_request<S>(
        &self,
        websocket: &mut WebSocketStream<S>,
        auth_request: AuthRequestMessage,
    ) -> Result<HandshakeOutcome, AgentGatewayError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let message_id = auth_request.envelope.message_id;

        // CRITICAL ordering: protocol_version is checked before redeem is
        // ever called. An incompatible version must never consume/rotate an
        // otherwise-valid credential.
        if !auth_request.envelope.protocol_version.is_v1() {
            return self.reject(websocket, Some(message_id)).await;
        }

        // Persist-before-send (ADR-0012): `redeem` returns only after the
        // accepted credential/identity transition has already committed. A
        // repository/Application failure here is a genuine Gateway error,
        // never reinterpreted as a credential rejection.
        let outcome = self
            .enrollment
            .redeem(&auth_request.body.credential)
            .await?;

        match outcome {
            RedeemResult::Rejected => self.reject(websocket, Some(message_id)).await,
            RedeemResult::Established {
                endpoint_id,
                runtime_credential,
                credential_expires_at,
            } => {
                let session_id = ProtocolId::generate();
                let response = SessionEstablishedMessage::new(
                    session_id,
                    runtime_credential.to_wire_value(),
                    MessageTimestamp::from_datetime(credential_expires_at),
                )
                .with_correlation_id(message_id);

                let wire = encode(&AgentProtocolMessage::SessionEstablished(response))
                    .expect("a well-formed SessionEstablished always encodes");
                websocket
                    .send(Message::text(wire))
                    .await
                    .map_err(AgentGatewayError::Send)?;

                Ok(HandshakeOutcome::Established(AuthenticatedSession {
                    endpoint_id,
                    session_id,
                }))
            }
        }
    }

    /// Sends the single generic `AuthError` this checkpoint ever produces and
    /// returns [`HandshakeOutcome::Rejected`]. `correlation_id` is set only
    /// when a trustworthy `message_id` was already decoded.
    async fn reject<S>(
        &self,
        websocket: &mut WebSocketStream<S>,
        correlation_id: Option<ProtocolId>,
    ) -> Result<HandshakeOutcome, AgentGatewayError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut error = AuthErrorMessage::new(GENERIC_AUTH_ERROR_REASON);
        if let Some(id) = correlation_id {
            error = error.with_correlation_id(id);
        }
        let wire = encode(&AgentProtocolMessage::AuthError(error))
            .expect("a well-formed AuthError always encodes");
        websocket
            .send(Message::text(wire))
            .await
            .map_err(AgentGatewayError::Send)?;
        Ok(HandshakeOutcome::Rejected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::AgentDispatchPort;
    use bamep_agent_protocol::ActionDispatchMessage;

    /// Component-level regression proof for `SessionLifecycleGuard::drop`:
    /// dropping it removes this exact `SessionId` from BOTH registries, not
    /// only one — guarding against a future edit accidentally dropping one
    /// of the two `unregister` calls. `drop`'s two statements execute
    /// sequentially, single-threaded, with no `.await` between them, so the
    /// presence-then-outbound *order* this correction requires is guaranteed
    /// by ordinary Rust control flow (visible by reading the two-line `drop`
    /// body above) rather than something a black-box runtime test could
    /// observe without sleeps/instrumentation — there is no async gap for an
    /// external observer to ever see an in-between state.
    #[tokio::test]
    async fn dropping_the_lifecycle_guard_unregisters_both_registries() {
        let presence = Arc::new(PresenceRegistry::new());
        let outbound_sessions = Arc::new(OutboundSessionDirectory::new());
        let endpoint_id = EndpointId::new();
        let session_id = ProtocolId::generate();

        presence.register(endpoint_id, session_id);
        let (tx, _rx) = outbound_channel();
        outbound_sessions.register(endpoint_id, session_id, tx);
        assert!(presence.is_present(endpoint_id));

        let guard = SessionLifecycleGuard {
            presence: Arc::clone(&presence),
            outbound_sessions: Arc::clone(&outbound_sessions),
            endpoint_id,
            session_id,
        };
        drop(guard);

        assert!(
            !presence.is_present(endpoint_id),
            "drop must remove presence"
        );
        // No direct "is registered" query exists on OutboundSessionDirectory
        // beyond `AgentDispatchPort::dispatch_action` itself — checking that
        // it now reports `NoSession` is the Port-level proof that outbound
        // delivery was also removed.
        let dispatch_result = outbound_sessions
            .dispatch_action(
                endpoint_id,
                ActionDispatchMessage::new(
                    ProtocolId::generate(),
                    "bamep.m1.simulated-execution",
                    "1",
                    serde_json::Map::new(),
                ),
            )
            .await;
        assert_eq!(
            dispatch_result,
            Err(crate::ports::AgentDispatchError::NoSession),
            "drop must also remove the outbound delivery registration"
        );
    }
}
