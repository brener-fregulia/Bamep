//! Real HTTPS integration tests for the Worker data-plane `chunk_upload` and
//! `seal` operations (Issue #39 Phase E2B):
//!
//! ```text
//! real hyper-1 HTTPS client (exact leaf pin, TLS 1.3)
//!   -> real Worker TLS server (bamep_worker::data_plane::DataPlane)
//!     -> real E1 control client + real D1 staging + real D2 reconstruction
//!       -> fake bamepd UDS peer (real bamep-worker-protocol codec)
//! ```
//!
//! Proves the full HTTPS <-> E1 <-> D1/D2 composition, the exact
//! `/api/data/v1/` PUT/POST contract shapes, the durable-acceptance ordering
//! (authorize -> stream -> hash -> finalize -> commit -> respond), and
//! fail-closed behavior, without needing PostgreSQL. A real-`bamepd` +
//! real-PostgreSQL vertical lives in
//! `crates/server/tests/worker_data_plane_transfer_interop.rs`.

#![cfg(unix)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::{worker_control, WorkerControlHandle};
use bamep_worker::storage::FilesystemChunkStore;
use bamep_worker::tls::{build_server_config, load_server_identity};
use bamep_worker_protocol::{
    ArtifactVerificationAckMessage, AuthorizationDecisionMessage, ChunkAcceptanceDecisionMessage,
    ChunkAcceptanceRejectionReason, ManifestSealDecisionMessage, ManifestSealRejectionReason,
    SealedManifestFacts, ServerHelloMessage, WireArtifactStatus, WireDigestAlgorithm,
    WorkerProtocolMessage,
};
use base64::Engine as _;
use http_body::Body as HttpBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn sha256_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn fresh() -> Self {
        let dir = std::env::temp_dir().join(format!("bamep-worker-e2b-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
        let dir = TempDir::fresh();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["bamep-worker-e2b-test.local".to_string()])
                .expect("generate cert");
        let cert_path = dir.0.join("cert.pem");
        let key_path = dir.0.join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            leaf_der: cert.der().to_vec(),
            cert_path,
            key_path,
            _dir: dir,
        }
    }
}

struct FakeBamepd {
    stream: UnixStream,
}

impl FakeBamepd {
    async fn accept_and_handshake(listener: &UnixListener) -> Self {
        let (mut stream, _) = timeout(TEST_TIMEOUT, listener.accept())
            .await
            .expect("no timeout")
            .expect("accept");
        let hello = match timeout(TEST_TIMEOUT, bamep_worker_protocol::receive(&mut stream))
            .await
            .expect("no timeout")
            .expect("recv WorkerHello")
        {
            WorkerProtocolMessage::WorkerHello(h) => h,
            other => panic!("expected WorkerHello, got {other:?}"),
        };
        bamep_worker_protocol::send(
            &mut stream,
            &WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(hello.envelope.message_id)),
        )
        .await
        .expect("send ServerHello");
        Self { stream }
    }

    async fn recv(&mut self) -> WorkerProtocolMessage {
        timeout(
            TEST_TIMEOUT,
            bamep_worker_protocol::receive(&mut self.stream),
        )
        .await
        .expect("no timeout")
        .expect("recv")
    }

    async fn send(&mut self, message: WorkerProtocolMessage) {
        bamep_worker_protocol::send(&mut self.stream, &message)
            .await
            .expect("send");
    }
}

struct Harness {
    _identity: TestIdentity,
    _socket_dir: TempDir,
    storage_dir: TempDir,
    server_addr: SocketAddr,
    leaf_der: Vec<u8>,
    control: WorkerControlHandle,
    listener: UnixListener,
    driver_task: JoinHandle<()>,
    server_task: JoinHandle<Result<(), bamep_worker::data_plane::DataPlaneError>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with_control_timeout(Duration::from_millis(800)).await
    }

    async fn start_with_control_timeout(control_timeout: Duration) -> Self {
        let identity = TestIdentity::generate();
        let leaf_der = identity.leaf_der.clone();
        let tls = build_server_config(
            &load_server_identity(&identity.cert_path, &identity.key_path).expect("load identity"),
        )
        .expect("build server config");

        let socket_dir = TempDir::fresh();
        let socket_path = socket_dir.0.join("worker.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind fake bamepd");

        let storage_dir = TempDir::fresh();
        let chunk_store =
            FilesystemChunkStore::initialize(&storage_dir.0).expect("initialize chunk store");

        let (control, driver) = worker_control(
            socket_path,
            Duration::from_millis(20),
            control_timeout,
            Uuid::new_v4(),
        );
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let driver_shutdown = {
            let mut rx = shutdown_rx.clone();
            async move {
                let _ = rx.wait_for(|s| *s).await;
            }
        };
        let driver_task = tokio::spawn(driver.run(driver_shutdown));

        let data_plane = DataPlane::new(
            "127.0.0.1:0".parse().unwrap(),
            tls,
            control.clone(),
            chunk_store,
        );
        let server_handle = data_plane.handle();
        let server_shutdown = {
            let mut rx = shutdown_rx;
            async move {
                let _ = rx.wait_for(|s| *s).await;
            }
        };
        let server_task = tokio::spawn(data_plane.run(server_shutdown));
        let server_addr = timeout(TEST_TIMEOUT, server_handle.listening())
            .await
            .expect("no timeout")
            .expect("bound");

        Self {
            _identity: identity,
            _socket_dir: socket_dir,
            storage_dir,
            server_addr,
            leaf_der,
            control,
            listener,
            driver_task,
            server_task,
            shutdown,
        }
    }

    async fn fake_bamepd(&self) -> FakeBamepd {
        let peer = FakeBamepd::accept_and_handshake(&self.listener).await;
        timeout(
            TEST_TIMEOUT,
            self.control.authority().wait_for(|s| s.is_available()),
        )
        .await
        .expect("no timeout")
        .expect("watch");
        peer
    }

    fn finalized_chunk_path(&self, transfer_id: Uuid, chunk_index: u64) -> PathBuf {
        self.storage_dir
            .0
            .join("transfers")
            .join(transfer_id.as_hyphenated().to_string())
            .join("chunks")
            .join(format!("{chunk_index}.chunk"))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.driver_task.abort();
        self.server_task.abort();
    }
}

// ---------------------------------------------------------------------
// exact-leaf-pin hyper HTTPS client (mirrors the Agent's exact-fingerprint
// model)
// ---------------------------------------------------------------------

