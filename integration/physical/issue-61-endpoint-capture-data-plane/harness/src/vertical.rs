//! Issue #61 CP2 — vertical composition, adapted VERBATIM (bar the four edits
//! noted below) from `crates/server/tests/support/transfer_vertical.rs` at
//! HEAD. THROWAWAY Spike scaffolding; NOT production architecture; NOT the
//! `bamepd` composition root.
//!
//! Edits vs. the upstream support module:
//!   1. `use super::TestDatabase` -> `use crate::testdb::TestDatabase`
//!      (this crate's own disposable-database helper);
//!   2. `CHUNK_SIZE` 4096 -> 8 MiB (physical-representative, CP0 finding H);
//!   3. `SOURCE_LEN` 10_000 -> 35 MiB (5 chunks: 4x8 MiB + a short 3 MiB final);
//!   4. `TempDir::fresh` roots under `<harness>/runtime/` (git-ignored) instead
//!      of the system temp dir, and a `chunk_store_root()` accessor is added.
//! Everything else is upstream code so CP2 exercises the exact real vertical.
//!
//! Shared integrated RF-005 Agent -> Server transfer harness (Issue #19
//! checkpoints C3/C4).
//!
//! Composes every real boundary the vertical needs — real Agent Protocol v1
//! WSS (real loopback TCP + pinned TLS 1.3), real trusted-bootstrap pinning,
//! real `TransferAuthorizationRequest`/`Grant`, real Worker HTTPS
//! (`bamep_worker::data_plane::DataPlane`), real `WorkerControlPlane` over
//! AF_UNIX (`bamep-worker-protocol` v1), and real PostgreSQL — and exposes the
//! composable operations the happy-path vertical and the failure/resilience
//! matrix both drive.
//!
//! Fidelity note (Issue #19 C3 §13 / C4 §15): the Worker runs as an in-process
//! `DataPlane` + IPC-client runtime rather than a spawned `bamep-worker`
//! process, mirroring `worker_data_plane_transfer_interop.rs` (Issue #39 Phase
//! E2B). Process/runtime isolation is proven separately by
//! `worker_process_supervision.rs` / `worker_runtime_ownership.rs`. The
//! transfer still crosses a real HTTPS listener and a real AF_UNIX control
//! path into real PostgreSQL, and [`Vertical::restart_worker`] performs a real
//! runtime interruption (HTTPS listener + IPC client + control plane all torn
//! down and rebuilt) against restart-stable staging and unchanged durable
//! state.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bamep_agent_protocol::{
    decode, encode, ActionDispatchMessage, AgentProtocolMessage, ProtocolId,
    TransferAuthorizationGrantMessage, TransferAuthorizationRequestMessage,
};
use bamep_domain::{
    Actor, Attempt, BootNonce, ChunkSize, DigestAlgorithm, EndpointId, JobId, SourceProvenance,
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
    CancellationService, ChunkAcceptanceService, EnrollmentService, JobSchedulingService,
    JobService, ManifestSealService, RedeemResult, TransferAuthorizationService,
    TransferDispatchResult, TransferDispatchService, TransferService,
    TransferTerminalEvidenceService,
};
use bamep_server::ports::{AgentDispatchPort, JobRepository};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::outbound_sessions::OutboundSessionDirectory;
use bamep_server::runtime::presence::PresenceRegistry;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ReservationId, ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_simulator::{
    connect_after_trusted_bootstrap, send_bootstrap_evidence, AgentProofKey,
    DataPlaneTransferAgent, SimulatedBootstrapMaterial, SimulatedPairedTrust,
    SimulatorHandshakeOutcome, TransferProgress, TrustedBootstrapFixtureIssuer,
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
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use crate::testdb::TestDatabase;

pub const TEST_TIMEOUT: Duration = Duration::from_secs(60);
/// `SOURCE_LEN` at `CHUNK_SIZE` spans 5 chunks (`4 x 8 MiB + 3 MiB`), so the
/// final chunk is genuinely short, progress carries real intermediate values,
/// and the parameters match later physical use (CP0 finding H).
pub const CHUNK_SIZE: u32 = 8 * 1024 * 1024;
pub const SOURCE_LEN: usize = 35 * 1024 * 1024;

pub type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
pub type Gateway =
    AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
pub type ClientWs = WebSocketStream<TlsStream<tokio::net::TcpStream>>;
pub type GatewayError = bamep_server::adapters::agent_gateway::AgentGatewayError;

// =====================================================================
// one Server TLS leaf identity, shared by WSS and Worker HTTPS
// (Issue #19 §7/§14: the Simulator pins exactly one fingerprint)
// =====================================================================

pub struct TempDir(pub PathBuf);

/// CP2 edit: git-ignored runtime root under the harness crate, so the
/// disposable chunk store / UDS / TLS material live on the ordinary root
/// filesystem inside this Spike directory (never `sda`/`sdb`/`sdc`, never the
/// system temp dir) and are trivially identifiable and cleaned.
pub fn runtime_root() -> PathBuf {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime");
    let _ = std::fs::create_dir_all(&base);
    base
}

impl TempDir {
    pub fn fresh(tag: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = runtime_root().join(format!("bamep-i61cp2-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create runtime dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct SharedServerIdentity {
    _dir: TempDir,
    cert_path: PathBuf,
    key_path: PathBuf,
    cert_der: CertificateDer<'static>,
    key_pkcs8_der: Vec<u8>,
    pub fingerprint: ServerCertFingerprint,
}

impl SharedServerIdentity {
    pub fn generate() -> Self {
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

    pub fn wss_acceptor(&self) -> AgentTransportAcceptor {
        AgentTransportAcceptor::new(
            vec![self.cert_der.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8_der.clone())),
        )
        .expect("build acceptor")
    }

    pub fn worker_tls(&self) -> Arc<rustls::ServerConfig> {
        build_server_config(
            &load_server_identity(&self.cert_path, &self.key_path).expect("load server identity"),
        )
        .expect("build server config")
    }
}

// =====================================================================
// durable pre-dispatch fixture — Attempt{Dispatched}, pre-ActionAck
// =====================================================================

pub struct DispatchedFixture {
    pub endpoint_id: EndpointId,
    pub transfer: bamep_domain::Transfer,
    pub attempt: Attempt,
    pub reservation: ReservationId,
    pub action_id: ProtocolId,
    pub job_id: Uuid,
    pub step_id: Uuid,
    /// The signal both the fixture's enrollment credential and every later WSS
    /// session credential are issued for — they all resolve to one Endpoint.
    pub signal: String,
}

/// Builds a durably committed non-destructive `Attempt{Dispatched}` for a
/// `bamep.m1.data-plane-transfer` action through the real Application services,
/// stopping *before* any `ActionAck` evidence. Shares `job_repo`/`arbiter` so
/// the reservation the terminal evidence releases is the one dispatch
/// acquired. `source_provenance` is the concrete durable per-Transfer source
/// identity (Issue #19 §13/§14).
pub async fn dispatched_fixture(
    pool: &PgPool,
    job_repo: &Arc<PostgresJobRepository>,
    arbiter: &Arc<TechnicalResourceArbiter>,
    signal: &str,
    source_provenance: &str,
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
                label: "rf005-vertical-harness".into(),
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
            SourceProvenance::new(source_provenance),
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
        signal: signal.to_string(),
    }
}

// =====================================================================
// the restartable Worker runtime (HTTPS data plane + IPC + control plane)
// =====================================================================

struct WorkerStack {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    _storage: Arc<TempDir>,
}

impl WorkerStack {
    /// `bind_port == 0` picks an ephemeral port; a fixed port lets a restart
    /// rebind the same origin so the granted `data_plane_base_url` is stable.
    #[allow(clippy::too_many_arguments)]
    async fn start(
        identity: &SharedServerIdentity,
        bind_port: u16,
        socket_path: PathBuf,
        storage: Arc<TempDir>,
        authorization: Arc<TransferAuthorizationService>,
        chunk_acceptance: Arc<ChunkAcceptanceService>,
        manifest_seal: Arc<ManifestSealService>,
        artifact_verification: Arc<ArtifactVerificationService>,
    ) -> Self {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let mut tasks = Vec::new();

        let (control, driver) = worker_control(
            socket_path.clone(),
            Duration::from_millis(20),
            Duration::from_secs(8),
            Uuid::new_v4(),
        );
        let chunk_store =
            FilesystemChunkStore::initialize(&storage.0).expect("initialize chunk store");
        let data_plane = DataPlane::new(
            format!("127.0.0.1:{bind_port}").parse().unwrap(),
            identity.worker_tls(),
            control.clone(),
            chunk_store,
        );
        let handle = data_plane.handle();
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
        let addr = timeout(TEST_TIMEOUT, handle.listening())
            .await
            .expect("no timeout")
            .expect("worker HTTPS bound");

        let plane = WorkerControlPlane::bind(&socket_path).expect("bind worker control plane");
        tasks.push(tokio::spawn({
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

        Self {
            addr,
            shutdown,
            tasks,
            _storage: storage,
        }
    }

    async fn stop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in self.tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
    }
}

// =====================================================================
// the vertical
// =====================================================================

pub struct Vertical {
    pub pool: PgPool,
    pub identity: Arc<SharedServerIdentity>,
    pub fixture: DispatchedFixture,

    pub arbiter: Arc<TechnicalResourceArbiter>,
    pub reservations: Arc<AttemptReservationRegistry>,
    pub job_repo: Arc<PostgresJobRepository>,
    pub job_repo_dyn: Arc<dyn JobRepository>,

    pub outbound: Arc<OutboundSessionDirectory>,
    pub presence: Arc<PresenceRegistry>,
    pub issuer: TrustedBootstrapFixtureIssuer,
    pub capability_store: Arc<CapabilityStore>,
    pub replay_cache: Arc<ReplayCache>,
    pub enrollment: Arc<Enrollment>,

    pub authorization: Arc<TransferAuthorizationService>,
    pub data_plane_base_url: String,

    worker: WorkerStack,
    worker_port: u16,
    worker_socket: TempSocketDir,
    worker_storage: Arc<TempDir>,
    // worker services shared into the (restartable) control plane
    chunk_acceptance: Arc<ChunkAcceptanceService>,
    manifest_seal: Arc<ManifestSealService>,
    artifact_verification: Arc<ArtifactVerificationService>,

    wss_listener: Arc<TcpListener>,
    pub wss_addr: SocketAddr,
}

/// A UDS pathname under a fresh owner-only dir (kept alive for the vertical's
/// whole life so a Worker restart can rebind the same path).
///
/// CP2 edit: AF_UNIX paths must fit `SUN_LEN` (~108 bytes), and this Spike
/// directory is nested deep enough that a `runtime/`-rooted socket path
/// overflows it. The socket therefore lives under a short per-user tmpfs base
/// (`$XDG_RUNTIME_DIR` / `/run/user/<uid>`, else `/tmp`), removed on drop. The
/// *chunk store* and TLS material still live under `<harness>/runtime/`.
struct TempSocketDir {
    dir: PathBuf,
    path: PathBuf,
}

impl TempSocketDir {
    fn fresh() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("b61-{}", &Uuid::new_v4().simple().to_string()[..12]));
        std::fs::create_dir_all(&dir).expect("create uds dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("w.sock");
        Self { dir, path }
    }
}

impl Drop for TempSocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Vertical {
    pub async fn start(db: &TestDatabase, signal: &str) -> Self {
        Self::start_with_provenance(db, signal, "disk-0").await
    }

    pub async fn start_with_provenance(
        db: &TestDatabase,
        signal: &str,
        source_provenance: &str,
    ) -> Self {
        let identity = Arc::new(SharedServerIdentity::generate());
        let arbiter = Arc::new(TechnicalResourceArbiter::new([(
            ResourceKind::new("network"),
            10,
        )]));
        let job_repo = Arc::new(PostgresJobRepository::new(db.pool.clone()));
        let job_repo_dyn: Arc<dyn JobRepository> = Arc::clone(&job_repo) as Arc<dyn JobRepository>;

        let fixture =
            dispatched_fixture(&db.pool, &job_repo, &arbiter, signal, source_provenance).await;

        // Reserve an ephemeral port so a later `restart_worker` rebinds the
        // same origin (SO_REUSEADDR on loopback makes the immediate rebind
        // safe).
        let worker_port = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let worker_socket = TempSocketDir::fresh();
        let worker_storage = Arc::new(TempDir::fresh("store"));

        // Shared capability authority: one instance both the Agent WSS grant
        // path and the Worker UDS decision path consume.
        let capability_store = Arc::new(CapabilityStore::new());
        let replay_cache = Arc::new(ReplayCache::new());
        let data_plane_base_url = format!("https://127.0.0.1:{worker_port}");
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

        let worker = WorkerStack::start(
            &identity,
            worker_port,
            worker_socket.path.clone(),
            Arc::clone(&worker_storage),
            Arc::clone(&authorization),
            Arc::clone(&chunk_acceptance),
            Arc::clone(&manifest_seal),
            Arc::clone(&artifact_verification),
        )
        .await;

        let wss_listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.expect("bind wss"));
        let wss_addr = wss_listener.local_addr().unwrap();

        Self {
            pool: db.pool.clone(),
            identity,
            fixture,
            arbiter,
            reservations: Arc::new(AttemptReservationRegistry::new()),
            job_repo,
            job_repo_dyn,
            outbound: Arc::new(OutboundSessionDirectory::new()),
            presence: Arc::new(PresenceRegistry::new()),
            issuer: TrustedBootstrapFixtureIssuer::from_seed([0x19; 32]),
            capability_store,
            replay_cache,
            enrollment: Arc::new(EnrollmentService::new(
                Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
                Arc::new(PostgresCredentialRedemptionRepository::new(db.pool.clone())),
            )),
            authorization,
            data_plane_base_url,
            worker,
            worker_port,
            worker_socket,
            worker_storage,
            chunk_acceptance,
            manifest_seal,
            artifact_verification,
            wss_listener,
            wss_addr,
        }
    }

    pub fn agent(&self) -> DataPlaneTransferAgent {
        DataPlaneTransferAgent::new(self.identity.fingerprint)
    }

    /// CP2 accessor: the disposable Worker filesystem chunk-store root for this
    /// vertical (git-ignored, under `<harness>/runtime/`).
    pub fn chunk_store_root(&self) -> &std::path::Path {
        self.worker_storage.0.as_path()
    }

    pub fn transfer_terminal_service(&self) -> Arc<TransferTerminalEvidenceService> {
        Arc::new(TransferTerminalEvidenceService::new(
            Arc::clone(&self.job_repo_dyn),
            Arc::clone(&self.reservations),
            Arc::clone(&self.arbiter),
        ))
    }

    fn action_evidence_service(&self) -> Arc<ActionEvidenceService> {
        Arc::new(ActionEvidenceService::new(
            Arc::clone(&self.job_repo_dyn),
            Arc::clone(&self.reservations),
            Arc::clone(&self.arbiter),
        ))
    }

    pub fn cancellation_service(&self) -> CancellationService {
        CancellationService::new(
            Arc::clone(&self.job_repo_dyn),
            Arc::clone(&self.reservations),
            Arc::clone(&self.arbiter),
            Arc::clone(&self.outbound) as Arc<dyn AgentDispatchPort>,
        )
    }

    /// A fresh production-wired gateway sharing this vertical's presence /
    /// outbound directory / services. Built per session so a post-restart
    /// reconnect naturally picks up the current `authorization` wiring.
    fn build_gateway(&self) -> Arc<Gateway> {
        Arc::new(
            Gateway::new(Arc::clone(&self.enrollment))
                .with_presence_registry(Arc::clone(&self.presence))
                .with_bootstrap_evidence_service(Arc::new(BootstrapEvidenceService::new(
                    Arc::new(PostgresEndpointRepository::new(self.pool.clone())),
                    AcceptedSiteKeys::single(self.issuer.public_key()),
                )))
                .with_outbound_session_directory(Arc::clone(&self.outbound))
                .with_action_evidence_service(self.action_evidence_service())
                .with_transfer_authorization_service(Arc::clone(&self.authorization))
                .with_transfer_terminal_evidence_service(self.transfer_terminal_service())
                .with_cancellation_service(Arc::new(self.cancellation_service()))
                .with_reconciliation_service(Arc::new(self.reconciliation_service())),
        )
    }

    /// Opens one real authenticated Agent Protocol WSS session as the
    /// fixture's Endpoint. Real loopback TCP -> pinned TLS 1.3 -> WebSocket ->
    /// AuthRequest/SessionEstablished -> BootstrapEvidence.
    pub async fn connect_agent(&self) -> AgentSession {
        let gateway = self.build_gateway();
        let acceptor = self.identity.wss_acceptor();
        let listener = Arc::clone(&self.wss_listener);
        let server_task: JoinHandle<Result<(), GatewayError>> = tokio::spawn({
            let gateway = Arc::clone(&gateway);
            async move {
                let (tcp, _peer) = listener.accept().await.expect("accept tcp");
                let mut conn = acceptor.accept(tcp).await.expect("tls + ws accept");
                let HandshakeOutcome::Established(session) =
                    gateway.handshake(&mut conn.websocket).await?
                else {
                    panic!("handshake must establish for a valid credential");
                };
                gateway
                    .run_authenticated_session(
                        &mut conn.websocket,
                        session,
                        conn.server_fingerprint,
                    )
                    .await
            }
        });

        let boot = BootOrchestrationService::new(
            Arc::new(PostgresBootContextRepository::new(self.pool.clone())),
            chrono::Duration::minutes(5),
        );
        let nonce = BootNonce::generate().unwrap();
        let credential = boot
            .issue_enrollment_credential(&self.fixture.signal, nonce, Utc::now())
            .await
            .unwrap();
        let assertion = self.issuer.issue(nonce, self.identity.fingerprint);
        let material = SimulatedBootstrapMaterial::from_assertion(&assertion);
        let paired = SimulatedPairedTrust::single(self.issuer.public_key());
        let mut conn =
            connect_after_trusted_bootstrap(self.wss_addr, "localhost", &paired, nonce, &material)
                .await
                .expect("local trust then pinned WSS succeeds");
        assert_eq!(
            conn.established.server_fingerprint(),
            self.identity.fingerprint,
            "the WSS-verified Server fingerprint is the single data-plane pin"
        );
        let SimulatorHandshakeOutcome::Established(_) =
            bamep_simulator::authenticate(&mut conn.websocket, &credential.to_wire_value())
                .await
                .expect("handshake helper must not error")
        else {
            panic!("credential must establish a session");
        };
        send_bootstrap_evidence(&mut conn.websocket, &conn.established)
            .await
            .unwrap();

        AgentSession {
            ws: conn.websocket,
            server_task,
            gateway,
        }
    }

    /// Transmits the RF-005 `ActionDispatch` exactly once through the real
    /// `OutboundSessionDirectory` (#26/#40 path), after `session` is
    /// outbound-ready.
    pub async fn dispatch_transfer(&self, session: &AgentSession) {
        wait_for_presence(&session.gateway, self.fixture.endpoint_id).await;
        let svc = ActionDispatchService::new(
            Arc::clone(&self.reservations),
            Arc::clone(&self.outbound) as Arc<dyn AgentDispatchPort>,
        );
        let sent = svc
            .dispatch_transfer(
                self.fixture.endpoint_id,
                self.fixture.attempt,
                self.fixture.reservation,
                &self.fixture.transfer,
            )
            .await;
        assert_eq!(sent, ActionDispatchOutcome::Sent);
    }

    /// A real runtime interruption: tear the Worker HTTPS listener, IPC
    /// client, and control plane down, then rebuild them against the SAME UDS
    /// path, the SAME restart-stable staging directory, and the SAME shared
    /// bamepd services / durable PostgreSQL state (Issue #19 §15).
    pub async fn restart_worker(&mut self) {
        self.worker.stop().await;
        // Let the aborted listener fully release before rebinding the port.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.worker = WorkerStack::start(
            &self.identity,
            self.worker_port,
            self.worker_socket.path.clone(),
            Arc::clone(&self.worker_storage),
            Arc::clone(&self.authorization),
            Arc::clone(&self.chunk_acceptance),
            Arc::clone(&self.manifest_seal),
            Arc::clone(&self.artifact_verification),
        )
        .await;
    }

    /// Simulates a `bamepd` restart's effect on transient authorization: a
    /// fresh in-memory `CapabilityStore`/`ReplayCache` (capabilities never
    /// survive a restart), rewired into a new `TransferAuthorizationService`
    /// and the Worker control plane. Durable PostgreSQL state is untouched
    /// (Issue #19 §30/§31).
    pub async fn restart_bamepd_transient_authority(&mut self) {
        self.worker.stop().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.capability_store = Arc::new(CapabilityStore::new());
        self.replay_cache = Arc::new(ReplayCache::new());
        self.authorization = Arc::new(TransferAuthorizationService::new(
            Arc::new(PostgresTransferAuthorizationRepository::new(
                self.pool.clone(),
            )),
            Arc::clone(&self.capability_store),
            Arc::clone(&self.replay_cache),
            self.data_plane_base_url.clone(),
        ));
        self.manifest_seal = Arc::new(ManifestSealService::new(
            Arc::new(PostgresTransferRepository::new(self.pool.clone())),
            Arc::clone(&self.capability_store),
            Arc::clone(&self.replay_cache),
        ));
        self.worker = WorkerStack::start(
            &self.identity,
            self.worker_port,
            self.worker_socket.path.clone(),
            Arc::clone(&self.worker_storage),
            Arc::clone(&self.authorization),
            Arc::clone(&self.chunk_acceptance),
            Arc::clone(&self.manifest_seal),
            Arc::clone(&self.artifact_verification),
        )
        .await;
    }

    // ---- durable readers -------------------------------------------------

    pub async fn artifact_state(&self) -> String {
        sqlx::query_scalar::<_, String>(
            "SELECT ar.state::text FROM artifacts ar \
             JOIN transfers t ON t.artifact_id = ar.id WHERE t.id = $1",
        )
        .bind(self.fixture.transfer.id.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    pub async fn artifact_capture_consistency(&self) -> String {
        sqlx::query_scalar::<_, String>(
            "SELECT ar.capture_consistency::text FROM artifacts ar \
             JOIN transfers t ON t.artifact_id = ar.id WHERE t.id = $1",
        )
        .bind(self.fixture.transfer.id.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    pub async fn attempt_state(&self) -> String {
        self.scalar(
            "SELECT state::text FROM attempts WHERE id = $1",
            self.fixture.attempt.id.0,
        )
        .await
    }

    pub async fn job_step_state(&self) -> String {
        self.scalar(
            "SELECT state::text FROM job_steps WHERE id = $1",
            self.fixture.step_id,
        )
        .await
    }

    pub async fn job_step_failure_reason(&self) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT failure_reason::text FROM job_steps WHERE id = $1",
        )
        .bind(self.fixture.step_id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    pub async fn job_state(&self) -> String {
        self.scalar(
            "SELECT state::text FROM jobs WHERE id = $1",
            self.fixture.job_id,
        )
        .await
    }

    pub async fn source_provenance(&self) -> String {
        self.scalar(
            "SELECT source_provenance FROM transfers WHERE id = $1",
            self.fixture.transfer.id.0,
        )
        .await
    }

    pub async fn held_chunk_indices(&self) -> Vec<i32> {
        sqlx::query_scalar::<_, i32>(
            "SELECT chunk_index FROM chunk_identities \
             WHERE artifact_id = $1 AND held ORDER BY chunk_index",
        )
        .bind(self.fixture.transfer.artifact_id.0)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// The durable recorded per-chunk expected identity (its 32-byte digest),
    /// whether or not the chunk is `held`. `None` = no identity recorded yet.
    pub async fn recorded_chunk_digest(&self, chunk_index: i32) -> Option<Vec<u8>> {
        sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT digest FROM chunk_identities WHERE artifact_id = $1 AND chunk_index = $2",
        )
        .bind(self.fixture.transfer.artifact_id.0)
        .bind(chunk_index)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
    }

    pub async fn event_count(&self, event_type: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM domain_events WHERE job_id = $1 AND event_type::text = $2",
        )
        .bind(self.fixture.job_id)
        .bind(event_type)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    pub async fn terminal_audit_count(&self) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM audit_records \
             WHERE attempt_id = $1 AND detail LIKE '%terminal state%'",
        )
        .bind(self.fixture.attempt.id.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    pub async fn attempt_count_for_step(&self) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM attempts WHERE job_step_id = $1")
            .bind(self.fixture.step_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    pub async fn transfer_and_artifact_ids(&self) -> (Uuid, Uuid) {
        (
            self.fixture.transfer.id.0,
            self.fixture.transfer.artifact_id.0,
        )
    }

    async fn scalar(&self, sql: &'static str, id: Uuid) -> String {
        sqlx::query_scalar::<_, String>(sql)
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Marks the fixture's Attempt uncertain via #28's connection-loss trigger,
    /// exactly as `run_authenticated_session` does on a lost session that
    /// carried an `ActionDispatch`.
    pub async fn mark_uncertain_via_reconciliation(&self) {
        let recon = self.reconciliation_service();
        recon
            .mark_endpoint_uncertain(self.fixture.endpoint_id, self.fixture.action_id)
            .await
            .unwrap();
    }

    pub fn reconciliation_service(&self) -> bamep_server::application::ReconciliationService {
        bamep_server::application::ReconciliationService::new(
            Arc::clone(&self.job_repo_dyn),
            Arc::clone(&self.reservations),
            Arc::clone(&self.arbiter),
            Arc::clone(&self.outbound) as Arc<dyn AgentDispatchPort>,
        )
    }
}

impl Drop for Vertical {
    fn drop(&mut self) {
        let _ = self.worker.shutdown.send(true);
        for task in &self.worker.tasks {
            task.abort();
        }
    }
}

// =====================================================================
// one open Agent WSS session
// =====================================================================

pub struct AgentSession {
    pub ws: ClientWs,
    pub server_task: JoinHandle<Result<(), GatewayError>>,
    pub gateway: Arc<Gateway>,
}

impl AgentSession {
    pub async fn recv(&mut self) -> AgentProtocolMessage {
        let frame = timeout(TEST_TIMEOUT, self.ws.next())
            .await
            .expect("no timeout waiting for a frame")
            .expect("a frame is present")
            .expect("frame read ok");
        decode(frame.into_text().expect("text frame").as_str()).expect("decode ok")
    }

    pub async fn send(&mut self, message: AgentProtocolMessage) {
        let wire = encode(&message).expect("encode ok");
        self.ws.send(Message::text(wire)).await.expect("send ok");
    }

    pub async fn expect_dispatch(&mut self) -> ActionDispatchMessage {
        match self.recv().await {
            AgentProtocolMessage::ActionDispatch(d) => d,
            other => panic!("expected ActionDispatch, got {other:?}"),
        }
    }

    /// Sends `TransferAuthorizationRequest` for a freshly generated ephemeral
    /// proof key and returns the matching `TransferAuthorizationGrant` — the
    /// full real WSS authorization round trip (fresh on every call, so a
    /// resume always uses fresh proof material).
    pub async fn obtain_grant(
        &mut self,
        action_id: ProtocolId,
        transfer_id: Uuid,
    ) -> (AgentProofKey, TransferAuthorizationGrantMessage) {
        let key = AgentProofKey::generate();
        self.send(AgentProtocolMessage::TransferAuthorizationRequest(
            TransferAuthorizationRequestMessage::new(
                action_id,
                ProtocolId::from_uuid(transfer_id).unwrap(),
                key.public_key_wire(),
            ),
        ))
        .await;
        match self.recv().await {
            AgentProtocolMessage::TransferAuthorizationGrant(g) => (key, g),
            other => panic!("expected TransferAuthorizationGrant, got {other:?}"),
        }
    }

    /// Closes the client side and joins the server session task, deterministically
    /// flushing every already-sent inbound frame through the message loop.
    pub async fn close_and_join(mut self) {
        let _ = self.ws.close(None).await;
        self.server_task
            .await
            .expect("server task did not panic")
            .expect("authenticated session ended cleanly");
    }

    /// Drops the client socket *without* a Close frame — a real transport
    /// interruption — and awaits the server task's own detection of it.
    pub async fn drop_ungracefully(self) {
        drop(self.ws);
        let _ = timeout(TEST_TIMEOUT, self.server_task).await;
    }
}

// =====================================================================
// C1 transfer run with ActionProgress streamed over the WSS session
// =====================================================================

pub struct TransferRun {
    pub outcome: bamep_simulator::TransferRunOutcome,
    pub progress_observed: Vec<u64>,
}

/// Drives one C1 `DataPlaneTransferAgent::run` against the real Worker HTTPS
/// origin, streaming each `ActionProgress` it produces over `session` as it is
/// produced (`tokio::join!` of the run future and a progress pump).
pub async fn run_transfer_streaming_progress<S>(
    agent: &DataPlaneTransferAgent,
    accepted: &bamep_simulator::AcceptedTransfer,
    authorization: &bamep_simulator::AgentTransferAuthorization,
    source: &S,
    options: &bamep_simulator::TransferRunOptions,
    session: &mut AgentSession,
    action_id: ProtocolId,
) -> TransferRun
where
    S: bamep_simulator::TransferSource,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
    let run = async {
        let mut sink = move |p: TransferProgress| {
            let _ = tx.send(p.bytes_processed);
        };
        agent
            .run(accepted, authorization, source, options, &mut sink)
            .await
    };
    let pump = async {
        let mut observed = Vec::new();
        while let Some(bytes) = rx.recv().await {
            observed.push(bytes);
            session
                .send(AgentProtocolMessage::ActionProgress(
                    TransferProgress {
                        bytes_processed: bytes,
                    }
                    .into_action_progress(action_id),
                ))
                .await;
        }
        observed
    };
    let (run_result, progress_observed) = tokio::join!(run, pump);
    TransferRun {
        outcome: run_result.expect("C1 run returns an outcome, not a caller-misuse error"),
        progress_observed,
    }
}

/// Bounded wait for the authenticated session to become outbound-ready.
pub async fn wait_for_presence(gateway: &Arc<Gateway>, endpoint_id: EndpointId) {
    let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if gateway.presence().is_present(endpoint_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the authenticated session never became outbound-ready");
}

/// A convenience for the many tests that need `JobId`.
pub fn job_id(v: &Vertical) -> JobId {
    JobId(v.fixture.job_id)
}
