//! Issue #19 checkpoint C1 — real-HTTPS integration tests for the Agent-side
//! `bamep.m1.data-plane-transfer` participant.
//!
//! ```text
//! bamep_simulator::DataPlaneTransferAgent
//!   -> bamep_simulator::DataPlaneClient  (real hyper-1 HTTPS, exact leaf pin, TLS 1.3)
//!     -> real bamep_worker::data_plane::DataPlane  (real Worker TLS server)
//!       -> real bamep_worker::ipc control client + real D1 staging + real D2 reconstruction
//!         -> fake `bamepd` UDS peer  (real bamep_worker_protocol codec)
//! ```
//!
//! The missing Server business authority (real capability/proof verification,
//! real durable chunk acceptance, `bamepd`'s own Artifact-digest comparison)
//! belongs to later integration checkpoints
//! (`m0-simulator-contract-and-validation-strategy.md`; Issue #19 C1 §33) — the
//! fake peer stands in for it here with deterministic, policy-driven replies.
//! No PostgreSQL, no `bamep-server`, no WSS composition.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bamep_agent_protocol::{
    ActionAckOutcome, ActionDispatchMessage, ActionResultOutcome, ProtocolId,
};
use bamep_simulator::{
    AgentProofKey, AgentTransferAuthorization, DataPlaneClient, DataPlaneTransferAgent,
    DataPlaneTransferDirection, DataPlaneTransportError, InMemoryTransferSource, ResumeOutcome,
    SuspendReason, TransferActionResult, TransferProgress, TransferRunOptions, TransferRunOutcome,
    M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
};
use bamep_trusted_bootstrap::ServerCertFingerprint;
use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::{worker_control, WorkerControlHandle};
use bamep_worker::storage::FilesystemChunkStore;
use bamep_worker::tls::{build_server_config, load_server_identity};
use bamep_worker_protocol::{
    receive, send, ArtifactVerificationAckMessage, AuthorizationDecisionMessage,
    ChunkAcceptanceDecisionMessage, ChunkAcceptanceRejectionReason, HeldChunk,
    ManifestSealDecisionMessage, ManifestSealRejectionReason, SealedManifestFacts,
    ServerHelloMessage, WireArtifactStatus, WireDigestAlgorithm, WorkerProtocolMessage,
};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(5);
const PEER_IDLE: Duration = Duration::from_millis(400);

// =====================================================================
// self-signed TLS identity (mirrors the Agent's exact-fingerprint model)
// =====================================================================

struct TempDir(PathBuf);

impl TempDir {
    fn fresh(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bamep-sim-c1-{tag}-{}", Uuid::new_v4()));
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
        let dir = TempDir::fresh("tls");
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["bamep-sim-c1-test.local".to_string()])
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

    fn fingerprint(&self) -> ServerCertFingerprint {
        ServerCertFingerprint::from_leaf_der(&self.leaf_der)
    }
}

// =====================================================================
// fake `bamepd` UDS peer
// =====================================================================

#[derive(Clone)]
struct PeerConfig {
    chunk_size: u32,
    chunk_count: u64,
    artifact_id: Uuid,
    deny_resume: bool,
    /// Deny the `AuthorizationQuery` for these chunk indices.
    deny_chunk_auth: BTreeSet<u64>,
    /// Deny **every** authorizing request received after this many total
    /// requests have been served (used to model a replay-cache rejection on a
    /// later run without re-implementing #38).
    deny_after_requests: Option<usize>,
    seal: SealPlan,
    verification: WireArtifactStatus,
}

#[derive(Clone, Copy)]
enum SealPlan {
    Sealed,
    Denied,
    IncompleteManifest,
}

impl PeerConfig {
    fn happy(chunk_size: u32, chunk_count: u64, artifact_id: Uuid) -> Self {
        Self {
            chunk_size,
            chunk_count,
            artifact_id,
            deny_resume: false,
            deny_chunk_auth: BTreeSet::new(),
            deny_after_requests: None,
            seal: SealPlan::Sealed,
            verification: WireArtifactStatus::Verified,
        }
    }
}

