//! Pinned TLS 1.3 client config for the probe. The Server's exact leaf
//! certificate SHA-256 is the identity authority — not Web PKI, not a root
//! store, not hostname validation (`m0-agent-protocol-contract.md`
//! "Transport and handshake"; `m0-trusted-bootstrap-and-server-fingerprint-
//! contract.md`). Signature verification of the handshake still runs via
//! rustls's own ring provider; only chain/name validation is replaced by the
//! exact-fingerprint check.
//!
//! This mirrors `bamep_simulator::verifier::PinnedServerCertVerifier`;
//! reimplemented here (over `sha2`) so the WinPE probe stays a small
//! standalone build and does not pull the Simulator crate.

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use sha2::{Digest, Sha256};

pub fn parse_pin_hex(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn leaf_sha256(der: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(der);
    let mut out = [0u8; 32];
    out.copy_from_slice(hasher.finalize().as_slice());
    out
}

pub fn pinned_tls13_client_config(expected_leaf_sha256: [u8; 32]) -> Result<ClientConfig, RustlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerCertVerifier::new(
            expected_leaf_sha256,
        )))
        .with_no_client_auth())
}

struct PinnedServerCertVerifier {
    expected: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl PinnedServerCertVerifier {
    fn new(expected: [u8; 32]) -> Self {
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
        if leaf_sha256(end_entity.as_ref()) == self.expected {
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
