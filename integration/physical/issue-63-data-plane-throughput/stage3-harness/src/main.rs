//! Issue #63 Stage 3 — the #61/CP7-shaped real Server/Postgres/WSS/Worker
//! harness for the physical throughput matrix. THROWAWAY Spike.
//!
//! PROVENANCE: the composition is adapted from the closed Issue #61 CP7A harness
//! (`integration/physical/issue-61-endpoint-capture-data-plane/harness/src/bin/cp7-harness.rs`).
//! Removed: the Gate-4 auth-denial episode decorator, the `FaultMode` selection,
//! and the data-plane listener-restart supervisor (the Issue-63 clean fast path
//! does ZERO deliberate fault injection). Added: a PER-CASE orchestration loop —
//! one fresh Job / Transfer / Artifact lineage per matrix case, driven by the
//! probe's `source_selection` coord message (which carries `chunk_size` +
//! `case_id`), over ONE long-lived enrolled endpoint. #61 is not modified.
//!
//! Real boundaries (same as CP7A): PostgreSQL adapter, `AgentControlGateway` /
//! WSS transport, Worker control plane, Worker HTTPS `DataPlane`,
//! `FilesystemChunkStore`, `TransferTerminalEvidenceService`.
//!
//! `bamep_physint_spike` is used but NEVER created/dropped; the CP6/CP7 lineages
//! are never touched. The action is `bamep.m1.data-plane-transfer` — NOT
//! `bamep.m2.endpoint-capture-transfer`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bamep_domain::{
    Actor, BootNonce, ChunkSize, DigestAlgorithm, EndpointId, SourceProvenance, TransferDirection,
};
use bamep_server::adapters::agent_gateway::{AgentControlGateway, HandshakeOutcome};
use bamep_server::adapters::agent_transport::AgentTransportAcceptor;
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository, PostgresEndpointRepository,
    PostgresInventoryRepository, PostgresJobRepository, PostgresTransferAuthorizationRepository,
    PostgresTransferRepository,
};
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::{
    ActionDispatchOutcome, ActionDispatchService, ActionEvidenceService, ArtifactVerificationService,
    BootOrchestrationService, BootstrapEvidenceService, ChunkAcceptanceService, EnrollmentService,
    InventoryService, JobSchedulingService, JobService, ManifestSealService,
    TransferAuthorizationService, TransferDispatchResult, TransferDispatchService, TransferService,
    TransferTerminalEvidenceService,
};
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
use bamep_worker::ipc::worker_control;
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
/// The Stage-3 matrix pins the transfer chunk size PER CASE (8/16/32/64 MiB); a
/// coord message without a `chunk_size` falls back to this.
const CHUNK_SIZE_FALLBACK: u64 = 8 * 1024 * 1024;
/// 36 preserved 2 GiB Artifacts + margin. The Stage-3 supervisor also runs the
/// pure `budget` gate; this is the harness's own fail-closed floor.
const MIN_FREE_BYTES: u64 = 90 * 1024 * 1024 * 1024;

