//! Issue #19 checkpoint C3 — the integrated RF-005 happy-path vertical.
//!
//! One deterministic successful M1 Agent -> Server capture, every boundary real:
//!
//! ```text
//! durable Job / JobStep / Transfer / Artifact (real PostgreSQL)
//!   -> #40 non-destructive commit_transfer_dispatch -> Attempt{Dispatched}
//!   -> ActionDispatchService.dispatch_transfer over the real
//!      OutboundSessionDirectory / AgentControlGateway session
//!   -> real loopback TCP -> pinned TLS 1.3 -> WebSocket -> Agent Protocol v1
//!        ActionDispatch
//!   -> committed C1 DataPlaneTransferAgent::accept -> ActionAck{Accepted}
//!   -> real WSS TransferAuthorizationRequest (ephemeral Ed25519 proof key)
//!   -> real Server TransferAuthorizationService -> TransferAuthorizationGrant{token, base_url}
//!   -> committed C1 DataPlaneTransferAgent::run:
//!        real hyper-1 HTTPS (exact leaf pin) GET resume / PUT chunks / POST seal
//!          -> real bamep_worker::data_plane::DataPlane (Worker TLS server)
//!            -> real Worker IPC client + real D1 staging + real D2 reconstruction
//!              -> real WorkerControlPlane over AF_UNIX (bamep-worker-protocol v1)
//!                -> real PostgreSQL-backed chunk acceptance / manifest seal /
//!                   Artifact verification -> durable Artifact Verified
//!   -> ActionProgress{bytes_processed} over the same WSS session
//!   -> ActionResult{Succeeded, TRANSFER_VERIFIED} over the same WSS session
//!   -> C2 TransferTerminalEvidenceService through the real AgentControlGateway
//!   -> Attempt Succeeded -> JobStep Succeeded -> Job Succeeded
//! ```
//!
//! Fidelity note (Issue #19 C3 §13): the Worker runs as an in-process
//! `bamep_worker::data_plane::DataPlane` + IPC client runtime rather than a
//! spawned `bamep-worker` process. This mirrors the repository's strongest
//! existing data-plane vertical (`worker_data_plane_transfer_interop.rs`,
//! Issue #39 Phase E2B): the transfer still crosses a real HTTPS listener and
//! a real AF_UNIX `bamep-worker-protocol` v1 control path into real
//! PostgreSQL. Process/runtime isolation itself is proven separately by
//! `worker_process_supervision.rs` and `worker_runtime_ownership.rs`; C3 does
//! not re-prove it and does not lower any other boundary's fidelity.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bamep_agent_protocol::{
    decode, encode, AgentProtocolMessage, ProtocolId, TransferAuthorizationRequestMessage,
};
use bamep_domain::{
    Actor, Attempt, BootNonce, ChunkSize, DigestAlgorithm, EndpointId, SourceProvenance,
    TransferDirection,
};
use bamep_server::adapters::agent_gateway::{AgentControlGateway, HandshakeOutcome};
use bamep_server::adapters::agent_transport::AgentTransportAcceptor;
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository, PostgresTransferAuthorizationRepository,
    PostgresTransferRepository,
};
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::{
    ActionDispatchOutcome, ActionDispatchService, ActionEvidenceService,
    ArtifactVerificationService, BootOrchestrationService, BootstrapEvidenceService,
    ChunkAcceptanceService, EnrollmentService, JobSchedulingService, JobService,
    ManifestSealService, RedeemResult, TransferAuthorizationService, TransferDispatchResult,
    TransferDispatchService, TransferService, TransferTerminalEvidenceService,
};
use bamep_server::ports::{AgentDispatchPort, JobRepository};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::outbound_sessions::OutboundSessionDirectory;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ReservationId, ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_simulator::{
    connect_after_trusted_bootstrap, send_bootstrap_evidence, AgentProofKey,
    AgentTransferAuthorization, DataPlaneTransferAgent, DataPlaneTransferDirection,
    InMemoryTransferSource, SimulatedBootstrapMaterial, SimulatedPairedTrust,
    SimulatorHandshakeOutcome, TransferActionResult, TransferProgress, TransferRunOptions,
    TransferRunOutcome, TrustedBootstrapFixtureIssuer,
};
use bamep_trusted_bootstrap::{AcceptedSiteKeys, ServerCertFingerprint};
use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::worker_control;
use bamep_worker::storage::FilesystemChunkStore;
use bamep_worker::tls::{build_server_config, load_server_identity};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sqlx::{PgPool, Row};
use support::TestDatabase;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(15);
/// The chunk size the durable `Transfer` is created with — `SOURCE_LEN` spans
/// three chunks (`4096 + 4096 + 1808`), so progress carries genuine
/// intermediate values (Issue #19 C3 §17/§19).
const CHUNK_SIZE: u32 = 4096;
const SOURCE_LEN: u64 = 10_000;

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type Gateway =
    AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type ClientWs = WebSocketStream<TlsStream<tokio::net::TcpStream>>;

