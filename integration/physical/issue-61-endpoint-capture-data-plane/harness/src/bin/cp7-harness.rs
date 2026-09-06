//! Issue #61 CP7A — bounded-prefix physical data-plane pressure harness.
//!
//! THROWAWAY Spike scaffolding. NOT production architecture, NOT `crates/agent`,
//! NOT the `bamepd` composition root, NOT the Administrative API. Standalone
//! from the FROZEN CP6 harness (`cp6-harness.rs`) — limited local duplication is
//! intentional; cross-checkpoint refactoring is deferred until after Issue #61.
//!
//! It composes the same already-proven #60 + CP2 real boundaries as CP6, plus:
//!
//!   * `TransferTerminalEvidenceService` wired into the gateway, so the probe's
//!     terminal `ActionResult{Succeeded, TRANSFER_VERIFIED}` drives the durable
//!     Attempt/JobStep/Job transition (Issue #19 C2 CASE A — commit workflow
//!     success only after independently confirming `Artifact::Verified`);
//!   * a MANDATORY `--storage-root <path>` (no default, no random-UUID fallback,
//!     no silent fallback to `runtime-cp6` or the repo root; fail closed before
//!     the Worker starts);
//!   * a narrow lab-only coordination RESPONSE carrying the Server's current UTC
//!     (`{"cp7_coord_ack":true,"server_utc_ms":N}`) for the probe's asymmetric
//!     clock pre-flight gate;
//!   * one controlled DATA-PLANE-ONLY Worker-listener interruption, auto-fired
//!     once ~N chunks are durably held (WSS / PostgreSQL / storage-root / Worker
//!     durable chunk files / Agent process all preserved);
//!   * a read-only fixture SQL final-state readout (for the Spike record and the
//!     N1 uncertain-terminal-seal finding).
//!
//! The action is `bamep.m1.data-plane-transfer`. CP7A captures ONE bounded
//! prefix of the physical source (default 2,148,532,224 bytes -> 257 chunks;
//! final chunk 1 MiB), seals, verifies, and reaches a durable Job terminal
//! state. It is NOT a complete `\\.\PhysicalDrive0` capture, NOT walkthrough D,
//! NOT Outcome A, and NOT the production `bamep.m2.endpoint-capture-transfer`
//! action (RF-2 / RF-6 / RF-7 remain unimplemented and MUST NOT be implemented
//! here). The `bamep_physint_spike` database is used but NEVER created/dropped;
//! CP7A creates a completely fresh lineage and never touches the CP6 lineage.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bamep_domain::{
    Actor, ChunkSize, DigestAlgorithm, EndpointId, SourceProvenance, TransferDirection,
};
use bamep_server::adapters::agent_gateway::{AgentControlGateway, HandshakeOutcome};
use bamep_server::adapters::agent_transport::AgentTransportAcceptor;
use bamep_server::adapters::postgres::{
    PostgresCredentialRedemptionRepository, PostgresEndpointRepository, PostgresInventoryRepository,
    PostgresJobRepository, PostgresTransferAuthorizationRepository, PostgresTransferRepository,
};
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::adapters::postgres::PostgresBootContextRepository;
use bamep_server::application::{
    ActionDispatchOutcome, ActionDispatchService, ActionEvidenceService, ArtifactVerificationService,
    BootOrchestrationService, BootstrapEvidenceService, ChunkAcceptanceService, EnrollmentService,
    JobSchedulingService, JobService, ManifestSealService, TransferAuthorizationService,
    TransferDispatchResult, TransferDispatchService, TransferService, TransferTerminalEvidenceService,
};
use bamep_domain::BootNonce;
use bamep_server::ports::{AgentDispatchPort, JobRepository};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::outbound_sessions::OutboundSessionDirectory;
use bamep_server::runtime::presence::PresenceRegistry;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{ResourceClaim, ResourceKind, TechnicalResourceArbiter};
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_trusted_bootstrap::{AcceptedSiteKeys, ServerCertFingerprint};
use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::{worker_control, WorkerControlHandle};
use bamep_worker::storage::FilesystemChunkStore;
use bamep_worker::tls::{build_server_config, load_server_identity};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const DEFAULT_DB: &str = "bamep_physint_spike";
/// CP7A bounded extent (2 GiB + 1 MiB). 256 * 8,388,608 + 1,048,576.
const PREFIX_BYTES_DEFAULT: u64 = 2_148_532_224;
const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// ~2.15 GB Artifact + a comfortable margin. Fail closed below this.
const MIN_FREE_BYTES: u64 = 3_000_000_000;