#[derive(Default)]
struct PeerObservations {
    /// Durable held-chunk identities `(chunk_index -> digest)`.
    held: BTreeMap<u64, String>,
    authorization_query_indices: Vec<u64>,
    chunk_acceptance_indices: Vec<u64>,
    resume_query_count: usize,
    proof_ids_seen: Vec<String>,
    seal_requests: usize,
    verification_reports: usize,
}

type Obs = Arc<Mutex<PeerObservations>>;

async fn serve_peer(mut stream: UnixStream, config: PeerConfig, obs: Obs) {
    // Handshake first (`WorkerHello` -> `ServerHello`).
    let hello = match timeout(TIMEOUT, receive(&mut stream)).await {
        Ok(Ok(WorkerProtocolMessage::WorkerHello(h))) => h,
        other => panic!("expected WorkerHello, got {other:?}"),
    };
    send(
        &mut stream,
        &WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(hello.envelope.message_id)),
    )
    .await
    .expect("send ServerHello");

    let mut requests_served = 0usize;
    loop {
        let message = match timeout(PEER_IDLE, receive(&mut stream)).await {
            Ok(Ok(message)) => message,
            _ => return, // idle / disconnect — the client has stopped for this scenario
        };

        let deny_now = config
            .deny_after_requests
            .is_some_and(|limit| requests_served >= limit);

        let reply = match message {
            WorkerProtocolMessage::AuthorizationQuery(q) => {
                requests_served += 1;
                let mut guard = obs.lock().unwrap();
                guard.authorization_query_indices.push(q.body.chunk_index);
                guard.proof_ids_seen.push(q.body.proof_id.clone());
                let expected = guard.held.get(&q.body.chunk_index).cloned();
                drop(guard);
                if deny_now || config.deny_chunk_auth.contains(&q.body.chunk_index) {
                    WorkerProtocolMessage::AuthorizationDecision(
                        AuthorizationDecisionMessage::denied(q.envelope.message_id),
                    )
                } else {
                    WorkerProtocolMessage::AuthorizationDecision(
                        AuthorizationDecisionMessage::approved(
                            q.envelope.message_id,
                            WireDigestAlgorithm::Sha256,
                            config.chunk_size,
                            "acceptance-handle",
                            expected,
                        ),
                    )
                }
            }
            WorkerProtocolMessage::ChunkAcceptanceRequest(r) => {
                let mut guard = obs.lock().unwrap();
                guard.chunk_acceptance_indices.push(r.body.chunk_index);
                let previous = guard.held.get(&r.body.chunk_index).cloned();
                guard.held.insert(r.body.chunk_index, r.body.digest.clone());
                drop(guard);
                let decision = match previous {
                    Some(digest) if digest == r.body.digest => {
                        ChunkAcceptanceDecisionMessage::already_committed(r.envelope.message_id)
                    }
                    Some(_) => ChunkAcceptanceDecisionMessage::rejected(
                        r.envelope.message_id,
                        ChunkAcceptanceRejectionReason::ChunkIdentityConflict,
                    ),
                    None => ChunkAcceptanceDecisionMessage::committed(r.envelope.message_id),
                };
                WorkerProtocolMessage::ChunkAcceptanceDecision(decision)
            }
            WorkerProtocolMessage::ResumeDiscoveryQuery(q) => {
                requests_served += 1;
                let mut guard = obs.lock().unwrap();
                guard.resume_query_count += 1;
                guard.proof_ids_seen.push(q.body.proof_id.clone());
                let held_chunks: Vec<HeldChunk> = guard
                    .held
                    .iter()
                    .map(|(index, digest)| HeldChunk {
                        chunk_index: *index,
                        digest: digest.clone(),
                    })
                    .collect();
                drop(guard);
                if deny_now || config.deny_resume {
                    WorkerProtocolMessage::ResumeDiscoveryPage(
                        bamep_worker_protocol::ResumeDiscoveryPageMessage::denied(
                            q.envelope.message_id,
                        ),
                    )
                } else {
                    WorkerProtocolMessage::ResumeDiscoveryPage(
                        bamep_worker_protocol::ResumeDiscoveryPageMessage::first_page(
                            q.envelope.message_id,
                            q.body.transfer_id,
                            false,
                            WireDigestAlgorithm::Sha256,
                            config.chunk_size,
                            None,
                            held_chunks,
                            None,
                        ),
                    )
                }
            }
            WorkerProtocolMessage::ManifestSealRequest(r) => {
                requests_served += 1;
                {
                    let mut guard = obs.lock().unwrap();
                    guard.seal_requests += 1;
                    guard.proof_ids_seen.push(r.body.proof_id.clone());
                }
                let decision = match config.seal {
                    SealPlan::Denied => ManifestSealDecisionMessage::denied(r.envelope.message_id),
                    SealPlan::IncompleteManifest => ManifestSealDecisionMessage::rejected(
                        r.envelope.message_id,
                        ManifestSealRejectionReason::IncompleteManifest,
                    ),
                    SealPlan::Sealed => ManifestSealDecisionMessage::sealed(
                        r.envelope.message_id,
                        SealedManifestFacts {
                            verification_handle: "verification-handle".to_string(),
                            artifact_id: config.artifact_id,
                            digest_algorithm: WireDigestAlgorithm::Sha256,
                            chunk_size: config.chunk_size,
                            chunk_count: config.chunk_count,
                            // Echoed; the Worker verifies against this, and the
                            // fake peer's `ArtifactVerificationAck` decides the
                            // status directly (real digest comparison is #38's).
                            expected_artifact_digest: r.body.artifact_digest.clone(),
                        },
                    ),
                };
                WorkerProtocolMessage::ManifestSealDecision(decision)
            }
            WorkerProtocolMessage::ArtifactVerificationReport(r) => {
                obs.lock().unwrap().verification_reports += 1;
                WorkerProtocolMessage::ArtifactVerificationAck(
                    ArtifactVerificationAckMessage::committed(
                        r.envelope.message_id,
                        config.verification,
                    ),
                )
            }
            other => panic!("fake bamepd received an unexpected message: {other:?}"),
        };

        send(&mut stream, &reply).await.expect("send reply");
    }
}