#[derive(Debug)]
struct ExactLeafPin {
    expected_leaf_der: Vec<u8>,
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
        if end_entity.as_ref() == self.expected_leaf_der.as_slice() {
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

struct HttpResponse {
    status: StatusCode,
    cache_control: Option<String>,
    body: Value,
}

async fn connect(
    addr: SocketAddr,
    pinned_leaf_der: &[u8],
) -> hyper::client::conn::http1::SendRequest<BoxBody> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ExactLeafPin {
            expected_leaf_der: pinned_leaf_der.to_vec(),
            provider,
        }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .expect("tcp connect");
    let tls = connector
        .connect(
            ServerName::try_from("bamep-worker-e2b-test.local").unwrap(),
            tcp,
        )
        .await
        .expect("tls handshake");
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .expect("http1 handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
}

type BoxBody =
    http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

fn box_body<B>(body: B) -> BoxBody
where
    B: HttpBody<Data = Bytes> + Send + Sync + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    body.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        .boxed()
}

async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: BoxBody,
    pinned_leaf_der: &[u8],
) -> Result<HttpResponse, hyper::Error> {
    let mut sender = connect(addr, pinned_leaf_der).await;
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("https://bamep-worker-e2b-test.local{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(body).unwrap();
    let response = timeout(TEST_TIMEOUT, sender.send_request(request))
        .await
        .expect("no timeout")?;
    let status = response.status();
    let cache_control = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json body");
    Ok(HttpResponse {
        status,
        cache_control,
        body,
    })
}

async fn https_put(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
    pinned_leaf_der: &[u8],
) -> HttpResponse {
    send(
        addr,
        "PUT",
        path,
        headers,
        box_body(Full::new(Bytes::from(body))),
        pinned_leaf_der,
    )
    .await
    .expect("request completed")
}

async fn https_post(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
    pinned_leaf_der: &[u8],
) -> HttpResponse {
    send(
        addr,
        "POST",
        path,
        headers,
        box_body(Full::new(Bytes::from(body))),
        pinned_leaf_der,
    )
    .await
    .expect("request completed")
}

// ---------------------------------------------------------------------
// request builders
// ---------------------------------------------------------------------

const CANONICAL_PROOF_CARRIER: &str = concat!(
    "AAAAAAAAAAAAAAAAAAAAAA.1700000000000.",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
);

fn put_headers(digest: &str) -> Vec<(&'static str, String)> {
    vec![
        ("x-bamep-capability", "opaque-capability-token".to_string()),
        (
            "x-bamep-transfer-proof",
            CANONICAL_PROOF_CARRIER.to_string(),
        ),
        ("x-bamep-chunk-digest", digest.to_string()),
        ("content-type", "application/octet-stream".to_string()),
    ]
}

fn seal_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("x-bamep-capability", "opaque-capability-token"),
        ("x-bamep-transfer-proof", CANONICAL_PROOF_CARRIER),
        ("content-type", "application/json"),
    ]
}

fn borrow<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

fn chunk_path(transfer_id: Uuid, chunk_index: u64) -> String {
    format!("/api/data/v1/transfers/{transfer_id}/chunks/{chunk_index}")
}

fn seal_path(transfer_id: Uuid) -> String {
    format!("/api/data/v1/transfers/{transfer_id}/seal")
}

fn sealed_facts(chunk_count: u64, chunk_size: u32, artifact_digest: &str) -> SealedManifestFacts {
    SealedManifestFacts {
        verification_handle: "verification-handle-1".to_string(),
        artifact_id: Uuid::new_v4(),
        digest_algorithm: WireDigestAlgorithm::Sha256,
        chunk_size,
        chunk_count,
        expected_artifact_digest: artifact_digest.to_string(),
    }
}

// =====================================================================
// chunk_upload — happy path + durable-acceptance ordering
// =====================================================================

#[tokio::test]
async fn put_new_chunk_streams_finalizes_and_returns_201_accepted() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let payload = vec![7u8; 4096];
    let digest = sha256_b64(&payload);

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        let headers = put_headers(&digest);
        let payload = payload.clone();
        async move {
            https_put(
                addr,
                &chunk_path(transfer_id, 2),
                &borrow(&headers),
                payload,
                &leaf,
            )
            .await
        }
    });

    // 1. AuthorizationQuery -> approved (chunk not yet durable).
    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("expected AuthorizationQuery, got {other:?}"),
    };
    assert_eq!(query.body.transfer_id, transfer_id);
    assert_eq!(query.body.chunk_index, 2);
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::approved(
            query.envelope.message_id,
            WireDigestAlgorithm::Sha256,
            8192,
            "acceptance-handle-1",
            None,
        ),
    ))
    .await;

    // 2. ChunkAcceptanceRequest carries ONLY metadata — the Worker-verified
    //    digest and exact size, never the bytes.
    let acceptance = match peer.recv().await {
        WorkerProtocolMessage::ChunkAcceptanceRequest(r) => r,
        other => panic!("expected ChunkAcceptanceRequest, got {other:?}"),
    };
    assert_eq!(acceptance.body.transfer_id, transfer_id);
    assert_eq!(acceptance.body.chunk_index, 2);
    assert_eq!(acceptance.body.digest, digest);
    assert_eq!(acceptance.body.size, 4096);
    peer.send(WorkerProtocolMessage::ChunkAcceptanceDecision(
        ChunkAcceptanceDecisionMessage::committed(acceptance.envelope.message_id),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::CREATED);
    assert_eq!(response.cache_control.as_deref(), Some("no-store"));
    assert_eq!(
        response.body,
        json!({ "chunk_index": 2, "status": "accepted" })
    );

    // The finalized restart-stable file exists with exactly the sent bytes.
    let final_path = harness.finalized_chunk_path(transfer_id, 2);
    assert_eq!(std::fs::read(&final_path).unwrap(), payload);
}

