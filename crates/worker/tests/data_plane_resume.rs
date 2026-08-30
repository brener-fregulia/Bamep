//! Real HTTPS integration tests for the Worker data-plane resume-discovery
//! operation (Issue #39 Phase E2A):
//!
//! ```text
//! real hyper-1 HTTPS client (exact leaf pin, TLS 1.3)
//!   -> real Worker TLS server (bamep_worker::data_plane::DataPlane)
//!     -> real E1 control client (bamep_worker::ipc::worker_control)
//!       -> fake bamepd UDS peer (real bamep-worker-protocol codec)
//! ```
//!
//! This proves the HTTPS <-> E1 composition, the exact `/api/data/v1/`
//! contract shapes, and fail-closed behavior without needing PostgreSQL. A
//! real-`bamepd` + real-PostgreSQL vertical lives in
//! `crates/server/tests/worker_data_plane_resume_interop.rs`.

#![cfg(unix)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::{worker_control, WorkerControlHandle};
use bamep_worker::tls::{build_server_config, load_server_identity};
use bamep_worker_protocol::{
    receive, send, HeldChunk, ResumeDiscoveryPageMessage, ServerHelloMessage, WireDigestAlgorithm,
    WorkerProtocolMessage,
};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde_json::Value;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------
// fixtures: a generated Server TLS identity + a fake bamepd UDS peer
// ---------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn fresh() -> Self {
        let dir = std::env::temp_dir().join(format!("bamep-worker-e2a-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A generated Server TLS identity written to protected PEM files, plus the
/// exact leaf DER the client pins.
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
            generate_simple_self_signed(["bamep-worker-e2a-test.local".to_string()])
                .expect("generate cert");
        let cert_path = dir.0.join("cert.pem");
        let key_path = dir.0.join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
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
        let hello = match timeout(TEST_TIMEOUT, receive(&mut stream))
            .await
            .expect("no timeout")
            .expect("recv WorkerHello")
        {
            WorkerProtocolMessage::WorkerHello(h) => h,
            other => panic!("expected WorkerHello, got {other:?}"),
        };
        send(
            &mut stream,
            &WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(hello.envelope.message_id)),
        )
        .await
        .expect("send ServerHello");
        Self { stream }
    }

    async fn recv(&mut self) -> WorkerProtocolMessage {
        timeout(TEST_TIMEOUT, receive(&mut self.stream))
            .await
            .expect("no timeout")
            .expect("recv")
    }

    async fn send(&mut self, message: WorkerProtocolMessage) {
        send(&mut self.stream, &message).await.expect("send");
    }
}