// =====================================================================
// harness
// =====================================================================

struct Harness {
    _identity: TestIdentity,
    server_addr: SocketAddr,
    fingerprint: ServerCertFingerprint,
    _control: WorkerControlHandle,
    _socket_dir: TempDir,
    _storage_dir: TempDir,
    obs: Obs,
    shutdown: tokio::sync::watch::Sender<bool>,
    driver_task: JoinHandle<()>,
    server_task: JoinHandle<Result<(), bamep_worker::data_plane::DataPlaneError>>,
    peer_task: JoinHandle<()>,
}

impl Harness {
    async fn start(config: PeerConfig) -> Self {
        let identity = TestIdentity::generate();
        let fingerprint = identity.fingerprint();
        let tls = build_server_config(
            &load_server_identity(&identity.cert_path, &identity.key_path).expect("load identity"),
        )
        .expect("build server config");

        let socket_dir = TempDir::fresh("uds");
        let socket_path = socket_dir.0.join("worker.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind fake bamepd");

        let storage_dir = TempDir::fresh("store");
        let chunk_store =
            FilesystemChunkStore::initialize(&storage_dir.0).expect("initialize chunk store");

        let (control, driver) = worker_control(
            socket_path,
            Duration::from_millis(20),
            Duration::from_millis(2000),
            Uuid::new_v4(),
        );
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let driver_task = tokio::spawn({
            let mut rx = shutdown_rx.clone();
            driver.run(async move {
                let _ = rx.wait_for(|s| *s).await;
            })
        });

        let obs: Obs = Arc::new(Mutex::new(PeerObservations::default()));
        let peer_task = tokio::spawn({
            let obs = Arc::clone(&obs);
            async move {
                let (stream, _) = timeout(TIMEOUT, listener.accept())
                    .await
                    .expect("no timeout")
                    .expect("accept");
                serve_peer(stream, config, obs).await;
            }
        });

        let data_plane = DataPlane::new(
            "127.0.0.1:0".parse().unwrap(),
            tls,
            control.clone(),
            chunk_store,
        );
        let server_handle = data_plane.handle();
        let server_task = tokio::spawn({
            let mut rx = shutdown_rx;
            data_plane.run(async move {
                let _ = rx.wait_for(|s| *s).await;
            })
        });
        let server_addr = timeout(TIMEOUT, server_handle.listening())
            .await
            .expect("no timeout")
            .expect("bound");

        timeout(TIMEOUT, control.authority().wait_for(|s| s.is_available()))
            .await
            .expect("no timeout")
            .expect("control available");

        Self {
            _identity: identity,
            server_addr,
            fingerprint,
            _control: control,
            _socket_dir: socket_dir,
            _storage_dir: storage_dir,
            obs,
            shutdown,
            driver_task,
            server_task,
            peer_task,
        }
    }

