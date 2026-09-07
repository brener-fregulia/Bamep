//! Issue #63 Stage 2 — Linux one-transfer harness. THROWAWAY Spike.
//!
//! Owner-approved backend (option (a), Phase-A shape): real Worker HTTPS
//! `DataPlane`, real `FilesystemChunkStore` staging/fsync/linkat, real D2
//! `FullArtifactHasher`, real `bamep_simulator::DataPlaneClient` +
//! per-request Ed25519 proof; `bamepd` faked over UDS (ADR-0018) — authority
//! seam only. NO keep-alive / pipelining / connection reuse / parallel PUTs:
//! the current serial fresh-connection client shape, unchanged.
//!
//! Modes:
//!   --smoke                 host synthetic vertical: one transfer at 8/16/32/64
//!                           MiB chunk over a 128 MiB extent; prints a
//!                           `CaseResult` NDJSON line per size + a summary.
//!   --case-file <Case.json> run ONE transfer for a `matrix::Case` (synthetic
//!                           source); prints its `CaseResult`. This is the shape
//!                           the Stage-3 physical harness replaces with the real
//!                           #61/CP7 Server/Worker path + the WinPE probe source.
//!
//! Host throughput numbers here are NOT physical evidence and must not be
//! interpreted. Nothing in this binary arms a physical matrix.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use bamep_i63_stage2_engine::matrix::{
    self, verify_chunk_agreement_at_extent, Case, ChunkAgreement, Phase,
};
use bamep_i63_stage2_engine::result::{CaseResult, ConnectionCount};
use bamep_i63_stage2_engine::MIB;

use bamep_simulator::{
    AgentProofKey, AgentTransferAuthorization, DataPlaneClient, DataPlaneTransferDirection,
    PutChunkOutcome, ResumeOutcome, SealArtifactStatus, SealOutcome, TransferOperation,
};
use bamep_trusted_bootstrap::ServerCertFingerprint;
use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::{worker_control, WorkerControlHandle};
use bamep_worker::storage::{FilesystemChunkStore, FullArtifactHasher, FullArtifactRequest};
use bamep_worker::tls::{build_server_config, load_server_identity};
use bamep_worker_protocol::{
    ArtifactVerificationAckMessage, AuthorizationDecisionMessage, ChunkAcceptanceDecisionMessage,
    ManifestSealDecisionMessage, ResumeDiscoveryPageMessage, SealedManifestFacts, ServerHelloMessage,
    WireArtifactStatus, WireDigestAlgorithm, WorkerProtocolMessage,
};
use tokio::net::UnixListener;
use tokio::sync::watch;

/// Stage-2 host synthetic smoke extent: the smallest common extent divisible by
/// 8/16/32/64 MiB (⇒ 16/8/4/2 whole chunks; no partial final chunk).
const SMOKE_EXTENT_BYTES: u64 = 128 * MIB;

fn die(m: impl AsRef<str>) -> ! {
    eprintln!("stage2-harness: FATAL: {}", m.as_ref());
    std::process::exit(1);
}

fn sha256_wire(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

/// Deterministic synthetic source chunk (same pattern family as the frozen
/// Phase-A benchmark; host-only, never physical evidence).
fn make_source(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[0];
    }
    v
}

// ---- throwaway temp dir -------------------------------------------------