fn cfg(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
fn lab_ip() -> String {
    cfg("I63_LAB_IP", "192.168.99.1")
}
fn wss_port() -> u16 {
    cfg("I63_WSS_PORT", "8443").parse().unwrap()
}
fn data_plane_port() -> u16 {
    cfg("I63_DP_PORT", "9207").parse().unwrap()
}
fn coord_port() -> u16 {
    cfg("I63_COORD_PORT", "9206").parse().unwrap()
}

fn runtime_dir() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime-stage3");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn ev(event: &str, kv: &[(&str, String)]) {
    let mut l = format!(
        r#"{{"ts_ms":{},"component":"stage3-harness","event":"{event}""#,
        chrono::Utc::now().timestamp_millis()
    );
    for (k, v) in kv {
        l.push_str(&format!(r#","{k}":"{}""#, v.replace('"', "'")));
    }
    l.push('}');
    println!("{l}");
}
fn die(m: impl AsRef<str>) -> ! {
    eprintln!("stage3-harness: FATAL: {}", m.as_ref());
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

/// The mandatory `--storage-root` (or `I63_STORAGE_ROOT`). Fails closed before
/// the Worker starts on: absent, not a directory, not writable, resolves under
/// an Issue #61 `runtime-cp*` tree, or insufficient free space.
fn resolve_storage_root() -> PathBuf {
    let raw = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .windows(2)
        .find(|w| w[0] == "--storage-root")
        .map(|w| w[1].clone())
        .or_else(|| std::env::var("I63_STORAGE_ROOT").ok())
        .unwrap_or_else(|| {
            die("--storage-root <path> is MANDATORY (no default, no fallback). Use a git-ignored \
                 path with >= 90 GiB free, e.g. \
                 integration/physical/issue-63-data-plane-throughput/stage3-harness/runtime-stage3/chunkstore")
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
    if canon.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s == "runtime-cp6" || s == "runtime-cp7a")
    }) {
        die("--storage-root must NOT resolve under an Issue #61 runtime-cp* tree (CP6/CP7 frozen)");
    }
    let probe = canon.join(format!(".i63s3-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"ok")
        .unwrap_or_else(|e| die(format!("--storage-root {} is not writable: {e}", canon.display())));
    let _ = std::fs::remove_file(&probe);

    match fs_free_bytes(&canon) {
        Ok(free) if free >= MIN_FREE_BYTES => ev(
            "storage_root.ok",
            &[
                ("path", canon.display().to_string()),
                ("free_bytes", free.to_string()),
                ("min_free_bytes", MIN_FREE_BYTES.to_string()),
            ],
        ),
        Ok(free) => die(format!(
            "--storage-root {} free {free} < required {MIN_FREE_BYTES} for the 36 x 2 GiB matrix",
            canon.display()
        )),
        Err(e) => die(format!(
            "--storage-root {}: cannot determine free space ({e}); refusing to start",
            canon.display()
        )),
    }
    canon
}

/// Persisted self-signed leaf shared by the WSS acceptor and the Worker HTTPS
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
        let (cp, kp) = (d.join("i63s3-cert.pem"), d.join("i63s3-key.pem"));
        let (cd, kd) = (d.join("i63s3-cert.der"), d.join("i63s3-key.pkcs8.der"));
        if [&cp, &kp, &cd, &kd].iter().all(|p| p.exists()) {
            std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600)).ok();
            std::fs::set_permissions(&kd, std::fs::Permissions::from_mode(0o600)).ok();
            let cert_der = CertificateDer::from(std::fs::read(&cd).unwrap());
            let key_pkcs8_der = std::fs::read(&kd).unwrap();
            let fingerprint = ServerCertFingerprint::from_leaf_der(cert_der.as_ref());
            return Self { cert_der, key_pkcs8_der, cert_pem_path: cp, key_pem_path: kp, fingerprint };
        }
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["localhost".to_string()]).expect("gen cert");
        std::fs::write(&cp, cert.pem()).unwrap();
        std::fs::write(&kp, signing_key.serialize_pem()).unwrap();
        std::fs::write(&cd, cert.der()).unwrap();
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
        self.fingerprint.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
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

/// The lab-only coordination payload from the probe (no PhysicalDriveN / model /
/// serial). `chunk_size` + `case_id` are Issue-63 Stage-3 additions.
#[derive(Clone, Debug)]
struct SourceSelection {
    source_observation_id: String,
    selected_agent_source_id: String,
    chunk_size: u64,
    case_id: String,
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
    let inventory = Arc::new(InventoryService::new(Arc::new(PostgresInventoryRepository::new(
        pool.clone(),
    ))));
    let signer = bamep_trusted_bootstrap::fixture::FixtureAssertionSigner::from_seed([0x63; 32]);
    let evidence = Arc::new(BootstrapEvidenceService::new(
        endpoint_repo,
        AcceptedSiteKeys::single(signer.public_key()),
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

/// `stage3-harness issue-credential <signal>` — mint one fresh first-contact
/// enrollment credential against `bamep_physint_spike` and print it. The
/// database is never created/dropped and no CP6/CP7 row is touched.
async fn issue_credential(signal: &str) -> ! {
    let url = db_url();
    eprintln!("stage3-harness: issue-credential -> {}", redact(&url));
    let pool = bamep_server::adapters::postgres::connect(&url)
        .await
        .unwrap_or_else(|e| die(format!("connect: {e}")));
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(pool)),
        chrono::Duration::minutes(90),
    );
    let credential = boot
        .issue_enrollment_credential(signal, BootNonce::generate().unwrap(), chrono::Utc::now())
        .await
        .unwrap_or_else(|e| die(format!("issue_enrollment_credential: {e:?}")));
    println!("{}", credential.to_wire_value());
    std::process::exit(0);
}

/// Shared deps the per-case orchestrator needs.
struct CaseDeps {
    pool: PgPool,
    job_repo: Arc<PostgresJobRepository>,
    arbiter: Arc<TechnicalResourceArbiter>,
    reservations: Arc<AttemptReservationRegistry>,
    outbound: Arc<OutboundSessionDirectory>,
    presence: Arc<PresenceRegistry>,
}

/// Run ONE matrix case: match this probe's fresh inventory revision, ensure the
/// endpoint is enrolled (first case only), create a fresh Job / Transfer /
/// Artifact lineage with the case's exact `chunk_size`, dispatch the
/// `bamep.m1.data-plane-transfer` action, then poll the durable final state.
async fn run_one_case(deps: &CaseDeps, sel: SourceSelection) {
    let pool = &deps.pool;
    let chunk_size = if sel.chunk_size > 0 { sel.chunk_size } else { CHUNK_SIZE_FALLBACK };
    ev(
        "case.begin",
        &[
            ("case_id", sel.case_id.clone()),
            ("chunk_size", chunk_size.to_string()),
            ("source_observation_id", sel.source_observation_id.clone()),
        ],
    );

    // ---- match this probe's fresh inventory revision -------------------
    let (endpoint_id, revision_id) = {
        let mut waited = 0;
        loop {
            let row: Option<(Uuid, Uuid, serde_json::Value)> = sqlx::query_as(
                "SELECT endpoint_id, revision_id, inventory \
                 FROM inventory_revisions \
                 WHERE inventory->>'capture_source_observation_id' = $1 \
                 ORDER BY recorded_at DESC LIMIT 1",
            )
            .bind(&sel.source_observation_id)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
            if let Some((eid, rid, inv)) = row {
                let ids: Vec<String> = inv
                    .get("capturable_sources")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| {
                                s.get("agent_source_id").and_then(|x| x.as_str()).map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !ids.contains(&sel.selected_agent_source_id) {
                    ev("case.selected_id_not_in_revision", &[("revision_id", rid.to_string())]);
                    return;
                }
                ev(
                    "case.inventory_revision_matched",
                    &[("endpoint_id", eid.to_string()), ("inventory_revision_id", rid.to_string())],
                );
                break (eid, rid);
            }
            waited += 1;
            if waited > 400 {
                ev("case.inventory_revision_timeout", &[("case_id", sel.case_id.clone())]);
                return;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };

    let provenance = serde_json::json!({
        "_schema": "issue-63-stage3.descriptive-source-provenance.v1",
        "capture_extent": "bounded_matrix_case",
        "not_a_complete_source_capture": true,
        "descriptive_only": true,
        "not_a_validated_source_reference": true,
        "server_side_freshness_validation_exists": false,
        "action_type": "bamep.m1.data-plane-transfer",
        "matrix_case_id": sel.case_id,
        "chunk_size_bytes": chunk_size,
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
    let ident: String = sqlx::query_scalar("SELECT identity_state::text FROM endpoints WHERE id=$1")
        .bind(endpoint_id)
        .fetch_one(pool)
        .await
        .unwrap();
    if ident != "Enrolled" {
        enrollment
            .approve_enrollment(
                EndpointId(endpoint_id),
                Actor::Operator { label: "issue-63-stage3-harness".into() },
                chrono::Utc::now(),
            )
            .await
            .unwrap_or_else(|e| die(format!("approve_enrollment: {e:?}")));
        ev("case.endpoint_enrolled", &[("endpoint_id", endpoint_id.to_string())]);
    }

    let jobs = JobService::new(Arc::clone(&deps.job_repo));
    let scheduling = JobSchedulingService::new(Arc::clone(&deps.job_repo));
    let transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
    let dispatch =
        TransferDispatchService::new(Arc::clone(&deps.job_repo), Arc::clone(&deps.arbiter));

    let job = jobs
        .create_workflow(EndpointId(endpoint_id), 1)
        .await
        .unwrap_or_else(|e| die(format!("create_workflow: {e:?}")));
    let step = job.steps[0].id;
    scheduling.admit(job.id).await.unwrap();
    scheduling.satisfy_current_step_preconditions(job.id, step).await.unwrap();
    let ctx = transfers
        .create_transfer_context(
            EndpointId(endpoint_id),
            job.id,
            step,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(chunk_size as u32).unwrap_or_else(|e| {
                die(format!("case {}: invalid chunk_size {chunk_size}: {e:?}", sel.case_id))
            }),
            SourceProvenance::new(provenance),
        )
        .await
        .unwrap_or_else(|e| die(format!("create_transfer_context: {e:?}")));
    let artifact_id = ctx.transfer.artifact_id.0;
    let transfer_id = ctx.transfer.id.0;
    ev(
        "case.context_created",
        &[
            ("case_id", sel.case_id.clone()),
            ("job_id", job.id.0.to_string()),
            ("transfer_id", transfer_id.to_string()),
            ("artifact_id", artifact_id.to_string()),
            ("chunk_size", chunk_size.to_string()),
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
    ev(
        "case.dispatch_committed",
        &[("case_id", sel.case_id.clone()), ("action_id", action_id.to_string())],
    );

    for _ in 0..400 {
        if deps.presence.is_present(EndpointId(endpoint_id)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let svc = ActionDispatchService::new(
        Arc::clone(&deps.reservations),
        Arc::clone(&deps.outbound) as Arc<dyn AgentDispatchPort>,
    );
    let sent = svc
        .dispatch_transfer(EndpointId(endpoint_id), outcome.attempt, reservation, &outcome.transfer)
        .await;
    ev("case.action_dispatched", &[("case_id", sel.case_id.clone()), ("outcome", format!("{sent:?}"))]);
    if sent != ActionDispatchOutcome::Sent {
        ev("case.dispatch_not_sent", &[("case_id", sel.case_id.clone())]);
    }

    // ---- poll the durable final state for THIS job --------------------
    type FinalStateRow = (String, String, String, String, Option<bool>, Option<i32>);
    let job_id = job.id.0;
    for _ in 0..1200 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let row: Result<Option<FinalStateRow>, sqlx::Error> = sqlx::query_as(
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
        .fetch_optional(pool)
        .await;
        match row {
            Ok(Some((j, js, a, ar, sealed, cc))) => {
                let terminal = !matches!(j.as_str(), "Pending" | "Running");
                ev(
                    if terminal { "case.final_state" } else { "case.state.poll" },
                    &[
                        ("case_id", sel.case_id.clone()),
                        ("job", j.clone()),
                        ("job_step", js),
                        ("attempt", a),
                        ("artifact", ar.clone()),
                        ("manifest_sealed", format!("{sealed:?}")),
                        ("chunk_count", format!("{cc:?}")),
                    ],
                );
                if terminal {
                    ev(
                        "case.end",
                        &[
                            ("case_id", sel.case_id.clone()),
                            ("transfer_id", transfer_id.to_string()),
                            ("artifact_id", artifact_id.to_string()),
                            ("artifact_state", ar),
                            ("job_state", j),
                        ],
                    );
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => ev("case.state.poll_error", &[("error", e.to_string())]),
        }
    }
    ev("case.final_state_timeout", &[("case_id", sel.case_id)]);
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("issue-credential") {
        let signal = argv
            .get(2)
            .cloned()
            .unwrap_or_else(|| die("usage: stage3-harness issue-credential <signal>"));
        issue_credential(&signal).await;
    }

    let lab_ip = lab_ip();
    let (wss_port, dp_port, coord_port) = (wss_port(), data_plane_port(), coord_port());
    let storage_root = resolve_storage_root();

    let url = db_url();
    ev("db.connecting", &[("target", redact(&url))]);
    let pool = bamep_server::adapters::postgres::connect(&url)
        .await
        .unwrap_or_else(|e| die(format!("connect: {e}")));
    ev("db.connected_and_migrated", &[]);

    let identity = Arc::new(Identity::load_or_make());
    let fp = identity.hex_fingerprint();
    std::fs::write(runtime_dir().join("i63s3-fingerprint.txt"), format!("{fp}\n")).ok();
    ev("identity.ready", &[("server_leaf_sha256", fp.clone())]);
    println!("stage3-harness: server leaf-cert SHA-256 fingerprint:\n  {fp}");

    // ---- shared runtime state --------------------------------------------
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(ResourceKind::new("network"), 10)]));
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

    // ---- Worker stack: UDS control plane + ONE Worker HTTPS listener ---
    let socket_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("b63s3-{}", &Uuid::new_v4().simple().to_string()[..12]));
    std::fs::create_dir_all(&socket_dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket_path = socket_dir.join("w.sock");
    ev("worker.storage_root", &[("path", storage_root.display().to_string())]);

    let (control, driver) = worker_control(
        socket_path.clone(),
        Duration::from_millis(20),
        Duration::from_secs(8),
        Uuid::new_v4(),
    );
    let dp_addr: std::net::SocketAddr = format!("{lab_ip}:{dp_port}").parse().unwrap();
    {
        let tls = identity.worker_tls();
        let control = control.clone();
        let storage_root = storage_root.clone();
        let base_url = data_plane_base_url.clone();
        let mut kill_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let chunk_store = FilesystemChunkStore::initialize(&storage_root).expect("chunk store");
            let data_plane = DataPlane::new(dp_addr, tls, control, chunk_store);
            let handle = data_plane.handle();
            let jh = tokio::spawn(async move {
                let _ = data_plane
                    .run(async move {
                        let _ = kill_rx.wait_for(|s| *s).await;
                    })
                    .await;
            });
            let bound = handle.listening().await.expect("worker https bound");
            ev(
                "worker.https_listening",
                &[("origin", base_url), ("bound", bound.to_string())],
            );
            let _ = jh.await;
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

    // ---- coordination listener (Server UTC ACK + per-case source_selection) ----
    let (coord_tx, mut coord_rx) = mpsc::channel::<SourceSelection>(8);
    {
        let coord_addr = format!("{lab_ip}:{coord_port}");
        let listener = TcpListener::bind(&coord_addr)
            .await
            .unwrap_or_else(|e| die(format!("bind coord {coord_addr}: {e}")));
        ev("coord.listening", &[("addr", coord_addr)]);
        let tx = coord_tx.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let tx = tx.clone();
                tokio::spawn(handle_coord_conn(stream, tx));
            }
        });
    }

    // ---- per-case orchestrator: one lineage per source_selection ------
    {
        let deps = CaseDeps {
            pool: pool.clone(),
            job_repo: Arc::clone(&job_repo),
            arbiter: Arc::clone(&arbiter),
            reservations: Arc::clone(&reservations),
            outbound: Arc::clone(&outbound),
            presence: Arc::clone(&presence),
        };
        tokio::spawn(async move {
            let mut n = 0u64;
            while let Some(sel) = coord_rx.recv().await {
                n += 1;
                ev("orchestrator.case_dequeued", &[("n", n.to_string()), ("case_id", sel.case_id.clone())]);
                run_one_case(&deps, sel).await;
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
    ev("wss.listening", &[("addr", wss_addr.clone())]);
    println!(
        "stage3-harness: WSS on {wss_addr}  |  Worker HTTPS on {data_plane_base_url}  |  coord on {lab_ip}:{coord_port}"
    );
    println!("stage3-harness: storage-root = {}", storage_root.display());
    println!("stage3-harness: ready for the Stage-3 matrix (36 per-case probe sessions). Ctrl-C to stop.");

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

async fn handle_coord_conn(stream: TcpStream, tx: mpsc::Sender<SourceSelection>) {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    if let Ok(Some(line)) = lines.next_line().await {
        // Always answer with the Server's current UTC for the probe's
        // asymmetric clock pre-flight. Narrow lab-only fixture, NOT production
        // time sync.
        let ack = format!(
            r#"{{"cp7_coord_ack":true,"server_utc_ms":{}}}"#,
            chrono::Utc::now().timestamp_millis()
        );
        let _ = wr.write_all(format!("{ack}\n").as_bytes()).await;
        let _ = wr.flush().await;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.get("cp7_coord").and_then(|x| x.as_str()) == Some("source_selection") {
                let sel = SourceSelection {
                    source_observation_id: v["source_observation_id"].as_str().unwrap_or_default().to_string(),
                    selected_agent_source_id: v["selected_agent_source_id"].as_str().unwrap_or_default().to_string(),
                    chunk_size: v.get("chunk_size").and_then(|x| x.as_u64()).unwrap_or(0),
                    case_id: v.get("case_id").and_then(|x| x.as_str()).unwrap_or("unknown").to_string(),
                };
                ev(
                    "coord.received",
                    &[
                        ("source_observation_id", sel.source_observation_id.clone()),
                        ("selected_agent_source_id", sel.selected_agent_source_id.clone()),
                        ("chunk_size", sel.chunk_size.to_string()),
                        ("case_id", sel.case_id.clone()),
                    ],
                );
                let _ = tx.send(sel).await;
            }
        }
    }
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
                ],
            );
            match gateway.run_authenticated_session(&mut conn.websocket, session, fp).await {
                Ok(()) => ev("wss.session_closed", &[("peer", peer)]),
                Err(e) => ev("wss.session_error", &[("peer", peer), ("error", e.to_string())]),
            }
        }
        Ok(HandshakeOutcome::Rejected) => ev("wss.auth_rejected", &[("peer", peer)]),
        Err(e) => ev("wss.gateway_error", &[("peer", peer), ("error", e.to_string())]),
    }
}