    fn base_url(&self) -> String {
        format!("https://{}", self.server_addr)
    }

    fn observations(&self) -> std::sync::MutexGuard<'_, PeerObservations> {
        self.obs.lock().unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.driver_task.abort();
        self.server_task.abort();
        self.peer_task.abort();
    }
}

// =====================================================================
// fixtures
// =====================================================================

const CHUNK_SIZE: u32 = 4;

struct Fixture {
    action_id: ProtocolId,
    transfer_id: Uuid,
    artifact_id: Uuid,
}

impl Fixture {
    fn fresh() -> Self {
        Self {
            action_id: ProtocolId::generate(),
            transfer_id: Uuid::new_v4(),
            artifact_id: Uuid::new_v4(),
        }
    }

    fn dispatch(&self, chunk_size: u32) -> ActionDispatchMessage {
        let params = serde_json::json!({
            "transfer_id": self.transfer_id.to_string(),
            "artifact_id": self.artifact_id.to_string(),
            "direction": "agent_to_server",
            "digest_algorithm": "sha256",
            "chunk_size": chunk_size,
        })
        .as_object()
        .unwrap()
        .clone();
        ActionDispatchMessage::new(
            self.action_id,
            M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
            "1",
            params,
        )
    }

    fn authorization(&self, base_url: &str) -> AgentTransferAuthorization {
        AgentTransferAuthorization::new(
            AgentProofKey::generate(),
            "opaque-capability-token",
            self.transfer_id,
            self.artifact_id,
            DataPlaneTransferDirection::AgentToServer,
            base_url,
        )
    }
}

// =====================================================================
// tests
// =====================================================================