#[tokio::test]
async fn put_idempotent_resubmit_returns_200_already_held() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let payload = vec![3u8; 1024];
    let digest = sha256_b64(&payload);

    for (attempt, expected_status, expected_body) in [
        (
            "first",
            StatusCode::CREATED,
            json!({ "chunk_index": 0, "status": "accepted" }),
        ),
        (
            "retry-after-lost-response",
            StatusCode::OK,
            json!({ "chunk_index": 0, "status": "already_held" }),
        ),
    ] {
        let request = tokio::spawn({
            let addr = harness.server_addr;
            let leaf = harness.leaf_der.clone();
            let headers = put_headers(&digest);
            let payload = payload.clone();
            async move {
                https_put(
                    addr,
                    &chunk_path(transfer_id, 0),
                    &borrow(&headers),
                    payload,
                    &leaf,
                )
                .await
            }
        });

        let query = match peer.recv().await {
            WorkerProtocolMessage::AuthorizationQuery(q) => q,
            other => panic!("{attempt}: expected AuthorizationQuery, got {other:?}"),
        };
        // On the retry bamepd reports the already-recorded expected digest.
        let expected_digest = (attempt != "first").then(|| digest.clone());
        peer.send(WorkerProtocolMessage::AuthorizationDecision(
            AuthorizationDecisionMessage::approved(
                query.envelope.message_id,
                WireDigestAlgorithm::Sha256,
                4096,
                "acceptance-handle",
                expected_digest,
            ),
        ))
        .await;

        let acceptance = match peer.recv().await {
            WorkerProtocolMessage::ChunkAcceptanceRequest(r) => r,
            other => panic!("{attempt}: expected ChunkAcceptanceRequest, got {other:?}"),
        };
        let decision = if attempt == "first" {
            ChunkAcceptanceDecisionMessage::committed(acceptance.envelope.message_id)
        } else {
            ChunkAcceptanceDecisionMessage::already_committed(acceptance.envelope.message_id)
        };
        peer.send(WorkerProtocolMessage::ChunkAcceptanceDecision(decision))
            .await;

        let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
        assert_eq!(response.status, expected_status, "{attempt}");
        assert_eq!(response.body, expected_body, "{attempt}");
    }
}

#[tokio::test]
async fn put_authorization_denied_returns_the_fixed_generic_401() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let payload = vec![1u8; 64];
    let digest = sha256_b64(&payload);

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        let headers = put_headers(&digest);
        async move {
            https_put(
                addr,
                &chunk_path(transfer_id, 0),
                &borrow(&headers),
                payload,
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::denied(query.envelope.message_id),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "AUTHORIZATION_DENIED" } })
    );
    // No ChunkAcceptanceRequest was ever sent.
    assert!(
        timeout(Duration::from_millis(150), peer.recv())
            .await
            .is_err(),
        "a denied authorization must not trigger a durable acceptance"
    );
}

#[tokio::test]
async fn put_digest_mismatch_returns_409_and_never_commits() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let payload = vec![9u8; 512];
    // Declared digest is for *different* bytes.
    let wrong_digest = sha256_b64(&[0u8; 512]);

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        let headers = put_headers(&wrong_digest);
        async move {
            https_put(
                addr,
                &chunk_path(transfer_id, 0),
                &borrow(&headers),
                payload,
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::approved(
            query.envelope.message_id,
            WireDigestAlgorithm::Sha256,
            4096,
            "handle",
            None,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "DIGEST_MISMATCH" } })
    );
    assert!(
        timeout(Duration::from_millis(150), peer.recv())
            .await
            .is_err(),
        "invalid bytes must never be sent as a durable chunk"
    );
    assert!(
        !harness.finalized_chunk_path(transfer_id, 0).exists(),
        "invalid bytes must never be finalized"
    );
}

#[tokio::test]
async fn put_pre_body_identity_conflict_returns_409_before_reading_the_body() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let payload = vec![5u8; 4096];
    let digest = sha256_b64(&payload);

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        let headers = put_headers(&digest);
        async move {
            https_put(
                addr,
                &chunk_path(transfer_id, 1),
                &borrow(&headers),
                payload,
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    // Already durable with a DIFFERENT expected digest.
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::approved(
            query.envelope.message_id,
            WireDigestAlgorithm::Sha256,
            8192,
            "handle",
            Some(sha256_b64(&[42u8; 10])),
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "CHUNK_IDENTITY_CONFLICT" } })
    );
    assert!(
        timeout(Duration::from_millis(150), peer.recv())
            .await
            .is_err(),
        "a pre-body identity conflict never reaches durable acceptance"
    );
}

#[tokio::test]
async fn put_body_over_authorized_chunk_size_returns_413() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let payload = vec![2u8; 5000];
    let digest = sha256_b64(&payload);

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        let headers = put_headers(&digest);
        async move {
            https_put(
                addr,
                &chunk_path(transfer_id, 0),
                &borrow(&headers),
                payload,
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    // Authoritative chunk_size is smaller than the body.
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::approved(
            query.envelope.message_id,
            WireDigestAlgorithm::Sha256,
            4096,
            "handle",
            None,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "CHUNK_TOO_LARGE" } })
    );
    assert!(
        timeout(Duration::from_millis(150), peer.recv())
            .await
            .is_err(),
        "an oversized body never reaches durable acceptance"
    );
}

#[tokio::test]
async fn put_empty_body_returns_400() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let digest = sha256_b64(&[]);

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        let headers = put_headers(&digest);
        async move {
            https_put(
                addr,
                &chunk_path(transfer_id, 0),
                &borrow(&headers),
                Vec::new(),
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::AuthorizationQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::AuthorizationDecision(
        AuthorizationDecisionMessage::approved(
            query.envelope.message_id,
            WireDigestAlgorithm::Sha256,
            4096,
            "handle",
            None,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "MALFORMED_REQUEST" } })
    );
}

#[tokio::test]
async fn put_commit_rejections_map_to_the_exact_409_codes() {
    for (reason, code) in [
        (
            ChunkAcceptanceRejectionReason::ChunkIdentityConflict,
            "CHUNK_IDENTITY_CONFLICT",
        ),
        (
            ChunkAcceptanceRejectionReason::TransferNotContinuable,
            "TRANSFER_NOT_CONTINUABLE",
        ),
    ] {
        let harness = Harness::start().await;
        let mut peer = harness.fake_bamepd().await;
        let transfer_id = Uuid::new_v4();
        let payload = vec![4u8; 256];
        let digest = sha256_b64(&payload);

        let request = tokio::spawn({
            let addr = harness.server_addr;
            let leaf = harness.leaf_der.clone();
            let headers = put_headers(&digest);
            async move {
                https_put(
                    addr,
                    &chunk_path(transfer_id, 0),
                    &borrow(&headers),
                    payload,
                    &leaf,
                )
                .await
            }
        });

        let query = match peer.recv().await {
            WorkerProtocolMessage::AuthorizationQuery(q) => q,
            other => panic!("got {other:?}"),
        };
        peer.send(WorkerProtocolMessage::AuthorizationDecision(
            AuthorizationDecisionMessage::approved(
                query.envelope.message_id,
                WireDigestAlgorithm::Sha256,
                4096,
                "handle",
                None,
            ),
        ))
        .await;
        let acceptance = match peer.recv().await {
            WorkerProtocolMessage::ChunkAcceptanceRequest(r) => r,
            other => panic!("got {other:?}"),
        };
        peer.send(WorkerProtocolMessage::ChunkAcceptanceDecision(
            ChunkAcceptanceDecisionMessage::rejected(acceptance.envelope.message_id, reason),
        ))
        .await;

        let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
        assert_eq!(response.status, StatusCode::CONFLICT, "{code}");
        assert_eq!(
            response.body,
            json!({ "error": { "code": code } }),
            "{code}"
        );
    }
}