struct Harness {
    _identity: TestIdentity,
    _socket_dir: TempDir,
    server_addr: SocketAddr,
    leaf_der: Vec<u8>,
    control: WorkerControlHandle,
    listener: UnixListener,
    driver_task: JoinHandle<()>,
    server_task: JoinHandle<Result<(), bamep_worker::data_plane::DataPlaneError>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Harness {
    /// Stands up the full HTTPS server + E1 control client. The caller
    /// accepts the fake `bamepd` connection separately (so tests that need
    /// E1 unavailable can skip that).
    async fn start() -> Self {
        let identity = TestIdentity::generate();
        let leaf_der = identity.leaf_der.clone();
        let tls = build_server_config(
            &load_server_identity(&identity.cert_path, &identity.key_path).expect("load identity"),
        )
        .expect("build server config");

        let socket_dir = TempDir::fresh();
        let socket_path = socket_dir.0.join("worker.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind fake bamepd");

        let (control, driver) = worker_control(
            socket_path,
            Duration::from_millis(20),
            Duration::from_millis(800),
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

        let data_plane = DataPlane::new("127.0.0.1:0".parse().unwrap(), tls, control.clone());
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
        // Wait until E1 is ready so the resume request goes over a live gen.
        timeout(
            TEST_TIMEOUT,
            self.control.authority().wait_for(|s| s.is_available()),
        )
        .await
        .expect("no timeout")
        .expect("watch");
        peer
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
// a minimal exact-leaf-pin hyper HTTPS client (mirrors the Agent's model:
// exact ServerCertFingerprint, never hostname/DNS)
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
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected_leaf_der.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "data-plane leaf pin mismatch".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
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
    content_type: Option<String>,
    cache_control: Option<String>,
    body: Value,
    served_leaf_der: Vec<u8>,
}

async fn https_get(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    pinned_leaf_der: &[u8],
) -> HttpResponse {
    https_request("GET", addr, path, headers, pinned_leaf_der).await
}

async fn https_request(
    method: &str,
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    pinned_leaf_der: &[u8],
) -> HttpResponse {
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
    let server_name = ServerName::try_from("bamep-worker-e2a-test.local").unwrap();
    let tls = connector
        .connect(server_name, tcp)
        .await
        .expect("tls handshake");
    let served_leaf_der = tls
        .get_ref()
        .1
        .peer_certificates()
        .expect("peer certs")
        .first()
        .expect("leaf")
        .as_ref()
        .to_vec();

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .expect("http1 handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut builder = Request::builder()
        .method(method)
        .uri(format!("https://bamep-worker-e2a-test.local{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Empty::<Bytes>::new()).unwrap();

    let response = timeout(TEST_TIMEOUT, sender.send_request(request))
        .await
        .expect("no timeout")
        .expect("send request");
    let status = response.status();
    let content_type = header_string(response.headers(), "content-type");
    let cache_control = header_string(response.headers(), "cache-control");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json body");

    HttpResponse {
        status,
        content_type,
        cache_control,
        body,
        served_leaf_der,
    }
}

fn header_string(headers: &hyper::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

// ---------------------------------------------------------------------
// request builders
// ---------------------------------------------------------------------

/// A canonical proof carrier: `proof_id` (22-char base64url-no-pad) `.`
/// `issued_at` (decimal) `.` `signature` (86-char base64url-no-pad). The
/// Worker validates only this *shape*; `bamepd` verifies it cryptographically.
const CANONICAL_PROOF_CARRIER: &str = concat!(
    "AAAAAAAAAAAAAAAAAAAAAA.1700000000000.",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
);

fn valid_common_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("x-bamep-capability", "opaque-capability-token"),
        ("x-bamep-transfer-proof", CANONICAL_PROOF_CARRIER),
    ]
}

fn resume_path(transfer_id: Uuid) -> String {
    format!("/api/data/v1/transfers/{transfer_id}/chunks")
}

fn held(index: u64) -> HeldChunk {
    HeldChunk {
        chunk_index: index,
        digest: format!("digest-{index:03}"),
    }
}

// ---------------------------------------------------------------------
// §30 / §19 / §20 — multipage aggregate rendered exactly
// ---------------------------------------------------------------------

#[tokio::test]
async fn resume_success_renders_the_complete_multipage_aggregate() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_get(
                addr,
                &resume_path(transfer_id),
                &valid_common_headers(),
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    assert_eq!(query.body.transfer_id, transfer_id);
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::first_page(
            query.envelope.message_id,
            transfer_id,
            true,
            WireDigestAlgorithm::Sha256,
            4096,
            Some(5),
            vec![held(0), held(1)],
            Some("cursor-A".to_string()),
        ),
    ))
    .await;

    let cont = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryContinue(c) => c,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::continuation_page(
            cont.envelope.message_id,
            vec![held(2), held(3), held(4)],
            None,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.content_type.as_deref(), Some("application/json"));
    assert_eq!(response.cache_control.as_deref(), Some("no-store"));
    assert_eq!(
        response.body,
        serde_json::json!({
            "transfer_id": transfer_id.to_string(),
            "sealed": true,
            "digest_algorithm": "sha256",
            "chunk_size": 4096,
            "expected_chunk_count": 5,
            "held_chunks": [
                { "chunk_index": 0, "digest": "digest-000" },
                { "chunk_index": 1, "digest": "digest-001" },
                { "chunk_index": 2, "digest": "digest-002" },
                { "chunk_index": 3, "digest": "digest-003" },
                { "chunk_index": 4, "digest": "digest-004" },
            ],
        })
    );
}

#[tokio::test]
async fn resume_unsealed_renders_expected_chunk_count_null() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_get(
                addr,
                &resume_path(transfer_id),
                &valid_common_headers(),
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::first_page(
            query.envelope.message_id,
            transfer_id,
            false,
            WireDigestAlgorithm::Sha256,
            8192,
            None,
            vec![],
            None,
        ),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["sealed"], serde_json::json!(false));
    assert_eq!(
        response.body["expected_chunk_count"],
        serde_json::Value::Null
    );
    assert_eq!(response.body["held_chunks"], serde_json::json!([]));
}

// ---------------------------------------------------------------------
// §32 — generic authorization denial (exact fixed body)
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_bamepd_denial_returns_the_fixed_generic_body_with_no_reason() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_get(
                addr,
                &resume_path(transfer_id),
                &valid_common_headers(),
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::denied(query.envelope.message_id),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.body,
        serde_json::json!({ "error": { "code": "AUTHORIZATION_DENIED" } })
    );
    // Nothing enumerable leaked.
    let rendered = response.body.to_string();
    for forbidden in [
        "token",
        "proof",
        "signature",
        "replay",
        "endpoint",
        "credential",
    ] {
        assert!(!rendered.contains(forbidden), "leaked {forbidden}");
    }
}

// ---------------------------------------------------------------------
// §33 — structurally malformed requests -> 400 (never reaches E1)
// ---------------------------------------------------------------------

#[tokio::test]
async fn structurally_malformed_requests_return_400_without_reaching_bamepd() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let good = Uuid::new_v4();

