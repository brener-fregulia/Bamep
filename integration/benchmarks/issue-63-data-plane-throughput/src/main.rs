//! Issue #63 Spike — THROWAWAY endpoint-capture data-plane throughput benchmark.
//!
//! Phase A1/A2 approved; Case B approved; the 8/16/32/64 MiB knee sweep
//! approved. Current defensible reading of the chunk-size work (see
//! `runs/UPPER-FINDINGS.md`): throughput improves materially from the
//! small-chunk regime up toward 32 MiB; the earlier 2 GiB / n=8 knee sweep
//! initially suggested further improvement through 64 MiB, but that did NOT
//! replicate in this larger 3 GiB / n=12 within-cycle paired sweep — no size
//! above 32 MiB demonstrated a defensible throughput benefit on this noisy XFS
//! host. 32 MiB is the smallest conservative candidate with no demonstrated
//! loss above it; the exact knee is NOT statistically located. The tmpfs
//! request-overhead curve flattens early.
//!
//! This build runs **ONLY the upper chunk-size experiment**:
//! sweep `chunk_size` across **32 / 64 / 96 / 128 MiB** with everything else
//! held equal to Case A (serial Agent flow; fresh TCP + fresh pinned TLS 1.3 +
//! fresh HTTP/1.1 per data-plane request; same Ed25519 proof; same E1
//! authorize/commit path; same Worker staging / per-chunk SHA-256 / digest
//! verification / fsync(file) / no-replace linkat / fsync(chunks dir) / durable
//! commit ordering / D2 independent full-Artifact seal verification). NO
//! keep-alive, NO pipelining, NO parallel uploads, NO connection reuse, NO
//! production code change. Cases C/D/E are NOT implemented.
//!
//! `chunk_size` is the ONLY experiment variable. The question is where the
//! marginal throughput improvement stops justifying the LINEAR growth in RAM,
//! retransmission granularity, and single-chunk stall — NOT "which is fastest".
//!
//! Primary extent = 3072 MiB (divisible by 32/64/96/128 -> 96/48/32/24 whole
//! chunks; no partial final chunk in any cell; identical byte total per cell).
//! tmpfs control extent = 768 MiB (24/12/8/6 whole chunks), diagnostic only.
//!
//! Ordering: the shared dev disk shows +/-30-40% temporal variance, so sizes
//! run in >= 12 balanced (rotated) cycles; each cycle contains all four sizes
//! and no size sits in a fixed temporal slot. Analysis reports BOTH independent
//! medians AND within-cycle paired ratios (the four sizes in one cycle share a
//! closer disk-load epoch). Every raw run is printed and kept.
//!
//! NOT production code, NOT a production Agent, NOT an observability framework.
//!
//! Exact path exercised end-to-end, per chunk (unchanged from A1/A2):
//!
//! ```text
//! <chunk_size> source buffer (owned to_vec() == "materialization")
//!   -> per-chunk SHA-256                (bamep_simulator wire digest)
//!   -> rolling full-Artifact SHA-256
//!   -> fresh Ed25519 per-request proof  (AgentTransferAuthorization)
//!   -> bamep_simulator::DataPlaneClient::put_chunk
//!        -> FRESH TCP connect -> FRESH pinned TLS 1.3 -> FRESH HTTP/1.1
//!        -> one PUT .../chunks/{n}, body = <chunk_size> -> await response
//!        -> connection torn down
//!   -> real Worker DataPlane (Axum): structural parse
//!        -> E1 authorize_chunk        (UDS -> fake bamepd)
//!        -> stream body frames -> D1 StagingChunk.write (append + SHA-256)
//!        -> digest compare
//!        -> D1 finalize: flush + fsync(file) + linkat(no-replace) + fsync(dir)
//!        -> E1 commit_chunk           (UDS -> fake bamepd)
//!        -> 201 { "status": "accepted" }
//!   -> POST /seal -> Worker D2 FullArtifactHasher independent full reread
//!        -> E1 report -> fake bamepd compares digest -> Verified / Failed
//! ```
//!
//! bamepd is faked (ADR-0018). The real bamepd + PostgreSQL durable-commit cost
//! is a Phase B / physical-lab measurement, NOT modelled here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bamep_simulator::{
    pinned_tls13_client_config, AgentProofKey, AgentTransferAuthorization, DataPlaneClient,
    DataPlaneTransferDirection, PutChunkOutcome, ResumeOutcome, SealArtifactStatus, SealOutcome,
    TransferOperation,
};
use bamep_trusted_bootstrap::ServerCertFingerprint;
use bamep_worker::data_plane::DataPlane;
use bamep_worker::ipc::{worker_control, WorkerControlHandle};
use bamep_worker::storage::{
    ChunkStore, FilesystemChunkStore, FullArtifactHasher, FullArtifactRequest,
};
use bamep_worker::tls::{build_server_config, load_server_identity};
use bamep_worker_protocol::{
    ArtifactVerificationAckMessage, AuthorizationDecisionMessage, ChunkAcceptanceDecisionMessage,
    ManifestSealDecisionMessage, ResumeDiscoveryPageMessage, SealedManifestFacts,
    ServerHelloMessage, WireArtifactStatus, WireDigestAlgorithm, WorkerProtocolMessage,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};
use tokio::net::{TcpStream, UnixListener};
use tokio::sync::watch;
use uuid::Uuid;

