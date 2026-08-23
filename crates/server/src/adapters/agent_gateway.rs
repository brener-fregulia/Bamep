//! Agent Control Gateway: Agent Protocol v1 handshake semantics over an
//! already-established WebSocket, plus the authenticated post-session
//! message loop (`docs/specifications/m0-agent-protocol-contract.md`
//! "Transport and handshake", "Runtime credential issuance and rotation").
//!
//! Boundary (Issue #17 handshake checkpoint, extended by Issue #18's
//! post-session `InventoryReport`): `agent_transport` owns TCP/TLS/WebSocket
//! establishment; this module owns only what happens on the Agent Protocol
//! JSON stream once that WebSocket already exists —
//! `AuthRequest` -> `EnrollmentService::redeem` -> `SessionEstablished`/
//! `AuthError`. It never touches TLS/fingerprint verification (already
//! complete by the time [`AgentControlGateway::handshake`] is called), never
//! contains SQL, and never re-derives a Domain/Application decision — every
//! accept/reject decision is exactly the one `EnrollmentService::redeem`
//! already made.
//!
//! After authentication this module drives the session loop, delegating
//! `BootstrapEvidence` verification and `InventoryReport` recording to their
//! respective Application services, and handles post-session protocol
//! violations.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use bamep_agent_protocol::{
    decode, encode, AgentProtocolMessage, AuthErrorMessage, AuthRequestMessage, MessageTimestamp,
    ProtocolErrorMessage, ProtocolId, SessionEstablishedMessage,
};
use bamep_domain::EndpointId;
use bamep_trusted_bootstrap::ServerCertFingerprint;

use crate::application::{
    ApplicationError, BootstrapEvidenceService, EnrollmentService, RedeemResult,
};
use crate::ports::{CredentialRedemptionRepository, EndpointRepository};
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

/// The result of a successful Agent Protocol v1 handshake. `session_id` is a
/// fresh, transient value — not persisted to PostgreSQL in WP1 (no session
/// repository/table exists).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedSession {
    pub endpoint_id: EndpointId,
    pub session_id: ProtocolId,
}

/// Reliably unregisters exactly one `(endpoint_id, session_id)` presence
/// registration on drop — covering the normal `Ok(())` return, every `?`
/// early-return, and an unwinding panic alike (`m0-stack-and-boundaries-baseline.md`
/// "Runtime Presence Registry"). Constructed only by
/// [`AgentControlGateway::run_authenticated_session`].
struct PresenceGuard {
    registry: Arc<PresenceRegistry>,
    endpoint_id: EndpointId,
    session_id: ProtocolId,
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        self.registry.unregister(self.endpoint_id, self.session_id);
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
    #[error(transparent)]
    Application(#[from] ApplicationError),
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
    /// The Runtime Presence Registry this Gateway's authenticated sessions
    /// register with/unregister from (`m0-stack-and-boundaries-baseline.md`
    /// "Runtime Presence Registry"). Owned by the Gateway by default so every
    /// `AgentControlGateway::new` caller gets working presence tracking
    /// without opting in; [`Self::with_presence_registry`] lets a caller
    /// share one `PresenceRegistry` instance across multiple Runtime
    /// Services (e.g. a future scheduler that must observe the same
    /// presence facts).
    presence: Arc<PresenceRegistry>,
}

impl<R: EndpointRepository, C: CredentialRedemptionRepository> AgentControlGateway<R, C> {
    pub fn new(enrollment: Arc<EnrollmentService<R, C>>) -> Self {
        Self {
            enrollment,
            bootstrap_evidence: None,
            inventory: None,
            presence: Arc::new(PresenceRegistry::new()),
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

    pub fn with_presence_registry(mut self, presence: Arc<PresenceRegistry>) -> Self {
        self.presence = presence;
        self
    }

    /// The shared [`PresenceRegistry`] this Gateway's authenticated sessions
    /// register with.
    pub fn presence(&self) -> Arc<PresenceRegistry> {
        Arc::clone(&self.presence)
    }

    /// Runs the authenticated post-handshake phase on the same WebSocket.
    /// Evidence rejection is deliberately silent and non-terminal.
    pub async fn run_authenticated_session<S>(
        &self,
        websocket: &mut WebSocketStream<S>,
        session: AuthenticatedSession,
        connection_fingerprint: ServerCertFingerprint,
    ) -> Result<(), AgentGatewayError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let bootstrap_evidence = self
            .bootstrap_evidence
            .as_ref()
            .ok_or(AgentGatewayError::BootstrapEvidenceServiceNotConfigured)?;

        // Registration happens only here — after the caller has already
        // confirmed `HandshakeOutcome::Established` and every configuration
        // precondition above has passed — never for a rejected/failed
        // authentication. `_presence_guard`'s `Drop` unregisters exactly this
        // `SessionId` on every exit path below: normal Close/disconnect
        // (`Ok(())`), a genuine Gateway error (`?`), and an unwinding panic
        // alike, so cleanup is reliable even on error paths.
        self.presence
            .register(session.endpoint_id, session.session_id);
        let _presence_guard = PresenceGuard {
            registry: Arc::clone(&self.presence),
            endpoint_id: session.endpoint_id,
            session_id: session.session_id,
        };

        loop {
            let Some(frame) = websocket.next().await else {
                return Ok(());
            };
            let frame = frame.map_err(AgentGatewayError::Receive)?;
            match frame {
                Message::Close(_) => return Ok(()),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Binary(_) => self.send_protocol_error(websocket, None).await?,
                Message::Text(text) => {
                    let Ok(message) = decode(text.as_str()) else {
                        self.send_protocol_error(websocket, None).await?;
                        continue;
                    };
                    let id = message.envelope().message_id;
                    if !message.envelope().protocol_version.is_v1() {
                        self.send_protocol_error(websocket, Some(id)).await?;
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
                        AgentProtocolMessage::ProtocolError(_) => {}
                        AgentProtocolMessage::AuthRequest(_)
                        | AgentProtocolMessage::SessionEstablished(_)
                        | AgentProtocolMessage::AuthError(_) => {
                            self.send_protocol_error(websocket, Some(id)).await?;
                        }
                    }
                }
            }
        }
    }

    async fn send_protocol_error<S>(
        &self,
        websocket: &mut WebSocketStream<S>,
        correlation_id: Option<ProtocolId>,
    ) -> Result<(), AgentGatewayError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut error =
            ProtocolErrorMessage::new(GENERIC_PROTOCOL_ERROR_CODE, GENERIC_PROTOCOL_ERROR_MESSAGE);
        if let Some(id) = correlation_id {
            error = error.with_correlation_id(id);
        }
        let wire = encode(&AgentProtocolMessage::ProtocolError(error))
            .expect("a well-formed ProtocolError always encodes");
        websocket
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
