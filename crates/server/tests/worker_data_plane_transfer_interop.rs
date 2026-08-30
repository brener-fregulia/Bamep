//! The strongest Phase E2B vertical (Issue #39): a real HTTPS `chunk_upload`
//! and `seal` all the way through to real durable PostgreSQL.
//!
//! ```text
//! real hyper-1 HTTPS client (exact leaf pin)
//!   -> real bamep_worker::data_plane::DataPlane (Worker TLS server)
//!     -> real bamep_worker::ipc control client + real D1 staging + real D2
//!       -> real bamep_server::adapters::worker_control_plane::WorkerControlPlane
//!         -> real PostgreSQL-backed durable chunk acceptance, manifest seal,
//!            and Artifact verification
//! ```
//!
//! Requires a reachable PostgreSQL — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bamep_domain::AuthorizationOperation;
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::worker_control;
use bamep_worker::storage::FilesystemChunkStore;
use bamep_worker::tls::{build_server_config, load_server_identity};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::SigningKey;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;

use support::{
    build_worker_control_services, dispatched_transfer_fixture, issue_capability, sign_proof,
    DispatchedTransfer, TempSocketPath, TestDatabase, IPC_TEST_TIMEOUT as TEST_TIMEOUT,
};

fn sha256_wire(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

struct Stack {
    server_addr: SocketAddr,
    leaf_der: Vec<u8>,
    token: String,
    signing_key: SigningKey,
    fixture: DispatchedTransfer,
    _identity: TestIdentity,
    _storage: TempDir,
    _socket: TempSocketPath,
    shutdown: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn stand_up(db: &TestDatabase, signal: &str) -> Stack {
    let fixture = dispatched_transfer_fixture(&db.pool, signal).await;
    let services = build_worker_control_services(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&services.authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (shutdown, shutdown_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    tasks.push(tokio::spawn({
        let plane_shutdown = shutdown_rx.clone();
        async move {
            let _ = plane
                .run(
                    registry,
                    services.authorization,
                    services.chunk_acceptance,
                    services.manifest_seal,
                    services.artifact_verification,
                    plane_shutdown,
                )
                .await;
        }
    }));

    let identity = TestIdentity::generate();
    let leaf_der = identity.leaf_der.clone();
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
    tasks.push(tokio::spawn({
        let mut rx = shutdown_rx.clone();
        driver.run(async move {
            let _ = rx.wait_for(|s| *s).await;
        })
    }));

    let storage = TempDir::new();
    let chunk_store = FilesystemChunkStore::initialize(&storage.0).expect("chunk store");

    let data_plane = DataPlane::new(
        "127.0.0.1:0".parse().unwrap(),
        tls,
        control.clone(),
        chunk_store,
    );
    let server_handle = data_plane.handle();
    tasks.push(tokio::spawn({
        let mut rx = shutdown_rx;
        async move {
            let _ = data_plane
                .run(async move {
                    let _ = rx.wait_for(|s| *s).await;
                })
                .await;
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

    Stack {
        server_addr,
        leaf_der,
        token,
        signing_key,
        fixture,
        _identity: identity,
        _storage: storage,
        _socket: socket,
        shutdown,
        tasks,
    }
}

impl Stack {
    fn proof_carrier(&self, operation: AuthorizationOperation, chunk_index: Option<u64>) -> String {
        let (proof_id, issued_at, signature) = sign_proof(
            &self.signing_key,
            &self.token,
            &self.fixture,
            operation,
            chunk_index,
        );
        format!("{proof_id}.{issued_at}.{signature}")
    }

    async fn put_chunk(&self, chunk_index: u64, payload: &[u8]) -> (hyper::StatusCode, Value) {
        let carrier = self.proof_carrier(AuthorizationOperation::ChunkUpload, Some(chunk_index));
        https_request(
            self.server_addr,
            "PUT",
            &format!(
                "/api/data/v1/transfers/{}/chunks/{chunk_index}",
                self.fixture.transfer_id.0
            ),
            &[
                ("x-bamep-capability", self.token.as_str()),
                ("x-bamep-transfer-proof", carrier.as_str()),
                ("x-bamep-chunk-digest", &sha256_wire(payload)),
                ("content-type", "application/octet-stream"),
            ],
            payload.to_vec(),
            &self.leaf_der,
        )
        .await
    }

    async fn resume(&self) -> (hyper::StatusCode, Value) {
        let carrier = self.proof_carrier(AuthorizationOperation::ResumeDiscovery, None);
        https_request(
            self.server_addr,
            "GET",
            &format!(
                "/api/data/v1/transfers/{}/chunks",
                self.fixture.transfer_id.0
            ),
            &[
                ("x-bamep-capability", self.token.as_str()),
                ("x-bamep-transfer-proof", carrier.as_str()),
            ],
            Vec::new(),
            &self.leaf_der,
        )
        .await
    }

    async fn seal(&self, chunk_count: u64, artifact_digest: &str) -> (hyper::StatusCode, Value) {
        let carrier = self.proof_carrier(AuthorizationOperation::SealManifest, None);
        let body = serde_json::json!({
            "chunk_count": chunk_count,
            "artifact_digest": artifact_digest,
        })
        .to_string();
        https_request(
            self.server_addr,
            "POST",
            &format!("/api/data/v1/transfers/{}/seal", self.fixture.transfer_id.0),
            &[
                ("x-bamep-capability", self.token.as_str()),
                ("x-bamep-transfer-proof", carrier.as_str()),
                ("content-type", "application/json"),
            ],
            body.into_bytes(),
            &self.leaf_der,
        )
        .await
    }
}

#[tokio::test]
async fn a_real_https_put_then_resume_then_seal_verified() {
    let db = TestDatabase::setup().await;
    let stack = stand_up(&db, "e2b-verified-vertical").await;

    let chunk0 = vec![0xAB; 4096];
    let chunk1 = vec![0xCD; 1000];

    let (status, body) = stack.put_chunk(0, &chunk0).await;
    assert_eq!(status, hyper::StatusCode::CREATED, "{body}");
    assert_eq!(
        body,
        serde_json::json!({ "chunk_index": 0, "status": "accepted" })
    );

    let (status, body) = stack.put_chunk(1, &chunk1).await;
    assert_eq!(status, hyper::StatusCode::CREATED, "{body}");

    // A retried identical PUT idempotently returns already_held (models a
    // lost 201 response).
    let (status, body) = stack.put_chunk(0, &chunk0).await;
    assert_eq!(status, hyper::StatusCode::OK);
    assert_eq!(body["status"], serde_json::json!("already_held"));

    // Resume discovery reflects the real durable held set.
    let (status, body) = stack.resume().await;
    assert_eq!(status, hyper::StatusCode::OK);
    let held: Vec<u64> = body["held_chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["chunk_index"].as_u64().unwrap())
        .collect();
    assert_eq!(held, vec![0, 1]);
    assert_eq!(body["sealed"], serde_json::json!(false));

    // Seal with the honest full-Artifact digest -> bamepd's own comparison
    // against its durable expected value -> Verified.
    let full: Vec<u8> = [chunk0.as_slice(), chunk1.as_slice()].concat();
    let (status, body) = stack.seal(2, &sha256_wire(&full)).await;
    assert_eq!(status, hyper::StatusCode::OK, "{body}");
    assert_eq!(
        body["transfer_id"],
        serde_json::json!(stack.fixture.transfer_id.0.to_string())
    );
    assert_eq!(body["sealed"], serde_json::json!(true));
    assert_eq!(body["artifact_status"], serde_json::json!("Verified"));

    drop(stack);
    db.teardown().await;
}

#[tokio::test]
async fn a_real_https_seal_with_a_wrong_declared_digest_is_failed_not_an_error() {
    let db = TestDatabase::setup().await;
    let stack = stand_up(&db, "e2b-failed-vertical").await;

    let chunk0 = vec![0x11; 2048];
    let (status, _) = stack.put_chunk(0, &chunk0).await;
    assert_eq!(status, hyper::StatusCode::CREATED);

    // The Agent declares a digest that does NOT match the real bytes. The
    // Worker still recomputes honestly; bamepd commits Failed. Failed is a
    // 200 response, never a 409.
    let (status, body) = stack.seal(1, &sha256_wire(b"not the real artifact")).await;
    assert_eq!(status, hyper::StatusCode::OK, "{body}");
    assert_eq!(body["artifact_status"], serde_json::json!("Failed"));
    assert_eq!(body["sealed"], serde_json::json!(true));

    drop(stack);
    db.teardown().await;
}

// --- a generated Server TLS identity ----------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("bamep-e2b-interop-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct TestIdentity {
    _dir: TempDir,
    cert_path: PathBuf,
    key_path: PathBuf,
    leaf_der: Vec<u8>,
}

impl TestIdentity {
    fn generate() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(["bamep-e2b-interop.local".to_string()]).unwrap();
        let cert_path = dir.0.join("cert.pem");
        let key_path = dir.0.join("key.pem");
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

// --- minimal exact-leaf-pin HTTPS client -----------------------------

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

async fn https_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
    pinned_leaf: &[u8],
) -> (hyper::StatusCode, Value) {
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
            ServerName::try_from("bamep-e2b-interop.local").unwrap(),
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
        .method(method)
        .uri(format!("https://bamep-e2b-interop.local{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = timeout(
        TEST_TIMEOUT,
        sender.send_request(builder.body(Full::<Bytes>::new(Bytes::from(body))).unwrap()),
    )
    .await
    .expect("no timeout")
    .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
