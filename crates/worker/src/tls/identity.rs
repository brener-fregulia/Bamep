//! Loads the same Server TLS identity the Agent already trusts
//! (ADR-0018 "TLS identity": "The Worker uses the same Server TLS identity
//! already trusted by the Agent. No second Server identity, trust anchor,
//! CA hierarchy, mTLS, or Web PKI is introduced.") from host-local
//! certificate/private-key PEM files.
//!
//! Uses `rustls-pemfile` — the same rustls-ecosystem crate family already
//! used transitively by this workspace's TLS stack — to parse PEM into the
//! `rustls::pki_types` DER types `crates/server/src/adapters/agent_transport.rs`
//! already accepts already-loaded; this module is what performs that
//! loading for Worker, independently of `bamep-server` (Issue #37 "TLS
//! configuration ownership": avoid moving private-key loading into
//! Domain/trusted-bootstrap, and do not broadly refactor the existing Agent
//! transport).

use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use bamep_trusted_bootstrap::ServerCertFingerprint;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

/// Exactly TLS 1.3, mirroring the existing Agent transport's own choice
/// (`crates/server/src/adapters/agent_transport.rs`).
const SUPPORTED_TLS_VERSIONS: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];

#[derive(Debug, thiserror::Error)]
pub enum TlsIdentityError {
    #[error("failed to read {path}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} contains no PEM certificate")]
    NoCertificate { path: String },
    #[error("{path} contains no PEM private key")]
    NoPrivateKey { path: String },
    #[error("failed to parse PEM material in {path}")]
    MalformedPem {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the loaded certificate/private key pair is not usable for TLS server termination")]
    TlsConfig(#[source] rustls::Error),
}

/// The Server TLS identity Worker loaded independently from `bamepd`
/// configuration — the exact same leaf certificate the Agent trusts.
pub struct ServerTlsIdentity {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub private_key: PrivateKeyDer<'static>,
    /// SHA-256 over the exact leaf certificate DER bytes
    /// (`bamep_trusted_bootstrap::ServerCertFingerprint`), so tests/
    /// diagnostics can prove Worker loaded the same identity as `bamepd`
    /// without ever handling the private key bytes themselves.
    pub fingerprint: ServerCertFingerprint,
}

/// Manual `Debug`, never derived: `private_key` must never appear in debug
/// output (`m1-worker-data-plane-control-contract.md` "Security and
/// logging": "the Server TLS private key ... MUST be redacted from logs and
/// debug output").
impl std::fmt::Debug for ServerTlsIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerTlsIdentity")
            .field("cert_chain_len", &self.cert_chain.len())
            .field("private_key", &"REDACTED")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

pub fn load_server_identity(
    cert_path: &Path,
    key_path: &Path,
) -> Result<ServerTlsIdentity, TlsIdentityError> {
    let cert_chain = load_certificate_chain(cert_path)?;
    let private_key = load_private_key(key_path)?;
    let leaf = cert_chain
        .first()
        .ok_or_else(|| TlsIdentityError::NoCertificate {
            path: cert_path.display().to_string(),
        })?;
    let fingerprint = ServerCertFingerprint::from_leaf_der(leaf.as_ref());

    Ok(ServerTlsIdentity {
        cert_chain,
        private_key,
        fingerprint,
    })
}

fn load_certificate_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsIdentityError> {
    let bytes = std::fs::read(path).map_err(|source| TlsIdentityError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut reader = BufReader::new(bytes.as_slice());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .map_err(|source| TlsIdentityError::MalformedPem {
            path: path.display().to_string(),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsIdentityError::NoCertificate {
            path: path.display().to_string(),
        });
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsIdentityError> {
    let bytes = std::fs::read(path).map_err(|source| TlsIdentityError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut reader = BufReader::new(bytes.as_slice());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|source| TlsIdentityError::MalformedPem {
            path: path.display().to_string(),
            source,
        })?
        .ok_or_else(|| TlsIdentityError::NoPrivateKey {
            path: path.display().to_string(),
        })
}

/// Proves the loaded identity is actually usable by rustls to terminate
/// Server TLS: TLS 1.3 only, no client-certificate authentication, the
/// `ring` `CryptoProvider` explicitly selected — the same configuration
/// shape as `AgentTransportAcceptor::new`, independently constructed here
/// since Worker does not depend on `bamep-server`.
///
/// #37 does not bind any listener with this configuration in production
/// (see module docs); this function exists so startup can fail fast if the
/// certificate/key pair loaded from disk is not actually mutually
/// consistent and rustls-loadable.
pub fn build_server_config(
    identity: &ServerTlsIdentity,
) -> Result<Arc<ServerConfig>, TlsIdentityError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(SUPPORTED_TLS_VERSIONS)
        .map_err(TlsIdentityError::TlsConfig)?
        .with_no_client_auth()
        .with_single_cert(
            identity.cert_chain.clone(),
            identity.private_key.clone_key(),
        )
        .map_err(TlsIdentityError::TlsConfig)?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use std::io::Write;

    struct TempPemFiles {
        dir: std::path::PathBuf,
        cert_path: std::path::PathBuf,
        key_path: std::path::PathBuf,
        /// The exact leaf certificate DER bytes as generated, independent of
        /// anything this module's own loader parsed back out of the PEM
        /// file — the reference value [`fingerprint_matches_an_independently_computed_reference`]
        /// checks against.
        reference_leaf_der: Vec<u8>,
    }

    impl Drop for TempPemFiles {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn write_pair(subject_alt_name: &str) -> TempPemFiles {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed([subject_alt_name.to_string()]).expect("generate cert");

        let dir =
            std::env::temp_dir().join(format!("bamep-worker-tls-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");

        std::fs::File::create(&cert_path)
            .and_then(|mut f| f.write_all(cert.pem().as_bytes()))
            .expect("write cert.pem");
        std::fs::File::create(&key_path)
            .and_then(|mut f| f.write_all(signing_key.serialize_pem().as_bytes()))
            .expect("write key.pem");

        TempPemFiles {
            dir,
            cert_path,
            key_path,
            reference_leaf_der: cert.der().to_vec(),
        }
    }

    #[test]
    fn loads_a_matching_certificate_and_key() {
        let files = write_pair("worker-tls-test.bamep.local");
        let identity =
            load_server_identity(&files.cert_path, &files.key_path).expect("load identity");
        assert_eq!(identity.cert_chain.len(), 1);
        build_server_config(&identity).expect("rustls config builds from the loaded pair");
    }

    #[test]
    fn fingerprint_matches_the_exact_loaded_leaf() {
        let files = write_pair("fingerprint-test.bamep.local");
        let identity =
            load_server_identity(&files.cert_path, &files.key_path).expect("load identity");
        assert!(identity
            .fingerprint
            .matches_leaf_der(identity.cert_chain[0].as_ref()));
    }

    /// Proves Worker's independently-loaded identity is exactly the same
    /// Server leaf certificate — computed here from the original generated
    /// DER bytes, entirely independent of `load_server_identity`'s own PEM
    /// parsing (ADR-0018 "TLS identity": "The Worker uses the same Server
    /// TLS identity already trusted by the Agent").
    #[test]
    fn fingerprint_matches_an_independently_computed_reference() {
        let files = write_pair("independent-reference-test.bamep.local");
        let expected = ServerCertFingerprint::from_leaf_der(&files.reference_leaf_der);
        let identity =
            load_server_identity(&files.cert_path, &files.key_path).expect("load identity");
        assert_eq!(identity.fingerprint, expected);
    }

    #[test]
    fn mismatched_key_is_rejected() {
        let files_a = write_pair("cert-owner.bamep.local");
        let files_b = write_pair("key-owner.bamep.local");
        // cert from A, private key from B: individually well-formed PEM,
        // but not a usable pair.
        let identity = load_server_identity(&files_a.cert_path, &files_b.key_path)
            .expect("both files parse individually");
        let err = build_server_config(&identity).unwrap_err();
        assert!(matches!(err, TlsIdentityError::TlsConfig(_)));
    }

    #[test]
    fn malformed_key_is_rejected() {
        let files = write_pair("malformed-key.bamep.local");
        std::fs::write(&files.key_path, b"not a pem private key").expect("overwrite key");
        let err = load_server_identity(&files.cert_path, &files.key_path).unwrap_err();
        assert!(matches!(
            err,
            TlsIdentityError::NoPrivateKey { .. } | TlsIdentityError::MalformedPem { .. }
        ));
    }

    #[test]
    fn malformed_certificate_is_rejected() {
        let files = write_pair("malformed-cert.bamep.local");
        std::fs::write(&files.cert_path, b"not a pem certificate").expect("overwrite cert");
        let err = load_server_identity(&files.cert_path, &files.key_path).unwrap_err();
        assert!(matches!(
            err,
            TlsIdentityError::NoCertificate { .. } | TlsIdentityError::MalformedPem { .. }
        ));
    }

    #[test]
    fn missing_certificate_file_fails_clearly() {
        let files = write_pair("missing-cert.bamep.local");
        let missing = files.dir.join("does-not-exist.pem");
        let err = load_server_identity(&missing, &files.key_path).unwrap_err();
        assert!(matches!(err, TlsIdentityError::Io { .. }));
    }

    #[test]
    fn missing_key_file_fails_clearly() {
        let files = write_pair("missing-key.bamep.local");
        let missing = files.dir.join("does-not-exist.pem");
        let err = load_server_identity(&files.cert_path, &missing).unwrap_err();
        assert!(matches!(err, TlsIdentityError::Io { .. }));
    }
}