// =====================================================================
// one Server TLS leaf identity, shared by WSS and Worker HTTPS
// (Issue #19 C3 §7/§14: the Simulator pins exactly one fingerprint)
// =====================================================================

struct TempDir(PathBuf);

impl TempDir {
    fn fresh(tag: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bamep-rf005-c3-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct SharedServerIdentity {
    _dir: TempDir,
    cert_path: PathBuf,
    key_path: PathBuf,
    cert_der: CertificateDer<'static>,
    key_pkcs8_der: Vec<u8>,
    fingerprint: ServerCertFingerprint,
}

impl SharedServerIdentity {
    fn generate() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::fresh("tls");
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["localhost".to_string()]).expect("generate cert");
        let cert_path = dir.0.join("cert.pem");
        let key_path = dir.0.join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            fingerprint: ServerCertFingerprint::from_leaf_der(cert.der()),
            cert_der: cert.der().clone(),
            key_pkcs8_der: signing_key.serialize_der(),
            cert_path,
            key_path,
            _dir: dir,
        }
    }

    /// The Agent Protocol WSS acceptor, built from this exact leaf.
    fn wss_acceptor(&self) -> AgentTransportAcceptor {
        AgentTransportAcceptor::new(
            vec![self.cert_der.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8_der.clone())),
        )
        .expect("build acceptor")
    }

    /// The Worker HTTPS `rustls` server config, built from the same PEM files.
    fn worker_tls(&self) -> Arc<rustls::ServerConfig> {
        build_server_config(
            &load_server_identity(&self.cert_path, &self.key_path).expect("load server identity"),
        )
        .expect("build server config")
    }
}

// =====================================================================
// durable pre-dispatch fixture — Attempt{Dispatched}, pre-ActionAck
// (the real WSS ActionAck must be what advances it to InProgress)
// =====================================================================

struct DispatchedFixture {
    endpoint_id: EndpointId,
    transfer: bamep_domain::Transfer,
    attempt: Attempt,
    reservation: ReservationId,
    action_id: ProtocolId,
    job_id: Uuid,
    step_id: Uuid,
}