struct TempDir(PathBuf);
impl TempDir {
    fn fresh(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bamep-i63s2-{tag}-{}", Uuid::new_v4()));
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

// ---- fake bamepd: UDS auto-responder (ADR-0018) -----------------------
//
// Approves every authorization, commits every chunk, and at seal returns
// Verified IFF the Worker's INDEPENDENT D2 reconstruction digest equals the
// Agent-declared digest — so `Artifact::Verified` here is a real D2 check, not
// a rubber stamp. The authoritative `chunk_size` it hands back is driven by a
// watch channel (one value per case), exactly as the frozen benchmark does.

async fn fake_bamepd(listener: UnixListener, cs_rx: watch::Receiver<u32>) {
    let (mut stream, _) = listener.accept().await.expect("accept worker uds");
    let hello = match bamep_worker_protocol::receive(&mut stream).await {
        Ok(WorkerProtocolMessage::WorkerHello(h)) => h,
        other => panic!("fake bamepd: expected WorkerHello, got {other:?}"),
    };
    bamep_worker_protocol::send(
        &mut stream,
        &WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(hello.envelope.message_id)),
    )
    .await
    .expect("send ServerHello");

    let mut expected_digest: Option<String> = None;
    loop {
        let msg = match bamep_worker_protocol::receive(&mut stream).await {
            Ok(m) => m,
            Err(_) => break,
        };
        let chunk_size = *cs_rx.borrow();
        let reply = match msg {
            WorkerProtocolMessage::AuthorizationQuery(q) => {
                WorkerProtocolMessage::AuthorizationDecision(AuthorizationDecisionMessage::approved(
                    q.envelope.message_id,
                    WireDigestAlgorithm::Sha256,
                    chunk_size,
                    "issue63-s2-acceptance-handle",
                    None,
                ))
            }
            WorkerProtocolMessage::ChunkAcceptanceRequest(r) => {
                WorkerProtocolMessage::ChunkAcceptanceDecision(
                    ChunkAcceptanceDecisionMessage::committed(r.envelope.message_id),
                )
            }
            WorkerProtocolMessage::ResumeDiscoveryQuery(q) => {
                WorkerProtocolMessage::ResumeDiscoveryPage(ResumeDiscoveryPageMessage::first_page(
                    q.envelope.message_id,
                    q.body.transfer_id,
                    false,
                    WireDigestAlgorithm::Sha256,
                    chunk_size,
                    None,
                    Vec::new(),
                    None,
                ))
            }
            WorkerProtocolMessage::ManifestSealRequest(r) => {
                expected_digest = Some(r.body.artifact_digest.clone());
                WorkerProtocolMessage::ManifestSealDecision(ManifestSealDecisionMessage::sealed(
                    r.envelope.message_id,
                    SealedManifestFacts {
                        verification_handle: "issue63-s2-verification-handle".to_string(),
                        artifact_id: Uuid::new_v4(),
                        digest_algorithm: WireDigestAlgorithm::Sha256,
                        chunk_size,
                        chunk_count: r.body.chunk_count,
                        expected_artifact_digest: r.body.artifact_digest.clone(),
                    },
                ))
            }
            WorkerProtocolMessage::ArtifactVerificationReport(r) => {
                let ok =
                    expected_digest.as_deref() == Some(r.body.computed_artifact_digest.as_str());
                WorkerProtocolMessage::ArtifactVerificationAck(
                    ArtifactVerificationAckMessage::committed(
                        r.envelope.message_id,
                        if ok {
                            WireArtifactStatus::Verified
                        } else {
                            WireArtifactStatus::Failed
                        },
                    ),
                )
            }
            _ => continue,
        };
        if bamep_worker_protocol::send(&mut stream, &reply).await.is_err() {
            break;
        }
    }
}

// ---- Worker stack -----------------------------------------------------

struct WorkerStack {
    base_url: String,
    fingerprint: ServerCertFingerprint,
    store: FilesystemChunkStore,
    cs_tx: watch::Sender<u32>,
    _storage: TempDir,
    _tls: TempDir,
    _uds: TempDir,
}

async fn bring_up_worker(initial_chunk_size: u32) -> WorkerStack {
    let storage = TempDir::fresh("store");
    let tls_dir = TempDir::fresh("tls");
    let uds = TempDir::fresh("uds");

    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(["localhost".to_string()]).expect("gen cert");
    let cert_path = tls_dir.0.join("cert.pem");
    let key_path = tls_dir.0.join("key.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let identity = load_server_identity(&cert_path, &key_path).expect("load identity");
    let fingerprint = identity.fingerprint;
    let tls = build_server_config(&identity).expect("server config");

    let sock_path = uds.0.join("worker.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind uds");
    let (cs_tx, cs_rx) = watch::channel::<u32>(initial_chunk_size);
    tokio::spawn(fake_bamepd(listener, cs_rx));

    let (control, driver): (WorkerControlHandle, _) = worker_control(
        sock_path,
        Duration::from_millis(20),
        Duration::from_secs(120),
        Uuid::new_v4(),
    );
    let driver = driver.with_pending_capacity(100_000);
    tokio::spawn(driver.run(std::future::pending::<()>()));

    let store = FilesystemChunkStore::initialize(&storage.0).expect("init store");
    let data_plane = DataPlane::new(
        "127.0.0.1:0".parse().unwrap(),
        tls,
        control.clone(),
        store.clone(),
    );
    let handle = data_plane.handle();
    tokio::spawn(data_plane.run(std::future::pending::<()>()));
    let addr = handle.listening().await.expect("listening");
    control
        .authority()
        .wait_for(|s| s.is_available())
        .await
        .expect("control available");

    WorkerStack {
        base_url: format!("https://127.0.0.1:{}", addr.port()),
        fingerprint,
        store,
        cs_tx,
        _storage: storage,
        _tls: tls_dir,
        _uds: uds,
    }
}

// ---- one transfer ----------------------------------------------------

#[derive(Debug)]
enum TransferFailure {
    ChunkAgreement(String),
    Transport(String),
    NotAccepted(String),
    SealNotVerified(String),
    D2Mismatch(String),
}

/// Run ONE transfer for `case` against a synthetic source and produce its
/// `CaseResult`. Measurement boundaries:
///   A (bulk stream wall) — first source read .. last chunk durably accepted.
///   B (verified transfer wall) — before resume discovery .. `Artifact Verified`
///       observed (the `seal` call blocks on the Worker D2 reconstruction).
#[allow(clippy::too_many_lines)]
async fn run_one_transfer(
    ws: &WorkerStack,
    case: &Case,
    expected_extent_bytes: u64,
) -> Result<CaseResult, TransferFailure> {
    let chunk_size = case.chunk_size_bytes;
    let chunks = case.expected_chunk_count;
    let transfer_id = Uuid::new_v4();
    let artifact_id = Uuid::new_v4();

    // Hand the fake bamepd this case's authoritative chunk_size BEFORE the
    // transfer (drives AuthorizationDecision / ResumeDiscoveryPage / seal facts).
    let _ = ws.cs_tx.send(chunk_size as u32);

    let proof_key = AgentProofKey::generate();
    let auth = AgentTransferAuthorization::new(
        proof_key,
        "issue63-s2-opaque-capability-token",
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        ws.base_url.clone(),
    );
    let client = DataPlaneClient::connect(&ws.base_url, ws.fingerprint)
        .map_err(|e| TransferFailure::Transport(format!("connect: {e}")))?
        .with_request_timeout(Duration::from_secs(300));

    let source_chunk = make_source(chunk_size as usize);

    let mut read_ns = 0u128;
    let mut chunk_sha_ns = 0u128;
    let mut rolling_sha_ns = 0u128;
    let mut proof_ns = 0u128;
    let mut put_ack_ns = 0u128;
    let mut rolling = Sha256::new();

    // ---- boundary B start ----
    let wall_b = Instant::now();

    let rproof = auth
        .create_proof_now(TransferOperation::ResumeDiscovery, None)
        .map_err(|e| TransferFailure::Transport(format!("resume proof: {e}")))?;
    let t = Instant::now();
    let resume = client
        .discover_resume(auth.token(), transfer_id, &rproof)
        .await
        .map_err(|e| TransferFailure::Transport(format!("resume: {e}")))?;
    let resume_ms = t.elapsed().as_secs_f64() * 1000.0;
    let ResumeOutcome::Approved(manifest) = resume else {
        return Err(TransferFailure::Transport(format!("resume not approved: {resume:?}")));
    };
    let server_transfer_chunk_size = manifest.chunk_size as u64;

    // ---- exact chunk-size agreement gate — fail closed before any bulk bytes.
    let agreement = ChunkAgreement {
        plan_extent_bytes: case.extent_bytes,
        plan_chunk_size_bytes: chunk_size,
        plan_expected_chunk_count: case.expected_chunk_count,
        server_transfer_chunk_size,
        // In the host smoke the coordinator's ActionDispatch chunk_size == the
        // plan's; Stage 3 substitutes the real dispatch value here.
        action_dispatch_chunk_size: chunk_size,
        probe_chunk_size: chunk_size,
    };
    let agreed_chunks = verify_chunk_agreement_at_extent(expected_extent_bytes, &agreement)
        .map_err(|e| TransferFailure::ChunkAgreement(format!("{e:?}")))?;
    if agreed_chunks != chunks {
        return Err(TransferFailure::ChunkAgreement(format!(
            "agreed {agreed_chunks} != case {chunks}"
        )));
    }

    // ---- boundary A start (first bulk source read) ----
    let wall_a = Instant::now();
    let mut total_bytes = 0u64;
    for index in 0..chunks {
        let t = Instant::now();
        let body: Vec<u8> = source_chunk.clone();
        read_ns += t.elapsed().as_nanos();

        let t = Instant::now();
        let chunk_digest = sha256_wire(&body);
        chunk_sha_ns += t.elapsed().as_nanos();

        let t = Instant::now();
        rolling.update(&body);
        rolling_sha_ns += t.elapsed().as_nanos();

        let t = Instant::now();
        let proof = auth
            .create_proof_now(TransferOperation::ChunkUpload, Some(index))
            .map_err(|e| TransferFailure::Transport(format!("chunk proof: {e}")))?;
        proof_ns += t.elapsed().as_nanos();

        total_bytes += body.len() as u64;

        let t = Instant::now();
        let outcome = client
            .put_chunk(auth.token(), transfer_id, index, &chunk_digest, &proof, body)
            .await
            .map_err(|e| TransferFailure::Transport(format!("put {index}: {e}")))?;
        put_ack_ns += t.elapsed().as_nanos();
        match outcome {
            PutChunkOutcome::Accepted { .. } => {}
            other => {
                return Err(TransferFailure::NotAccepted(format!(
                    "chunk {index}: {other:?}"
                )))
            }
        }
    }
    // ---- boundary A end (final chunk durably accepted) ----
    let bulk_stream_wall_ms = wall_a.elapsed().as_secs_f64() * 1000.0;

    let artifact_digest = URL_SAFE_NO_PAD.encode(rolling.finalize());
    let sproof = auth
        .create_proof_now(TransferOperation::SealManifest, None)
        .map_err(|e| TransferFailure::Transport(format!("seal proof: {e}")))?;
    let t = Instant::now();
    let seal = client
        .seal(auth.token(), transfer_id, &sproof, chunks, &artifact_digest)
        .await
        .map_err(|e| TransferFailure::Transport(format!("seal: {e}")))?;
    let seal_d2_ms = t.elapsed().as_secs_f64() * 1000.0;
    // ---- boundary B end (Artifact observed Verified) ----
    let verified_transfer_wall_ms = wall_b.elapsed().as_secs_f64() * 1000.0;

    match seal {
        SealOutcome::Completed { artifact_status, .. } => {
            if artifact_status != SealArtifactStatus::Verified {
                return Err(TransferFailure::SealNotVerified(format!("{artifact_status:?}")));
            }
        }
        other => return Err(TransferFailure::SealNotVerified(format!("{other:?}"))),
    }

    // ---- independent D2 recheck (same public API the Worker D2 path uses) ----
    let d2 = FullArtifactHasher::for_store(&ws.store)
        .compute_blocking(FullArtifactRequest {
            transfer_id,
            chunk_count: chunks,
            chunk_size: chunk_size as u32,
        })
        .await
        .map_err(|e| TransferFailure::D2Mismatch(format!("recheck: {e}")))?;
    if d2.digest.to_base64url_no_pad() != artifact_digest {
        return Err(TransferFailure::D2Mismatch("digest".into()));
    }
    if d2.total_size != total_bytes || d2.chunk_count != chunks {
        return Err(TransferFailure::D2Mismatch(format!(
            "count/size: {} bytes, {} chunks",
            d2.total_size, d2.chunk_count
        )));
    }

    // keep the store bounded across cases
    let _ = std::fs::remove_dir_all(
        Path::new(&ws._storage.0)
            .join("transfers")
            .join(transfer_id.as_hyphenated().to_string()),
    );

    let ns_ms = |n: u128| n as f64 / 1_000_000.0;
    let (b_mib, b_mb) = CaseResult::rates(case.extent_bytes, bulk_stream_wall_ms);
    let (v_mib, v_mb) = CaseResult::rates(case.extent_bytes, verified_transfer_wall_ms);

    Ok(CaseResult {
        run_id: case.run_id.clone(),
        case_id: case.case_id.clone(),
        phase: case.phase,
        cycle: case.cycle,
        slot: case.slot,
        chunk_size_bytes: chunk_size,
        extent_bytes: case.extent_bytes,
        chunk_count: chunks,
        transfer_id: Some(transfer_id.to_string()),
        artifact_id: Some(artifact_id.to_string()),
        source_safety_verdict: "n/a(host-synthetic-source)".into(),
        clock_skew_verdict: "n/a(host)".into(),
        bulk_stream_wall_ms,
        bulk_stream_mib_s: b_mib,
        bulk_stream_mb_s: b_mb,
        verified_transfer_wall_ms,
        verified_transfer_mib_s: v_mib,
        verified_transfer_mb_s: v_mb,
        resume_ms,
        seal_d2_ms,
        read_ms: ns_ms(read_ns),
        chunk_sha_ms: ns_ms(chunk_sha_ns),
        rolling_sha_ms: ns_ms(rolling_sha_ns),
        proof_ms: ns_ms(proof_ns),
        put_ack_ms: ns_ms(put_ack_ns),
        connection_count: ConnectionCount::by_construction(chunks),
        final_artifact_status: "Verified".into(),
        case_status: "completed".into(),
    })
}

fn failed_result(case: &Case, why: &TransferFailure) -> CaseResult {
    let (status, detail) = match why {
        TransferFailure::ChunkAgreement(m) => ("failed:chunk_agreement", m),
        TransferFailure::Transport(m) => ("failed:transport", m),
        TransferFailure::NotAccepted(m) => ("failed:not_accepted", m),
        TransferFailure::SealNotVerified(m) => ("failed:seal_not_verified", m),
        TransferFailure::D2Mismatch(m) => ("failed:d2_mismatch", m),
    };
    CaseResult {
        run_id: case.run_id.clone(),
        case_id: case.case_id.clone(),
        phase: case.phase,
        cycle: case.cycle,
        slot: case.slot,
        chunk_size_bytes: case.chunk_size_bytes,
        extent_bytes: case.extent_bytes,
        chunk_count: case.expected_chunk_count,
        transfer_id: None,
        artifact_id: None,
        source_safety_verdict: "n/a(host-synthetic-source)".into(),
        clock_skew_verdict: "n/a(host)".into(),
        bulk_stream_wall_ms: 0.0,
        bulk_stream_mib_s: 0.0,
        bulk_stream_mb_s: 0.0,
        verified_transfer_wall_ms: 0.0,
        verified_transfer_mib_s: 0.0,
        verified_transfer_mb_s: 0.0,
        resume_ms: 0.0,
        seal_d2_ms: 0.0,
        read_ms: 0.0,
        chunk_sha_ms: 0.0,
        rolling_sha_ms: 0.0,
        proof_ms: 0.0,
        put_ack_ms: 0.0,
        connection_count: ConnectionCount::by_construction(case.expected_chunk_count),
        final_artifact_status: format!("not-verified ({detail})"),
        case_status: status.into(),
    }
}

// ---- modes ----------------------------------------------------------

fn smoke_case(mib: u64) -> Case {
    let chunk_size_bytes = mib * MIB;
    let expected_chunk_count = matrix::expected_chunk_count(SMOKE_EXTENT_BYTES, chunk_size_bytes)
        .unwrap_or_else(|e| die(format!("{mib} MiB does not divide the smoke extent: {e:?}")));
    Case {
        run_id: "i63s2-host-smoke".into(),
        case_id: format!("i63s2-host-smoke/{mib:02}mib"),
        phase: Phase::Warmup, // host smoke, never analysed as measured
        cycle: None,
        slot: None,
        chunk_size_bytes,
        extent_bytes: SMOKE_EXTENT_BYTES,
        expected_chunk_count,
    }
}

async fn run_smoke() -> i32 {
    eprintln!(
        "stage2-harness: host synthetic vertical — 8/16/32/64 MiB chunk over a {} MiB extent",
        SMOKE_EXTENT_BYTES / MIB
    );
    eprintln!("stage2-harness: host throughput is NOT physical evidence and is not interpreted.");
    let ws = bring_up_worker(8 * MIB as u32).await;

    let mut all_ok = true;
    let mut results = Vec::new();
    for mib in [8u64, 16, 32, 64] {
        let case = smoke_case(mib);
        let expected_before = case.expected_chunk_count;
        match run_one_transfer(&ws, &case, SMOKE_EXTENT_BYTES).await {
            Ok(r) => {
                assert_eq!(r.chunk_count, expected_before, "{mib} MiB chunk count");
                assert_eq!(
                    r.chunk_size_bytes,
                    mib * MIB,
                    "{mib} MiB chunk size propagated"
                );
                assert_eq!(r.connection_count.expected_total, expected_before + 2);
                assert!(r.is_completed_and_verified(), "{mib} MiB must verify");
                eprintln!(
                    "  {mib:>2} MiB: {} chunks, Artifact {}, B-wall {:.1} ms, A-wall {:.1} ms, conns(by-construction) {}",
                    r.chunk_count,
                    r.final_artifact_status,
                    r.verified_transfer_wall_ms,
                    r.bulk_stream_wall_ms,
                    r.connection_count.expected_total,
                );
                println!("{}", r.to_ndjson_line());
                results.push(r);
            }
            Err(why) => {
                all_ok = false;
                let r = failed_result(&case, &why);
                eprintln!("  {mib:>2} MiB: FAILED — {why:?}");
                println!("{}", r.to_ndjson_line());
            }
        }
    }

    if all_ok && results.len() == 4 {
        eprintln!(
            "stage2-harness: HOST_SMOKE_PASS — 4/4 sizes, all Artifact::Verified, all chunk counts exact"
        );
        0
    } else {
        eprintln!("stage2-harness: HOST_SMOKE_FAIL");
        1
    }
}

async fn run_case_file(path: &str) -> i32 {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| die(format!("read {path}: {e}")));
    let case: Case = serde_json::from_str(&raw).unwrap_or_else(|e| die(format!("parse Case: {e}")));
    // Synthetic source, one transfer. Stage 3 substitutes the real path.
    let ws = bring_up_worker(case.chunk_size_bytes as u32).await;
    match run_one_transfer(&ws, &case, case.extent_bytes).await {
        Ok(r) => {
            println!("{}", r.to_ndjson_line());
            i32::from(!r.is_completed_and_verified())
        }
        Err(why) => {
            println!("{}", failed_result(&case, &why).to_ndjson_line());
            1
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("--smoke") => run_smoke().await,
        Some("--case-file") => {
            let p = args.get(2).unwrap_or_else(|| die("--case-file <Case.json>"));
            run_case_file(p).await
        }
        _ => {
            eprintln!(
                "usage:\n  stage2-harness --smoke\n  stage2-harness --case-file <Case.json>"
            );
            2
        }
    };
    std::process::exit(code);
}
