//! Issue #61 CP2 — off-device endpoint-capture data-plane Spike driver.
//!
//! THROWAWAY Spike scaffolding. Proves, entirely off-device and before any
//! physical source read, that a narrow Issue #61 composition drives the
//! EXISTING real M1 data-plane vertical slice end to end:
//!
//! ```text
//! real Endpoint/Job/JobStep/Attempt (Application authority, real PostgreSQL)
//!   -> real Transfer + Artifact{Incomplete} + ChunkManifest
//!   -> real ActionDispatch / ActionAck{Accepted} over real Agent WSS
//!   -> real TransferAuthorizationRequest -> TransferAuthorizationGrant
//!   -> real sender-constrained Ed25519 per-request proof
//!   -> real GET resume discovery / PUT chunks / POST seal over real Worker HTTPS
//!   -> real Server<->Worker UDS chunk acceptance / seal / verification
//!   -> real Worker full-Artifact streaming SHA-256 reconstruction
//!   -> durable Artifact::Verified
//! ```
//!
//! The action exercised is the existing `bamep.m1.data-plane-transfer`
//! reference path (via `bamep_simulator::DataPlaneTransferAgent` and
//! `DataPlaneClient`). It is NOT `bamep.m2.endpoint-capture-transfer`; see the
//! "M2 gaps" note printed at the end.
//!
//! Synthetic deterministic source only (35 MiB, 8 MiB chunks -> 5 chunks with a
//! short 3 MiB final). No performance claim is made from it.

mod testdb;
mod vertical;

use std::path::Path;

use base64::Engine as _;
use bamep_agent_protocol::AgentProtocolMessage;
use bamep_simulator::{
    AgentTransferAuthorization, DataPlaneClient, DataPlaneTransferDirection, InMemoryTransferSource,
    PutChunkOutcome, ResumeOutcome, SealArtifactStatus, SealOutcome, TransferActionResult,
    TransferOperation, TransferRunOptions, TransferRunOutcome,
};
use sha2::{Digest, Sha256};
use testdb::TestDatabase;
use vertical::{run_transfer_streaming_progress, Vertical, CHUNK_SIZE, SOURCE_LEN};