/// §35 — a multi-chunk Agent -> Server transfer completes end to end across the
/// real HTTPS boundary and builds the exact `TRANSFER_VERIFIED` `ActionResult`.
#[tokio::test]
async fn happy_agent_side_transfer_reaches_verified_and_builds_transfer_verified_result() {
    let fixture = Fixture::fresh();
    // 10 bytes at chunk_size 4 -> 3 chunks (4, 4, 2).
    let source = InMemoryTransferSource::pattern(10, 1);
    let harness = Harness::start(PeerConfig::happy(CHUNK_SIZE, 3, fixture.artifact_id)).await;

    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let response = agent.accept(&fixture.dispatch(CHUNK_SIZE));
    assert!(matches!(
        response.ack.body.outcome,
        ActionAckOutcome::Accepted
    ));
    let accepted = response.accepted.expect("accepted");
    assert_eq!(accepted.action_id(), fixture.action_id);
    assert_eq!(accepted.transfer_id(), fixture.transfer_id);
    assert_eq!(accepted.artifact_id(), fixture.artifact_id);

    let progress = Arc::new(Mutex::new(Vec::<u64>::new()));
    let outcome = {
        let progress = Arc::clone(&progress);
        let mut sink = move |p: TransferProgress| progress.lock().unwrap().push(p.bytes_processed);
        agent
            .run(
                &accepted,
                &fixture.authorization(&harness.base_url()),
                &source,
                &TransferRunOptions::default(),
                &mut sink,
            )
            .await
            .expect("run")
    };

    let TransferRunOutcome::Completed(TransferActionResult::Verified { artifact_id }) = outcome
    else {
        panic!("expected Completed(Verified), got {outcome:?}");
    };
    assert_eq!(artifact_id, fixture.artifact_id);

    // §44 — progress follows durably-accepted bytes: 0 (initial), 4, 8, 10.
    assert_eq!(*progress.lock().unwrap(), vec![0, 4, 8, 10]);

    // The constructed ActionResult is exactly the RF-005 shape.
    let result_msg =
        TransferActionResult::Verified { artifact_id }.into_action_result(fixture.action_id);
    assert_eq!(result_msg.body.action_id, fixture.action_id);
    assert!(matches!(
        result_msg.body.outcome,
        ActionResultOutcome::Succeeded
    ));
    assert_eq!(
        result_msg
            .body
            .detail
            .get("code")
            .unwrap()
            .as_str()
            .unwrap(),
        "TRANSFER_VERIFIED"
    );
    assert_eq!(
        result_msg
            .body
            .detail
            .get("artifact_id")
            .unwrap()
            .as_str()
            .unwrap(),
        fixture.artifact_id.to_string()
    );

    let obs = harness.observations();
    assert_eq!(obs.authorization_query_indices, vec![0, 1, 2]);
    assert_eq!(obs.chunk_acceptance_indices, vec![0, 1, 2]);
    assert_eq!(obs.seal_requests, 1);
    assert_eq!(obs.verification_reports, 1);
    // Every per-request proof carried a fresh `proof_id`.
    let mut unique: BTreeSet<&String> = BTreeSet::new();
    for id in &obs.proof_ids_seen {
        assert!(unique.insert(id), "proof_id {id} was reused");
    }
}

/// §36 / §44 — an interrupted run suspends non-terminally; a resumed run with a
/// fresh authorization skips the durably-held chunk and never regenerates
/// identity.
#[tokio::test]
async fn resume_discovery_skips_durably_held_chunks_and_preserves_identity() {
    let fixture = Fixture::fresh();
    let source = InMemoryTransferSource::pattern(10, 2); // 3 chunks
    let harness = Harness::start(PeerConfig::happy(CHUNK_SIZE, 3, fixture.artifact_id)).await;
    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");

    // --- run 1: interrupt after the first durably-held chunk ---
    let progress1 = Arc::new(Mutex::new(Vec::<u64>::new()));
    let outcome1 = {
        let progress = Arc::clone(&progress1);
        let mut sink = move |p: TransferProgress| progress.lock().unwrap().push(p.bytes_processed);
        agent
            .run(
                &accepted,
                &fixture.authorization(&harness.base_url()),
                &source,
                &TransferRunOptions {
                    interrupt_after_newly_held_chunks: Some(1),
                    ..Default::default()
                },
                &mut sink,
            )
            .await
            .expect("run 1")
    };
    let TransferRunOutcome::Suspended(suspended) = outcome1 else {
        panic!("expected Suspended, got {outcome1:?}");
    };
    assert_eq!(suspended.reason, SuspendReason::InterruptionHookFired);
    assert_eq!(suspended.action_id, fixture.action_id);
    assert_eq!(suspended.transfer_id, fixture.transfer_id);
    assert_eq!(suspended.artifact_id, fixture.artifact_id);
    assert_eq!(suspended.durably_held_bytes, 4);
    assert_eq!(*progress1.lock().unwrap(), vec![0, 4]);
    assert_eq!(
        harness
            .observations()
            .held
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![0]
    );

    let run1_auth_queries = harness.observations().authorization_query_indices.clone();
    assert_eq!(run1_auth_queries, vec![0]);

    // --- run 2: fresh authorization, same accepted handle ---
    let progress2 = Arc::new(Mutex::new(Vec::<u64>::new()));
    let outcome2 = {
        let progress = Arc::clone(&progress2);
        let mut sink = move |p: TransferProgress| progress.lock().unwrap().push(p.bytes_processed);
        agent
            .run(
                &accepted,
                &fixture.authorization(&harness.base_url()),
                &source,
                &TransferRunOptions::default(),
                &mut sink,
            )
            .await
            .expect("run 2")
    };
    let TransferRunOutcome::Completed(TransferActionResult::Verified { artifact_id }) = outcome2
    else {
        panic!("expected Completed(Verified), got {outcome2:?}");
    };
    assert_eq!(artifact_id, fixture.artifact_id);
    // Progress resumes from the already-durable 4 bytes, never regressing.
    assert_eq!(*progress2.lock().unwrap(), vec![4, 8, 10]);

    let obs = harness.observations();
    // Chunk 0 was authorized once (run 1) and never re-PUT in run 2.
    assert_eq!(obs.authorization_query_indices, vec![0, 1, 2]);
    assert_eq!(obs.chunk_acceptance_indices, vec![0, 1, 2]);
    assert_eq!(obs.resume_query_count, 2);
    assert_eq!(obs.held.len(), 3);
}

