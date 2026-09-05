//! Pinned TLS 1.3 client config, used only by the harness `selftest`
//! subcommand to exercise its own `AgentTransportAcceptor` end to end.
//! Mirrors `bamep_simulator::verifier::PinnedServerCertVerifier` — the exact
//! leaf-certificate fingerprint is the Server identity authority; no Web PKI,
//! no root store, no hostname validation. Copied rather than imported so the
//! harness does not pull the whole `bamep-simulator` crate.

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};

use bamep_trusted_bootstrap::ServerCertFingerprint;

pub fn pinned_tls13_client_config(expected: ServerCertFingerprint) -> Result<ClientConfig, RustlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerCertVerifier::new(expected)))
        .with_no_client_auth())
}

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
        f.debug_struct("PinnedServerCertVerifier").finish_non_exhaustive()
    }
}

impl ServerCertVerifier for PinnedServerCertVerifier {
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
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