    let cases: Vec<(String, Vec<(&str, &str)>)> = vec![
        // malformed route UUID
        (
            "/api/data/v1/transfers/not-a-uuid/chunks".to_string(),
            valid_common_headers(),
        ),
        // uppercase (non-canonical) UUID
        (
            resume_path(good).to_uppercase().replace("/API/DATA/V1/TRANSFERS/", "/api/data/v1/transfers/").replace("/CHUNKS", "/chunks"),
            valid_common_headers(),
        ),
        // missing capability header
        (
            resume_path(good),
            vec![("x-bamep-transfer-proof", "a.b.c")],
        ),
        // malformed proof carrier (2 segments)
        (
            resume_path(good),
            vec![
                ("x-bamep-capability", "t"),
                ("x-bamep-transfer-proof", "only.two"),
            ],
        ),
        // non-canonical issued_at (leading zero)
        (
            resume_path(good),
            vec![
                ("x-bamep-capability", "t"),
                (
                    "x-bamep-transfer-proof",
                    concat!(
                        "AAAAAAAAAAAAAAAAAAAAAA.01.",
                        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                    ),
                ),
            ],
        ),
    ];

    for (path, headers) in cases {
        let response = https_get(harness.server_addr, &path, &headers, &harness.leaf_der).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "path {path} headers {headers:?}"
        );
        assert_eq!(
            response.body,
            serde_json::json!({ "error": { "code": "MALFORMED_REQUEST" } })
        );
    }

    // bamepd received nothing.
    assert!(
        timeout(Duration::from_millis(150), peer.recv())
            .await
            .is_err(),
        "a structurally malformed request must never reach bamepd"
    );
}

// ---------------------------------------------------------------------
// §34 / §35 / §25 — trailing slash, unknown route, wrong method
// ---------------------------------------------------------------------

#[tokio::test]
async fn trailing_slash_is_not_found_and_never_a_redirect() {
    let harness = Harness::start().await;
    let _peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let response = https_get(
        harness.server_addr,
        &format!("{}/", resume_path(transfer_id)),
        &valid_common_headers(),
        &harness.leaf_der,
    )
    .await;
    // Exact route means exact route: no 3xx, a genuine 404.
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        response.body,
        serde_json::json!({ "error": { "code": "UNKNOWN_ROUTE" } })
    );
}

#[tokio::test]
async fn an_unknown_data_plane_route_returns_a_json_404_not_html() {
    let harness = Harness::start().await;
    let _peer = harness.fake_bamepd().await;

    let response = https_get(
        harness.server_addr,
        "/api/data/v1/nonsense",
        &valid_common_headers(),
        &harness.leaf_der,
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        response.body,
        serde_json::json!({ "error": { "code": "UNKNOWN_ROUTE" } })
    );
}

#[tokio::test]
async fn a_wrong_method_on_the_resume_path_returns_405() {
    let harness = Harness::start().await;
    let _peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let response = https_request(
        "DELETE",
        harness.server_addr,
        &resume_path(transfer_id),
        &valid_common_headers(),
        &harness.leaf_der,
    )
    .await;
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response.body,
        serde_json::json!({ "error": { "code": "METHOD_NOT_ALLOWED" } })
    );
}

// ---------------------------------------------------------------------
// §36 / §37 / §38 — control unavailable / timeout / continuation failure
// ---------------------------------------------------------------------

#[tokio::test]
async fn control_unavailable_returns_the_fixed_generic_denial() {
    // Start the harness but never accept the fake bamepd connection: E1 is
    // NotConnected.
    let harness = Harness::start().await;
    let transfer_id = Uuid::new_v4();

    let response = https_get(
        harness.server_addr,
        &resume_path(transfer_id),
        &valid_common_headers(),
        &harness.leaf_der,
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.body,
        serde_json::json!({ "error": { "code": "AUTHORIZATION_DENIED" } })
    );
}

#[tokio::test]
async fn a_control_timeout_returns_the_same_generic_denial() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_get(
                addr,
                &resume_path(transfer_id),
                &valid_common_headers(),
                &leaf,
            )
            .await
        }
    });

    // Receive the query but never answer it — E1's 800ms request timeout fires.
    let _ = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryQuery(q) => q,
        other => panic!("got {other:?}"),
    };

    let response = timeout(Duration::from_secs(4), request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.body,
        serde_json::json!({ "error": { "code": "AUTHORIZATION_DENIED" } })
    );
    let rendered = response.body.to_string();
    assert!(!rendered.contains("timeout") && !rendered.contains("message_id"));
}

