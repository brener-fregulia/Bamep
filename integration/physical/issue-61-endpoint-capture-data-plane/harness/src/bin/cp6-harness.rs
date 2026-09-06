//! Issue #61 CP6 — physical one-chunk data-plane Spike harness.
//!
//! THROWAWAY Spike scaffolding. NOT production architecture, NOT `crates/agent`,
//! NOT the `bamepd` composition root, NOT the Administrative API. Composes the
//! already-proven #60 + CP2 pieces, network-exposed on the lab interface, plus
//! a narrow FIXTURE-ONLY source-selection coordination:
//!
//!   real AgentTransportAcceptor + AgentControlGateway (WSS, TLS 1.3, pinned)
//!     + EnrollmentService + InventoryService + OutboundSessionDirectory
//!     + ActionEvidenceService + TransferAuthorizationService
//!   real WorkerControlPlane (UDS) + network-exposed Worker HTTPS DataPlane
//!     + FilesystemChunkStore + ChunkAcceptanceService
//!   real PostgreSQL (bamep_physint_spike — NOT created, NOT dropped)
//!   JobService / JobSchedulingService / TransferService / TransferDispatchService
//!
//! Coordination (fixture orchestration only — NOT a protocol, NOT M2
//! SourceReference validation, NOT an Admin API, NOT a new Agent message):
//! the probe sends one lab-only TCP line `{cp6_coord, source_observation_id,
//! selected_agent_source_id}` (NO PhysicalDriveN/model/serial). The harness
//! then waits for the matching InventoryRevision to persist, correlates it
//! through fixture-local read-only SQL, verifies the selected agent_source_id
//! is present, and builds the exact descriptive SourceProvenance tuple for a
//! fresh M1-shaped context.
//!
//! The action is `bamep.m1.data-plane-transfer`. CP6 transfers EXACTLY ONE
//! 8 MiB chunk (index 0) + one idempotent retry, then STOPS. It never seals,
//! reconstructs, verifies, marks Verified, or sends ActionResult{Succeeded}.

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
use bamep_server::application::{
    ActionDispatchOutcome, ActionDispatchService, ActionEvidenceService, ArtifactVerificationService,
    BootstrapEvidenceService, ChunkAcceptanceService, EnrollmentService, JobSchedulingService,
    JobService, ManifestSealService, TransferAuthorizationService, TransferDispatchResult,
    TransferDispatchService, TransferService,
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
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const DEFAULT_DB: &str = "bamep_physint_spike";

fn cfg(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
fn lab_ip() -> String { cfg("CP6_LAB_IP", "192.168.99.1") }
fn wss_port() -> u16 { cfg("CP6_WSS_PORT", "8443").parse().unwrap() }
fn data_plane_port() -> u16 { cfg("CP6_DP_PORT", "9107").parse().unwrap() }
fn coord_port() -> u16 { cfg("CP6_COORD_PORT", "9106").parse().unwrap() }

fn runtime_dir() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime-cp6");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn ev(event: &str, kv: &[(&str, String)]) {
    let mut l = format!(
        r#"{{"ts_ms":{},"component":"cp6-harness","event":"{event}""#,
        chrono::Utc::now().timestamp_millis()
    );
    for (k, v) in kv {
        l.push_str(&format!(r#","{k}":"{}""#, v.replace('"', "'")));
    }
    l.push('}');
    println!("{l}");
}
fn die(m: impl AsRef<str>) -> ! {
    eprintln!("cp6-harness: FATAL: {}", m.as_ref());
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
        let (cp, kp) = (d.join("cp6-cert.pem"), d.join("cp6-key.pem"));
        let (cd, kd) = (d.join("cp6-cert.der"), d.join("cp6-key.pkcs8.der"));
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
        std::fs::write(&cd, cert.der().to_vec()).unwrap();
        std::fs::write(&kd, signing_key.serialize_der()).unwrap();
        for p in [&kp, &kd] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let fingerprint = ServerCertFingerprint::from_leaf_der(cert_der.as_ref());
        Self { cert_der, key_pkcs8_der: signing_key.serialize_der(), cert_pem_path: cp, key_pem_path: kp, fingerprint }
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
        build_server_config(&load_server_identity(&self.cert_pem_path, &self.key_pem_path).expect("identity"))
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
    // Unused fixture accepted-key set (probe never sends BootstrapEvidence).
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
    Arc::new(
        Gateway::new(enrollment)
            .with_bootstrap_evidence_service(evidence)
            .with_inventory_service(inventory)
            .with_presence_registry(presence)
            .with_outbound_session_directory(outbound)
            .with_action_evidence_service(action_evidence)
            .with_transfer_authorization_service(authorization),
    )
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let lab_ip = lab_ip();
    let (wss_port, dp_port, coord_port) = (wss_port(), data_plane_port(), coord_port());
    let url = db_url();
    ev("db.connecting", &[("target", redact(&url))]);
    let pool = bamep_server::adapters::postgres::connect(&url)
        .await
        .unwrap_or_else(|e| die(format!("connect: {e}")));
    ev("db.connected_and_migrated", &[]);

    let identity = Arc::new(Identity::load_or_make());
    let fp = identity.hex_fingerprint();
    std::fs::write(runtime_dir().join("cp6-fingerprint.txt"), format!("{fp}\n")).ok();
    ev("identity.ready", &[("server_leaf_sha256", fp.clone())]);
    println!("cp6-harness: server leaf-cert SHA-256 fingerprint:\n  {fp}");

    // ---- shared runtime state ----------------------------------------
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
    // seal/verify are NEVER invoked by CP6 (no seal); their own stores are fine.
    let manifest_seal = Arc::new(ManifestSealService::new(
        Arc::new(PostgresTransferRepository::new(pool.clone())),
        Arc::new(CapabilityStore::new()),
        Arc::new(ReplayCache::new()),
    ));
    let artifact_verification = Arc::new(ArtifactVerificationService::new(Arc::new(
        PostgresTransferRepository::new(pool.clone()),
    )));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ---- Worker stack: UDS control plane + network Worker HTTPS -------
    let socket_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("b61cp6-{}", &Uuid::new_v4().simple().to_string()[..12]));
    std::fs::create_dir_all(&socket_dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        // WorkerControlPlane::bind requires an owner-only (0700) parent dir.
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket_path = socket_dir.join("w.sock");
    let storage_root = runtime_dir().join(format!("chunkstore-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&storage_root).unwrap();
    ev("worker.storage_root", &[("path", storage_root.display().to_string())]);

    let (control, driver) = worker_control(
        socket_path.clone(),
        Duration::from_millis(20),
        Duration::from_secs(8),
        Uuid::new_v4(),
    );
    let chunk_store = FilesystemChunkStore::initialize(&storage_root).expect("chunk store");
    let dp_addr = format!("{lab_ip}:{dp_port}").parse().unwrap();
    let data_plane = DataPlane::new(dp_addr, identity.worker_tls(), control.clone(), chunk_store);
    let dp_handle = data_plane.handle();
    {
        let mut rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let _ = data_plane.run(async move { let _ = rx.wait_for(|s| *s).await; }).await;
        });
    }
    let bound = dp_handle.listening().await.expect("worker https bound");
    ev("worker.https_listening", &[("origin", data_plane_base_url.clone()), ("bound", bound.to_string())]);

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
        tokio::spawn(driver.run(async move { let _ = rx.wait_for(|s| *s).await; }));
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
        let listener = TcpListener::bind(&coord_addr).await.unwrap_or_else(|e| die(format!("bind coord {coord_addr}: {e}")));
        ev("coord.listening", &[("addr", coord_addr)]);
        let tx = coord_tx.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let mut lines = BufReader::new(stream).lines();
                if let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        if v.get("cp6_coord").and_then(|x| x.as_str()) == Some("source_selection") {
                            let sel = SourceSelection {
                                source_observation_id: v["source_observation_id"].as_str().unwrap_or_default().to_string(),
                                selected_agent_source_id: v["selected_agent_source_id"].as_str().unwrap_or_default().to_string(),
                            };
                            ev("coord.received", &[
                                ("source_observation_id", sel.source_observation_id.clone()),
                                ("selected_agent_source_id", sel.selected_agent_source_id.clone()),
                            ]);
                            let _ = tx.send(sel).await;
                        }
                    }
                }
            }
        });
    }

    // ---- orchestrator: once coord + InventoryRevision are present, ----
    // ---- create + dispatch the fresh M1-shaped context ---------------
    {
        let pool = pool.clone();
        let job_repo = Arc::clone(&job_repo);
        let arbiter = Arc::clone(&arbiter);
        let reservations = Arc::clone(&reservations);
        let outbound = Arc::clone(&outbound);
        let presence = Arc::clone(&presence);
        tokio::spawn(async move {
            let Some(sel) = coord_rx.recv().await else { return };
            // wait for the InventoryRevision carrying this exact epoch
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
                        .map(|a| a.iter().filter_map(|s| s.get("agent_source_id").and_then(|x| x.as_str()).map(String::from)).collect())
                        .unwrap_or_default();
                    if !ids.contains(&sel.selected_agent_source_id) {
                        ev("orchestrator.selected_id_not_in_revision", &[("revision_id", rid.to_string())]);
                        return;
                    }
                    ev("orchestrator.inventory_revision_matched", &[
                        ("endpoint_id", eid.to_string()),
                        ("inventory_revision_id", rid.to_string()),
                    ]);
                    break (eid, rid);
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            };

            // fresh coherent lineage tuple
            let provenance = serde_json::json!({
                "_schema": "issue-61-cp6.descriptive-source-provenance",
                "descriptive_only": true,
                "not_a_validated_source_reference": true,
                "server_side_freshness_validation_exists": false,
                "source_reference": {
                    "inventory_revision_id": revision_id.to_string(),
                    "source_observation_id": sel.source_observation_id,
                    "agent_source_id": sel.selected_agent_source_id,
                }
            })
            .to_string();

            // approve the physical Endpoint (existing operator-decision path)
            let enrollment = EnrollmentService::new(
                Arc::new(PostgresEndpointRepository::new(pool.clone())),
                Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone())),
            );
            let ident: String = sqlx::query_scalar("SELECT identity_state::text FROM endpoints WHERE id=$1")
                .bind(endpoint_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            if ident != "Enrolled" {
                enrollment
                    .approve_enrollment(
                        EndpointId(endpoint_id),
                        Actor::Operator { label: "issue-61-cp6-harness".into() },
                        chrono::Utc::now(),
                    )
                    .await
                    .unwrap_or_else(|e| die(format!("approve_enrollment: {e:?}")));
                ev("orchestrator.endpoint_enrolled", &[("endpoint_id", endpoint_id.to_string())]);
            }

            let jobs = JobService::new(Arc::clone(&job_repo));
            let scheduling = JobSchedulingService::new(Arc::clone(&job_repo));
            let transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
            let dispatch = TransferDispatchService::new(Arc::clone(&job_repo), Arc::clone(&arbiter));

            let job = jobs.create_workflow(EndpointId(endpoint_id), 1).await.unwrap_or_else(|e| die(format!("create_workflow: {e:?}")));
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
                    ChunkSize::new(8 * 1024 * 1024).unwrap(),
                    SourceProvenance::new(provenance),
                )
                .await
                .unwrap_or_else(|e| die(format!("create_transfer_context: {e:?}")));
            ev("orchestrator.context_created", &[
                ("job_id", job.id.0.to_string()),
                ("job_step_id", step.0.to_string()),
                ("transfer_id", ctx.transfer.id.0.to_string()),
                ("artifact_id", ctx.transfer.artifact_id.0.to_string()),
            ]);

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
            ev("orchestrator.dispatch_committed", &[
                ("attempt_id", outcome.attempt.id.0.to_string()),
                ("action_id", action_id.to_string()),
            ]);

            // wait for the live physical session to be outbound-ready
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
                .dispatch_transfer(EndpointId(endpoint_id), outcome.attempt, reservation, &outcome.transfer)
                .await;
            ev("orchestrator.action_dispatched", &[("outcome", format!("{sent:?}"))]);
            if sent != ActionDispatchOutcome::Sent {
                ev("orchestrator.dispatch_not_sent", &[]);
            }
            // hint IDs for the operator/report
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
    let listener = TcpListener::bind(&wss_addr).await.unwrap_or_else(|e| die(format!("bind wss {wss_addr}: {e}")));
    ev("wss.listening", &[("addr", wss_addr.clone()), ("checkpoint", "cp6".into())]);
    println!("cp6-harness: WSS on {wss_addr}  |  Worker HTTPS on {data_plane_base_url}  |  coord on {lab_ip}:{coord_port}");
    println!("cp6-harness: waiting for one physical CP6 probe session. Ctrl-C to stop.");

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
            ev("wss.session_established", &[
                ("peer", peer.clone()),
                ("endpoint_id", session.endpoint_id.0.to_string()),
                ("session_id", format!("{:?}", session.session_id)),
            ]);
            match gateway.run_authenticated_session(&mut conn.websocket, session, fp).await {
                Ok(()) => ev("wss.session_closed", &[("peer", peer), ("endpoint_id", session.endpoint_id.0.to_string())]),
                Err(e) => ev("wss.session_error", &[("peer", peer), ("error", e.to_string())]),
            }
        }
        Ok(HandshakeOutcome::Rejected) => ev("wss.auth_rejected", &[("peer", peer)]),
        Err(e) => ev("wss.gateway_error", &[("peer", peer), ("error", e.to_string())]),
    }
}
