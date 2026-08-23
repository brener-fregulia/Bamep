//! Simulator-side Agent Protocol v1 handshake helper, over an
//! already-established pinned WSS connection ([`crate::transport`]).
//! Kept separate from `connect_pinned_wss`: transport establishes WSS,
//! this module uses the established WSS
//! (`docs/specifications/m0-agent-protocol-contract.md` "Transport and
//! handshake").
//!
//! `runtime_credential` on a successful [`SimulatorHandshakeOutcome::Established`]
//! is deliberately left as the opaque wire string Agent Protocol carries it
//! as — this crate does not parse it as a Domain `PresentedCredential`, and
//! does not implement credential retention/retry policy.

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use bamep_agent_protocol::{
    decode, encode, AgentProtocolMessage, AuthErrorMessage, AuthRequestMessage,
    BootstrapEvidenceMessage, DecodeError, InventoryReportMessage, SessionEstablishedMessage,
};
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum SimulatorHandshakeError {
    #[error("failed to send AuthRequest")]
    Send(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("failed to receive the handshake response")]
    Receive(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("connection closed before a handshake response was received")]
    ConnectionClosed,
    #[error("received a non-text handshake response frame")]
    UnexpectedFrame,
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("received an unexpected Agent Protocol message type during the handshake")]
    UnexpectedMessageType,
    #[error("the handshake response protocol_version is not v1")]
    UnsupportedProtocolVersion,
    #[error("the handshake response correlation_id is missing or does not match the AuthRequest")]
    CorrelationMismatch,
}

pub async fn send_bootstrap_evidence<S>(
    websocket: &mut WebSocketStream<S>,
    established: &crate::trusted_bootstrap::EstablishedTrustedBootstrap,
) -> Result<(), SimulatorHandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let evidence = BootstrapEvidenceMessage::new(
        established.boot_nonce().to_wire_value(),
        established.assertion_wire_value(),
    );
    let wire = encode(&AgentProtocolMessage::BootstrapEvidence(evidence))
        .expect("established bootstrap evidence always encodes");
    websocket
        .send(Message::text(wire))
        .await
        .map_err(SimulatorHandshakeError::Send)
}

pub async fn send_inventory_report<S>(
    websocket: &mut WebSocketStream<S>,
    inventory: Map<String, Value>,
) -> Result<(), SimulatorHandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let wire = encode(&AgentProtocolMessage::InventoryReport(
        InventoryReportMessage::new(inventory),
    ))
    .expect("an inventory JSON object always encodes");
    websocket
        .send(Message::text(wire))
        .await
        .map_err(SimulatorHandshakeError::Send)
}

/// The Simulator's own view of a handshake result: either message, carried
/// in full for the caller to inspect.
#[derive(Debug)]
pub enum SimulatorHandshakeOutcome {
    Established(SessionEstablishedMessage),
    Rejected(AuthErrorMessage),
}

/// Sends `AuthRequest{credential_wire}` over `websocket` and validates the
/// handshake response: `protocol_version` must be v1, and `correlation_id`
/// must be present and equal to the `AuthRequest`'s own `message_id` — this
/// Server implementation deliberately correlates handshake responses, so a
/// missing/mismatched `correlation_id` is treated as a protocol failure here,
/// not tolerated as absent.
///
/// Does not make hostname/TLS decisions — pinned TLS already happened before
/// this is called.
pub async fn authenticate<S>(
    websocket: &mut WebSocketStream<S>,
    credential_wire: &str,
) -> Result<SimulatorHandshakeOutcome, SimulatorHandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = AuthRequestMessage::new(credential_wire);
    let request_message_id = request.envelope.message_id;

    let wire = encode(&AgentProtocolMessage::AuthRequest(request))
        .expect("a well-formed AuthRequest always encodes");
    websocket
        .send(Message::text(wire))
        .await
        .map_err(SimulatorHandshakeError::Send)?;

    let frame = websocket
        .next()
        .await
        .ok_or(SimulatorHandshakeError::ConnectionClosed)?
        .map_err(SimulatorHandshakeError::Receive)?;

    let Message::Text(text) = frame else {
        return Err(SimulatorHandshakeError::UnexpectedFrame);
    };

    let message = decode(text.as_str())?;

    match message {
        AgentProtocolMessage::SessionEstablished(established) => {
            validate_response_envelope(&established.envelope, request_message_id)?;
            Ok(SimulatorHandshakeOutcome::Established(established))
        }
        AgentProtocolMessage::AuthError(error) => {
            validate_response_envelope(&error.envelope, request_message_id)?;
            Ok(SimulatorHandshakeOutcome::Rejected(error))
        }
        _ => Err(SimulatorHandshakeError::UnexpectedMessageType),
    }
}

fn validate_response_envelope(
    envelope: &bamep_agent_protocol::Envelope,
    request_message_id: bamep_agent_protocol::ProtocolId,
) -> Result<(), SimulatorHandshakeError> {
    if !envelope.protocol_version.is_v1() {
        return Err(SimulatorHandshakeError::UnsupportedProtocolVersion);
    }
    if envelope.correlation_id != Some(request_message_id) {
        return Err(SimulatorHandshakeError::CorrelationMismatch);
    }
    Ok(())
}