#[tokio::test]
async fn put_structurally_malformed_requests_return_400_without_reaching_bamepd() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let payload = vec![1u8; 8];
    let digest = sha256_b64(&payload);

    let good = put_headers(&digest);
    let no_digest: Vec<(&str, String)> = good
        .iter()
        .filter(|(k, _)| *k != "x-bamep-chunk-digest")
        .cloned()
        .collect();
    let bad_ct: Vec<(&str, String)> = good
        .iter()
        .map(|(k, v)| {
            if *k == "content-type" {
                (*k, "text/plain".to_string())
            } else {
                (*k, v.clone())
            }
        })
        .collect();
    let bad_digest: Vec<(&str, String)> = good
        .iter()
        .map(|(k, v)| {
            if *k == "x-bamep-chunk-digest" {
                (*k, "not-a-canonical-digest".to_string())
            } else {
                (*k, v.clone())
            }
        })
        .collect();

    let cases: Vec<(String, Vec<(&str, String)>)> = vec![
        // non-canonical chunk_index (leading zero)
        (
            format!("/api/data/v1/transfers/{transfer_id}/chunks/01"),
            good.clone(),
        ),
        // signed chunk_index
        (
            format!("/api/data/v1/transfers/{transfer_id}/chunks/-1"),
            good.clone(),
        ),
        // missing X-Bamep-Chunk-Digest
        (chunk_path(transfer_id, 0), no_digest),
        // wrong Content-Type
        (chunk_path(transfer_id, 0), bad_ct),
        // non-canonical digest value
        (chunk_path(transfer_id, 0), bad_digest),
    ];

    for (path, headers) in cases {
        let response = https_put(
            harness.server_addr,
            &path,
            &borrow(&headers),
            payload.clone(),
            &harness.leaf_der,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "path {path}");
        assert_eq!(
            response.body,
            json!({ "error": { "code": "MALFORMED_REQUEST" } }),
            "path {path}"
        );
    }

    assert!(
        timeout(Duration::from_millis(150), peer.recv())
            .await
            .is_err(),
        "structurally malformed PUTs never reach bamepd"
    );
}

#[tokio::test]
async fn put_control_unavailable_returns_the_generic_401() {
    // Never accept the fake bamepd: E1 is NotConnected.
    let harness = Harness::start().await;
    let transfer_id = Uuid::new_v4();
    let payload = vec![1u8; 8];
    let digest = sha256_b64(&payload);

    let response = https_put(
        harness.server_addr,
        &chunk_path(transfer_id, 0),
        &borrow(&put_headers(&digest)),
        payload,
        &harness.leaf_der,
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "AUTHORIZATION_DENIED" } })
    );
}

// A deterministic client-disconnect-mid-upload vertical would require
// racing the transport teardown; the truncation -> fail-closed logic is
// unit-tested directly against `stage_chunk_body` in
// `crate::data_plane::upload` (a body that errors mid-stream must yield
// `StorageUnavailable`, never `Finalized`).

#[tokio::test]
async fn concurrent_uploads_for_distinct_chunks_each_finalize_and_commit() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let payloads: Vec<Vec<u8>> = (0..4u8)
        .map(|b| vec![b + 1; 1024 * (b as usize + 1)])
        .collect();
    let requests: Vec<_> = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            let addr = harness.server_addr;
            let leaf = harness.leaf_der.clone();
            let digest = sha256_b64(payload);
            let headers = put_headers(&digest);
            let payload = payload.clone();
            tokio::spawn(async move {
                (
                    index as u64,
                    https_put(
                        addr,
                        &chunk_path(transfer_id, index as u64),
                        &borrow(&headers),
                        payload,
                        &leaf,
                    )
                    .await,
                )
            })
        })
        .collect();

    // The four uploads run concurrently: all four AuthorizationQueries can
    // arrive before any ChunkAcceptanceRequest. Drain each phase fully.
    for _ in 0..payloads.len() {
        let query = match peer.recv().await {
            WorkerProtocolMessage::AuthorizationQuery(q) => q,
            other => panic!("phase 1: got {other:?}"),
        };
        peer.send(WorkerProtocolMessage::AuthorizationDecision(
            AuthorizationDecisionMessage::approved(
                query.envelope.message_id,
                WireDigestAlgorithm::Sha256,
                8192,
                format!("handle-{}", query.body.chunk_index),
                None,
            ),
        ))
        .await;
    }
    for _ in 0..payloads.len() {
        let acceptance = match peer.recv().await {
            WorkerProtocolMessage::ChunkAcceptanceRequest(r) => r,
            other => panic!("phase 2: got {other:?}"),
        };
        peer.send(WorkerProtocolMessage::ChunkAcceptanceDecision(
            ChunkAcceptanceDecisionMessage::committed(acceptance.envelope.message_id),
        ))
        .await;
    }

    for request in requests {
        let (index, response) = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
        assert_eq!(response.status, StatusCode::CREATED, "chunk {index}");
        assert_eq!(
            std::fs::read(harness.finalized_chunk_path(transfer_id, index)).unwrap(),
            payloads[index as usize]
        );
    }
}