const MIB: f64 = 1_048_576.0;
const MB: f64 = 1_000_000.0;
const KIB: u64 = 1024;

// ---- config ----------------------------------------------------------

struct BenchCfg {
    /// Equal byte extent for every cell, in MiB. Primary matrix uses 3072 so
    /// 32/64/96/128 MiB all divide it into whole chunks (96/48/32/24) with no
    /// partial final chunk. tmpfs control uses 768 (24/12/8/6).
    extent_mib: u64,
    /// Chunk sizes (MiB) to sweep; default is the upper matrix {32, 64, 96, 128}.
    chunk_mibs: Vec<u32>,
    /// Cycles over the size set. Each cycle runs every size once, in a balanced
    /// (rotated) order, so temporal disk-load/cache effects do not favour one
    /// size. Measured runs per size == cycles (one warm-up per size is excluded).
    cycles: u32,
    /// Real-disk parent for the Worker chunk store (default: $TMPDIR = tmpfs).
    /// Point at a real filesystem (xfs) for the primary result.
    storage_parent: Option<PathBuf>,
}

// ---- throwaway temp dir --------------------------------------------------

struct TempDir(PathBuf);
impl TempDir {
    fn fresh(tag: &str) -> Self {
        Self::fresh_in(&std::env::temp_dir(), tag)
    }
    fn fresh_in(parent: &Path, tag: &str) -> Self {
        let dir = parent.join(format!("bamep-issue63-{tag}-{}", Uuid::new_v4()));
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

fn sha256_wire(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

// ---- peak-RSS sampler -------------------------------------------------

fn proc_status_kib(key: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0);
        }
    }
    0
}

