//! Simulator-side TCP -> pinned TLS 1.3 -> WebSocket Upgrade composition
//! (`docs/specifications/m0-agent-protocol-contract.md` "Transport and
//! handshake"). `tokio-tungstenite`'s own TLS connector is deliberately not
//! used: TLS authentication is established explicitly, in full, before the
//! WebSocket Upgrade is even attempted — the required security ordering is
//! structural, not a configuration choice.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::pki_types::{InvalidDnsNameError, ServerName};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::WebSocketStream;

use crate::verifier::pinned_tls13_client_config;
use bamep_trusted_bootstrap::ServerCertFingerprint;

#[derive(Debug, thiserror::Error)]
pub enum SimulatorTransportError {
    #[error("failed to build TLS client configuration")]
    TlsConfig(#[source] rustls::Error),
    #[error("invalid TLS server name")]
    ServerName(#[source] InvalidDnsNameError),
    #[error("TCP connect failed")]
    Connect(#[source] std::io::Error),
    #[error("TLS handshake failed (pinned Server certificate verification, or peer TLS signature verification, failed)")]
    TlsHandshake(#[source] std::io::Error),
    #[error("WebSocket upgrade failed")]
    WebSocketUpgrade(#[source] tokio_tungstenite::tungstenite::Error),
}

/// Connects to `addr`, performs a TLS 1.3 handshake with the Server's
/// certificate pinned to `expected_fingerprint`
/// ([`PinnedServerCertVerifier`]), and only once that succeeds performs the
/// WebSocket Upgrade. A pin mismatch (or any other TLS failure) returns
/// `Err` before any `WebSocketStream` exists — the caller structurally
/// cannot reach WebSocket or Agent Protocol code on failure.
///
/// `server_name` is required by the TLS/SNI machinery, but is not an
/// identity authority in this architecture: the pinned exact leaf
/// fingerprint is. Using e.g. `"localhost"` for a loopback test/fixture
/// connection is acceptable even against a self-signed certificate.
pub async fn connect_pinned_wss(
    addr: SocketAddr,
    server_name: &str,
    expected_fingerprint: ServerCertFingerprint,
) -> Result<WebSocketStream<TlsStream<TcpStream>>, SimulatorTransportError> {
    // The WSS control plane uses no ALPN; the identical exact-leaf-pin trust
    // model is shared with the Worker data-plane HTTPS client via this helper.
    let config = pinned_tls13_client_config(expected_fingerprint, Vec::new())
        .map_err(SimulatorTransportError::TlsConfig)?;

    let connector = TlsConnector::from(Arc::new(config));

    let tcp_stream = TcpStream::connect(addr)
        .await
        .map_err(SimulatorTransportError::Connect)?;

    let dns_name = ServerName::try_from(server_name.to_string())
        .map_err(SimulatorTransportError::ServerName)?;

    let tls_stream = connector
        .connect(dns_name, tcp_stream)
        .await
        .map_err(SimulatorTransportError::TlsHandshake)?;

    let (ws_stream, _response) = tokio_tungstenite::client_async("wss://bamep-agent/", tls_stream)
        .await
        .map_err(SimulatorTransportError::WebSocketUpgrade)?;

    Ok(ws_stream)
}
