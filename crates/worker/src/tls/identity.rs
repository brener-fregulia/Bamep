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
    #[error("refusing to read private key at {path}: {reason}")]
    InsecureKeyPermissions { path: String, reason: String },
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
    let bytes = read_private_key_bytes_with_verified_permissions(path)?;
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

/// Opens the Server TLS *private key* path exactly once, validates the
/// least-privilege Unix policy against the metadata of the **same opened
/// file descriptor**, and reads its bytes from that same descriptor
/// (correction audit "TLS private key — open once, validate that file"):
/// the security policy must apply to the actual bytes read, not to a
/// separate `symlink_metadata(path)` resolution followed by an independent
/// `std::fs::read(path)` — two path resolutions that can each observe a
/// different filesystem object if the final path component is swapped
/// between them (a TOCTOU race), or if it is a symlink pointing outside the
/// intended protected location. `O_NOFOLLOW` rejects a symlink at the final
/// component outright, so no attacker-controlled indirection is ever
/// followed, and every check below runs against the exact bytes read next.
///
/// Deliberately applied only to the private key, never mechanically to the
/// public certificate, which carries no equivalent secrecy requirement.
///
/// Policy, chosen to be the narrowest that still catches the realistic
/// host-local misconfigurations for a protected-file key
/// (ADR-0018 "TLS identity"):
///
/// - the opened path must not be a symlink (enforced by `O_NOFOLLOW` at
///   open time, and re-confirmed from the resulting `fstat`);
/// - the opened file must be a regular file — not a directory, device, or
///   other special file;
/// - the file must grant no permission bits at all to `group` or `other`
///   (mode `& 0o077 == 0`), i.e. owner-only access. This deliberately
///   rejects group-read as well as any write bit: a private key has no
///   legitimate multi-principal read use case on a host-local single-
///   product deployment, so the strictest owner-only shape is chosen over a
///   narrower "reject only write" policy.
///
/// This does not require the key's Unix *owner* to equal a hardcoded
/// username — only that the mode bits themselves are owner-only; whichever
/// local account Worker runs as must already be able to read the file for
/// startup to succeed at all, which is sufficient without hardcoding an
/// identity here.
#[cfg(unix)]
fn read_private_key_bytes_with_verified_permissions(
    path: &Path,
) -> Result<Vec<u8>, TlsIdentityError> {
    use std::io::Read;

    use rustix::fs::{fstat, open, FileType, Mode, OFlags};

    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| {
        let source: std::io::Error = errno.into();
        if source.kind() == std::io::ErrorKind::NotFound {
            TlsIdentityError::Io {
                path: path.display().to_string(),
                source,
            }
        } else {
            // Most relevantly `ELOOP`/`ENOTDIR`-shaped failures from
            // `O_NOFOLLOW` hitting a symlink at the final component.
            TlsIdentityError::InsecureKeyPermissions {
                path: path.display().to_string(),
                reason: format!(
                    "failed to open private key without following a final-component symlink: {source}"
                ),
            }
        }
    })?;

    let stat = fstat(&fd).map_err(|errno| TlsIdentityError::Io {
        path: path.display().to_string(),
        source: errno.into(),
    })?;

    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(TlsIdentityError::InsecureKeyPermissions {
            path: path.display().to_string(),
            reason: "private key path is not a regular file".to_string(),
        });
    }
    if stat.st_mode & 0o077 != 0 {
        return Err(TlsIdentityError::InsecureKeyPermissions {
            path: path.display().to_string(),
            reason:
                "private key file grants group/other permissions; expected owner-only (0600 or stricter)"
                    .to_string(),
        });
    }

    // Same descriptor whose metadata was just validated — no second path
    // resolution.
    let mut file = std::fs::File::from(fd);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| TlsIdentityError::Io {
            path: path.display().to_string(),
            source,
        })?;
    Ok(bytes)
}

/// Non-Unix platforms have no equivalent POSIX permission/symlink model to
/// check here. Linux is the Worker reference/production environment
/// (`docs/development/testing.md`); this is a compile/test portability
/// fallback only, never a claim that it validates Linux deployment
/// security.
#[cfg(not(unix))]
fn read_private_key_bytes_with_verified_permissions(
    path: &Path,
) -> Result<Vec<u8>, TlsIdentityError> {
    std::fs::read(path).map_err(|source| TlsIdentityError::Io {
        path: path.display().to_string(),
        source,
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
        // The private key must satisfy `ensure_secure_private_key_permissions`
        // regardless of the test process's umask — every test that expects
        // `load_server_identity` to succeed depends on this.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .expect("set key.pem permissions");
        }

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

    #[cfg(unix)]
    mod key_permission_tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn secure_owner_only_key_permissions_are_accepted() {
            // `write_pair` already sets 0600; every other passing test in
            // this module already depends on this policy accepting it, but
            // this test names the property explicitly.
            let files = write_pair("secure-key-permissions.bamep.local");
            load_server_identity(&files.cert_path, &files.key_path)
                .expect("owner-only key permissions must be accepted");
        }

        #[test]
        fn world_readable_key_is_rejected() {
            let files = write_pair("world-readable-key.bamep.local");
            std::fs::set_permissions(&files.key_path, std::fs::Permissions::from_mode(0o644))
                .expect("relax key permissions");
            let err = load_server_identity(&files.cert_path, &files.key_path).unwrap_err();
            assert!(matches!(
                err,
                TlsIdentityError::InsecureKeyPermissions { .. }
            ));
        }

        #[test]
        fn group_writable_key_is_rejected() {
            let files = write_pair("group-writable-key.bamep.local");
            std::fs::set_permissions(&files.key_path, std::fs::Permissions::from_mode(0o660))
                .expect("relax key permissions");
            let err = load_server_identity(&files.cert_path, &files.key_path).unwrap_err();
            assert!(matches!(
                err,
                TlsIdentityError::InsecureKeyPermissions { .. }
            ));
        }

        #[test]
        fn other_writable_key_is_rejected() {
            let files = write_pair("other-writable-key.bamep.local");
            std::fs::set_permissions(&files.key_path, std::fs::Permissions::from_mode(0o602))
                .expect("relax key permissions");
            let err = load_server_identity(&files.cert_path, &files.key_path).unwrap_err();
            assert!(matches!(
                err,
                TlsIdentityError::InsecureKeyPermissions { .. }
            ));
        }

        #[test]
        fn non_regular_key_path_is_rejected() {
            let files = write_pair("non-regular-key-path.bamep.local");
            let dir_as_key = files.dir.join("key-is-a-directory");
            std::fs::create_dir_all(&dir_as_key).expect("create directory standing in for a key");
            let err = load_server_identity(&files.cert_path, &dir_as_key).unwrap_err();
            assert!(matches!(
                err,
                TlsIdentityError::InsecureKeyPermissions { .. }
            ));
        }

        #[test]
        fn symlink_key_path_is_rejected() {
            let files = write_pair("symlink-key-path.bamep.local");
            let symlink_path = files.dir.join("key-symlink.pem");
            std::os::unix::fs::symlink(&files.key_path, &symlink_path)
                .expect("create symlink to the real key");
            let err = load_server_identity(&files.cert_path, &symlink_path).unwrap_err();
            assert!(matches!(
                err,
                TlsIdentityError::InsecureKeyPermissions { .. }
            ));
        }
    }
}