/// Builds a durably committed non-destructive `Attempt{Dispatched}` for a
/// `bamep.m1.data-plane-transfer` action through the real Application services,
/// stopping *before* any `ActionAck` evidence — everything after the durable
/// dispatch commitment is proven over the real wire by the vertical itself.
/// Shares `job_repo` and `arbiter` with the caller so the reservation the
/// terminal evidence releases is the exact one the dispatch acquired.
async fn dispatched_fixture(
    pool: &PgPool,
    job_repo: &Arc<PostgresJobRepository>,
    arbiter: &Arc<TechnicalResourceArbiter>,
    signal: &str,
) -> DispatchedFixture {
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(pool.clone())),
        chrono::Duration::minutes(5),
    );
    let enrollment: Enrollment = EnrollmentService::new(
        Arc::new(PostgresEndpointRepository::new(pool.clone())),
        Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone())),
    );
    let jobs = JobService::new(Arc::clone(job_repo));
    let scheduling = JobSchedulingService::new(Arc::clone(job_repo));
    let transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
    let dispatch = TransferDispatchService::new(Arc::clone(job_repo), Arc::clone(arbiter));

    let now = Utc::now();
    let credential = boot
        .issue_enrollment_credential(signal, BootNonce::generate().unwrap(), now)
        .await
        .expect("issue enrollment credential");
    let RedeemResult::Established { endpoint_id, .. } = enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("first contact must establish a session");
    };
    enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "rf005-c3-vertical-harness".into(),
            },
            now,
        )
        .await
        .unwrap();

    let job = jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;
    scheduling.admit(job.id).await.unwrap();
    scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();

    let context = transfers
        .create_transfer_context(
            endpoint_id,
            job.id,
            step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(CHUNK_SIZE).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap();

    let TransferDispatchResult::Committed {
        outcome,
        reservation,
    } = dispatch
        .commit_transfer_dispatch(
            job.id,
            step_id,
            context.transfer.id,
            vec![ResourceClaim::new(ResourceKind::new("network"), 1)],
        )
        .await
        .unwrap()
    else {
        panic!("expected a successful non-destructive dispatch commitment");
    };
    let action_id = ProtocolId::from_uuid(outcome.attempt.action_id.0)
        .expect("a Domain ActionId is always a valid UUID v4");

    DispatchedFixture {
        endpoint_id,
        transfer: outcome.transfer,
        attempt: outcome.attempt,
        reservation,
        action_id,
        job_id: job.id.0,
        step_id: step_id.0,
    }
}

// =====================================================================
// small DB readers
// =====================================================================

