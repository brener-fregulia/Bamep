//! The strongest Phase E2A vertical (Issue #39): a real HTTPS request all the
//! way through to real durable PostgreSQL resume facts.
//!
//! ```text
//! real hyper-1 HTTPS client (exact leaf pin)
//!   -> real bamep_worker::data_plane::DataPlane (Worker TLS server)
//!     -> real bamep_worker::ipc control client
//!       -> real bamep_server::adapters::worker_control_plane::WorkerControlPlane
//!         -> real PostgreSQL-backed TransferAuthorizationService + durable chunk facts
//! ```
//!
//! `bamep-worker` is a dev-dependency of `bamep-server` (one-directional; the
//! Worker never depends on the server). Requires a reachable PostgreSQL — see
//! `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::ChunkAcceptanceService;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::worker_control;
use bamep_worker::tls::{build_server_config, load_server_identity};
use ed25519_dalek::SigningKey;
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;

use support::{
    build_artifact_verification_service, build_authorization_service,
    build_chunk_acceptance_service, build_manifest_seal_service, dispatched_transfer_fixture,
    issue_capability, sign_proof, TempSocketPath, TestDatabase, IPC_TEST_TIMEOUT as TEST_TIMEOUT,
};

#[tokio::test]
async fn a_real_https_resume_get_returns_real_durable_held_chunks() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "e2a-resume-interop").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    // Durably hold chunks 0 and 2 through the real ChunkAcceptanceService.
    let acceptance = ChunkAcceptanceService::new(Arc::new(
        bamep_server::adapters::postgres::PostgresTransferRepository::new(db.pool.clone()),
    ));
    for index in [0u64, 2] {
        let outcome = acceptance
            .commit_chunk_acceptance(fixture.transfer_id, index, digest_wire(index as u8), 4096)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            bamep_server::ports::ChunkAcceptanceCommit::Committed
        );
    }

    // Real bamepd-side control plane.
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_plane_shutdown_tx, plane_shutdown_rx) = watch::channel(false);
    let plane_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        build_chunk_acceptance_service(db.pool.clone()),
        build_manifest_seal_service(db.pool.clone()),
        build_artifact_verification_service(db.pool.clone()),
        plane_shutdown_rx,
    ));

    // Real Worker: E1 control client + E2A HTTPS data plane.
    let identity = TestIdentity::generate();
    let tls = build_server_config(
        &load_server_identity(&identity.cert_path, &identity.key_path).expect("identity"),
    )
    .expect("server config");

    let (control, driver) = worker_control(
        socket.0.clone(),
        Duration::from_millis(20),
        Duration::from_secs(4),
        Uuid::new_v4(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let driver_task = tokio::spawn(driver.run({
        let mut rx = shutdown_rx.clone();
        async move {
            let _ = rx.wait_for(|s| *s).await;
        }
    }));

    let storage_root =
        std::env::temp_dir().join(format!("bamep-e2a-interop-store-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let chunk_store = bamep_worker::storage::FilesystemChunkStore::initialize(&storage_root)
        .expect("initialize chunk store");

    let data_plane = DataPlane::new(
        "127.0.0.1:0".parse().unwrap(),
        tls,
        control.clone(),
        chunk_store,
    );
    let server_handle = data_plane.handle();
    let server_task = tokio::spawn(data_plane.run({
        let mut rx = shutdown_rx;
        async move {
            let _ = rx.wait_for(|s| *s).await;
        }
    }));
    let server_addr = timeout(TEST_TIMEOUT, server_handle.listening())
        .await
        .expect("no timeout")
        .expect("bound");
    timeout(
        TEST_TIMEOUT,
        control.authority().wait_for(|s| s.is_available()),
    )
    .await
    .expect("no timeout")
    .expect("watch");

    // A real signed `resume_discovery` proof.
    let (proof_id, issued_at, signature) = sign_proof(
        &signing_key,
        &token,
        &fixture,
        bamep_domain::AuthorizationOperation::ResumeDiscovery,
        None,
    );
    let proof_carrier = format!("{proof_id}.{issued_at}.{signature}");

    let response = https_get(
        server_addr,
        &format!("/api/data/v1/transfers/{}/chunks", fixture.transfer_id.0),
        &[
            ("x-bamep-capability", token.as_str()),
            ("x-bamep-transfer-proof", proof_carrier.as_str()),
        ],
        &identity.leaf_der,
    )
    .await;

    assert_eq!(response.0, hyper::StatusCode::OK);
    assert_eq!(
        response.1["transfer_id"],
        serde_json::json!(fixture.transfer_id.0.to_string())
    );
    assert_eq!(response.1["sealed"], serde_json::json!(false));
    assert_eq!(response.1["digest_algorithm"], serde_json::json!("sha256"));
    assert_eq!(response.1["expected_chunk_count"], serde_json::Value::Null);
    let held = response.1["held_chunks"].as_array().expect("held_chunks");
    let indices: Vec<u64> = held
        .iter()
        .map(|c| c["chunk_index"].as_u64().unwrap())
        .collect();
    assert_eq!(
        indices,
        vec![0, 2],
        "the durable held set, in ascending order"
    );
    assert_eq!(held[0]["digest"], serde_json::json!(digest_wire(0)));

    let _ = shutdown_tx.send(true);
    driver_task.abort();
    server_task.abort();
    plane_task.abort();
    db.teardown().await;
    let _ = std::fs::remove_dir_all(&storage_root);
}

fn digest_wire(byte: u8) -> String {
    bamep_domain::Digest::new(bamep_domain::DigestAlgorithm::Sha256, vec![byte; 32])
        .unwrap()
        .to_wire_value()
}

// --- a generated Server TLS identity ------------------------------------

struct TestIdentity {
    _dir: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
    leaf_der: Vec<u8>,
}

impl TestIdentity {
    fn generate() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bamep-e2a-interop-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(["bamep-e2a-interop.local".to_string()]).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            leaf_der: cert.der().to_vec(),
            cert_path,
            key_path,
            _dir: dir,
        }
    }
}

impl Drop for TestIdentity {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

// --- minimal exact-leaf-pin HTTPS GET ----------------------------------

#[derive(Debug)]
struct ExactLeafPin {
    expected: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for ExactLeafPin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("leaf pin mismatch".into()))
        }
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("no TLS 1.2".into()))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn https_get(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    pinned_leaf: &[u8],
) -> (hyper::StatusCode, serde_json::Value) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ExactLeafPin {
            expected: pinned_leaf.to_vec(),
            provider,
        }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let tls = connector
        .connect(
            ServerName::try_from("bamep-e2a-interop.local").unwrap(),
            tcp,
        )
        .await
        .unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("https://bamep-e2a-interop.local{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = timeout(
        TEST_TIMEOUT,
        sender.send_request(builder.body(Empty::<Bytes>::new()).unwrap()),
    )
    .await
    .expect("no timeout")
    .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