/// §37 — transmission corruption is rejected by the Worker's independent hash
/// (`DIGEST_MISMATCH`), never durably accepted, and mapped to
/// `CHUNK_VERIFICATION_FAILED`. The source-of-truth digest is unchanged.
#[tokio::test]
async fn transmission_corruption_is_rejected_and_never_committed() {
    let fixture = Fixture::fresh();
    let source = InMemoryTransferSource::pattern(8, 3); // 2 chunks
    let harness = Harness::start(PeerConfig::happy(CHUNK_SIZE, 2, fixture.artifact_id)).await;
    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");

    let outcome = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions {
                corrupt_transmitted_bytes_of_chunk: Some(0),
                ..Default::default()
            },
            &mut |_p| {},
        )
        .await
        .expect("run");

    assert_eq!(
        outcome,
        TransferRunOutcome::Completed(TransferActionResult::ChunkVerificationFailed {
            artifact_id: fixture.artifact_id,
        })
    );

    let obs = harness.observations();
    // The corrupt chunk was authorized but never durably accepted.
    assert_eq!(obs.authorization_query_indices, vec![0]);
    assert!(obs.chunk_acceptance_indices.is_empty());
    assert!(obs.held.is_empty());
    assert_eq!(obs.seal_requests, 0);
}

/// §38 — a source mutation of an already-recorded chunk identity fails closed
/// on the resumed run without rewriting the expected identity or creating a new
/// Transfer/Artifact.
#[tokio::test]
async fn source_mutation_of_a_recorded_chunk_fails_closed_without_rewriting_identity() {
    let fixture = Fixture::fresh();
    let mut source = InMemoryTransferSource::pattern(10, 4); // 3 chunks
    let harness = Harness::start(PeerConfig::happy(CHUNK_SIZE, 3, fixture.artifact_id)).await;
    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");

    // Run 1 durably records chunk 0, then interrupts.
    agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions {
                interrupt_after_newly_held_chunks: Some(1),
                ..Default::default()
            },
            &mut |_p| {},
        )
        .await
        .expect("run 1");
    let recorded_digest_0 = harness
        .observations()
        .held
        .get(&0)
        .cloned()
        .expect("chunk 0 recorded");

    // The source's chunk-0 bytes now change.
    source.mutate_chunk(0, CHUNK_SIZE);

    let outcome = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions::default(),
            &mut |_p| {},
        )
        .await
        .expect("run 2");

    assert_eq!(
        outcome,
        TransferRunOutcome::Completed(TransferActionResult::ChunkVerificationFailed {
            artifact_id: fixture.artifact_id,
        })
    );

    let obs = harness.observations();
    // The recorded expected identity for chunk 0 was never rewritten, and no
    // new chunk-acceptance request for chunk 0 was issued on run 2.
    assert_eq!(obs.held.get(&0), Some(&recorded_digest_0));
    assert_eq!(obs.chunk_acceptance_indices, vec![0]); // only run 1's
    assert_eq!(obs.seal_requests, 0);
}