/// Issue #63 window_8 CONTRACT / ORDERING CHECK: deliberately answers every
/// chunk's `AuthorizationDecision` and `ChunkAcceptanceDecision` in
/// DESCENDING `chunk_index` order (the highest index finalizes/durably
/// commits FIRST, the lowest LAST) — the durable-acceptance completion order
/// is the exact REVERSE of ascending, proving independently addressed
/// non-overlapping chunk PUTs may complete/commit out of order before seal:
///
/// * every chunk is still `201 Accepted` regardless of completion order;
/// * every chunk's finalized bytes on disk are exactly its own declared
///   bytes at its own `chunk_index` path (D1 identity is positional, not
///   arrival-ordered);
/// * `seal` — which reconstructs strictly ASCENDING regardless of commit
///   order (`m0-data-plane-and-storage-contracts.md` "Full-Artifact byte
///   reconstruction": "each chunk contributes its bytes at one fixed
///   position regardless of transfer order") — still reaches
///   `Artifact::Verified`.
///
/// This is the empirical basis (together with `bamepd`'s
/// `commit_chunk_acceptance`, which carries no chunk-index ordering
/// precondition — see `crates/server/src/application/mod.rs`) for arming the
/// window_8 candidate's bounded-concurrent-PUT design.
#[tokio::test]
async fn chunks_committed_in_strictly_descending_order_still_seal_verified() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    const CHUNK_SIZE: u32 = 4096;

    // 4 chunks: the first 3 exactly `CHUNK_SIZE` (non-final), the last short
    // (`m0-...` "every chunk except the last has size == chunk_size").
    let chunks: Vec<Vec<u8>> = vec![
        vec![1u8; CHUNK_SIZE as usize],
        vec![2u8; CHUNK_SIZE as usize],
        vec![3u8; CHUNK_SIZE as usize],
        vec![4u8; 777],
    ];
    let n = chunks.len() as u64;

    let completion_order: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let requests: Vec<_> = chunks
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            let addr = harness.server_addr;
            let leaf = harness.leaf_der.clone();
            let digest = sha256_b64(payload);
            let headers = put_headers(&digest);
            let payload = payload.clone();
            let completion_order = Arc::clone(&completion_order);
            tokio::spawn(async move {
                let response = https_put(
                    addr,
                    &chunk_path(transfer_id, index as u64),
                    &borrow(&headers),
                    payload,
                    &leaf,
                )
                .await;
                completion_order.lock().unwrap().push(index as u64);
                (index as u64, response)
            })
        })
        .collect();

    // Phase 1: drain all 4 AuthorizationQuery messages, then answer them in
    // DESCENDING chunk_index order.
    let mut queries = Vec::new();
    for _ in 0..n {
        match peer.recv().await {
            WorkerProtocolMessage::AuthorizationQuery(q) => queries.push(q),
            other => panic!("phase 1: got {other:?}"),
        }
    }
    queries.sort_by_key(|q| std::cmp::Reverse(q.body.chunk_index));
    assert_eq!(
        queries.iter().map(|q| q.body.chunk_index).collect::<Vec<_>>(),
        (0..n).rev().collect::<Vec<_>>(),
        "sanity: exactly one AuthorizationQuery per chunk_index"
    );
    for q in &queries {
        peer.send(WorkerProtocolMessage::AuthorizationDecision(
            AuthorizationDecisionMessage::approved(
                q.envelope.message_id,
                WireDigestAlgorithm::Sha256,
                CHUNK_SIZE,
                format!("handle-{}", q.body.chunk_index),
                None,
            ),
        ))
        .await;
    }

    // Phase 2: same drain-then-answer-descending discipline for the
    // ChunkAcceptanceRequest (the actual DURABLE commit step) — this is what
    // makes chunk 3 durably commit (and its HTTP 201 return) BEFORE chunk 0.
    let mut acceptances = Vec::new();
    for _ in 0..n {
        match peer.recv().await {
            WorkerProtocolMessage::ChunkAcceptanceRequest(r) => acceptances.push(r),
            other => panic!("phase 2: got {other:?}"),
        }
    }
    acceptances.sort_by_key(|r| std::cmp::Reverse(r.body.chunk_index));
    assert_eq!(
        acceptances.iter().map(|r| r.body.chunk_index).collect::<Vec<_>>(),
        (0..n).rev().collect::<Vec<_>>(),
    );
    for r in &acceptances {
        peer.send(WorkerProtocolMessage::ChunkAcceptanceDecision(
            ChunkAcceptanceDecisionMessage::committed(r.envelope.message_id),
        ))
        .await;
    }

    for request in requests {
        let (index, response) = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
        assert_eq!(response.status, StatusCode::CREATED, "chunk {index}: must accept regardless of commit order");
        assert_eq!(
            std::fs::read(harness.finalized_chunk_path(transfer_id, index)).unwrap(),
            chunks[index as usize],
            "chunk {index}: D1 identity is positional (by chunk_index), not arrival-ordered"
        );
    }
    let observed = completion_order.lock().unwrap().clone();
    assert_ne!(
        observed,
        (0..n).collect::<Vec<_>>(),
        "completion order must NOT be ascending — the whole point of this check"
    );
    assert_eq!(
        observed[0], 3,
        "chunk 3 (answered first, in descending order) must be the FIRST to durably complete"
    );

    // seal: reconstruction is strictly ASCENDING regardless of commit order.
    let full: Vec<u8> = chunks.concat();
    let artifact_digest = sha256_b64(&full);
    let body = json!({ "chunk_count": n, "artifact_digest": artifact_digest }).to_string();
    let seal_request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_post(addr, &seal_path(transfer_id), &seal_headers(), body.into_bytes(), &leaf).await
        }
    });

    let seal = match peer.recv().await {
        WorkerProtocolMessage::ManifestSealRequest(r) => r,
        other => panic!("expected ManifestSealRequest, got {other:?}"),
    };
    assert_eq!(seal.body.chunk_count, n);
    let facts = sealed_facts(n, CHUNK_SIZE, &artifact_digest);
    peer.send(WorkerProtocolMessage::ManifestSealDecision(
        ManifestSealDecisionMessage::sealed(seal.envelope.message_id, facts),
    ))
    .await;

    let report = match peer.recv().await {
        WorkerProtocolMessage::ArtifactVerificationReport(r) => r,
        other => panic!("expected ArtifactVerificationReport, got {other:?}"),
    };
    assert_eq!(
        report.body.computed_artifact_digest, artifact_digest,
        "D2 reconstruction (ascending, positional) matches regardless of commit order"
    );
    peer.send(WorkerProtocolMessage::ArtifactVerificationAck(
        ArtifactVerificationAckMessage::committed(
            report.envelope.message_id,
            WireArtifactStatus::Verified,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, seal_request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body.get("artifact_status").and_then(|v| v.as_str()),
        Some("Verified"),
        "out-of-order durable commit must still reach Artifact::Verified"
    );
}

// =====================================================================
// seal — Verified / Failed / conflicts
// =====================================================================

/// Uploads chunks `0..count` (each `size` bytes of `fill(i)`) through the real
/// PUT path so D1 has real finalized files for the seal reread. Returns the
/// concatenated full-Artifact bytes.
async fn upload_chunks(
    harness: &Harness,
    peer: &mut FakeBamepd,
    transfer_id: Uuid,
    chunks: &[Vec<u8>],
) {
    for (index, payload) in chunks.iter().enumerate() {
        let digest = sha256_b64(payload);
        let request = tokio::spawn({
            let addr = harness.server_addr;
            let leaf = harness.leaf_der.clone();
            let headers = put_headers(&digest);
            let payload = payload.clone();
            async move {
                https_put(
                    addr,
                    &chunk_path(transfer_id, index as u64),
                    &borrow(&headers),
                    payload,
                    &leaf,
                )
                .await
            }
        });
        let query = match peer.recv().await {
            WorkerProtocolMessage::AuthorizationQuery(q) => q,
            other => panic!("got {other:?}"),
        };
        peer.send(WorkerProtocolMessage::AuthorizationDecision(
            AuthorizationDecisionMessage::approved(
                query.envelope.message_id,
                WireDigestAlgorithm::Sha256,
                4096,
                "handle",
                None,
            ),
        ))
        .await;
        let acceptance = match peer.recv().await {
            WorkerProtocolMessage::ChunkAcceptanceRequest(r) => r,
            other => panic!("got {other:?}"),
        };
        peer.send(WorkerProtocolMessage::ChunkAcceptanceDecision(
            ChunkAcceptanceDecisionMessage::committed(acceptance.envelope.message_id),
        ))
        .await;
        assert_eq!(
            timeout(TEST_TIMEOUT, request)
                .await
                .unwrap()
                .unwrap()
                .status,
            StatusCode::CREATED
        );
    }
}

#[tokio::test]
async fn seal_reconstructs_the_full_artifact_and_returns_200_verified() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let chunks = vec![vec![10u8; 4096], vec![20u8; 4096], vec![30u8; 100]];
    upload_chunks(&harness, &mut peer, transfer_id, &chunks).await;
    let full: Vec<u8> = chunks.concat();
    let artifact_digest = sha256_b64(&full);

    let body = json!({ "chunk_count": 3, "artifact_digest": artifact_digest }).to_string();
    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_post(
                addr,
                &seal_path(transfer_id),
                &seal_headers(),
                body.into_bytes(),
                &leaf,
            )
            .await
        }
    });

    // 1. ManifestSealRequest -> first durable commit.
    let seal = match peer.recv().await {
        WorkerProtocolMessage::ManifestSealRequest(r) => r,
        other => panic!("expected ManifestSealRequest, got {other:?}"),
    };
    assert_eq!(seal.body.chunk_count, 3);
    assert_eq!(seal.body.artifact_digest, artifact_digest);
    let facts = sealed_facts(3, 4096, &artifact_digest);
    let artifact_id = facts.artifact_id;
    peer.send(WorkerProtocolMessage::ManifestSealDecision(
        ManifestSealDecisionMessage::sealed(seal.envelope.message_id, facts),
    ))
    .await;

    // 2. ArtifactVerificationReport carries the INDEPENDENTLY recomputed
    //    digest (matches, because D1 holds the real bytes).
    let report = match peer.recv().await {
        WorkerProtocolMessage::ArtifactVerificationReport(r) => r,
        other => panic!("expected ArtifactVerificationReport, got {other:?}"),
    };
    assert_eq!(report.body.computed_artifact_digest, artifact_digest);
    peer.send(WorkerProtocolMessage::ArtifactVerificationAck(
        ArtifactVerificationAckMessage::committed(
            report.envelope.message_id,
            WireArtifactStatus::Verified,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body,
        json!({
            "transfer_id": transfer_id.to_string(),
            "artifact_id": artifact_id.to_string(),
            "sealed": true,
            "artifact_status": "Verified",
        })
    );
}

#[tokio::test]
async fn seal_failed_verdict_is_a_200_response_not_a_409() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let chunks = vec![vec![1u8; 4096], vec![2u8; 50]];
    upload_chunks(&harness, &mut peer, transfer_id, &chunks).await;
    // bamepd's sealed expected digest is for DIFFERENT bytes; the Worker
    // still reports its own honest recomputation and bamepd decides Failed.
    let sealed_expected = sha256_b64(&[0u8; 10]);

    let body = json!({ "chunk_count": 2, "artifact_digest": sealed_expected }).to_string();
    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_post(
                addr,
                &seal_path(transfer_id),
                &seal_headers(),
                body.into_bytes(),
                &leaf,
            )
            .await
        }
    });

    let seal = match peer.recv().await {
        WorkerProtocolMessage::ManifestSealRequest(r) => r,
        other => panic!("got {other:?}"),
    };
    let facts = sealed_facts(2, 4096, &sealed_expected);
    let artifact_id = facts.artifact_id;
    peer.send(WorkerProtocolMessage::ManifestSealDecision(
        ManifestSealDecisionMessage::sealed(seal.envelope.message_id, facts),
    ))
    .await;

    let report = match peer.recv().await {
        WorkerProtocolMessage::ArtifactVerificationReport(r) => r,
        other => panic!("got {other:?}"),
    };
    // The Worker recomputed the REAL bytes, not the sealed expected value.
    assert_eq!(
        report.body.computed_artifact_digest,
        sha256_b64(&chunks.concat())
    );
    peer.send(WorkerProtocolMessage::ArtifactVerificationAck(
        ArtifactVerificationAckMessage::committed(
            report.envelope.message_id,
            WireArtifactStatus::Failed,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body,
        json!({
            "transfer_id": transfer_id.to_string(),
            "artifact_id": artifact_id.to_string(),
            "sealed": true,
            "artifact_status": "Failed",
        })
    );
}

#[tokio::test]
async fn seal_uses_authoritative_sealed_facts_not_the_request_body() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let chunks = vec![vec![7u8; 4096], vec![8u8; 4096], vec![9u8; 7]];
    upload_chunks(&harness, &mut peer, transfer_id, &chunks).await;
    let real_digest = sha256_b64(&chunks.concat());

    // The Agent's declared body lies about chunk_count; bamepd's sealed
    // decision is authoritative (3 chunks, 4096). D2 must use the sealed
    // facts and still recompute the real digest.
    let body = json!({ "chunk_count": 99, "artifact_digest": sha256_b64(&[1u8; 3]) }).to_string();
    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_post(
                addr,
                &seal_path(transfer_id),
                &seal_headers(),
                body.into_bytes(),
                &leaf,
            )
            .await
        }
    });

    let seal = match peer.recv().await {
        WorkerProtocolMessage::ManifestSealRequest(r) => r,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ManifestSealDecision(
        ManifestSealDecisionMessage::sealed(
            seal.envelope.message_id,
            sealed_facts(3, 4096, &real_digest),
        ),
    ))
    .await;

    let report = match peer.recv().await {
        WorkerProtocolMessage::ArtifactVerificationReport(r) => r,
        other => panic!("got {other:?}"),
    };
    assert_eq!(report.body.computed_artifact_digest, real_digest);
    peer.send(WorkerProtocolMessage::ArtifactVerificationAck(
        ArtifactVerificationAckMessage::committed(
            report.envelope.message_id,
            WireArtifactStatus::Verified,
        ),
    ))
    .await;

    assert_eq!(
        timeout(TEST_TIMEOUT, request)
            .await
            .unwrap()
            .unwrap()
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn seal_missing_local_chunk_file_fails_closed_with_the_generic_401() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    // No chunks were ever uploaded; the seal reread cannot form the stream.
    let body = json!({ "chunk_count": 2, "artifact_digest": sha256_b64(&[0u8; 4]) }).to_string();
    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_post(
                addr,
                &seal_path(transfer_id),
                &seal_headers(),
                body.into_bytes(),
                &leaf,
            )
            .await
        }
    });

    let seal = match peer.recv().await {
        WorkerProtocolMessage::ManifestSealRequest(r) => r,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ManifestSealDecision(
        ManifestSealDecisionMessage::sealed(
            seal.envelope.message_id,
            sealed_facts(2, 4096, &sha256_b64(&[0u8; 4])),
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "AUTHORIZATION_DENIED" } })
    );
    // The Artifact stays PendingVerification: no fabricated Ack was sent.
    assert!(
        timeout(Duration::from_millis(200), peer.recv())
            .await
            .is_err(),
        "a failed reread must not send an ArtifactVerificationReport"
    );
}