/// Background RSS sampler; returns (stop_flag, max_rss_kib) shared handles.
fn start_rss_sampler() -> (Arc<AtomicBool>, Arc<AtomicU64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let max = Arc::new(AtomicU64::new(0));
    let (s, m) = (stop.clone(), max.clone());
    std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            let rss = proc_status_kib("VmRSS:");
            m.fetch_max(rss, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    (stop, max)
}

// ---- fake bamepd: UDS auto-responder --------------------------------
//
// THROWAWAY. Approves every authorization, commits every chunk. At seal it
// remembers the Agent-declared digest and, when the Worker's D2 report arrives,
// returns Verified IFF the Worker's independently recomputed digest matches it
// (so the end-to-end "Verified" assertion is a real D2 check, not a rubber
// stamp). ~0 latency: NOT a bamepd model.

// ---- per-rep result ----------------------------------------------------

#[derive(Default, Clone, Copy)]
struct StageNs {
    materialize: u128,
    chunk_sha: u128,
    rolling_sha: u128,
    proof: u128,
    put_call: u128,
}

struct RepResult {
    bytes: u64,
    chunks: u64,
    wall: Duration,
    resume_call: Duration,
    seal_call: Duration,
    d2_recheck: Duration,
    stage: StageNs,
    connections: u64,
    /// Peak process RSS (KiB) observed during this single transfer. Set by the
    /// caller from the background sampler; `run_one_transfer` leaves it 0.
    peak_rss_kib: u64,
}

#[allow(clippy::too_many_arguments)]
async fn run_one_transfer(
    base_url: &str,
    fingerprint: ServerCertFingerprint,
    store: &FilesystemChunkStore,
    source_chunk: &[u8],
    chunk_size: u32,
    chunks: u64,
) -> RepResult {
    let transfer_id = Uuid::new_v4();
    let artifact_id = Uuid::new_v4();
    let proof_key = AgentProofKey::generate();
    let auth = AgentTransferAuthorization::new(
        proof_key,
        "issue63-opaque-capability-token",
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        base_url.to_string(),
    );
    let client = DataPlaneClient::connect(base_url, fingerprint)
        .expect("build DataPlaneClient")
        .with_request_timeout(Duration::from_secs(300));

    let mut stage = StageNs::default();
    let mut rolling = Sha256::new();
    let wall_start = Instant::now();

    let rproof = auth
        .create_proof_now(TransferOperation::ResumeDiscovery, None)
        .expect("resume proof");
    let t = Instant::now();
    let resume = client
        .discover_resume(auth.token(), transfer_id, &rproof)
        .await
        .expect("resume transport");
    let resume_call = t.elapsed();
    assert!(
        matches!(resume, ResumeOutcome::Approved(_)),
        "resume approved"
    );

    let mut total_bytes = 0u64;
    for index in 0..chunks {
        let t = Instant::now();
        let body: Vec<u8> = source_chunk.to_vec();
        stage.materialize += t.elapsed().as_nanos();

        let t = Instant::now();
        let chunk_digest = sha256_wire(&body);
        stage.chunk_sha += t.elapsed().as_nanos();

        let t = Instant::now();
        rolling.update(&body);
        stage.rolling_sha += t.elapsed().as_nanos();

        let t = Instant::now();
        let proof = auth
            .create_proof_now(TransferOperation::ChunkUpload, Some(index))
            .expect("chunk proof");
        stage.proof += t.elapsed().as_nanos();

        total_bytes += body.len() as u64;

        let t = Instant::now();
        let outcome = client
            .put_chunk(
                auth.token(),
                transfer_id,
                index,
                &chunk_digest,
                &proof,
                body,
            )
            .await
            .expect("put transport");
        stage.put_call += t.elapsed().as_nanos();
        match outcome {
            PutChunkOutcome::Accepted { .. } => {}
            other => panic!("chunk {index}: expected Accepted, got {other:?}"),
        }
    }

    let artifact_digest = URL_SAFE_NO_PAD.encode(rolling.finalize());
    let sproof = auth
        .create_proof_now(TransferOperation::SealManifest, None)
        .expect("seal proof");
    let t = Instant::now();
    let seal = client
        .seal(auth.token(), transfer_id, &sproof, chunks, &artifact_digest)
        .await
        .expect("seal transport");
    let seal_call = t.elapsed();
    // The transfer is complete and durably verified here — stop the wall clock
    // BEFORE the harness's own extra D2 recheck below (which would otherwise
    // double-count a full-Artifact reread into the throughput number).
    let wall = wall_start.elapsed();
    match seal {
        SealOutcome::Completed {
            artifact_status, ..
        } => {
            assert_eq!(
                artifact_status,
                SealArtifactStatus::Verified,
                "artifact must verify"
            );
        }
        other => panic!("seal: expected Completed/Verified, got {other:?}"),
    }

    // ---- independent post-seal correctness check (same public API the Worker
    //      D2 path uses): digest and exact reconstructed byte count ----
    let t = Instant::now();
    let d2 = FullArtifactHasher::for_store(store)
        .compute_blocking(FullArtifactRequest {
            transfer_id,
            chunk_count: chunks,
            chunk_size,
        })
        .await
        .expect("harness D2 recheck");
    let d2_recheck = t.elapsed();
    assert_eq!(
        d2.digest.to_base64url_no_pad(),
        artifact_digest,
        "D2 reread digest must equal the Agent rolling digest"
    );
    assert_eq!(d2.total_size, total_bytes, "exact reconstructed byte count");
    assert_eq!(d2.chunk_count, chunks, "all expected chunks held");

    // keep the store bounded across reps (throwaway; not part of any timing)
    let _ = std::fs::remove_dir_all(
        store_transfers_dir(store).join(transfer_id.as_hyphenated().to_string()),
    );

    RepResult {
        bytes: total_bytes,
        chunks,
        wall,
        resume_call,
        seal_call,
        d2_recheck,
        stage,
        connections: chunks + 2,
        peak_rss_kib: 0,
    }
}

// The store root is a private field of `FilesystemChunkStore`; `main` records
// it once (throwaway harness convenience) so per-rep cleanup can target
// `<root>/transfers/<transfer_id>` without touching production API surface.
static STORE_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn store_transfers_dir(_store: &FilesystemChunkStore) -> PathBuf {
    STORE_ROOT
        .get()
        .expect("store root recorded in main")
        .join("transfers")
}

// ---- independent ceilings + instrumented staging split ---------------

fn sha256_ceiling(buf: &[u8], iters: u32) -> f64 {
    let start = Instant::now();
    let mut sink = 0u8;
    for _ in 0..iters {
        sink ^= Sha256::digest(buf)[0];
    }
    std::hint::black_box(sink);
    (buf.len() as f64 * iters as f64) / MB / start.elapsed().as_secs_f64()
}

fn memcpy_ceiling(buf: &[u8], iters: u32) -> f64 {
    let start = Instant::now();
    let mut sink = 0u8;
    for _ in 0..iters {
        let c = buf.to_vec();
        sink ^= c[c.len() - 1];
    }
    std::hint::black_box(sink);
    (buf.len() as f64 * iters as f64) / MB / start.elapsed().as_secs_f64()
}

async fn handshake_ceiling(
    host: &str,
    port: u16,
    fingerprint: ServerCertFingerprint,
    iters: u32,
) -> Duration {
    let cfg = Arc::new(
        pinned_tls13_client_config(fingerprint, vec![b"http/1.1".to_vec()]).expect("client config"),
    );
    let sni = ServerName::try_from(host.to_string()).expect("sni");
    let start = Instant::now();
    for _ in 0..iters {
        let tcp = TcpStream::connect((host, port)).await.expect("tcp");
        let connector = tokio_rustls::TlsConnector::from(cfg.clone());
        let tls = connector.connect(sni.clone(), tcp).await.expect("tls");
        let (sender, conn): (
            hyper::client::conn::http1::SendRequest<http_body_util::Empty<hyper::body::Bytes>>,
            _,
        ) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .expect("http1 handshake");
        let task = tokio::spawn(async move {
            let _ = conn.await;
        });
        drop(sender);
        task.abort();
    }
    start.elapsed() / iters
}

#[derive(Default, Clone, Copy)]
struct StagingSplit {
    begin_ns: u128,
    write_sha_ns: u128,
    finalize_ns: u128,
    n: u64,
}
impl StagingSplit {
    fn per_chunk_ms(&self, f: fn(&Self) -> u128) -> f64 {
        f(self) as f64 / self.n as f64 / 1_000_000.0
    }
}

/// Real D1 staging via the public `ChunkStore` API, split into the three
/// separable intervals: `begin_stage`, the `write` loop (append + incremental
/// SHA-256), and `finalize` (flush + fsync(file) + linkat no-replace +
/// fsync(chunks dir) — MONOLITHIC in the public API; reported combined).
fn worker_staging_split(
    store: &FilesystemChunkStore,
    source_chunk: &[u8],
    n: u64,
    chunk_size: u32,
) -> StagingSplit {
    let transfer_id = Uuid::new_v4();
    let mut sp = StagingSplit {
        n,
        ..Default::default()
    };
    // stream the body in ~64 KiB frames, like hyper delivers it
    let frame = 64 * KIB as usize;
    for index in 0..n {
        let t = Instant::now();
        let mut staging = store
            .begin_stage(transfer_id, index, u64::from(chunk_size))
            .expect("begin_stage");
        sp.begin_ns += t.elapsed().as_nanos();

        let t = Instant::now();
        for part in source_chunk.chunks(frame) {
            staging.write(part).expect("write");
        }
        sp.write_sha_ns += t.elapsed().as_nanos();

        let t = Instant::now();
        staging.finalize().expect("finalize");
        sp.finalize_ns += t.elapsed().as_nanos();
    }
    let _ = std::fs::remove_dir_all(
        store_transfers_dir(store).join(transfer_id.as_hyphenated().to_string()),
    );
    sp
}

/// APPROXIMATE raw-syscall decomposition of finalize's internal steps, on the
/// SAME filesystem. NOT the real `StagingChunk::finalize_inner` (that is
/// private); this is a like-for-like syscall probe to answer the
/// fixed-vs-byte-proportional question. Reported as approximate.
fn raw_fsync_decomposition(dir: &Path, size: usize) -> (f64, f64, f64, f64) {
    use std::io::Write;
    let n = 12u32;
    let (mut w_flush, mut w_fsync, mut w_link, mut w_dirfsync) = (0u128, 0u128, 0u128, 0u128);
    let buf = vec![0xA5u8; size];
    for i in 0..n {
        let staging = dir.join(format!("raw-{i}.part"));
        let finalp = dir.join(format!("raw-{i}.final"));
        let t = Instant::now();
        {
            let mut f = std::fs::File::create(&staging).unwrap();
            f.write_all(&buf).unwrap();
            f.flush().unwrap();
            w_flush += t.elapsed().as_nanos();
            let t2 = Instant::now();
            f.sync_all().unwrap();
            w_fsync += t2.elapsed().as_nanos();
        }
        let t = Instant::now();
        std::fs::hard_link(&staging, &finalp).unwrap();
        w_link += t.elapsed().as_nanos();
        let t = Instant::now();
        std::fs::File::open(dir).unwrap().sync_all().unwrap();
        w_dirfsync += t.elapsed().as_nanos();
        let _ = std::fs::remove_file(&staging);
        let _ = std::fs::remove_file(&finalp);
    }
    let ms = |x: u128| x as f64 / n as f64 / 1_000_000.0;
    (ms(w_flush), ms(w_fsync), ms(w_link), ms(w_dirfsync))
}

// ---- main -------------------------------------------------------------

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let cfg = parse_args();
    let (rss_stop, rss_max) = start_rss_sampler();

    println!("=== Issue #63 Spike — CHUNK-SIZE UPPER SWEEP: 32 / 64 / 96 / 128 MiB (all else held equal) ===");
    println!(
        "host: os={} arch={} cpus={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );

    let storage_dir = match &cfg.storage_parent {
        Some(p) => TempDir::fresh_in(p, "store"),
        None => TempDir::fresh("store"),
    };
    let _ = STORE_ROOT.set(storage_dir.0.clone());
    let fs_kind = fs_kind_of(&storage_dir.0);
    // Deliberately NOT logging the absolute scratch path (host-local, exposes a
    // username + a random throwaway dir name, no evidentiary value). The
    // filesystem kind is the part that matters for the result.
    println!("storage: fs ~= {fs_kind}  (throwaway scratch dir under --storage-dir)");
    println!(
        "extent per transfer: {} MiB ; cycles: {} ; chunk sizes: {:?} MiB (1 warm-up/size excluded)\n",
        cfg.extent_mib, cfg.cycles, cfg.chunk_mibs
    );

    // ---- real Worker TLS identity ----
    let id_dir = TempDir::fresh("tls");
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(["localhost".to_string()]).expect("gen cert");
    let cert_path = id_dir.0.join("cert.pem");
    let key_path = id_dir.0.join("key.pem");
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

    // The fake bamepd must approve at whatever chunk_size the current cell uses.
    // The Worker forwards the authoritative chunk_size from AuthorizationDecision,
    // so a single long-lived fake is enough: a watch channel hands it the current
    // cell's chunk_size before each transfer.
    let sock_dir = TempDir::fresh("uds");
    let sock_path = sock_dir.0.join("worker.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind uds");
    let (cs_tx, cs_rx) = watch::channel::<u32>(cfg.chunk_mibs[0] * MIB as u32);
    tokio::spawn(fake_bamepd_dynamic(listener, cs_rx));

    let (control, driver): (WorkerControlHandle, _) = worker_control(
        sock_path,
        Duration::from_millis(20),
        Duration::from_secs(120),
        Uuid::new_v4(),
    );
    let driver = driver.with_pending_capacity(100_000);
    tokio::spawn(driver.run(std::future::pending::<()>()));

    let chunk_store = FilesystemChunkStore::initialize(&storage_dir.0).expect("init store");
    let data_plane = DataPlane::new(
        "127.0.0.1:0".parse().unwrap(),
        tls,
        control.clone(),
        chunk_store.clone(),
    );
    let handle = data_plane.handle();
    tokio::spawn(data_plane.run(std::future::pending::<()>()));
    let addr = handle.listening().await.expect("listening");
    control
        .authority()
        .wait_for(|s| s.is_available())
        .await
        .expect("control available");
    let base_url = format!("https://127.0.0.1:{}", addr.port());
    println!("worker data-plane listening on {addr}\n");

    // ---- chunk-size sweep: one equal byte extent, chunk_size the ONLY variable ----
    let extent_bytes = cfg.extent_mib * MIB as u64;
    for &chunk_mib in &cfg.chunk_mibs {
        assert_eq!(
            extent_bytes % u64::from(chunk_mib * MIB as u32),
            0,
            "extent {} MiB must be a whole multiple of chunk_size {chunk_mib} MiB",
            cfg.extent_mib
        );
    }
    let orderings = balanced_orderings(&cfg.chunk_mibs);

    // one warm-up transfer per size (page cache + code paths); excluded.
    println!("--- warm-up (excluded): one transfer per chunk size ---");
    for &chunk_mib in &cfg.chunk_mibs {
        let chunk_size = chunk_mib * MIB as u32;
        let chunks = extent_bytes / u64::from(chunk_size);
        let _ = cs_tx.send(chunk_size);
        let src = make_source(chunk_size as usize);
        let r = run_one_transfer(
            &base_url,
            fingerprint,
            &chunk_store,
            &src,
            chunk_size,
            chunks,
        )
        .await;
        println!(
            "  {chunk_mib:>2} MiB: {:>7.3} s  {:>7.2} MiB/s   (warm-up)",
            r.wall.as_secs_f64(),
            r.bytes as f64 / MIB / r.wall.as_secs_f64()
        );
    }
    println!();

    let mut all: Vec<(u32, Vec<RepResult>)> =
        cfg.chunk_mibs.iter().map(|&c| (c, Vec::new())).collect();

    for cycle in 0..cfg.cycles {
        let order = &orderings[cycle as usize % orderings.len()];
        println!(
            "--- cycle {}/{} | order {order:?} MiB | {} MiB extent | fs={fs_kind} ---",
            cycle + 1,
            cfg.cycles,
            cfg.extent_mib
        );
        for &chunk_mib in order {
            let chunk_size = chunk_mib * MIB as u32;
            let chunks = extent_bytes / u64::from(chunk_size);
            let _ = cs_tx.send(chunk_size);
            let source_chunk = make_source(chunk_size as usize);

            rss_max.store(proc_status_kib("VmRSS:"), Ordering::Relaxed);
            let mut r = run_one_transfer(
                &base_url,
                fingerprint,
                &chunk_store,
                &source_chunk,
                chunk_size,
                chunks,
            )
            .await;
            r.peak_rss_kib = rss_max.load(Ordering::Relaxed);
            drop(source_chunk);

            println!(
                "  {chunk_mib:>2} MiB ({chunks:>3} chunks): {:>7.3} s  {:>7.2} MiB/s  {:>7.2} MB/s  peakRSS {} MiB",
                r.wall.as_secs_f64(),
                r.bytes as f64 / MIB / r.wall.as_secs_f64(),
                r.bytes as f64 / MB / r.wall.as_secs_f64(),
                r.peak_rss_kib / 1024,
            );
            all.iter_mut()
                .find(|(c, _)| *c == chunk_mib)
                .unwrap()
                .1
                .push(r);
        }
        println!();
    }

    // ---- instrumented staging split + raw decomposition, per chunk size ----
    println!("--- Worker D1 staging split (real ChunkStore API) + raw fsync decomposition ---");
    for &chunk_mib in &cfg.chunk_mibs {
        let chunk_size = chunk_mib * MIB as u32;
        let src = make_source(chunk_size as usize);
        let n = (512 / chunk_mib.max(1)).clamp(4, 24) as u64; // ~512 MiB worth, bounded
        let sp = worker_staging_split(&chunk_store, &src, n, chunk_size);
        println!(
            "  {chunk_mib:>2} MiB x{n:>2}: begin={:.3} ms  write+SHA={:.3} ms  finalize(fsync+link+fsync)={:.3} ms   [per chunk]",
            sp.per_chunk_ms(|s| s.begin_ns),
            sp.per_chunk_ms(|s| s.write_sha_ns),
            sp.per_chunk_ms(|s| s.finalize_ns),
        );
        let (fl, fs, ln, df) = raw_fsync_decomposition(&storage_dir.0, chunk_size as usize);
        println!(
            "           raw approx: write+flush={fl:.3} ms  fsync(file)={fs:.3} ms  hard_link={ln:.3} ms  fsync(dir)={df:.3} ms"
        );
    }

    // ---- independent ceilings ----
    println!("\n--- Independent ceilings ---");
    let big = make_source(64 * MIB as usize);
    let sha = sha256_ceiling(&big, 40);
    let mc = memcpy_ceiling(&big, 80);
    println!(
        "  SHA-256 single pass (64 MiB buf x40):      {sha:>8.1} MB/s ({:.2} GB/s)",
        sha / 1000.0
    );
    println!(
        "  memcpy to_vec (64 MiB x80):                {mc:>8.1} MB/s ({:.2} GB/s)",
        mc / 1000.0
    );
    let hs = handshake_ceiling("127.0.0.1", addr.port(), fingerprint, 60).await;
    println!(
        "  bare TCP+TLS1.3+HTTP/1.1 handshake x60:    {:>8.3} ms / connection",
        hs.as_secs_f64() * 1000.0
    );

    // ---- analysis ----
    print_analysis(&all, &cfg, &fs_kind);
    print_paired_analysis(&all, &cfg, &fs_kind);

    rss_stop.store(true, Ordering::Relaxed);
    println!("\ndone.");

    // Return normally instead of `std::process::exit(0)`: letting `main` unwind
    // runs the `TempDir` destructors, so the Worker scratch dir, the generated
    // TLS cert/key dir and the UDS socket dir are removed on a clean run. The
    // still-pending server / driver / fake-bamepd tasks are aborted when the
    // Tokio runtime is dropped. All measurement and analysis output is already
    // flushed by this point.
}

fn make_source(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[0];
    }
    v
}

