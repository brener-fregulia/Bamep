//! Issue #60 Physical Integration Spike — narrow physical-integration harness.
//!
//! THROWAWAY. Composes the *existing* Server transport + gateway + Application
//! code so the physical WinPE probe can cross the real Agent Protocol v1
//! boundary. NOT the production `bamepd` composition root.
//!
//! Subcommands:
//!   serve [BIND_ADDR]   Checkpoint 4+: real TLS 1.3/WSS + AgentControlGateway
//!                       (EnrollmentService, BootstrapEvidenceService,
//!                       InventoryService) against real PostgreSQL.
//!                       Default bind 192.168.99.1:8443.
//!   provision [LABEL]   mint one disposable enrollment credential through
//!                       BootOrchestrationService and write it to
//!                       smb-share/agent-credential.txt (never logged).
//!   selftest            Checkpoint 3: drive the raw AgentTransportAcceptor
//!                       over loopback (TLS 1.3 + WSS only, no gateway).
//!
//! PostgreSQL DSN: $BAMEP_PHYSINT_DB_URL, default
//! postgresql://brener@%2Frun%2Fpostgresql/bamep_physint_spike

mod evidence;
mod pinned;

use std::path::Path;
use std::sync::Arc;

use bamep_domain::BootNonce;
use bamep_server::adapters::agent_gateway::{AgentControlGateway, HandshakeOutcome};
use bamep_server::adapters::agent_transport::AgentTransportAcceptor;
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository,
};
use bamep_server::application::{
    BootOrchestrationService, BootstrapEvidenceService, EnrollmentService, InventoryService,
};
use bamep_trusted_bootstrap::{AcceptedSiteKeys, ServerCertFingerprint};
use chrono::{Duration, Utc};
use evidence::{s, Log, V};
use futures_util::{SinkExt, StreamExt};
use rcgen::CertifiedKey;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_BIND: &str = "192.168.99.1:8443";
const DEFAULT_DB_URL: &str = "postgresql://brener@%2Frun%2Fpostgresql/bamep_physint_spike";
const EVIDENCE_FILE: &str = "evidence/harness-events.ndjson";
const CERT_DER_PATH: &str = "evidence/harness-cert.der";
const KEY_DER_PATH: &str = "evidence/harness-key.pkcs8.der";
const FINGERPRINT_PATH: &str = "evidence/harness-fingerprint.txt";
const CREDENTIAL_FILE: &str = "smb-share/agent-credential.txt";

type Gateway =
    AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn db_url() -> String {
    std::env::var("BAMEP_PHYSINT_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string())
}

/// Loads a persisted self-signed cert/key pair, or generates and persists a
/// fresh one so the pinned fingerprint is stable across harness restarts.
fn load_or_make_cert() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    if Path::new(CERT_DER_PATH).exists() && Path::new(KEY_DER_PATH).exists() {
        let cert = std::fs::read(CERT_DER_PATH).expect("read cert der");
        let key = std::fs::read(KEY_DER_PATH).expect("read key der");
        return (
            CertificateDer::from(cert),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        );
    }
    let CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["bamep-agent".to_string()])
            .expect("generate self-signed cert");
    let cert_der = cert.der().to_vec();
    let key_der = signing_key.serialize_der();
    let _ = std::fs::create_dir_all("evidence");
    std::fs::write(CERT_DER_PATH, &cert_der).expect("persist cert der");
    std::fs::write(KEY_DER_PATH, &key_der).expect("persist key der");
    (
        CertificateDer::from(cert_der),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
    )
}

async fn build_gateway(log: &Log) -> Arc<Gateway> {
    let url = db_url();
    log.emit("info", "db.connecting", &[("dsn", s(&url))]);
    let pool = bamep_server::adapters::postgres::connect(&url)
        .await
        .unwrap_or_else(|e| {
            log.emit("error", "db.connect_failed", &[("error", s(e.to_string()))]);
            std::process::exit(1);
        });
    log.emit("info", "db.connected_and_migrated", &[]);

    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let enrollment = Arc::new(EnrollmentService::new(endpoint_repo.clone(), redemption_repo));
    let inventory_service = Arc::new(InventoryService::new(Arc::new(
        PostgresInventoryRepository::new(pool.clone()),
    )));

    // BootstrapEvidenceService must be present for run_authenticated_session,
    // but this Spike's probe never sends BootstrapEvidence — this fixture
    // accepted-key set is unused on the exercised path.
    let signer = bamep_trusted_bootstrap::fixture::FixtureAssertionSigner::from_seed([0x60; 32]);
    let evidence_service = Arc::new(BootstrapEvidenceService::new(
        endpoint_repo,
        AcceptedSiteKeys::single(signer.public_key()),
    ));

    Arc::new(
        Gateway::new(enrollment)
            .with_bootstrap_evidence_service(evidence_service)
            .with_inventory_service(inventory_service),
    )
}