/// §39 — a generic authorization denial is non-enumerable: the Agent suspends
/// with `AuthorizationUnavailable`, no bytes are durably accepted, and no
/// terminal success is produced.
#[tokio::test]
async fn generic_authorization_denial_is_non_enumerable_and_accepts_no_bytes() {
    let fixture = Fixture::fresh();
    let source = InMemoryTransferSource::pattern(8, 5);
    let mut config = PeerConfig::happy(CHUNK_SIZE, 2, fixture.artifact_id);
    config.deny_chunk_auth = BTreeSet::from([0]);
    let harness = Harness::start(config).await;
    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");

    let outcome = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions::default(),
            &mut |_p| {},
        )
        .await
        .expect("run");

    let TransferRunOutcome::Suspended(suspended) = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };
    assert_eq!(suspended.reason, SuspendReason::AuthorizationUnavailable);
    assert_eq!(suspended.durably_held_bytes, 0);

    let obs = harness.observations();
    assert!(obs.chunk_acceptance_indices.is_empty());
    assert!(obs.held.is_empty());
    assert_eq!(obs.seal_requests, 0);
}

/// §40 — a replayed proof is denied by `bamepd`; the Agent only ever sees the
/// generic denial and can retry with a fresh authorization. Fidelity note: the
/// fake peer returns the generic denial after the first run (modelling a
/// replay-cache rejection); authoritative replay verification is exercised by
/// #38's own tests, not re-implemented here.
#[tokio::test]
async fn a_denied_later_authorization_is_only_ever_the_generic_denial() {
    let fixture = Fixture::fresh();
    let source = InMemoryTransferSource::pattern(8, 6); // 2 chunks
    let mut config = PeerConfig::happy(CHUNK_SIZE, 2, fixture.artifact_id);
    // Serve run 1 fully (resume + 2 auth + seal = 4 authorizing requests), then
    // deny everything after.
    config.deny_after_requests = Some(4);
    let harness = Harness::start(config).await;
    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");

    let first = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions::default(),
            &mut |_p| {},
        )
        .await
        .expect("run 1");
    assert_eq!(
        first,
        TransferRunOutcome::Completed(TransferActionResult::Verified {
            artifact_id: fixture.artifact_id,
        })
    );

    let second = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions::default(),
            &mut |_p| {},
        )
        .await
        .expect("run 2");
    let TransferRunOutcome::Suspended(suspended) = second else {
        panic!("expected Suspended, got {second:?}");
    };
    assert_eq!(suspended.reason, SuspendReason::AuthorizationUnavailable);
}

/// §41 — a `Failed` seal Artifact maps to `ARTIFACT_VERIFICATION_FAILED`; an
/// HTTP `200` is never itself read as action success.
#[tokio::test]
async fn seal_failed_artifact_maps_to_artifact_verification_failed() {
    let fixture = Fixture::fresh();
    let source = InMemoryTransferSource::pattern(8, 7);
    let mut config = PeerConfig::happy(CHUNK_SIZE, 2, fixture.artifact_id);
    config.verification = WireArtifactStatus::Failed;
    let harness = Harness::start(config).await;
    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");

    let outcome = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions::default(),
            &mut |_p| {},
        )
        .await
        .expect("run");

    assert_eq!(
        outcome,
        TransferRunOutcome::Completed(TransferActionResult::ArtifactVerificationFailed {
            artifact_id: fixture.artifact_id,
        })
    );
}

/// §42 — a `409 INCOMPLETE_MANIFEST` seal never yields `TRANSFER_VERIFIED`.
#[tokio::test]
async fn incomplete_manifest_seal_never_yields_transfer_verified() {
    let fixture = Fixture::fresh();
    let source = InMemoryTransferSource::pattern(8, 8);
    let mut config = PeerConfig::happy(CHUNK_SIZE, 2, fixture.artifact_id);
    config.seal = SealPlan::IncompleteManifest;
    let harness = Harness::start(config).await;
    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");

    let outcome = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions::default(),
            &mut |_p| {},
        )
        .await
        .expect("run");

    match outcome {
        TransferRunOutcome::Completed(TransferActionResult::Abandoned { artifact_id }) => {
            assert_eq!(artifact_id, fixture.artifact_id);
        }
        other => panic!("expected Completed(Abandoned), got {other:?}"),
    }
    let message = match &outcome {
        TransferRunOutcome::Completed(result) => result.into_action_result(fixture.action_id),
        _ => unreachable!(),
    };
    assert_ne!(
        message.body.detail.get("code").unwrap().as_str().unwrap(),
        "TRANSFER_VERIFIED"
    );
}