fn fs_kind_of(p: &Path) -> String {
    // best-effort: `stat -f -c %T`
    let out = std::process::Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(p)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// Long-lived fake bamepd that reads the current authoritative chunk_size from a
/// watch channel (one value per benchmark cell).
async fn fake_bamepd_dynamic(listener: UnixListener, cs_rx: watch::Receiver<u32>) {
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
                WorkerProtocolMessage::AuthorizationDecision(
                    AuthorizationDecisionMessage::approved(
                        q.envelope.message_id,
                        WireDigestAlgorithm::Sha256,
                        chunk_size,
                        "issue63-acceptance-handle",
                        None,
                    ),
                )
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
                        verification_handle: "issue63-verification-handle".to_string(),
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
        if bamep_worker_protocol::send(&mut stream, &reply)
            .await
            .is_err()
        {
            break;
        }
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// True median of a `u64` sample (KiB), returned as `f64`. Even-sized samples
/// return the mean of the two middle observations — matching `median()` for
/// f64. Input need not be sorted; a copy is sorted internally.
fn median_kib(xs: &[u64]) -> f64 {
    let mut v = xs.to_vec();
    v.sort_unstable();
    let n = v.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] as f64 + v[n / 2] as f64) / 2.0
    }
}

struct KneeRow {
    chunk_mib: u32,
    chunks: u64,
    conns: u64,
    n: usize,
    raw_secs: Vec<f64>,
    med_mib: f64,
    med_mb: f64,
    per_chunk_ms: f64,
    resume_ms: f64,
    seal_ms: f64,
    d2_ms: f64,
    put_ms: f64,
    materialize_ms: f64,
    chunk_sha_ms: f64,
    roll_sha_ms: f64,
    proof_ms: f64,
    rss_med_mib: f64,
    rss_max_mib: f64,
}