#[tokio::test]
async fn seal_semantic_conflicts_map_to_the_exact_409_codes() {
    for (reason, code) in [
        (
            ManifestSealRejectionReason::IncompleteManifest,
            "INCOMPLETE_MANIFEST",
        ),
        (
            ManifestSealRejectionReason::ManifestAlreadySealed,
            "MANIFEST_ALREADY_SEALED",
        ),
    ] {
        let harness = Harness::start().await;
        let mut peer = harness.fake_bamepd().await;
        let transfer_id = Uuid::new_v4();

        let body =
            json!({ "chunk_count": 1, "artifact_digest": sha256_b64(&[0u8; 4]) }).to_string();
        let request = tokio::spawn({
            let addr = harness.server_addr;
            let leaf = harness.leaf_der.clone();
            async move {
                https_post(
                    addr,
                    &seal_path(transfer_id),
                    &seal_headers(),
                    body.into_bytes(),
                    &leaf,
                )
                .await
            }
        });

        let seal = match peer.recv().await {
            WorkerProtocolMessage::ManifestSealRequest(r) => r,
            other => panic!("got {other:?}"),
        };
        peer.send(WorkerProtocolMessage::ManifestSealDecision(
            ManifestSealDecisionMessage::rejected(seal.envelope.message_id, reason),
        ))
        .await;

        let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
        assert_eq!(response.status, StatusCode::CONFLICT, "{code}");
        assert_eq!(
            response.body,
            json!({ "error": { "code": code } }),
            "{code}"
        );
    }
}