/// Checkpoint 3 transport-only handler (used by `selftest`): raw
/// AgentTransportAcceptor, one frame echoed, no gateway.
async fn handle_transport_only(
    log: Arc<Log>,
    acceptor: Arc<AgentTransportAcceptor>,
    tcp: tokio::net::TcpStream,
    peer: String,
) {
    match acceptor.accept(tcp).await {
        Ok(mut conn) => {
            log.emit(
                "info",
                "tls_wss.established",
                &[
                    ("peer", s(&peer)),
                    ("server_fingerprint_sha256", s(hex(conn.server_fingerprint.as_bytes()))),
                ],
            );
            if let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(std::time::Duration::from_secs(15), conn.websocket.next()).await
            {
                let shown: String = text.chars().take(400).collect();
                log.emit(
                    "info",
                    "wss.frame_recv",
                    &[("peer", s(&peer)), ("len", V::I(text.len() as i64)), ("text", s(shown))],
                );
            }
            let _ = conn
                .websocket
                .send(Message::text(r#"{"harness":"cp3","ok":true}"#))
                .await;
            let _ = conn.websocket.close(None).await;
            log.emit("info", "wss.connection_done", &[("peer", s(&peer))]);
        }
        Err(e) => log.emit(
            "warn",
            "tls_wss.failed",
            &[("peer", s(&peer)), ("error", s(e.to_string()))],
        ),
    }
}

/// Checkpoint 4+ handler: real AgentControlGateway handshake + authenticated
/// session over the accepted TLS 1.3 / WSS connection.
async fn handle_agent_connection(
    log: Arc<Log>,
    acceptor: Arc<AgentTransportAcceptor>,
    gateway: Arc<Gateway>,
    tcp: tokio::net::TcpStream,
    peer: String,
) {
    let mut conn = match acceptor.accept(tcp).await {
        Ok(c) => c,
        Err(e) => {
            log.emit(
                "warn",
                "tls_wss.failed",
                &[("peer", s(&peer)), ("error", s(e.to_string()))],
            );
            return;
        }
    };
    log.emit(
        "info",
        "tls_wss.established",
        &[
            ("peer", s(&peer)),
            ("server_fingerprint_sha256", s(hex(conn.server_fingerprint.as_bytes()))),
        ],
    );

    let fingerprint = conn.server_fingerprint;
    match gateway.handshake(&mut conn.websocket).await {
        Ok(HandshakeOutcome::Established(session)) => {
            log.emit(
                "info",
                "auth.session_established",
                &[
                    ("peer", s(&peer)),
                    ("endpoint_id", s(session.endpoint_id.0.to_string())),
                    ("session_id", s(format!("{:?}", session.session_id))),
                ],
            );
            match gateway
                .run_authenticated_session(&mut conn.websocket, session, fingerprint)
                .await
            {
                Ok(()) => log.emit(
                    "info",
                    "session.closed",
                    &[("peer", s(&peer)), ("endpoint_id", s(session.endpoint_id.0.to_string()))],
                ),
                Err(e) => log.emit(
                    "warn",
                    "session.error",
                    &[("peer", s(&peer)), ("error", s(e.to_string()))],
                ),
            }
        }
        Ok(HandshakeOutcome::Rejected) => {
            log.emit("info", "auth.rejected", &[("peer", s(&peer))]);
        }
        Err(e) => log.emit(
            "warn",
            "gateway.error",
            &[("peer", s(&peer)), ("error", s(e.to_string()))],
        ),
    }
}

async fn serve(bind: &str) {
    let log = Arc::new(Log::new(EVIDENCE_FILE));
    let (cert_der, key_der) = load_or_make_cert();
    let fingerprint = ServerCertFingerprint::from_leaf_der(cert_der.as_ref());
    let fp_hex = hex(fingerprint.as_bytes());
    let _ = std::fs::write(FINGERPRINT_PATH, format!("{fp_hex}\n"));

    let acceptor = Arc::new(
        AgentTransportAcceptor::new(vec![cert_der], key_der).expect("build AgentTransportAcceptor"),
    );
    let gateway = build_gateway(&log).await;

    let listener = match TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            log.emit(
                "error",
                "harness.bind_failed",
                &[("bind", s(bind)), ("error", s(e.to_string()))],
            );
            std::process::exit(1);
        }
    };

    log.emit(
        "info",
        "harness.listening",
        &[
            ("bind", s(bind)),
            ("server_fingerprint_sha256", s(&fp_hex)),
            ("checkpoint", s("4-agent-gateway")),
        ],
    );
    println!("bamep-physint-harness: listening on {bind} (Checkpoint 4: AgentControlGateway)");
    println!("server leaf-cert SHA-256 fingerprint:\n  {fp_hex}");

    loop {
        match listener.accept().await {
            Ok((tcp, peer)) => {
                let log = Arc::clone(&log);
                let acceptor = Arc::clone(&acceptor);
                let gateway = Arc::clone(&gateway);
                let peer = peer.to_string();
                log.emit("info", "tcp.accepted", &[("peer", s(&peer))]);
                tokio::spawn(handle_agent_connection(log, acceptor, gateway, tcp, peer));
            }
            Err(e) => log.emit("warn", "tcp.accept_error", &[("error", s(e.to_string()))]),
        }
    }
}