/// Raw 32-byte SHA-256, canonical RFC 4648 base64url without padding — the
/// exact chunk/Artifact digest wire encoding
/// (`m0-data-plane-and-storage-contracts.md` "Chunk manifest").
fn sha256_wire(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

macro_rules! ev {
    ($($arg:tt)*) => { println!("CP2 | {}", format!($($arg)*)) };
}

/// A recoverable CP2 failure: printed, teardown still runs, exit code 1.
type Cp2Result<T> = Result<T, String>;

/// `BAMEP_ISSUE61_CP2_KEEP=1` leaves the disposable database and the
/// `runtime/` chunk store on disk (and prints their locations) so an external
/// `psql` / `ls` can inspect the exact durable state this run produced. The
/// default run cleans everything.
fn keep_artifacts() -> bool {
    std::env::var_os("BAMEP_ISSUE61_CP2_KEEP").is_some_and(|v| v == "1")
}

/// Per-phase facts the driver returns so `main` can point external inspection
/// at the right rows/files.
struct PhaseArtifacts {
    transfer_id: uuid::Uuid,
    artifact_id: uuid::Uuid,
    chunk_store_root: std::path::PathBuf,
}

fn need(cond: bool, msg: impl AsRef<str>) -> Cp2Result<()> {
    if cond {
        Ok(())
    } else {
        Err(msg.as_ref().to_string())
    }
}

/// Descriptive-only source provenance carried through `create_transfer_context`.
/// This is a plain immutable text field (`m0-data-plane-and-storage-contracts.md`
/// "M1 scope of `SourceProvenance`"). It is NOT a validated `SourceReference`:
/// RF-2/RF-6 Server-side freshness validation is unimplemented and NOT exercised.
fn descriptive_provenance(tag: &str) -> String {
    serde_json::json!({
        "descriptive_only": true,
        "not_a_validated_source_reference": true,
        "cp": "issue-61-cp2",
        "tag": tag,
        // #59-tuple SHAPE only, synthetic values, never validated:
        "inventory_revision_id": uuid::Uuid::new_v4().to_string(),
        "source_observation_id": "CP2synthetic0observation0id0000000000000000",
        "agent_source_id": format!("cp2-synthetic-{tag}")
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// durable-state inspection helpers
// ---------------------------------------------------------------------------

async fn manifest_row(pool: &sqlx::PgPool, artifact_id: uuid::Uuid) -> (bool, i32, Vec<u8>) {
    let row: (bool, i32, Vec<u8>) = sqlx::query_as(
        "SELECT sealed, chunk_count, artifact_digest FROM chunk_manifests WHERE artifact_id = $1",
    )
    .bind(artifact_id)
    .fetch_one(pool)
    .await
    .expect("manifest row");
    row
}

async fn chunk_counts(pool: &sqlx::PgPool, artifact_id: uuid::Uuid) -> (i64, i64) {
    let held: i64 =
        sqlx::query_scalar("SELECT count(*) FROM chunk_identities WHERE artifact_id=$1 AND held")
            .bind(artifact_id)
            .fetch_one(pool)
            .await
            .unwrap();
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM chunk_identities WHERE artifact_id=$1")
        .bind(artifact_id)
        .fetch_one(pool)
        .await
        .unwrap();
    (held, total)
}

/// Regular files directly under `<root>/transfers/<transfer_id>/chunks/`,
/// ignoring the `.staging` subdirectory.
fn finalized_chunk_files(root: &Path, transfer_id: uuid::Uuid) -> Vec<String> {
    let chunks_dir = root
        .join("transfers")
        .join(transfer_id.as_hyphenated().to_string())
        .join("chunks");
    let mut names = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&chunks_dir) {
        for entry in rd.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// PHASE 1 — happy path through the M1 reference participant
// ---------------------------------------------------------------------------

async fn phase1_happy_path(db: &TestDatabase) -> Cp2Result<PhaseArtifacts> {
    ev!("phase1 | start (DataPlaneTransferAgent reference participant)");
    let provenance = descriptive_provenance("happy");
    let v = Vertical::start_with_provenance(db, "cp2-happy", &provenance).await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    ev!("phase1 | endpoint_id={}", v.fixture.endpoint_id.0);
    ev!("phase1 | job_id={} step_id={}", v.fixture.job_id, v.fixture.step_id);
    ev!("phase1 | transfer_id={} artifact_id={}", transfer_id, artifact_id);
    ev!("phase1 | chunk_store_root={}", v.chunk_store_root().display());
    ev!(
        "phase1 | source_provenance (descriptive-only text) = {}",
        v.source_provenance().await
    );

    // real WSS session + ActionDispatch (item 4)
    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    need(
        dispatch.body.action_type == "bamep.m1.data-plane-transfer",
        format!("expected M1 action_type, got {}", dispatch.body.action_type),
    )?;
    need(
        dispatch.body.action_id == v.fixture.action_id,
        "dispatch action_id mismatch",
    )?;
    ev!(
        "phase1 | ActionDispatch received: action_type={} action_id={}",
        dispatch.body.action_type,
        dispatch.body.action_id
    );

    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let accepted = response
        .accepted
        .clone()
        .ok_or("reference participant rejected the M1 dispatch")?;
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    ev!("phase1 | ActionAck{{Accepted}} sent -> Attempt eligible for authorization");

    // real TransferAuthorizationRequest -> Grant (items 5,6)
    let (proof_key, grant) = session.obtain_grant(v.fixture.action_id, transfer_id).await;
    need(!grant.body.token.is_empty(), "empty capability token")?;
    need(
        grant.body.data_plane_base_url == v.data_plane_base_url,
        "grant base_url != real Worker HTTPS origin",
    )?;
    ev!(
        "phase1 | TransferAuthorizationGrant: token_len={} base_url={}",
        grant.body.token.len(),
        grant.body.data_plane_base_url
    );
    let auth = AgentTransferAuthorization::new(
        proof_key,
        grant.body.token.clone(),
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        grant.body.data_plane_base_url.clone(),
    );

    // deterministic synthetic source (item: SYNTHETIC SOURCE)
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 61);
    let source_digest_wire = sha256_wire(source.as_bytes());
    let chunk_count_expected = SOURCE_LEN.div_ceil(CHUNK_SIZE as usize);
    ev!(
        "phase1 | synthetic source: {} bytes, chunk_size={} -> {} chunks (final short: {} bytes)",
        SOURCE_LEN,
        CHUNK_SIZE,
        chunk_count_expected,
        SOURCE_LEN - (chunk_count_expected - 1) * CHUNK_SIZE as usize
    );
    ev!("phase1 | independent source SHA-256 (base64url) = {}", source_digest_wire);

    // real resume discovery / PUT / seal / verify (items 7-14)
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &auth,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        v.fixture.action_id,
    )
    .await;

    let verified_artifact = match &run.outcome {
        TransferRunOutcome::Completed(TransferActionResult::Verified { artifact_id }) => *artifact_id,
        other => return Err(format!("expected Completed(Verified), got {other:?}")),
    };
    need(verified_artifact == artifact_id, "verified artifact_id mismatch")?;
    ev!("phase1 | DataPlaneTransferAgent::run -> Completed(Verified {{ {verified_artifact} }})");
    ev!("phase1 | progress_observed (cumulative durably-held bytes) = {:?}", run.progress_observed);
    need(
        run.progress_observed.first() == Some(&0)
            && run.progress_observed.last() == Some(&(SOURCE_LEN as u64))
            && run.progress_observed.windows(2).all(|w| w[0] <= w[1]),
        "progress not monotonic 0..SOURCE_LEN",
    )?;

    // §22 ordering: Artifact Verified durable BEFORE workflow success
    need(
        v.artifact_state().await == "Verified"
            && v.attempt_state().await == "InProgress"
            && v.job_state().await == "Running",
        "Verified must be durable before the terminal ActionResult drives the workflow",
    )?;
    ev!("phase1 | ordering ok: Artifact=Verified, Attempt=InProgress, Job=Running (pre-ActionResult)");

    // honest terminal completion flow (item: "exercise it too")
    session
        .send(AgentProtocolMessage::ActionResult(
            TransferActionResult::Verified {
                artifact_id: verified_artifact,
            }
            .into_action_result(v.fixture.action_id),
        ))
        .await;
    session.close_and_join().await;

    // durable coherence (item 15)
    let (m_sealed, m_count, m_digest) = manifest_row(&v.pool, artifact_id).await;
    let (held, total) = chunk_counts(&v.pool, artifact_id).await;
    let held_indices = v.held_chunk_indices().await;
    let files = finalized_chunk_files(v.chunk_store_root(), transfer_id);
    ev!(
        "phase1 | durable: artifact={} attempt={} job_step={} job={}",
        v.artifact_state().await,
        v.attempt_state().await,
        v.job_step_state().await,
        v.job_state().await
    );
    ev!(
        "phase1 | manifest: sealed={} chunk_count={} held={}/{} held_indices={:?}",
        m_sealed, m_count, held, total, held_indices
    );
    ev!("phase1 | chunk-store finalized files ({}) = {:?}", files.len(), files);
    let source_digest_hex = hex(&Sha256::digest(source.as_bytes()));
    ev!("phase1 | durable artifact_digest (hex)      = {}", hex(&m_digest));
    ev!("phase1 | independent source SHA-256 (hex)   = {}", source_digest_hex);

    need(m_sealed, "manifest not sealed")?;
    need(m_count as usize == chunk_count_expected, "sealed chunk_count wrong")?;
    need(
        held == chunk_count_expected as i64 && total == chunk_count_expected as i64,
        "held/total chunk identity count wrong",
    )?;
    need(
        held_indices == (0..chunk_count_expected as i32).collect::<Vec<_>>(),
        "held chunk indices not 0..N contiguous",
    )?;
    need(files.len() == chunk_count_expected, "finalized chunk file count wrong")?;
    need(
        hex(&m_digest) == source_digest_hex,
        "durable Artifact digest != independent source SHA-256",
    )?;
    need(
        v.artifact_state().await == "Verified"
            && v.attempt_state().await == "Succeeded"
            && v.job_step_state().await == "Succeeded"
            && v.job_state().await == "Succeeded",
        "final workflow state not all-Succeeded",
    )?;
    need(v.event_count("JobSucceeded").await == 1, "expected exactly one JobSucceeded")?;
    need(v.event_count("JobFailed").await == 0, "unexpected JobFailed")?;
    need(v.terminal_audit_count().await == 1, "expected exactly one terminal audit record")?;

    ev!("phase1 | PASS — source SHA-256 == verified Artifact SHA-256, workflow Succeeded");
    let out = PhaseArtifacts {
        transfer_id,
        artifact_id,
        chunk_store_root: v.chunk_store_root().to_path_buf(),
    };
    if keep_artifacts() {
        std::mem::forget(v);
    } else {
        drop(v);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// PHASE 2 — negative (independent Worker hash) + idempotency, raw client
// ---------------------------------------------------------------------------

async fn phase2_negative_and_idempotency(db: &TestDatabase) -> Cp2Result<PhaseArtifacts> {
    ev!("phase2 | start (raw DataPlaneClient: corruption + idempotent retry)");
    let provenance = descriptive_provenance("neg");
    let v = Vertical::start_with_provenance(db, "cp2-neg", &provenance).await;
    let (transfer_id, artifact_id) = v.transfer_and_artifact_ids().await;
    ev!("phase2 | transfer_id={} artifact_id={}", transfer_id, artifact_id);

    let mut session = v.connect_agent().await;
    v.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = v.agent();
    let response = agent.accept(&dispatch);
    let _accepted = response.accepted.clone().ok_or("dispatch rejected")?;
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (proof_key, grant) = session.obtain_grant(v.fixture.action_id, transfer_id).await;
    let auth = AgentTransferAuthorization::new(
        proof_key,
        grant.body.token.clone(),
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        grant.body.data_plane_base_url.clone(),
    );
    let client = DataPlaneClient::connect(&grant.body.data_plane_base_url, v.identity.fingerprint)
        .map_err(|e| format!("DataPlaneClient::connect: {e}"))?;

    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 62);
    let bytes = source.as_bytes().to_vec();
    let chunks: Vec<Vec<u8>> = bytes.chunks(CHUNK_SIZE as usize).map(|c| c.to_vec()).collect();
    let chunk_count = chunks.len();

    let put = |idx: usize, digest_wire: String, body: Vec<u8>| {
        let client = &client;
        let auth = &auth;
        async move {
            let proof = auth
                .create_proof_now(TransferOperation::ChunkUpload, Some(idx as u64))
                .expect("proof");
            client
                .put_chunk(auth.token(), transfer_id, idx as u64, &digest_wire, &proof, body)
                .await
                .map_err(|e| format!("put_chunk[{idx}] transport: {e}"))
        }
    };

    // --- chunk 0: honest upload ------------------------------------------------
    let out0 = put(0, sha256_wire(&chunks[0]), chunks[0].clone()).await?;
    need(
        matches!(out0, PutChunkOutcome::Accepted { .. }),
        format!("chunk 0 first upload: expected Accepted, got {out0:?}"),
    )?;
    let digest0_before = v.recorded_chunk_digest(0).await.ok_or("chunk 0 not recorded")?;
    ev!("phase2 | chunk 0 uploaded -> {out0:?}; recorded digest (hex)={}", hex(&digest0_before));

    // --- item 10: idempotent retry of an already-held chunk -------------------
    let out0_again = put(0, sha256_wire(&chunks[0]), chunks[0].clone()).await?;
    need(
        matches!(out0_again, PutChunkOutcome::AlreadyHeld { .. }),
        format!("chunk 0 idempotent retry: expected AlreadyHeld, got {out0_again:?}"),
    )?;
    let digest0_after = v.recorded_chunk_digest(0).await.ok_or("chunk 0 vanished")?;
    need(digest0_before == digest0_after, "idempotent retry rewrote the recorded chunk identity")?;
    need(
        v.held_chunk_indices().await == vec![0],
        "idempotent retry changed the held set",
    )?;
    ev!("phase2 | chunk 0 idempotent retry -> {out0_again:?}; identity unchanged, held=[0]");

    // --- NEGATIVE: mutated body, honest declared digest ----------------------
    // The Worker independently hashes the received bytes and MUST reject the
    // mismatch (409 DIGEST_MISMATCH) without durably holding chunk 1.
    let mut corrupted = chunks[1].clone();
    corrupted[0] ^= 0xFF;
    let neg = put(1, sha256_wire(&chunks[1]), corrupted).await?;
    need(
        matches!(neg, PutChunkOutcome::DigestMismatch),
        format!("corrupted chunk 1: expected DigestMismatch, got {neg:?}"),
    )?;
    need(
        v.recorded_chunk_digest(1).await.is_none(),
        "rejected corrupt chunk 1 was durably recorded",
    )?;
    need(
        v.held_chunk_indices().await == vec![0],
        "rejected corrupt chunk 1 changed the held set",
    )?;
    ev!("phase2 | corrupted chunk 1 -> {neg:?}; NOT recorded, NOT held (held still [0])");

    // --- continue successfully with the correct bytes -----------------------
    for idx in 1..chunk_count {
        let out = put(idx, sha256_wire(&chunks[idx]), chunks[idx].clone()).await?;
        need(
            matches!(out, PutChunkOutcome::Accepted { .. }),
            format!("chunk {idx} honest upload: expected Accepted, got {out:?}"),
        )?;
    }
    ev!("phase2 | chunks 1..{} uploaded honestly after the rejection", chunk_count - 1);

    // --- explicit resume discovery (item 7) --------------------------------
    let resume_proof = auth
        .create_proof_now(TransferOperation::ResumeDiscovery, None)
        .expect("resume proof");
    let resume = client
        .discover_resume(auth.token(), transfer_id, &resume_proof)
        .await
        .map_err(|e| format!("discover_resume transport: {e}"))?;
    let ResumeOutcome::Approved(manifest) = resume else {
        return Err(format!("resume discovery not Approved: {resume:?}"));
    };
    need(
        manifest.held_chunks.len() == chunk_count && !manifest.sealed,
        "resume discovery held set wrong / already sealed",
    )?;
    ev!(
        "phase2 | resume discovery: sealed={} chunk_size={} held={} of {}",
        manifest.sealed,
        manifest.chunk_size,
        manifest.held_chunks.len(),
        chunk_count
    );

    // --- seal + full-Artifact verification --------------------------------
    let seal_proof = auth
        .create_proof_now(TransferOperation::SealManifest, None)
        .expect("seal proof");
    let sealed = client
        .seal(
            auth.token(),
            transfer_id,
            &seal_proof,
            chunk_count as u64,
            &sha256_wire(&bytes),
        )
        .await
        .map_err(|e| format!("seal transport: {e}"))?;
    let SealOutcome::Completed {
        artifact_status, ..
    } = sealed
    else {
        return Err(format!("seal not Completed: {sealed:?}"));
    };
    need(
        artifact_status == SealArtifactStatus::Verified,
        format!("seal artifact_status expected Verified, got {artifact_status:?}"),
    )?;
    ev!("phase2 | seal -> Completed(Verified)");

    // terminal ActionResult over the same real WSS session
    session
        .send(AgentProtocolMessage::ActionResult(
            TransferActionResult::Verified { artifact_id }.into_action_result(v.fixture.action_id),
        ))
        .await;
    session.close_and_join().await;

    // durable coherence for phase 2
    let (m_sealed, m_count, m_digest) = manifest_row(&v.pool, artifact_id).await;
    let (held, total) = chunk_counts(&v.pool, artifact_id).await;
    ev!(
        "phase2 | durable: artifact={} attempt={} job={} | manifest sealed={} count={} held={}/{}",
        v.artifact_state().await,
        v.attempt_state().await,
        v.job_state().await,
        m_sealed,
        m_count,
        held,
        total
    );
    need(
        m_sealed
            && m_count as usize == chunk_count
            && held == chunk_count as i64
            && total == chunk_count as i64,
        "phase2 durable manifest/chunk counts wrong",
    )?;
    need(
        hex(&m_digest) == hex(&Sha256::digest(&bytes)),
        "phase2 durable Artifact digest != independent source SHA-256",
    )?;
    need(
        v.artifact_state().await == "Verified" && v.job_state().await == "Succeeded",
        "phase2 final state not Verified/Succeeded",
    )?;
    ev!("phase2 | PASS — corruption rejected + not held, idempotent retry stable, transfer completes Verified");
    let out = PhaseArtifacts {
        transfer_id,
        artifact_id,
        chunk_store_root: v.chunk_store_root().to_path_buf(),
    };
    if keep_artifacts() {
        std::mem::forget(v);
    } else {
        drop(v);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------

async fn run_cp2(db: &TestDatabase) -> Cp2Result<(PhaseArtifacts, PhaseArtifacts)> {
    ev!("db.created name={}", db.name());
    ev!("runtime_root={}", vertical::runtime_root().display());
    let p1 = phase1_happy_path(db).await?;
    let p2 = phase2_negative_and_idempotency(db).await?;
    Ok((p1, p2))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    ev!("Issue #61 CP2 — off-device M1 data-plane vertical (bamep.m1.data-plane-transfer)");
    let keep = keep_artifacts();
    let db = TestDatabase::setup().await;
    let name = db.name().to_string();
    let result = run_cp2(&db).await;

    if keep {
        ev!("KEEP mode: leaving disposable database + runtime chunk store for inspection");
        ev!("keep.db_name={name}");
        if let Ok((p1, p2)) = &result {
            ev!(
                "keep.phase1 transfer_id={} artifact_id={} chunk_store={}",
                p1.transfer_id, p1.artifact_id, p1.chunk_store_root.display()
            );
            ev!(
                "keep.phase2 transfer_id={} artifact_id={} chunk_store={}",
                p2.transfer_id, p2.artifact_id, p2.chunk_store_root.display()
            );
        }
        // deliberately no teardown / no runtime cleanup
    } else {
        db.teardown().await;
        ev!("db.dropped name={name}");
        // TempDir Drop already removed each per-vertical dir; clear the parent.
        let _ = std::fs::remove_dir(vertical::runtime_root());
    }

    match result.map(|_| ()) {
        Ok(()) => {
            ev!("---- M2 GAPS NOT PROVEN BY CP2 ----");
            for line in [
                "bamep.m2.endpoint-capture-transfer action (type/params/rejections)",
                "Server-side SourceReference freshness validation (RF-2/RF-6)",
                "Agent-side SOURCE_REFERENCE_STALE",
                "RF-6 atomic M2 target creation",
                "structured SourceProvenance authority (this run used descriptive text only)",
                "physical source reading",
                "WinPE compatibility of the async M1 transfer participant",
            ] {
                ev!("  NOT PROVEN: {line}");
            }
            ev!("RESULT: PASS");
        }
        Err(e) => {
            eprintln!("CP2 | RESULT: FAIL — {e}");
            std::process::exit(1);
        }
    }
}