#[tokio::test]
async fn seal_retry_after_worker_crash_resumes_at_verification() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let chunks = vec![vec![11u8; 4096], vec![22u8; 12]];
    upload_chunks(&harness, &mut peer, transfer_id, &chunks).await;
    let digest = sha256_b64(&chunks.concat());

    let body = json!({ "chunk_count": 2, "artifact_digest": digest }).to_string();
    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_post(
                addr,
                &seal_path(transfer_id),
                &seal_headers(),
                body.into_bytes(),
                &leaf,
            )
            .await
        }
    });

    // bamepd already sealed on an earlier attempt -> already_pending_verification.
    let seal = match peer.recv().await {
        WorkerProtocolMessage::ManifestSealRequest(r) => r,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ManifestSealDecision(
        ManifestSealDecisionMessage::already_pending_verification(
            seal.envelope.message_id,
            sealed_facts(2, 4096, &digest),
        ),
    ))
    .await;

    let report = match peer.recv().await {
        WorkerProtocolMessage::ArtifactVerificationReport(r) => r,
        other => panic!("got {other:?}"),
    };
    assert_eq!(report.body.computed_artifact_digest, digest);
    peer.send(WorkerProtocolMessage::ArtifactVerificationAck(
        ArtifactVerificationAckMessage::committed(
            report.envelope.message_id,
            WireArtifactStatus::Verified,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["artifact_status"], json!("Verified"));
}

#[tokio::test]
async fn seal_malformed_bodies_return_400_without_reaching_bamepd() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let good_digest = sha256_b64(&[0u8; 4]);
    let cases: Vec<String> = vec![
        "not json".to_string(),
        json!({ "chunk_count": 1 }).to_string(),
        json!({ "artifact_digest": good_digest }).to_string(),
        json!({ "chunk_count": -1, "artifact_digest": good_digest }).to_string(),
        json!({ "chunk_count": 1.5, "artifact_digest": good_digest }).to_string(),
        json!({ "chunk_count": 1, "artifact_digest": "short" }).to_string(),
        json!({ "chunk_count": 1, "artifact_digest": good_digest, "extra": true }).to_string(),
    ];

    for body in cases {
        let response = https_post(
            harness.server_addr,
            &seal_path(transfer_id),
            &seal_headers(),
            body.clone().into_bytes(),
            &harness.leaf_der,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "body {body}");
        assert_eq!(
            response.body,
            json!({ "error": { "code": "MALFORMED_REQUEST" } }),
            "body {body}"
        );
    }

    assert!(
        timeout(Duration::from_millis(150), peer.recv())
            .await
            .is_err(),
        "a malformed seal body never reaches bamepd"
    );
}

// =====================================================================
// routing — the new route shapes
// =====================================================================