fn print_analysis(all: &[(u32, Vec<RepResult>)], cfg: &BenchCfg, fs_kind: &str) {
    println!(
        "\n============== KNEE ANALYSIS ({fs_kind}, {} MiB equal-byte extent) ==============",
        cfg.extent_mib
    );

    let mut rows: Vec<KneeRow> = Vec::new();
    for (chunk_mib, reps) in all {
        assert!(!reps.is_empty(), "no measured runs for {chunk_mib} MiB");
        let n = reps.len();
        let chunks = reps[0].chunks;
        let conns = reps[0].connections;
        let sc = n as f64;
        let stage_ms = |f: fn(&StageNs) -> u128| -> f64 {
            reps.iter().map(|r| f(&r.stage)).sum::<u128>() as f64
                / (chunks as f64 * sc)
                / 1_000_000.0
        };
        let med_wall = median(reps.iter().map(|r| r.wall.as_secs_f64()).collect());
        let resume = median(reps.iter().map(|r| r.resume_call.as_secs_f64()).collect());
        let seal = median(reps.iter().map(|r| r.seal_call.as_secs_f64()).collect());
        let d2 = median(reps.iter().map(|r| r.d2_recheck.as_secs_f64()).collect());
        let rss: Vec<u64> = reps.iter().map(|r| r.peak_rss_kib).collect();
        rows.push(KneeRow {
            chunk_mib: *chunk_mib,
            chunks,
            conns,
            n,
            raw_secs: reps
                .iter()
                .map(|r| (r.wall.as_secs_f64() * 1000.0).round() / 1000.0)
                .collect(),
            med_mib: median(
                reps.iter()
                    .map(|r| r.bytes as f64 / MIB / r.wall.as_secs_f64())
                    .collect(),
            ),
            med_mb: median(
                reps.iter()
                    .map(|r| r.bytes as f64 / MB / r.wall.as_secs_f64())
                    .collect(),
            ),
            per_chunk_ms: (med_wall - resume - seal) / chunks as f64 * 1000.0,
            resume_ms: resume * 1000.0,
            seal_ms: seal * 1000.0,
            d2_ms: d2 * 1000.0,
            put_ms: stage_ms(|s| s.put_call),
            materialize_ms: stage_ms(|s| s.materialize),
            chunk_sha_ms: stage_ms(|s| s.chunk_sha),
            roll_sha_ms: stage_ms(|s| s.rolling_sha),
            proof_ms: stage_ms(|s| s.proof),
            rss_med_mib: median_kib(&rss) / 1024.0,
            rss_max_mib: rss.iter().copied().max().unwrap() as f64 / 1024.0,
        });
    }
    rows.sort_by_key(|r| r.chunk_mib);

    for r in &rows {
        let per_gib = 1024.0 / r.chunk_mib as f64;
        println!(
            "\n  {} MiB chunks  ({} chunks/xfer, {} TCP/TLS conns/xfer, n={})",
            r.chunk_mib, r.chunks, r.conns, r.n
        );
        println!("    raw rep wall (s):         {:?}", r.raw_secs);
        println!(
            "    median throughput:        {:.2} MiB/s   {:.2} MB/s",
            r.med_mib, r.med_mb
        );
        println!(
            "    chunks/GiB = {per_gib:.1}   durability boundaries/GiB = {per_gib:.1}  (1 fsync(file) + 1 fsync(dir) per chunk)"
        );
        println!("    median per-chunk wall:    {:.2} ms", r.per_chunk_ms);
        println!(
            "    full put_chunk (Agent-observed), per chunk: {:.2} ms",
            r.put_ms
        );
        println!(
            "    Agent per-chunk: materialize {:.3}  chunkSHA {:.3}  rollSHA {:.3}  proof {:.3}  (ms)",
            r.materialize_ms, r.chunk_sha_ms, r.roll_sha_ms, r.proof_ms
        );
        println!(
            "    once/xfer: resume {:.2} ms   seal+D2 {:.2} ms   (harness independent D2 recheck {:.2} ms)",
            r.resume_ms, r.seal_ms, r.d2_ms
        );
        println!(
            "    peak process RSS: median {:.1} MiB   max {:.1} MiB",
            r.rss_med_mib, r.rss_max_mib
        );
    }

    // ---- marginal-return curve ----
    println!("\n  -- marginal-return curve (median MiB/s) --");
    if let Some(first) = rows.first() {
        for w in rows.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            println!(
                "    {:>2} -> {:>2} MiB : {:+6.1}%  (x{:.3})   |  vs {} MiB baseline: x{:.3}",
                a.chunk_mib,
                b.chunk_mib,
                (b.med_mib / a.med_mib - 1.0) * 100.0,
                b.med_mib / a.med_mib,
                first.chunk_mib,
                b.med_mib / first.med_mib,
            );
        }
    }

    // ---- memory / retry envelope (design estimates, NOT measured prod RSS) ----
    println!("\n  -- memory / retry envelope (design estimates; A = measured, B/C = envelope) --");
    println!(
        "    {:<7} {:>16} {:>18} {:>20} {:>22} {:>16}",
        "size",
        "A observed peak",
        "A harness now",
        "B ownership serial",
        "C two-buffer pipeline",
        "retry bytes"
    );
    for r in &rows {
        println!(
            "    {:<7} {:>11.1} MiB {:>13.1} MiB {:>16} MiB {:>19} MiB {:>13} MiB",
            format!("{}MiB", r.chunk_mib),
            r.rss_med_mib,
            r.rss_med_mib,
            format!("~{}", r.chunk_mib),
            format!("~{}", r.chunk_mib * 2),
            r.chunk_mib,
        );
    }
    println!("    (B ~= 1 owned chunk-sized buffer + small transport overhead;");
    println!("     C ~= 2 chunk-sized buffers + transport overhead; a lost/uncertain PUT");
    println!("     re-sends up to one whole chunk.)");

    println!("\n  Caveat: small chunks (Case B, 8-32 MiB) show a strongly fixed-per-durability-");
    println!("  boundary regime. The isolated fsync decomposition above (run after the matrix,");
    println!("  size-ordered, on a variable disk) is consistent with a byte-dependent fsync(file)");
    println!(
        "  component becoming material by the 64 MiB+ regime in this observed XFS environment"
    );
    println!(
        "  -- a mechanism hypothesis, not a located transition. The robust end-to-end finding"
    );
    println!("  is that PUT/per-chunk wall is ~byte-proportional in the upper range and larger");
    println!("  chunks do not move the throughput wall. NOT a claim that fsync latency is");
    println!("  universally fixed or byte-independent.");
    println!(
        "\n(Connection count falls only because there are fewer chunks; connection reuse would"
    );
    println!(
        "recover only the ~0.66 ms/conn handshake -- a small fraction of the per-request cost.)"
    );
}

