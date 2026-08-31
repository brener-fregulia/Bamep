//! Custom pinned `rustls::client::danger::ServerCertVerifier`
//! (`docs/specifications/m0-agent-protocol-contract.md` "Transport and
//! handshake"): the exact leaf-certificate fingerprint, authenticated
//! through trusted bootstrap before this connection began, is the Server
//! identity authority — not Web/Public PKI, not a root store, not
//! hostname/DNS validation, and not X.509 validity-period enforcement.
//!
//! This does not bypass TLS handshake signature verification: the peer
//! must still prove possession of the private key corresponding to the
//! pinned leaf certificate. `verify_tls12_signature`/`verify_tls13_signature`
//! delegate to rustls's own `ring` `CryptoProvider` signature-verification
//! algorithms rather than reimplementing TLS signature verification, and
//! never return an unconditional `assertion()`.

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};

use bamep_trusted_bootstrap::ServerCertFingerprint;

/// Builds a TLS 1.3-only `rustls` client configuration that authenticates the
/// Server by the single exact leaf-certificate fingerprint `expected`
/// ([`PinnedServerCertVerifier`]) and presents no client certificate — the
/// identical trust model the WSS control plane already uses
/// (`docs/specifications/m0-agent-protocol-contract.md` "Transport and
/// handshake"). ADR-0018 and `m0-agent-protocol-contract.md` "Endpoint
/// discovery for the data-plane listener" require the Worker-owned
/// `data_plane_base_url` HTTPS origin to be verified against the *same* trusted
/// leaf fingerprint with the *same* exact-pin comparison — never
/// hostname/DNS/Web-PKI — so both [`crate::transport::connect_pinned_wss`] and
/// the Agent-side data-plane HTTPS client
/// ([`crate::data_plane::DataPlaneClient`]) build their client config here.
///
/// `alpn_protocols` is applied verbatim: empty for the WSS control plane,
/// `[b"http/1.1".to_vec()]` for the Worker HTTP/1.1 data plane.
pub fn pinned_tls13_client_config(
    expected: ServerCertFingerprint,
    alpn_protocols: Vec<Vec<u8>>,
) -> Result<ClientConfig, RustlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerCertVerifier::new(expected)))
        .with_no_client_auth();
    config.alpn_protocols = alpn_protocols;
    Ok(config)
}

/// Verifies the Server's presented leaf certificate against a single
/// authenticated expected [`ServerCertFingerprint`]. `server_name`/SNI is
/// still required by the TLS/rustls API surface, but this verifier never
/// treats it as an identity authority — see [`crate::transport::connect_pinned_wss`]
/// for the narrow rationale on why an arbitrary stable name (e.g.
/// `"localhost"`) is acceptable even against a self-signed certificate.
pub struct PinnedServerCertVerifier {
    expected: ServerCertFingerprint,
    provider: Arc<CryptoProvider>,
}

impl PinnedServerCertVerifier {
    pub fn new(expected: ServerCertFingerprint) -> Self {
        Self {
            expected,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }
}

impl fmt::Debug for PinnedServerCertVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedServerCertVerifier")
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for PinnedServerCertVerifier {
    /// The certificate-identity decision: exact leaf-DER fingerprint match
    /// or bust. A mismatch is a generic TLS/certificate verification
    /// failure — it deliberately does not log the expected/actual
    /// fingerprint values (neither is secret, but there is no need to
    /// pair them in ordinary error text).
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        if self.expected.matches_leaf_der(end_entity.as_ref()) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(RustlsError::General(
                "server certificate fingerprint mismatch".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