#[tokio::test]
async fn a_denied_continuation_fails_closed_with_no_first_page_chunks() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;
    let transfer_id = Uuid::new_v4();

    let request = tokio::spawn({
        let addr = harness.server_addr;
        let leaf = harness.leaf_der.clone();
        async move {
            https_get(
                addr,
                &resume_path(transfer_id),
                &valid_common_headers(),
                &leaf,
            )
            .await
        }
    });

    let query = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryQuery(q) => q,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::first_page(
            query.envelope.message_id,
            transfer_id,
            false,
            WireDigestAlgorithm::Sha256,
            4096,
            None,
            vec![held(0), held(1)],
            Some("cursor-A".to_string()),
        ),
    ))
    .await;
    let cont = match peer.recv().await {
        WorkerProtocolMessage::ResumeDiscoveryContinue(c) => c,
        other => panic!("got {other:?}"),
    };
    peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
        ResumeDiscoveryPageMessage::denied(cont.envelope.message_id),
    ))
    .await;

    let response = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.body,
        serde_json::json!({ "error": { "code": "AUTHORIZATION_DENIED" } })
    );
    // None of the first page's chunks leaked into the failure response.
    let rendered = response.body.to_string();
    assert!(!rendered.contains("digest-000") && !rendered.contains("held_chunks"));
}

// ---------------------------------------------------------------------
// §39 — concurrent HTTPS resume requests, each gets its own aggregate
// ---------------------------------------------------------------------

#[tokio::test]
async fn concurrent_https_resume_requests_each_receive_their_own_aggregate() {
    let harness = Harness::start().await;
    let mut peer = harness.fake_bamepd().await;

    let transfers: Vec<Uuid> = (0..6).map(|_| Uuid::new_v4()).collect();
    let requests: Vec<_> = transfers
        .iter()
        .map(|&transfer_id| {
            let addr = harness.server_addr;
            let leaf = harness.leaf_der.clone();
            tokio::spawn(async move {
                (
                    transfer_id,
                    https_get(
                        addr,
                        &resume_path(transfer_id),
                        &valid_common_headers(),
                        &leaf,
                    )
                    .await,
                )
            })
        })
        .collect();

    // Collect all queries, answer in reverse arrival order with a
    // per-transfer chunk_size so each caller can verify correlation.
    let mut queries = Vec::new();
    for _ in 0..transfers.len() {
        match peer.recv().await {
            WorkerProtocolMessage::ResumeDiscoveryQuery(q) => queries.push(q),
            other => panic!("got {other:?}"),
        }
    }
    for query in queries.into_iter().rev() {
        let transfer_id = query.body.transfer_id;
        let idx = transfers.iter().position(|t| *t == transfer_id).unwrap() as u32;
        peer.send(WorkerProtocolMessage::ResumeDiscoveryPage(
            ResumeDiscoveryPageMessage::first_page(
                query.envelope.message_id,
                transfer_id,
                false,
                WireDigestAlgorithm::Sha256,
                1000 + idx,
                None,
                vec![held(idx as u64)],
                None,
            ),
        ))
        .await;
    }

    for request in requests {
        let (transfer_id, response) = timeout(TEST_TIMEOUT, request).await.unwrap().unwrap();
        assert_eq!(response.status, StatusCode::OK);
        let idx = transfers.iter().position(|t| *t == transfer_id).unwrap() as u64;
        assert_eq!(
            response.body["transfer_id"],
            serde_json::json!(transfer_id.to_string())
        );
        assert_eq!(response.body["chunk_size"], serde_json::json!(1000 + idx));
        assert_eq!(
            response.body["held_chunks"],
            serde_json::json!([{ "chunk_index": idx, "digest": format!("digest-{idx:03}") }])
        );
    }
}

// ---------------------------------------------------------------------
// §28 — the served TLS leaf is exactly the configured identity
// ---------------------------------------------------------------------

#[tokio::test]
async fn the_served_tls_leaf_is_exactly_the_configured_identity() {
    let harness = Harness::start().await;
    let _peer = harness.fake_bamepd().await;

    // The exact-pin verifier already enforces this during the handshake; this
    // additionally captures and compares the served leaf DER.
    let response = https_get(
        harness.server_addr,
        "/api/data/v1/nonsense",
        &valid_common_headers(),
        &harness.leaf_der,
    )
    .await;
    assert_eq!(response.served_leaf_der, harness.leaf_der);
}