/// §42 / §25 — a denied seal (`401`) is the non-enumerable generic denial: the
/// Agent suspends with `AuthorizationUnavailable`, never `TRANSFER_VERIFIED`.
#[tokio::test]
async fn denied_seal_suspends_with_the_generic_denial() {
    let fixture = Fixture::fresh();
    let source = InMemoryTransferSource::pattern(8, 10);
    let mut config = PeerConfig::happy(CHUNK_SIZE, 2, fixture.artifact_id);
    config.seal = SealPlan::Denied;
    let harness = Harness::start(config).await;
    let agent = DataPlaneTransferAgent::new(harness.fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");

    let outcome = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions::default(),
            &mut |_p| {},
        )
        .await
        .expect("run");

    let TransferRunOutcome::Suspended(suspended) = outcome else {
        panic!("expected Suspended, got {outcome:?}");
    };
    assert_eq!(suspended.reason, SuspendReason::AuthorizationUnavailable);
    // All chunks were durably accepted before the seal failed closed.
    assert_eq!(harness.observations().held.len(), 2);
}

/// §34 / §43 — the correct configured leaf pin succeeds; a wrong leaf pin fails
/// the TLS connection before any HTTP request reaches the server.
#[tokio::test]
async fn exact_leaf_pin_succeeds_and_a_wrong_pin_fails_before_http() {
    let fixture = Fixture::fresh();
    let source = InMemoryTransferSource::pattern(4, 9); // 1 chunk
    let harness = Harness::start(PeerConfig::happy(CHUNK_SIZE, 1, fixture.artifact_id)).await;

    // --- correct pin: the raw client reaches the real server ---
    let good = DataPlaneClient::connect(&harness.base_url(), harness.fingerprint).expect("connect");
    let auth = fixture.authorization(&harness.base_url());
    let proof = auth
        .create_proof_now(bamep_simulator::TransferOperation::ResumeDiscovery, None)
        .unwrap();
    let outcome = good
        .discover_resume(auth.token(), fixture.transfer_id, &proof)
        .await
        .expect("transport ok");
    assert!(matches!(outcome, ResumeOutcome::Approved(_)));

    // --- wrong pin: TLS fails, no request is processed ---
    let wrong_fingerprint = ServerCertFingerprint::from_sha256_digest([0x11; 32]);
    let agent = DataPlaneTransferAgent::new(wrong_fingerprint);
    let accepted = agent
        .accept(&fixture.dispatch(CHUNK_SIZE))
        .accepted
        .expect("accepted");
    let run = agent
        .run(
            &accepted,
            &fixture.authorization(&harness.base_url()),
            &source,
            &TransferRunOptions::default(),
            &mut |_p| {},
        )
        .await
        .expect("run returns an outcome, not an error");
    let TransferRunOutcome::Suspended(suspended) = run else {
        panic!("expected Suspended(DataPlaneUnreachable), got {run:?}");
    };
    assert_eq!(suspended.reason, SuspendReason::DataPlaneUnreachable);

    // A direct wrong-pin client call surfaces the TLS transport error itself.
    let bad = DataPlaneClient::connect(&harness.base_url(), wrong_fingerprint).expect("connect");
    let proof = auth
        .create_proof_now(bamep_simulator::TransferOperation::ResumeDiscovery, None)
        .unwrap();
    let err = bad
        .discover_resume(auth.token(), fixture.transfer_id, &proof)
        .await
        .expect_err("wrong pin must fail the TLS handshake");
    assert!(matches!(err, DataPlaneTransportError::Tls(_)));

    // Only the two legitimate resume queries above ever reached the peer.
    assert_eq!(harness.observations().resume_query_count, 1);
}