fn cfg(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
fn lab_ip() -> String {
    cfg("CP7_LAB_IP", "192.168.99.1")
}
fn wss_port() -> u16 {
    cfg("CP7_WSS_PORT", "8443").parse().unwrap()
}
fn data_plane_port() -> u16 {
    cfg("CP7_DP_PORT", "9107").parse().unwrap()
}
fn coord_port() -> u16 {
    cfg("CP7_COORD_PORT", "9106").parse().unwrap()
}
fn interrupt_after_held() -> i64 {
    cfg("CP7_INTERRUPT_AFTER_HELD", "8").parse().unwrap_or(8)
}
fn prefix_bytes() -> u64 {
    cfg("CP7_PREFIX_BYTES", &PREFIX_BYTES_DEFAULT.to_string())
        .parse()
        .unwrap_or(PREFIX_BYTES_DEFAULT)
}

fn runtime_dir() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime-cp7a");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn ev(event: &str, kv: &[(&str, String)]) {
    let mut l = format!(
        r#"{{"ts_ms":{},"component":"cp7-harness","event":"{event}""#,
        chrono::Utc::now().timestamp_millis()
    );
    for (k, v) in kv {
        l.push_str(&format!(r#","{k}":"{}""#, v.replace('"', "'")));
    }
    l.push('}');
    println!("{l}");
}
fn die(m: impl AsRef<str>) -> ! {
    eprintln!("cp7-harness: FATAL: {}", m.as_ref());
    std::process::exit(1);
}

fn db_url() -> String {
    if let Ok(u) = std::env::var("BAMEP_PHYSINT_DB_URL") {
        return u;
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| die("set BAMEP_PHYSINT_DB_URL"));
    format!("postgresql://{user}@%2Frun%2Fpostgresql/{DEFAULT_DB}")
}
fn redact(u: &str) -> String {
    let (sch, rest) = u.split_once("://").unwrap_or(("postgresql", u));
    let rest = rest.split('?').next().unwrap_or(rest);
    let (auth, db) = rest.split_once('/').unwrap_or((rest, ""));
    let host = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    format!("{sch}://<redacted>@{host}/{db}")
}

/// The mandatory `--storage-root` (or `CP7_STORAGE_ROOT`). Fails closed before
/// the Worker starts on: absent, not a directory, not canonicalizable, not
/// writable, resolves under `runtime-cp6`, or obviously insufficient free space.
fn resolve_storage_root() -> PathBuf {
    let raw = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .windows(2)
        .find(|w| w[0] == "--storage-root")
        .map(|w| w[1].clone())
        .or_else(|| std::env::var("CP7_STORAGE_ROOT").ok())
        .unwrap_or_else(|| {
            die("--storage-root <path> is MANDATORY (no default, no fallback). \
                 For CP7A use a git-ignored path on the root filesystem, e.g. \
                 integration/physical/issue-61-endpoint-capture-data-plane/harness/runtime-cp7a/chunkstore")
        });

    let path = PathBuf::from(&raw);
    if !path.exists() {
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| die(format!("--storage-root {raw}: cannot create: {e}")));
    }
    let canon = path
        .canonicalize()
        .unwrap_or_else(|e| die(format!("--storage-root {raw}: cannot canonicalize: {e}")));
    if !canon.is_dir() {
        die(format!("--storage-root {} is not a directory", canon.display()));
    }
    if let Ok(cp6) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("runtime-cp6")
        .canonicalize()
    {
        if canon.starts_with(&cp6) {
            die("--storage-root must NOT resolve under runtime-cp6/ (CP6 is frozen)");
        }
    }
    let probe = canon.join(format!(".cp7a-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"ok")
        .unwrap_or_else(|e| die(format!("--storage-root {} is not writable: {e}", canon.display())));
    let _ = std::fs::remove_file(&probe);

    match fs_free_bytes(&canon) {
        Ok(free) if free >= MIN_FREE_BYTES => {
            ev(
                "storage_root.ok",
                &[
                    ("path", canon.display().to_string()),
                    ("free_bytes", free.to_string()),
                    ("min_free_bytes", MIN_FREE_BYTES.to_string()),
                ],
            );
        }
        Ok(free) => die(format!(
            "--storage-root {} free {free} < required {MIN_FREE_BYTES} for the bounded ~2.15 GB CP7A run",
            canon.display()
        )),
        Err(e) => die(format!(
            "--storage-root {}: cannot determine free space ({e}); refusing to start",
            canon.display()
        )),
    }
    canon
}

fn fs_free_bytes(p: &Path) -> Result<u64, String> {
    let out = std::process::Command::new("df")
        .args(["-B1", "--output=avail"])
        .arg(p)
        .output()
        .map_err(|e| format!("df: {e}"))?;
    if !out.status.success() {
        return Err("df exited non-zero".into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .nth(1)
        .and_then(|l| l.trim().parse::<u64>().ok())
        .ok_or_else(|| format!("could not parse df output: {text}"))
}

/// Persisted self-signed leaf, shared by the WSS acceptor and the Worker HTTPS
/// server, so the pinned fingerprint is stable across harness restarts.
struct Identity {
    cert_der: CertificateDer<'static>,
    key_pkcs8_der: Vec<u8>,
    cert_pem_path: PathBuf,
    key_pem_path: PathBuf,
    fingerprint: ServerCertFingerprint,
}
impl Identity {
    fn load_or_make() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let d = runtime_dir();
        let (cp, kp) = (d.join("cp7-cert.pem"), d.join("cp7-key.pem"));
        let (cd, kd) = (d.join("cp7-cert.der"), d.join("cp7-key.pkcs8.der"));
        if [&cp, &kp, &cd, &kd].iter().all(|p| p.exists()) {
            std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600)).ok();
            std::fs::set_permissions(&kd, std::fs::Permissions::from_mode(0o600)).ok();
            let cert_der = CertificateDer::from(std::fs::read(&cd).unwrap());
            let key_pkcs8_der = std::fs::read(&kd).unwrap();
            let fingerprint = ServerCertFingerprint::from_leaf_der(cert_der.as_ref());
            return Self {
                cert_der,
                key_pkcs8_der,
                cert_pem_path: cp,
                key_pem_path: kp,
                fingerprint,
            };
        }
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["localhost".to_string()]).expect("gen cert");
        std::fs::write(&cp, cert.pem()).unwrap();
        std::fs::write(&kp, signing_key.serialize_pem()).unwrap();
        std::fs::write(&cd, cert.der().to_vec()).unwrap();
        std::fs::write(&kd, signing_key.serialize_der()).unwrap();
        for p in [&kp, &kd] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let fingerprint = ServerCertFingerprint::from_leaf_der(cert_der.as_ref());
        Self {
            cert_der,
            key_pkcs8_der: signing_key.serialize_der(),
            cert_pem_path: cp,
            key_pem_path: kp,
            fingerprint,
        }
    }
    fn hex_fingerprint(&self) -> String {
        self.fingerprint
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    fn wss_acceptor(&self) -> AgentTransportAcceptor {
        AgentTransportAcceptor::new(
            vec![self.cert_der.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8_der.clone())),
        )
        .expect("acceptor")
    }
    fn worker_tls(&self) -> Arc<rustls::ServerConfig> {
        build_server_config(
            &load_server_identity(&self.cert_pem_path, &self.key_pem_path).expect("identity"),
        )
        .expect("server config")
    }
}

/// The lab-only coordination payload (no PhysicalDriveN/model/serial).
#[derive(Clone, Debug)]
struct SourceSelection {
    source_observation_id: String,
    selected_agent_source_id: String,
}

type Gateway =
    AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;

#[allow(clippy::too_many_arguments)]
async fn build_gateway(
    pool: &PgPool,
    presence: Arc<PresenceRegistry>,
    outbound: Arc<OutboundSessionDirectory>,
    authorization: Arc<TransferAuthorizationService>,
    job_repo_dyn: Arc<dyn JobRepository>,
    reservations: Arc<AttemptReservationRegistry>,
    arbiter: Arc<TechnicalResourceArbiter>,
) -> Arc<Gateway> {
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let enrollment = Arc::new(EnrollmentService::new(endpoint_repo.clone(), redemption_repo));
    let inventory = Arc::new(bamep_server::application::InventoryService::new(Arc::new(
        PostgresInventoryRepository::new(pool.clone()),
    )));
    let signer = bamep_trusted_bootstrap::fixture::FixtureAssertionSigner::from_seed([0x61; 32]);
    let evidence = Arc::new(BootstrapEvidenceService::new(
        endpoint_repo,
        AcceptedSiteKeys::single(signer.public_key()),
    ));
    let action_evidence = Arc::new(ActionEvidenceService::new(
        Arc::clone(&job_repo_dyn),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    ));
    // CP7A addition vs CP6: consume the probe's terminal ActionResult and drive
    // the durable Attempt/JobStep/Job transition (existing service, unchanged).
    let transfer_terminal = Arc::new(TransferTerminalEvidenceService::new(
        Arc::clone(&job_repo_dyn),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    ));
    Arc::new(
        Gateway::new(enrollment)
            .with_bootstrap_evidence_service(evidence)
            .with_inventory_service(inventory)
            .with_presence_registry(presence)
            .with_outbound_session_directory(outbound)
            .with_action_evidence_service(action_evidence)
            .with_transfer_authorization_service(authorization)
            .with_transfer_terminal_evidence_service(transfer_terminal),
    )
}

/// (Re)spawns the Worker HTTPS data plane against the SAME storage root + SAME
/// control-plane handle. Returns the task handle and the bound address.
async fn spawn_data_plane(
    dp_addr: std::net::SocketAddr,
    tls: Arc<rustls::ServerConfig>,
    control: WorkerControlHandle,
    storage_root: PathBuf,
    mut shutdown_rx: watch::Receiver<bool>,
) -> (tokio::task::JoinHandle<()>, std::net::SocketAddr) {
    let chunk_store = FilesystemChunkStore::initialize(&storage_root).expect("chunk store");
    let data_plane = DataPlane::new(dp_addr, tls, control, chunk_store);
    let handle = data_plane.handle();
    let jh = tokio::spawn(async move {
        let _ = data_plane
            .run(async move {
                let _ = shutdown_rx.wait_for(|s| *s).await;
            })
            .await;
    });
    let bound = handle.listening().await.expect("worker https bound");
    (jh, bound)
}

/// `cp7-harness issue-credential <signal>` — mint one fresh first-contact
/// enrollment credential against `bamep_physint_spike` and print it. Used by the
/// physical runbook and the host-loopback smoke; the database is never
/// created/dropped and no CP6 row is touched.
async fn issue_credential(signal: &str) -> ! {
    let url = db_url();
    eprintln!("cp7-harness: issue-credential -> {}", redact(&url));
    let pool = bamep_server::adapters::postgres::connect(&url)
        .await
        .unwrap_or_else(|e| die(format!("connect: {e}")));
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(pool)),
        chrono::Duration::minutes(30),
    );
    let credential = boot
        .issue_enrollment_credential(signal, BootNonce::generate().unwrap(), chrono::Utc::now())
        .await
        .unwrap_or_else(|e| die(format!("issue_enrollment_credential: {e:?}")));
    println!("{}", credential.to_wire_value());
    std::process::exit(0);
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("issue-credential") {
        let signal = argv.get(2).cloned().unwrap_or_else(|| {
            die("usage: cp7-harness issue-credential <signal>")
        });
        issue_credential(&signal).await;
    }

    let lab_ip = lab_ip();
    let (wss_port, dp_port, coord_port) = (wss_port(), data_plane_port(), coord_port());
    let storage_root = resolve_storage_root();
    let prefix_bytes = prefix_bytes();
    let chunk_count = prefix_bytes.div_ceil(CHUNK_SIZE);
    ev(
        "cp7a.plan",
        &[
            ("prefix_bytes", prefix_bytes.to_string()),
            ("chunk_size", CHUNK_SIZE.to_string()),
            ("chunk_count", chunk_count.to_string()),
            (
                "final_chunk_bytes",
                (prefix_bytes - (chunk_count - 1) * CHUNK_SIZE).to_string(),
            ),
            (
                "capture_extent",
                "bounded_prefix_pressure — NOT a complete capture, NOT walkthrough D, NOT Outcome A"
                    .into(),
            ),
        ],
    );

    let url = db_url();
    ev("db.connecting", &[("target", redact(&url))]);
    let pool = bamep_server::adapters::postgres::connect(&url)
        .await
        .unwrap_or_else(|e| die(format!("connect: {e}")));
    ev("db.connected_and_migrated", &[]);

    let identity = Arc::new(Identity::load_or_make());
    let fp = identity.hex_fingerprint();
    std::fs::write(runtime_dir().join("cp7-fingerprint.txt"), format!("{fp}\n")).ok();
    ev("identity.ready", &[("server_leaf_sha256", fp.clone())]);
    println!("cp7-harness: server leaf-cert SHA-256 fingerprint:\n  {fp}");

    // ---- shared runtime state ----------------------------------------
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        10,
    )]));
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let presence = Arc::new(PresenceRegistry::new());
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let job_repo_dyn: Arc<dyn JobRepository> = Arc::clone(&job_repo) as Arc<dyn JobRepository>;

    let capability_store = Arc::new(CapabilityStore::new());
    let replay_cache = Arc::new(ReplayCache::new());
    let data_plane_base_url = format!("https://{lab_ip}:{dp_port}");
    let authorization = Arc::new(TransferAuthorizationService::new(
        Arc::new(PostgresTransferAuthorizationRepository::new(pool.clone())),
        Arc::clone(&capability_store),
        Arc::clone(&replay_cache),
        data_plane_base_url.clone(),
    ));
    let chunk_acceptance = Arc::new(ChunkAcceptanceService::new(Arc::new(
        PostgresTransferRepository::new(pool.clone()),
    )));
    let manifest_seal = Arc::new(ManifestSealService::new(
        Arc::new(PostgresTransferRepository::new(pool.clone())),
        Arc::clone(&capability_store),
        Arc::clone(&replay_cache),
    ));
    let artifact_verification = Arc::new(ArtifactVerificationService::new(Arc::new(
        PostgresTransferRepository::new(pool.clone()),
    )));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ---- Worker stack: UDS control plane + RESTARTABLE Worker HTTPS ---
    let socket_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("b61cp7-{}", &Uuid::new_v4().simple().to_string()[..12]));
    std::fs::create_dir_all(&socket_dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket_path = socket_dir.join("w.sock");
    ev(
        "worker.storage_root",
        &[("path", storage_root.display().to_string())],
    );

    let (control, driver) = worker_control(
        socket_path.clone(),
        Duration::from_millis(20),
        Duration::from_secs(8),
        Uuid::new_v4(),
    );
    let dp_addr: std::net::SocketAddr = format!("{lab_ip}:{dp_port}").parse().unwrap();

    // The restartable data-plane supervisor: one task owns the listener's
    // lifecycle and rebinds the SAME port / SAME storage root on each restart
    // request (WSS / PostgreSQL / control plane / durable chunk files all
    // preserved).
    let (restart_tx, mut restart_rx) = mpsc::channel::<()>(4);
    {
        let tls = identity.worker_tls();
        let control = control.clone();
        let storage_root = storage_root.clone();
        let base_url = data_plane_base_url.clone();
        tokio::spawn(async move {
            let (mut kill_tx, kill_rx) = watch::channel(false);
            let (mut jh, bound) = spawn_data_plane(
                dp_addr,
                tls.clone(),
                control.clone(),
                storage_root.clone(),
                kill_rx,
            )
            .await;
            ev(
                "worker.https_listening",
                &[("origin", base_url.clone()), ("bound", bound.to_string())],
            );
            while restart_rx.recv().await.is_some() {
                ev("cp7a.interruption.begin", &[("note", "data-plane-only listener restart".into())]);
                let _ = kill_tx.send(true);
                let _ = jh.await;
                tokio::time::sleep(Duration::from_millis(400)).await;
                let (ntx, nrx) = watch::channel(false);
                kill_tx = ntx;
                let (njh, nbound) = spawn_data_plane(
                    dp_addr,
                    tls.clone(),
                    control.clone(),
                    storage_root.clone(),
                    nrx,
                )
                .await;
                jh = njh;
                ev(
                    "cp7a.interruption.recovered",
                    &[("bound", nbound.to_string())],
                );
            }
        });
    }

    let plane = WorkerControlPlane::bind(&socket_path).expect("bind control plane");
    {
        let rx = shutdown_rx.clone();
        let (a, ca, ms, av) = (
            Arc::clone(&authorization),
            Arc::clone(&chunk_acceptance),
            Arc::clone(&manifest_seal),
            Arc::clone(&artifact_verification),
        );
        tokio::spawn(async move {
            let _ = plane
                .run(Arc::new(WorkerAuthorityRegistry::new()), a, ca, ms, av, rx)
                .await;
        });
    }
    {
        let mut rx = shutdown_rx.clone();
        tokio::spawn(driver.run(async move {
            let _ = rx.wait_for(|s| *s).await;
        }));
    }
    control
        .authority()
        .wait_for(|s| s.is_available())
        .await
        .expect("worker ipc available");
    ev("worker.ipc_available", &[]);

    // ---- coordination listener (fixture orchestration only) ----------
    let (coord_tx, mut coord_rx) = mpsc::channel::<SourceSelection>(4);
    {
        let coord_addr = format!("{lab_ip}:{coord_port}");
        let listener = TcpListener::bind(&coord_addr)
            .await
            .unwrap_or_else(|e| die(format!("bind coord {coord_addr}: {e}")));
        ev("coord.listening", &[("addr", coord_addr)]);
        let tx = coord_tx.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (rd, mut wr) = stream.into_split();
                    let mut lines = BufReader::new(rd).lines();
                    if let Ok(Some(line)) = lines.next_line().await {
                        // Always answer with the Server's current UTC, so the
                        // probe's asymmetric clock pre-flight has an authoritative
                        // reference. Narrow lab-only fixture, NOT production sync.
                        let ack = format!(
                            r#"{{"cp7_coord_ack":true,"server_utc_ms":{}}}"#,
                            chrono::Utc::now().timestamp_millis()
                        );
                        let _ = wr.write_all(format!("{ack}\n").as_bytes()).await;
                        let _ = wr.flush().await;
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                            if v.get("cp7_coord").and_then(|x| x.as_str()) == Some("source_selection")
                            {
                                let sel = SourceSelection {
                                    source_observation_id: v["source_observation_id"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_string(),
                                    selected_agent_source_id: v["selected_agent_source_id"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_string(),
                                };
                                ev(
                                    "coord.received",
                                    &[
                                        ("source_observation_id", sel.source_observation_id.clone()),
                                        (
                                            "selected_agent_source_id",
                                            sel.selected_agent_source_id.clone(),
                                        ),
                                    ],
                                );
                                let _ = tx.send(sel).await;
                            }
                        }
                    }
                });
            }
        });
    }

    // ---- orchestrator: fresh lineage + dispatch + watchers ----------
    {
        let pool = pool.clone();
        let job_repo = Arc::clone(&job_repo);
        let arbiter = Arc::clone(&arbiter);
        let reservations = Arc::clone(&reservations);
        let outbound = Arc::clone(&outbound);
        let presence = Arc::clone(&presence);
        let restart_tx = restart_tx.clone();
        let interrupt_threshold = interrupt_after_held();
        tokio::spawn(async move {
            let Some(sel) = coord_rx.recv().await else {
                return;
            };
            let (endpoint_id, revision_id) = loop {
                let row: Option<(Uuid, Uuid, serde_json::Value)> = sqlx::query_as(
                    "SELECT endpoint_id, revision_id, inventory \
                     FROM inventory_revisions \
                     WHERE inventory->>'capture_source_observation_id' = $1 \
                     ORDER BY recorded_at DESC LIMIT 1",
                )
                .bind(&sel.source_observation_id)
                .fetch_optional(&pool)
                .await
                .unwrap_or(None);
                if let Some((eid, rid, inv)) = row {
                    let ids: Vec<String> = inv
                        .get("capturable_sources")
                        .and_then(|a| a.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| {
                                    s.get("agent_source_id")
                                        .and_then(|x| x.as_str())
                                        .map(String::from)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if !ids.contains(&sel.selected_agent_source_id) {
                        ev(
                            "orchestrator.selected_id_not_in_revision",
                            &[("revision_id", rid.to_string())],
                        );
                        return;
                    }
                    ev(
                        "orchestrator.inventory_revision_matched",
                        &[
                            ("endpoint_id", eid.to_string()),
                            ("inventory_revision_id", rid.to_string()),
                        ],
                    );
                    break (eid, rid);
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            };

            // LOUDLY descriptive-only bounded-prefix provenance (CP7A markers).
            let provenance = serde_json::json!({
                "_schema": "issue-61-cp7a.descriptive-source-provenance.v1",
                "capture_extent": "bounded_prefix_pressure",
                "not_a_complete_source_capture": true,
                "prefix_bytes": prefix_bytes,
                "descriptive_only": true,
                "not_a_validated_source_reference": true,
                "server_side_freshness_validation_exists": false,
                "action_type": "bamep.m1.data-plane-transfer",
                "source_reference": {
                    "inventory_revision_id": revision_id.to_string(),
                    "source_observation_id": sel.source_observation_id,
                    "agent_source_id": sel.selected_agent_source_id,
                }
            })
            .to_string();

            let enrollment = EnrollmentService::new(
                Arc::new(PostgresEndpointRepository::new(pool.clone())),
                Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone())),
            );
            let ident: String =
                sqlx::query_scalar("SELECT identity_state::text FROM endpoints WHERE id=$1")
                    .bind(endpoint_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if ident != "Enrolled" {
                enrollment
                    .approve_enrollment(
                        EndpointId(endpoint_id),
                        Actor::Operator {
                            label: "issue-61-cp7a-harness".into(),
                        },
                        chrono::Utc::now(),
                    )
                    .await
                    .unwrap_or_else(|e| die(format!("approve_enrollment: {e:?}")));
                ev(
                    "orchestrator.endpoint_enrolled",
                    &[("endpoint_id", endpoint_id.to_string())],
                );
            }

            let jobs = JobService::new(Arc::clone(&job_repo));
            let scheduling = JobSchedulingService::new(Arc::clone(&job_repo));
            let transfers =
                TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
            let dispatch = TransferDispatchService::new(Arc::clone(&job_repo), Arc::clone(&arbiter));

            let job = jobs
                .create_workflow(EndpointId(endpoint_id), 1)
                .await
                .unwrap_or_else(|e| die(format!("create_workflow: {e:?}")));
            let step = job.steps[0].id;
            scheduling.admit(job.id).await.unwrap();
            scheduling
                .satisfy_current_step_preconditions(job.id, step)
                .await
                .unwrap();
            let ctx = transfers
                .create_transfer_context(
                    EndpointId(endpoint_id),
                    job.id,
                    step,
                    TransferDirection::AgentToServer,
                    DigestAlgorithm::Sha256,
                    ChunkSize::new(CHUNK_SIZE as u32).unwrap(),
                    SourceProvenance::new(provenance),
                )
                .await
                .unwrap_or_else(|e| die(format!("create_transfer_context: {e:?}")));
            let artifact_id = ctx.transfer.artifact_id.0;
            ev(
                "orchestrator.context_created",
                &[
                    ("job_id", job.id.0.to_string()),
                    ("job_step_id", step.0.to_string()),
                    ("transfer_id", ctx.transfer.id.0.to_string()),
                    ("artifact_id", artifact_id.to_string()),
                ],
            );

            let TransferDispatchResult::Committed { outcome, reservation } = dispatch
                .commit_transfer_dispatch(
                    job.id,
                    step,
                    ctx.transfer.id,
                    vec![ResourceClaim::new(ResourceKind::new("network"), 1)],
                )
                .await
                .unwrap_or_else(|e| die(format!("commit_transfer_dispatch: {e:?}")))
            else {
                die("transfer dispatch not committed");
            };
            let action_id = outcome.attempt.action_id.0;
            let attempt_id = outcome.attempt.id.0;
            ev(
                "orchestrator.dispatch_committed",
                &[
                    ("attempt_id", attempt_id.to_string()),
                    ("action_id", action_id.to_string()),
                ],
            );

            // ---- one controlled data-plane interruption watcher --------
            {
                let pool = pool.clone();
                let restart_tx = restart_tx.clone();
                tokio::spawn(async move {
                    if interrupt_threshold <= 0 {
                        ev("cp7a.interruption.disabled", &[]);
                        return;
                    }
                    loop {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let n: i64 = sqlx::query_scalar(
                            "SELECT count(*) FROM chunk_identities WHERE artifact_id=$1 AND held",
                        )
                        .bind(artifact_id)
                        .fetch_one(&pool)
                        .await
                        .unwrap_or(0);
                        if n >= interrupt_threshold {
                            ev(
                                "cp7a.interruption.trigger",
                                &[
                                    ("held_chunks", n.to_string()),
                                    ("threshold", interrupt_threshold.to_string()),
                                ],
                            );
                            let _ = restart_tx.send(()).await;
                            return;
                        }
                    }
                });
            }

            // ---- read-only final-state watcher (Spike record) --------
            {
                let pool = pool.clone();
                let job_id = job.id.0;
                tokio::spawn(async move {
                    for _ in 0..900 {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        let row: Result<
                            Option<(String, String, String, String, Option<bool>, Option<i32>)>,
                            _,
                        > = sqlx::query_as(
                            "SELECT j.state::text, js.state::text, a.state::text, ar.state::text, \
                                    cm.sealed, cm.chunk_count \
                             FROM jobs j \
                             JOIN job_steps js ON js.job_id = j.id \
                             JOIN attempts a ON a.job_step_id = js.id \
                             JOIN transfers t ON t.job_step_id = js.id \
                             JOIN artifacts ar ON ar.id = t.artifact_id \
                             LEFT JOIN chunk_manifests cm ON cm.artifact_id = ar.id \
                             WHERE j.id = $1 LIMIT 1",
                        )
                        .bind(job_id)
                        .fetch_optional(&pool)
                        .await;
                        match row {
                            Ok(Some((j, js, a, ar, sealed, cc))) => {
                                let terminal = !matches!(j.as_str(), "Pending" | "Running");
                                ev(
                                    if terminal {
                                        "cp7a.final_state"
                                    } else {
                                        "cp7a.state.poll"
                                    },
                                    &[
                                        ("job", j.clone()),
                                        ("job_step", js),
                                        ("attempt", a),
                                        ("artifact", ar),
                                        ("manifest_sealed", format!("{sealed:?}")),
                                        ("chunk_count", format!("{cc:?}")),
                                    ],
                                );
                                if terminal {
                                    return;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => ev("cp7a.state.poll_error", &[("error", e.to_string())]),
                        }
                    }
                });
            }

            for _ in 0..200 {
                if presence.is_present(EndpointId(endpoint_id)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let svc = ActionDispatchService::new(
                Arc::clone(&reservations),
                Arc::clone(&outbound) as Arc<dyn AgentDispatchPort>,
            );
            let sent = svc
                .dispatch_transfer(
                    EndpointId(endpoint_id),
                    outcome.attempt,
                    reservation,
                    &outcome.transfer,
                )
                .await;
            ev(
                "orchestrator.action_dispatched",
                &[("outcome", format!("{sent:?}"))],
            );
            if sent != ActionDispatchOutcome::Sent {
                ev("orchestrator.dispatch_not_sent", &[]);
            }
        });
    }

    // ---- WSS accept loop --------------------------------------------
    let acceptor = Arc::new(identity.wss_acceptor());
    let gateway = build_gateway(
        &pool,
        Arc::clone(&presence),
        Arc::clone(&outbound),
        Arc::clone(&authorization),
        Arc::clone(&job_repo_dyn),
        Arc::clone(&reservations),
        Arc::clone(&arbiter),
    )
    .await;

    let wss_addr = format!("{lab_ip}:{wss_port}");
    let listener = TcpListener::bind(&wss_addr)
        .await
        .unwrap_or_else(|e| die(format!("bind wss {wss_addr}: {e}")));
    ev(
        "wss.listening",
        &[("addr", wss_addr.clone()), ("checkpoint", "cp7a".into())],
    );
    println!(
        "cp7-harness: WSS on {wss_addr}  |  Worker HTTPS on {data_plane_base_url}  |  coord on {lab_ip}:{coord_port}"
    );
    println!("cp7-harness: storage-root = {}", storage_root.display());
    println!("cp7-harness: waiting for one physical CP7A probe session. Ctrl-C to stop.");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                ev("shutdown.requested", &[]);
                let _ = shutdown_tx.send(true);
                break;
            }
            accepted = listener.accept() => {
                let Ok((tcp, peer)) = accepted else { continue };
                let acceptor = Arc::clone(&acceptor);
                let gateway = Arc::clone(&gateway);
                tokio::spawn(handle_conn(acceptor, gateway, tcp, peer.to_string()));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&socket_dir);
}

async fn handle_conn(
    acceptor: Arc<AgentTransportAcceptor>,
    gateway: Arc<Gateway>,
    tcp: TcpStream,
    peer: String,
) {
    let mut conn = match acceptor.accept(tcp).await {
        Ok(c) => c,
        Err(e) => {
            ev("wss.accept_failed", &[("peer", peer), ("error", e.to_string())]);
            return;
        }
    };
    let fp = conn.server_fingerprint;
    match gateway.handshake(&mut conn.websocket).await {
        Ok(HandshakeOutcome::Established(session)) => {
            ev(
                "wss.session_established",
                &[
                    ("peer", peer.clone()),
                    ("endpoint_id", session.endpoint_id.0.to_string()),
                    ("session_id", format!("{:?}", session.session_id)),
                ],
            );
            match gateway
                .run_authenticated_session(&mut conn.websocket, session, fp)
                .await
            {
                Ok(()) => ev(
                    "wss.session_closed",
                    &[("peer", peer), ("endpoint_id", session.endpoint_id.0.to_string())],
                ),
                Err(e) => ev("wss.session_error", &[("peer", peer), ("error", e.to_string())]),
            }
        }
        Ok(HandshakeOutcome::Rejected) => ev("wss.auth_rejected", &[("peer", peer)]),
        Err(e) => ev("wss.gateway_error", &[("peer", peer), ("error", e.to_string())]),
    }
}