async fn scalar(pool: &PgPool, sql: &'static str, id: Uuid) -> String {
    sqlx::query_scalar::<_, String>(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn artifact_state(pool: &PgPool, transfer_id: Uuid) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT ar.state::text FROM artifacts ar \
         JOIN transfers t ON t.artifact_id = ar.id WHERE t.id = $1",
    )
    .bind(transfer_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn attempt_state(pool: &PgPool, attempt_id: Uuid) -> String {
    scalar(
        pool,
        "SELECT state::text FROM attempts WHERE id = $1",
        attempt_id,
    )
    .await
}

async fn job_step_state(pool: &PgPool, step_id: Uuid) -> String {
    scalar(
        pool,
        "SELECT state::text FROM job_steps WHERE id = $1",
        step_id,
    )
    .await
}

async fn job_state(pool: &PgPool, job_id: Uuid) -> String {
    scalar(pool, "SELECT state::text FROM jobs WHERE id = $1", job_id).await
}

async fn event_count(pool: &PgPool, job_id: Uuid, event_type: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM domain_events WHERE job_id = $1 AND event_type::text = $2",
    )
    .bind(job_id)
    .bind(event_type)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn recv_msg(ws: &mut ClientWs) -> AgentProtocolMessage {
    let frame = timeout(TEST_TIMEOUT, ws.next())
        .await
        .expect("no timeout waiting for a frame")
        .expect("a frame is present")
        .expect("frame read ok");
    decode(frame.into_text().expect("text frame").as_str()).expect("decode ok")
}

async fn send_msg(ws: &mut ClientWs, message: AgentProtocolMessage) {
    let wire = encode(&message).expect("encode ok");
    ws.send(Message::text(wire)).await.expect("send ok");
}

// =====================================================================
// the vertical
// =====================================================================

#[tokio::test]
async fn rf005_happy_path_agent_to_server_capture_end_to_end() {
    let db = TestDatabase::setup().await;
    let identity = SharedServerIdentity::generate();
    let (shutdown, shutdown_rx) = watch::channel(false);
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    // ---- one arbiter / one job repository shared across every service -----
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        10,
    )]));
    let job_repo = Arc::new(PostgresJobRepository::new(db.pool.clone()));
    let job_repo_dyn: Arc<dyn JobRepository> = Arc::clone(&job_repo) as Arc<dyn JobRepository>;

    // ---- durable pre-dispatch state (Attempt{Dispatched}, pre-ActionAck) --
    let fixture = dispatched_fixture(&db.pool, &job_repo, &arbiter, "rf005-happy").await;
    let transfer_id = fixture.transfer.id.0;
    let artifact_id = fixture.transfer.artifact_id.0;
    let attempt_id = fixture.attempt.id.0;

    // Issue #19 C3 §9 — the seven-item destructive gate was never evaluated:
    // this JobStep carries no durable destructive-authorization snapshot, yet
    // the transfer dispatch committed through the #40 non-destructive path.
    let destructive_snapshot = sqlx::query(
        "SELECT authorized_inventory_revision_id, authorized_target_fingerprint \
         FROM job_steps WHERE id = $1",
    )
    .bind(fixture.step_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        destructive_snapshot
            .try_get::<Option<Uuid>, _>(0)
            .unwrap()
            .is_none()
            && destructive_snapshot
                .try_get::<Option<String>, _>(1)
                .unwrap()
                .is_none(),
        "the transfer path must dispatch without any destructive-only prerequisite"
    );

    // ---- real Worker: HTTPS data plane + IPC client runtime --------------
    let socket = support::TempSocketPath::fresh();
    let (control, driver) = worker_control(
        socket.0.clone(),
        Duration::from_millis(20),
        Duration::from_secs(6),
        Uuid::new_v4(),
    );
    let storage = TempDir::fresh("store");
    let chunk_store = FilesystemChunkStore::initialize(&storage.0).expect("initialize chunk store");
    let data_plane = DataPlane::new(
        "127.0.0.1:0".parse().unwrap(),
        identity.worker_tls(),
        control.clone(),
        chunk_store,
    );
    let data_plane_handle = data_plane.handle();
    tasks.push(tokio::spawn({
        let mut rx = shutdown_rx.clone();
        async move {
            let _ = data_plane
                .run(async move {
                    let _ = rx.wait_for(|s| *s).await;
                })
                .await;
        }
    }));
    let worker_addr: SocketAddr = timeout(TEST_TIMEOUT, data_plane_handle.listening())
        .await
        .expect("no timeout")
        .expect("worker HTTPS bound");
    let data_plane_base_url = format!("https://{worker_addr}");

    // ---- one shared Server transfer-authorization authority --------------
    // The same `TransferAuthorizationService` instance both the Agent WSS
    // grant path and the Worker UDS decision path consume, exactly as
    // `bamepd`'s composition root wires them. Its `data_plane_base_url` is the
    // real Worker HTTPS origin resolved just above.
    let capability_store = Arc::new(CapabilityStore::new());
    let replay_cache = Arc::new(ReplayCache::new());
    let authorization = Arc::new(TransferAuthorizationService::new(
        Arc::new(PostgresTransferAuthorizationRepository::new(
            db.pool.clone(),
        )),
        Arc::clone(&capability_store),
        Arc::clone(&replay_cache),
        data_plane_base_url.clone(),
    ));
    let manifest_seal = Arc::new(ManifestSealService::new(
        Arc::new(PostgresTransferRepository::new(db.pool.clone())),
        Arc::clone(&capability_store),
        Arc::clone(&replay_cache),
    ));
    let chunk_acceptance = Arc::new(ChunkAcceptanceService::new(Arc::new(
        PostgresTransferRepository::new(db.pool.clone()),
    )));
    let artifact_verification = Arc::new(ArtifactVerificationService::new(Arc::new(
        PostgresTransferRepository::new(db.pool.clone()),
    )));

    // ---- real Worker control plane over AF_UNIX -------------------------
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind worker control plane");
    tasks.push(tokio::spawn({
        let authorization = Arc::clone(&authorization);
        let rx = shutdown_rx.clone();
        async move {
            let _ = plane
                .run(
                    Arc::new(WorkerAuthorityRegistry::new()),
                    authorization,
                    chunk_acceptance,
                    manifest_seal,
                    artifact_verification,
                    rx,
                )
                .await;
        }
    }));
    tasks.push(tokio::spawn({
        let mut rx = shutdown_rx.clone();
        driver.run(async move {
            let _ = rx.wait_for(|s| *s).await;
        })
    }));
    timeout(
        TEST_TIMEOUT,
        control.authority().wait_for(|s| s.is_available()),
    )
    .await
    .expect("no timeout")
    .expect("worker IPC control available");

    // ---- the Server control plane: one production-wired gateway ---------
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x19; 32]);
    let enrollment: Arc<Enrollment> = Arc::new(EnrollmentService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        Arc::new(PostgresCredentialRedemptionRepository::new(db.pool.clone())),
    ));
    let action_evidence = Arc::new(ActionEvidenceService::new(
        Arc::clone(&job_repo_dyn),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    ));
    let transfer_terminal = Arc::new(TransferTerminalEvidenceService::new(
        Arc::clone(&job_repo_dyn),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    ));
    let gateway: Arc<Gateway> = Arc::new(
        Gateway::new(Arc::clone(&enrollment))
            .with_bootstrap_evidence_service(Arc::new(BootstrapEvidenceService::new(
                Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
                AcceptedSiteKeys::single(issuer.public_key()),
            )))
            .with_outbound_session_directory(Arc::clone(&outbound))
            .with_action_evidence_service(Arc::clone(&action_evidence))
            .with_transfer_authorization_service(Arc::clone(&authorization))
            .with_transfer_terminal_evidence_service(Arc::clone(&transfer_terminal)),
    );

    // ---- real loopback TCP -> pinned TLS 1.3 -> WebSocket ---------------
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind wss");
    let bound_addr = listener.local_addr().unwrap();
    let acceptor = identity.wss_acceptor();
    let server_task: JoinHandle<
        Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>,
    > = tokio::spawn({
        let gateway = Arc::clone(&gateway);
        async move {
            let (tcp, _peer) = listener.accept().await.expect("accept tcp");
            let mut connection = acceptor.accept(tcp).await.expect("tls + ws accept");
            let HandshakeOutcome::Established(session) =
                gateway.handshake(&mut connection.websocket).await?
            else {
                panic!("handshake must establish for a valid credential");
            };
            gateway
                .run_authenticated_session(
                    &mut connection.websocket,
                    session,
                    connection.server_fingerprint,
                )
                .await
        }
    });

    // ---- the Simulated Agent authenticates the real WSS session --------
    let wss_boot_nonce = BootNonce::generate().unwrap();
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(db.pool.clone())),
        chrono::Duration::minutes(5),
    );
    let session_credential = boot
        .issue_enrollment_credential("rf005-happy", wss_boot_nonce, Utc::now())
        .await
        .unwrap();
    let assertion = issuer.issue(wss_boot_nonce, identity.fingerprint);
    let material = SimulatedBootstrapMaterial::from_assertion(&assertion);
    let paired = SimulatedPairedTrust::single(issuer.public_key());
    let mut connection = connect_after_trusted_bootstrap(
        bound_addr,
        "localhost",
        &paired,
        wss_boot_nonce,
        &material,
    )
    .await
    .expect("local trust then pinned WSS succeeds");
    // §14 — the Agent pins exactly one fingerprint; the very value it will
    // reuse for Worker HTTPS is the one it just verified for WSS.
    assert_eq!(
        connection.established.server_fingerprint(),
        identity.fingerprint,
        "the WSS-verified Server fingerprint is the single data-plane pin"
    );
    let SimulatorHandshakeOutcome::Established(_established) = bamep_simulator::authenticate(
        &mut connection.websocket,
        &session_credential.to_wire_value(),
    )
    .await
    .expect("handshake helper must not error") else {
        panic!("credential must establish a session");
    };
    // The session authenticates as `fixture.endpoint_id` (both credentials
    // were issued for signal "rf005-happy", which resolves to the one
    // already-established Endpoint). The path enforces it end to end: the
    // `ActionDispatch` below is addressed to `fixture.endpoint_id`, the
    // `TransferAuthorizationRequest` is correlated to the authenticated
    // Endpoint by `TransferAuthorizationService::issue`, and C2's `classify`
    // rejects an `ActionResult` for a foreign Endpoint.
    send_bootstrap_evidence(&mut connection.websocket, &connection.established)
        .await
        .unwrap();
    let mut client_ws = connection.websocket;

    // ---- #26 outbound delivery of the RF-005 ActionDispatch ------------
    // Wait until this session is outbound-ready, then transmit exactly once
    // through the real `OutboundSessionDirectory` — the same boundary #26's
    // `ActionDispatchService` uses in production.
    wait_for_presence(&gateway, fixture.endpoint_id).await;
    let action_dispatch = ActionDispatchService::new(
        Arc::clone(&reservations),
        Arc::clone(&outbound) as Arc<dyn AgentDispatchPort>,
    );
    let sent = action_dispatch
        .dispatch_transfer(
            fixture.endpoint_id,
            fixture.attempt,
            fixture.reservation,
            &fixture.transfer,
        )
        .await;
    assert_eq!(
        sent,
        ActionDispatchOutcome::Sent,
        "local transport accepts the frame"
    );

    // ---- the Agent receives ActionDispatch and runs the C1 participant --
    let AgentProtocolMessage::ActionDispatch(dispatch) = recv_msg(&mut client_ws).await else {
        panic!("expected ActionDispatch");
    };
    assert_eq!(dispatch.body.action_id, fixture.action_id);
    assert_eq!(dispatch.body.action_type, "bamep.m1.data-plane-transfer");

    let agent = DataPlaneTransferAgent::new(identity.fingerprint);
    let response = agent.accept(&dispatch);
    let accepted = response.accepted.expect("C1 accepts the RF-005 dispatch");
    assert_eq!(accepted.transfer_id(), transfer_id);
    assert_eq!(accepted.artifact_id(), artifact_id);
    send_msg(
        &mut client_ws,
        AgentProtocolMessage::ActionAck(response.ack),
    )
    .await;

    // ---- real WSS TransferAuthorizationRequest / Grant ----------------
    let proof_key = AgentProofKey::generate();
    let request = TransferAuthorizationRequestMessage::new(
        fixture.action_id,
        ProtocolId::from_uuid(transfer_id).unwrap(),
        proof_key.public_key_wire(),
    );
    send_msg(
        &mut client_ws,
        AgentProtocolMessage::TransferAuthorizationRequest(request),
    )
    .await;
    let AgentProtocolMessage::TransferAuthorizationGrant(grant) = recv_msg(&mut client_ws).await
    else {
        panic!("expected TransferAuthorizationGrant");
    };
    assert!(!grant.body.token.is_empty());
    assert_eq!(
        grant.body.data_plane_base_url, data_plane_base_url,
        "the grant points the Agent at the real Worker HTTPS origin"
    );
    let authorization_material = AgentTransferAuthorization::new(
        proof_key,
        grant.body.token.clone(),
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        grant.body.data_plane_base_url.clone(),
    );

    // ---- C1 run: real HTTPS resume / upload / seal -> durable Verified --
    // Progress is streamed over the same WSS session as it is produced.
    let source = InMemoryTransferSource::pattern(SOURCE_LEN as usize, 19);
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<u64>();
    let run = async {
        let mut sink = move |p: TransferProgress| {
            let _ = progress_tx.send(p.bytes_processed);
        };
        agent
            .run(
                &accepted,
                &authorization_material,
                &source,
                &TransferRunOptions::default(),
                &mut sink,
            )
            .await
        // `sink` (and the sender) drop here, closing the progress channel.
    };
    let pump = async {
        let mut observed = Vec::new();
        while let Some(bytes) = progress_rx.recv().await {
            observed.push(bytes);
            send_msg(
                &mut client_ws,
                AgentProtocolMessage::ActionProgress(
                    TransferProgress {
                        bytes_processed: bytes,
                    }
                    .into_action_progress(fixture.action_id),
                ),
            )
            .await;
        }
        observed
    };
    let (run_result, progress_observed) = tokio::join!(run, pump);
    let outcome = run_result.expect("C1 run returns an outcome, not a caller-misuse error");

    let TransferRunOutcome::Completed(TransferActionResult::Verified {
        artifact_id: verified_artifact,
    }) = outcome
    else {
        panic!("expected Completed(Verified), got {outcome:?}");
    };
    assert_eq!(verified_artifact, artifact_id);
    // Progress followed durably-accepted bytes and carried real intermediate
    // values before the terminal outcome (§19).
    assert_eq!(progress_observed, vec![0, 4096, 8192, 10_000]);

    // ---- §22 ordering proof: Verified is durable BEFORE workflow success -
    // At this instant the Artifact is already `Verified` (the Worker seal
    // path committed it in its own earlier transaction) and no `ActionResult`
    // has crossed the wire yet — the workflow is still `Running`. Combined
    // with C2's CASE A gate (workflow success requires a durable `Verified`
    // Artifact read under lock — `transfer_terminal_evidence.rs`
    // "transfer_verified_fails_closed_against_every_non_verified_artifact_state"),
    // this proves an `ActionResult{Succeeded}` cannot commit workflow success
    // before `Verified`.
    assert_eq!(artifact_state(&db.pool, transfer_id).await, "Verified");
    assert_eq!(attempt_state(&db.pool, attempt_id).await, "InProgress");
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Running");
    assert_eq!(
        job_step_state(&db.pool, fixture.step_id).await,
        "Dispatching"
    );

    // ---- ActionResult{Succeeded, TRANSFER_VERIFIED} over the same session
    send_msg(
        &mut client_ws,
        AgentProtocolMessage::ActionResult(
            TransferActionResult::Verified {
                artifact_id: verified_artifact,
            }
            .into_action_result(fixture.action_id),
        ),
    )
    .await;

    // Deterministically flush the server's message loop: close, then join.
    client_ws.close(None).await.unwrap();
    server_task
        .await
        .expect("server task did not panic")
        .expect("authenticated session ended cleanly");

    // ---- §21 final durable assertions ---------------------------------
    assert_eq!(artifact_state(&db.pool, transfer_id).await, "Verified");
    assert_eq!(attempt_state(&db.pool, attempt_id).await, "Succeeded");
    assert_eq!(job_step_state(&db.pool, fixture.step_id).await, "Succeeded");
    assert_eq!(job_state(&db.pool, fixture.job_id).await, "Succeeded");
    assert_eq!(
        event_count(&db.pool, fixture.job_id, "JobSucceeded").await,
        1
    );
    assert_eq!(event_count(&db.pool, fixture.job_id, "JobFailed").await, 0);

    // Identity correlation held end to end.
    let bindings = sqlx::query(
        "SELECT t.artifact_id, t.attempt_id, a.action_id, s.job_id \
         FROM transfers t \
         JOIN attempts a ON a.id = t.attempt_id \
         JOIN job_steps s ON s.id = a.job_step_id \
         WHERE t.id = $1",
    )
    .bind(transfer_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(bindings.get::<Uuid, _>("artifact_id"), artifact_id);
    assert_eq!(bindings.get::<Uuid, _>("attempt_id"), attempt_id);
    assert_eq!(
        bindings.get::<Uuid, _>("action_id"),
        fixture.action_id.as_uuid()
    );
    assert_eq!(bindings.get::<Uuid, _>("job_id"), fixture.job_id);

    // §23 — no Worker -> Job coupling: the Worker only ever wrote mechanical
    // chunk/manifest/Artifact facts. It never touched the workflow tables;
    // the sole terminal workflow transition rode the Agent's WSS ActionResult
    // through C2, which is why exactly one terminal audit exists.
    let terminal_audits = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM audit_records \
         WHERE attempt_id = $1 AND detail LIKE '%terminal state%'",
    )
    .bind(attempt_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(terminal_audits, 1);

    // The reservation was released exactly once on the terminal transition —
    // full network capacity is available again.
    assert!(arbiter
        .acquire(vec![ResourceClaim::new(ResourceKind::new("network"), 10)])
        .is_ok());

    // ---- teardown ---------------------------------------------------
    let _ = shutdown.send(true);
    for task in &tasks {
        task.abort();
    }
    drop(client_ws);
    db.teardown().await;
}

/// Bounded wait for the authenticated session to become outbound-ready, so the
/// `ActionDispatch` transmit below cannot race session registration.
async fn wait_for_presence(gateway: &Arc<Gateway>, endpoint_id: EndpointId) {
    let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if gateway.presence().is_present(endpoint_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the authenticated session never became outbound-ready");
}