#[tokio::test]
async fn wrong_methods_and_unknown_shapes_are_405_and_404() {
    let harness = Harness::start().await;
    let _peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();
    let digest = sha256_b64(&[0u8; 4]);

    // GET on the chunk-index route shape -> 405 (route exists, method wrong).
    let response = send(
        harness.server_addr,
        "GET",
        &chunk_path(transfer_id, 0),
        &borrow(&put_headers(&digest)),
        box_body(Empty::<Bytes>::new()),
        &harness.leaf_der,
    )
    .await
    .unwrap();
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "METHOD_NOT_ALLOWED" } })
    );

    // PUT on the seal route shape -> 405.
    let response = https_put(
        harness.server_addr,
        &seal_path(transfer_id),
        &borrow(&put_headers(&digest)),
        vec![0u8; 4],
        &harness.leaf_der,
    )
    .await;
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);

    // Trailing slash on the seal route -> 404, never a redirect.
    let response = https_post(
        harness.server_addr,
        &format!("{}/", seal_path(transfer_id)),
        &seal_headers(),
        b"{}".to_vec(),
        &harness.leaf_der,
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        response.body,
        json!({ "error": { "code": "UNKNOWN_ROUTE" } })
    );
}

/// Isolated child process: the experimental environment cannot affect the
/// existing normal-path tests running concurrently in this integration binary.
#[test]
fn i63_batch8_https_durability_and_individual_commit_gates() {
    const CHILD: &str = "BAMEP_I63_BATCH_HTTP_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "i63_batch8_https_durability_and_individual_commit_gates",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("BAMEP_I63_WORKER_BATCH_FINALIZE", "8")
            .status()
            .unwrap();
        assert!(status.success(), "isolated batch HTTP proof failed");
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let harness = Harness::start_with_control_timeout(Duration::from_secs(8)).await;
            let mut peer = harness.fake_bamepd().await;
            let transfer_id = Uuid::new_v4();
            const SIZE: usize = 64 * 1024 * 1024;
            let payload = Bytes::from(vec![73; SIZE]);
            let digest = sha256_b64(&payload);
            let mut requests = Vec::new();
            for index in 0..8 {
                let addr = harness.server_addr;
                let leaf = harness.leaf_der.clone();
                let headers = put_headers(&digest);
                let body = payload.clone();
                requests.push(tokio::spawn(async move {
                    let mut sender = connect(addr, &leaf).await;
                    let mut builder = Request::builder().method("PUT").uri(format!(
                        "https://bamep-worker-e2b-test.local{}",
                        chunk_path(transfer_id, index)
                    ));
                    for (name, value) in headers {
                        builder = builder.header(name, value);
                    }
                    timeout(
                        Duration::from_secs(90),
                        sender.send_request(builder.body(box_body(Full::new(body))).unwrap()),
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .status()
                }));
                let query = match peer.recv().await {
                    WorkerProtocolMessage::AuthorizationQuery(q) => q,
                    other => panic!("commit before full batch: {other:?}"),
                };
                assert_eq!(query.body.chunk_index, index);
                if index == 7 {
                    // Wait for all seven actual bodies, not a timing assumption.
                    let staging = harness
                        .finalized_chunk_path(transfer_id, 0)
                        .parent()
                        .unwrap()
                        .join(".staging");
                    timeout(Duration::from_secs(60), async {
                        loop {
                            assert!(
                                (0..7).all(|i| !harness
                                    .finalized_chunk_path(transfer_id, i)
                                    .exists()),
                                "seven members must not finalize before batch is full"
                            );
                            let complete = std::fs::read_dir(&staging)
                                .map(|entries| {
                                    entries
                                        .filter_map(Result::ok)
                                        .filter(|e| {
                                            e.metadata().is_ok_and(|m| m.len() == SIZE as u64)
                                        })
                                        .count()
                                })
                                .unwrap_or(0);
                            if complete == 7 {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .expect("seven bodies staged");
                    assert!(
                        requests.iter().all(|r| !r.is_finished()),
                        "no early HTTP success"
                    );
                    assert!(
                        (0..8).all(|i| !harness.finalized_chunk_path(transfer_id, i).exists()),
                        "seven members must not be independently finalized"
                    );
                    assert!(
                        timeout(
                            Duration::from_millis(100),
                            bamep_worker_protocol::receive(&mut peer.stream)
                        )
                        .await
                        .is_err(),
                        "no commit before eighth member enters durability batch"
                    );
                }
                peer.send(WorkerProtocolMessage::AuthorizationDecision(
                    AuthorizationDecisionMessage::approved(
                        query.envelope.message_id,
                        WireDigestAlgorithm::Sha256,
                        SIZE as u32,
                        format!("batch-ticket-{index}"),
                        None,
                    ),
                ))
                .await;
            }
            let mut acceptances = Vec::new();
            for _ in 0..8 {
                let request = timeout(
                    Duration::from_secs(60),
                    bamep_worker_protocol::receive(&mut peer.stream),
                )
                .await
                .unwrap()
                .unwrap();
                match request {
                    WorkerProtocolMessage::ChunkAcceptanceRequest(r) => {
                        assert!(
                            (0..8).all(|i| harness.finalized_chunk_path(transfer_id, i).exists()),
                            "every batch final name precedes the first commit"
                        );
                        acceptances.push(r);
                    }
                    other => panic!("expected individual commit: {other:?}"),
                }
            }
            assert!(
                requests.iter().all(|r| !r.is_finished()),
                "durable bytes alone cannot ACK"
            );
            // Hold all but one control response. Each HTTP handler must wait for
            // its own durable commit, even after every batch file is durable.
            acceptances.sort_by_key(|r| r.body.chunk_index);
            for request in acceptances {
                let index = request.body.chunk_index as usize;
                assert_eq!(request.body.digest, digest);
                assert_eq!(request.body.size, SIZE as u32);
                let decision = if index == 7 {
                    ChunkAcceptanceDecisionMessage::rejected(
                        request.envelope.message_id,
                        ChunkAcceptanceRejectionReason::TransferNotContinuable,
                    )
                } else {
                    ChunkAcceptanceDecisionMessage::committed(request.envelope.message_id)
                };
                peer.send(WorkerProtocolMessage::ChunkAcceptanceDecision(decision))
                    .await;
                let status = timeout(TEST_TIMEOUT, &mut requests[index])
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    status,
                    if index == 7 {
                        StatusCode::CONFLICT
                    } else {
                        StatusCode::CREATED
                    }
                );
                assert!(
                    requests.iter().skip(index + 1).all(|r| !r.is_finished()),
                    "other requests require their own commit result"
                );
            }
            // Fresh store startup plus exact bytes proves restart-readable final
            // names. This does not claim power-cut validation on physical media.
            let _fresh = FilesystemChunkStore::initialize(&harness.storage_dir.0).unwrap();
            for index in 0..8 {
                let bytes =
                    std::fs::read(harness.finalized_chunk_path(transfer_id, index)).unwrap();
                assert_eq!(bytes.as_slice(), payload.as_ref());
                assert_eq!(sha256_b64(&bytes), digest);
            }
        });
}