/// Within-cycle PAIRED analysis. `all[i].1[k]` is cycle k's transfer for size
/// `all[i].0` (the sweep pushes exactly one transfer per size per cycle, in
/// cycle order), so all four sizes in cycle k share one disk-load epoch. For
/// each adjacent size pair we take the per-cycle throughput ratio
/// bigger/smaller (== wall_smaller / wall_bigger, since the byte extent is
/// identical), then summarise its centre and spread across cycles. The ratio of
/// independent overall medians is reported only as a secondary summary.
fn print_paired_analysis(all: &[(u32, Vec<RepResult>)], cfg: &BenchCfg, fs_kind: &str) {
    let mut sizes: Vec<(u32, &Vec<RepResult>)> = all.iter().map(|(c, v)| (*c, v)).collect();
    sizes.sort_by_key(|(c, _)| *c);
    let cycles = sizes.iter().map(|(_, v)| v.len()).min().unwrap_or(0);
    if cycles < 2 || sizes.len() < 2 {
        return;
    }
    println!(
        "\n============== WITHIN-CYCLE PAIRED ANALYSIS ({fs_kind}, {} MiB extent, {cycles} cycles) ==============",
        cfg.extent_mib
    );
    println!("  paired ratio = throughput(bigger) / throughput(smaller) in the SAME cycle");
    println!("  (> 1.000 => the bigger chunk was faster that cycle). Balanced order means");
    println!("  neither size in a pair sits in a fixed temporal slot.");

    let mibps = |r: &RepResult| r.bytes as f64 / MIB / r.wall.as_secs_f64();
    let pct = |x: f64| (x - 1.0) * 100.0;

    for w in sizes.windows(2) {
        let (a_mib, a_reps) = w[0];
        let (b_mib, b_reps) = w[1];
        let mut ratios: Vec<f64> = (0..cycles)
            .map(|k| mibps(&b_reps[k]) / mibps(&a_reps[k]))
            .collect();
        let raw: Vec<f64> = ratios
            .iter()
            .map(|r| (r * 1000.0).round() / 1000.0)
            .collect();

        let med = median(ratios.clone());
        ratios.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let quant = |p: f64| {
            let idx = p * (ratios.len() as f64 - 1.0);
            let (lo, hi) = (idx.floor() as usize, idx.ceil() as usize);
            ratios[lo] + (ratios[hi] - ratios[lo]) * (idx - lo as f64)
        };
        let (q1, q3) = (quant(0.25), quant(0.75));
        let mad = median(ratios.iter().map(|r| (r - med).abs()).collect());
        let favoured_bigger = ratios.iter().filter(|r| **r > 1.0).count();

        let a_om = median(a_reps.iter().take(cycles).map(mibps).collect());
        let b_om = median(b_reps.iter().take(cycles).map(mibps).collect());

        println!("\n  {a_mib} -> {b_mib} MiB");
        println!("    raw per-cycle ratios:  {raw:?}");
        println!(
            "    median paired ratio:   {med:.3}  ({:+.1}%)   [{favoured_bigger}/{cycles} cycles favoured {b_mib} MiB]",
            pct(med)
        );
        println!(
            "    spread:                IQR [{q1:.3}, {q3:.3}] width {:.3}   MAD {mad:.3}",
            q3 - q1
        );
        println!(
            "    overall-median ratio (secondary): {:.3}  ({:+.1}%)",
            b_om / a_om,
            pct(b_om / a_om)
        );
    }
    println!("\n  Read the paired ratio, not the independent medians, as the primary signal:");
    println!("  the four sizes in one cycle share a closer disk-load epoch than four");
    println!("  independent medians pooled across the whole run.");
}