async fn provision(label: &str) {
    let log = Arc::new(Log::new(EVIDENCE_FILE));
    let url = db_url();
    log.emit("info", "provision.db_connecting", &[("dsn", s(&url))]);
    let pool = bamep_server::adapters::postgres::connect(&url)
        .await
        .unwrap_or_else(|e| {
            log.emit("error", "provision.db_failed", &[("error", s(e.to_string()))]);
            std::process::exit(1);
        });

    let ttl = Duration::hours(24);
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(pool.clone())),
        ttl,
    );
    let nonce = BootNonce::generate().expect("OS CSPRNG");
    let now = Utc::now();
    let credential = boot
        .issue_enrollment_credential(label, nonce, now)
        .await
        .unwrap_or_else(|e| {
            log.emit("error", "provision.issue_failed", &[("error", s(e.to_string()))]);
            std::process::exit(1);
        });

    // The wire credential is a bearer secret: written only to the git-ignored
    // lab share file, never to stdout or the evidence log.
    let wire = credential.to_wire_value();
    let _ = std::fs::create_dir_all("smb-share");
    // A prior run may have left the file mode 0444; replace it outright.
    let _ = std::fs::remove_file(CREDENTIAL_FILE);
    std::fs::write(CREDENTIAL_FILE, format!("{wire}\n")).unwrap_or_else(|e| {
        log.emit("error", "provision.write_failed", &[("error", s(e.to_string()))]);
        std::process::exit(1);
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(CREDENTIAL_FILE, std::fs::Permissions::from_mode(0o444));
    }

    log.emit(
        "info",
        "provision.ok",
        &[
            ("label", s(label)),
            ("issued_at", s(now.to_rfc3339())),
            ("ttl_hours", V::I(24)),
            ("credential_file", s(CREDENTIAL_FILE)),
            ("wire_len", V::I(wire.len() as i64)),
        ],
    );
    println!("provisioned enrollment credential -> {CREDENTIAL_FILE} (label={label}, ttl=24h)");
}

async fn selftest() {
    let log = Arc::new(Log::new(EVIDENCE_FILE));
    let (cert_der, key_der) = load_or_make_cert();
    let fingerprint = ServerCertFingerprint::from_leaf_der(cert_der.as_ref());
    let acceptor = Arc::new(
        AgentTransportAcceptor::new(vec![cert_der], key_der).expect("build AgentTransportAcceptor"),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    log.emit("info", "selftest.server_up", &[("addr", s(addr.to_string()))]);

    let server = {
        let log = Arc::clone(&log);
        let acceptor = Arc::clone(&acceptor);
        tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            handle_transport_only(log, acceptor, tcp, peer.to_string()).await;
        })
    };

    let config = pinned::pinned_tls13_client_config(fingerprint).expect("client config");
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("tcp connect");
    let server_name = rustls::pki_types::ServerName::try_from("bamep-agent").unwrap();
    let tls = connector.connect(server_name, tcp).await.expect("tls 1.3 handshake");

    let negotiated = tls.get_ref().1.protocol_version();
    log.emit(
        "info",
        "selftest.tls_connected",
        &[("negotiated_version", s(format!("{negotiated:?}")))],
    );
    assert_eq!(
        negotiated,
        Some(rustls::ProtocolVersion::TLSv1_3),
        "harness selftest must negotiate exactly TLS 1.3"
    );

    let (mut ws, _resp) = tokio_tungstenite::client_async("wss://bamep-agent/", tls)
        .await
        .expect("websocket upgrade");
    log.emit("info", "selftest.wss_upgraded", &[]);
    ws.send(Message::text(r#"{"probe":"selftest-hello"}"#))
        .await
        .expect("send frame");
    let reply = ws.next().await.expect("reply").expect("reply ok");
    log.emit(
        "info",
        "selftest.reply",
        &[("text", s(reply.into_text().unwrap_or_default().to_string()))],
    );
    let _ = ws.close(None).await;
    server.await.unwrap();
    log.emit("info", "selftest.ok", &[]);
    println!("bamep-physint-harness: selftest OK (real AgentTransportAcceptor, TLS 1.3 + WSS)");
}

#[tokio::main]
async fn main() {
    let _ = std::fs::create_dir_all("evidence");
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") | None => {
            let bind = args.next().unwrap_or_else(|| DEFAULT_BIND.to_string());
            serve(&bind).await;
        }
        Some("provision") => {
            let label = args.next().unwrap_or_else(|| "physint-cp4".to_string());
            provision(&label).await;
        }
        Some("selftest") => selftest().await,
        Some(other) => {
            eprintln!("bamep-physint-harness: unknown subcommand {other:?}");
            eprintln!("usage: bamep-physint-harness [serve [BIND] | provision [LABEL] | selftest]");
            std::process::exit(2);
        }
    }
}