/// Balanced cycle orderings: `len` patterns in which each size occupies each
/// temporal slot exactly once. For the canonical 4-size sweep this is the
/// owner's balanced set (also balances which-size-follows-which); for any other
/// size count it is the `n` plain left-rotations, which are slot-balanced over
/// `n` cycles. (Plain rotation, not rotation+reversal: reversal cancels the
/// rotation for `n == 2` and would leave both patterns identical.)
fn balanced_orderings(sizes: &[u32]) -> Vec<Vec<u32>> {
    let n = sizes.len();
    if n == 4 {
        return [[0, 1, 2, 3], [3, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]]
            .iter()
            .map(|p| p.iter().map(|&i| sizes[i]).collect())
            .collect();
    }
    (0..n.max(1))
        .map(|k| {
            let mut v = sizes.to_vec();
            if n > 0 {
                v.rotate_left(k % n);
            }
            v
        })
        .collect()
}

fn parse_args() -> BenchCfg {
    let mut cfg = BenchCfg {
        // 3072 MiB is divisible by 32/64/96/128 -> 96/48/32/24 whole chunks,
        // so no cell carries a partial final chunk. Byte total is identical
        // for every size.
        extent_mib: 3072,
        chunk_mibs: vec![32, 64, 96, 128],
        cycles: 12,
        storage_parent: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--extent-mib" => {
                cfg.extent_mib = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(cfg.extent_mib)
            }
            "--cycles" => cfg.cycles = it.next().and_then(|v| v.parse().ok()).unwrap_or(cfg.cycles),
            "--chunk-mibs" => {
                if let Some(v) = it.next() {
                    cfg.chunk_mibs = v.split(',').filter_map(|s| s.parse().ok()).collect();
                }
            }
            "--storage-dir" => cfg.storage_parent = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_kib_even_is_mean_of_two_middle() {
        // RED before the fix (buggy body returns the upper-middle, 109.0);
        // GREEN after (mean of the two middle observations).
        assert_eq!(median_kib(&[93, 93, 109, 109]), 101.0);
        assert_eq!(median_kib(&[204, 205]), 204.5);
    }

    #[test]
    fn median_kib_odd_is_middle() {
        assert_eq!(median_kib(&[30, 10, 20]), 20.0); // unsorted input
        assert_eq!(median_kib(&[7]), 7.0);
    }

    #[test]
    fn balanced_orderings_four_is_the_documented_latin_square() {
        assert_eq!(
            balanced_orderings(&[32, 64, 96, 128]),
            vec![
                vec![32, 64, 96, 128],
                vec![128, 96, 64, 32],
                vec![64, 128, 32, 96],
                vec![96, 32, 128, 64],
            ]
        );
    }
}
