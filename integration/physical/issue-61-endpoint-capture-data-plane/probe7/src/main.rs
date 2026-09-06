//! Issue #61 CP7A — WinPE-native bounded-prefix physical data-plane pressure probe.
//!
//! THROWAWAY Spike. NOT the Bamep Agent, NOT `crates/agent`, NOT a production
//! Agent architecture, NOT the `bamepd` composition root. One process, one WinPE
//! session, fresh coherent lineage:
//!
//!   enumerate -> mint one fresh source epoch -> operator-local SSD selection
//!   -> lab-only coord line + Server-UTC ACK -> ASYMMETRIC clock pre-flight gate
//!   -> pinned TLS 1.3 / WSS -> real Agent Protocol v1 authentication
//!   -> InventoryReport carrying that exact epoch
//!   -> (harness persists the InventoryRevision, then dispatches the M1 action)
//!   -> ActionDispatch (bamep.m1.data-plane-transfer) -> ActionAck{Accepted}
//!   -> TransferAuthorizationRequest -> TransferAuthorizationGrant
//!   -> resolver: (obs_id, agent_source_id) -> local SSD locator
//!   -> CreateFileW(GENERIC_READ) -> SINGLE-PASS streaming read of the bounded
//!      prefix (2,148,532,224 bytes = 257 chunks; final chunk = 1 MiB)
//!        * each logical chunk is read AT MOST ONCE
//!        * each logical chunk enters the rolling full-Artifact SHA-256 EXACTLY ONCE
//!        * retries reuse the SAME buffered bytes / digest
//!   -> real sender-constrained Worker HTTPS PUTs
//!   -> ONE controlled same-process Worker-listener interruption (harness-driven)
//!   -> fresh authorization + resume discovery + reconcile-from-memory + continue
//!   -> POST /seal (chunk_count = 257, rolling digest)
//!   -> Worker full-Artifact reconstruction -> durable Artifact::Verified
//!   -> ActionResult{Succeeded, TRANSFER_VERIFIED}
//!   -> STOP.
//!
//! CP7A is explicitly a BOUNDED-PREFIX M1 pressure Artifact. It is NOT a complete
//! `\\.\PhysicalDrive0` capture, NOT walkthrough D, NOT Outcome A, and NOT the
//! production `bamep.m2.endpoint-capture-transfer` action. It reuses the EXISTING
//! M1 reference components from `bamep-simulator`; it implements no second
//! transfer protocol and no RF-2 / RF-6 / RF-7 production source-authority logic.

mod resolver;
mod sources;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bamep_agent_protocol::{
    decode, encode, ActionAckMessage, ActionResultMessage, ActionResultOutcome,
    AgentProtocolMessage, ProtocolId, TransferAuthorizationRequestMessage,
};
use bamep_simulator::{
    authenticate, connect_pinned_wss, send_inventory_report, AgentProofKey,
    AgentTransferAuthorization, DataPlaneClient, DataPlaneTransferDirection, PutChunkOutcome,
    ResumeOutcome, SealArtifactStatus, SealOutcome, ServerCertFingerprint, SimulatorHandshakeOutcome,
    TransferOperation,
};
use futures_util::{SinkExt, StreamExt};
use resolver::{CurrentEpoch, EpochEntry};
use sha2::{Digest, Sha256};
use sources::Counters;
use tokio_tungstenite::tungstenite::Message;

mod stream;
use stream::{
    run_stream_pass, ChunkReader, DataPlane, PassOutcome, ProgressTick, PutStatus, ResumeStatus,
    StreamError, StreamState,
};

const PROBE_NAME: &str = env!("CARGO_PKG_NAME");
const PROBE_VERSION: &str = env!("CARGO_PKG_VERSION");
const NET_TIMEOUT: Duration = Duration::from_secs(8);
const DISPATCH_WAIT: Duration = Duration::from_secs(120);

/// CP7A bounded extent: 2 GiB + 1 MiB. Verified: 256 * 8,388,608 + 1,048,576.
const PREFIX_BYTES_DEFAULT: u64 = 2_148_532_224;
const CHUNK_SIZE_DEFAULT: u64 = 8 * 1024 * 1024;
/// Asymmetric clock pre-flight gate (agent_now - server_utc), conservative
/// against the Server proof window [-30_000, +120_000] (future skew wall / past
/// window). See the CP7 design.
const SKEW_FLOOR_MS_DEFAULT: i64 = -60_000;
const SKEW_CEIL_MS_DEFAULT: i64 = 10_000;
/// Conservative bounded Spike seal timeout for the ~2.15 GB reconstruction.
const SEAL_TIMEOUT_SECS_DEFAULT: u64 = 120;
/// Max outer suspensions (fresh-grant re-acquisitions) tolerated in one pass.
const MAX_OUTER_SUSPENSIONS: u32 = 12;

mod base64_ct {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    pub fn b64url_nopad(input: &[u8]) -> String {
        let mut o = String::with_capacity(input.len().div_ceil(3) * 4);
        for c in input.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
            o.push(A[((n >> 18) & 63) as usize] as char);
            o.push(A[((n >> 12) & 63) as usize] as char);
            if c.len() > 1 {
                o.push(A[((n >> 6) & 63) as usize] as char);
            }
            if c.len() > 2 {
                o.push(A[(n & 63) as usize] as char);
            }
        }
        o
    }
}
use base64_ct::b64url_nopad;

pub(crate) fn sha256_wire(bytes: &[u8]) -> String {
    b64url_nopad(&Sha256::digest(bytes))
}

enum V {
    S(String),
    U(u64),
    I(i64),
    B(bool),
}
fn s(v: impl Into<String>) -> V {
    V::S(v.into())
}
fn esc(i: &str) -> String {
    let mut o = String::with_capacity(i.len() + 2);
    for c in i.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

struct Log {
    started: Instant,
    seq: Mutex<u64>,
    buf: Mutex<Vec<String>>,
}
impl Log {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            seq: Mutex::new(0),
            buf: Mutex::new(Vec::new()),
        }
    }
    fn emit(&self, level: &str, event: &str, fields: &[(&str, V)]) {
        let seq = {
            let mut g = self.seq.lock().unwrap();
            *g += 1;
            *g
        };
        let mut l = String::new();
        l.push('{');
        l.push_str(&format!(
            r#""ts_ms":{},"seq":{seq},"elapsed_ms":{}"#,
            now_ms(),
            self.started.elapsed().as_millis()
        ));
        l.push_str(&format!(
            r#","level":"{}","event":"{}","probe":"{}""#,
            esc(level),
            esc(event),
            esc(PROBE_NAME)
        ));
        for (k, v) in fields {
            match v {
                V::S(x) => l.push_str(&format!(r#","{}":"{}""#, esc(k), esc(x))),
                V::U(x) => l.push_str(&format!(r#","{}":{}"#, esc(k), x)),
                V::I(x) => l.push_str(&format!(r#","{}":{}"#, esc(k), x)),
                V::B(x) => l.push_str(&format!(r#","{}":{}"#, esc(k), x)),
            }
        }
        l.push('}');
        eprintln!("{l}");
        let _ = std::io::stderr().flush();
        self.buf.lock().unwrap().push(l);
    }
    fn snapshot(&self) -> String {
        self.buf.lock().unwrap().join("\n")
    }
}

fn write_local(log: &Log) {
    let dir = std::env::var("TEMP")
        .or_else(|_| std::env::var("TMP"))
        .unwrap_or_else(|_| ".".into());
    for p in [
        format!("{dir}\\bamep-issue61-cp7a-probe.ndjson"),
        "bamep-issue61-cp7a-probe.ndjson".into(),
    ] {
        if std::fs::write(&p, format!("{}\n", log.snapshot())).is_ok() {
            log.emit("info", "sink.file.ok", &[("path", s(p))]);
            return;
        }
    }
}
fn flush_sink(log: &Log, sink: &str) {
    let Some(addr) = sink.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return;
    };
    if let Ok(mut st) = TcpStream::connect_timeout(&addr, NET_TIMEOUT) {
        let _ = st.set_write_timeout(Some(NET_TIMEOUT));
        let _ = st.write_all(format!("{}\n", log.snapshot()).as_bytes());
        let _ = st.flush();
        let _ = st.set_read_timeout(Some(Duration::from_millis(400)));
        let _ = st.read(&mut [0u8; 32]);
    }
}

struct Args {
    sink: String,
    coord: String,
    wss: String,
    pin_hex: String,
    credential_file: String,
    select_model_substr: String,
    chunk_size: u64,
    prefix_bytes: u64,
    seal_timeout_secs: u64,
    skew_floor_ms: i64,
    skew_ceil_ms: i64,
}
fn parse_args() -> Args {
    let mut a = Args {
        sink: "192.168.99.1:9099".into(),
        coord: "192.168.99.1:9106".into(),
        wss: "192.168.99.1:8443".into(),
        pin_hex: String::new(),
        credential_file: String::new(),
        select_model_substr: "256GB".into(),
        chunk_size: CHUNK_SIZE_DEFAULT,
        prefix_bytes: PREFIX_BYTES_DEFAULT,
        seal_timeout_secs: SEAL_TIMEOUT_SECS_DEFAULT,
        skew_floor_ms: SKEW_FLOOR_MS_DEFAULT,
        skew_ceil_ms: SKEW_CEIL_MS_DEFAULT,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--sink" => a.sink = it.next().unwrap_or(a.sink),
            "--coord" => a.coord = it.next().unwrap_or(a.coord),
            "--wss" => a.wss = it.next().unwrap_or(a.wss),
            "--pin" => a.pin_hex = it.next().unwrap_or_default(),
            "--auth-credential-file" => a.credential_file = it.next().unwrap_or_default(),
            "--select-model-substr" => {
                a.select_model_substr = it.next().unwrap_or(a.select_model_substr)
            }
            "--chunk-size" => {
                a.chunk_size = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.chunk_size)
            }
            "--prefix-bytes" => {
                a.prefix_bytes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(a.prefix_bytes)
            }
            "--seal-timeout-secs" => {
                a.seal_timeout_secs = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(a.seal_timeout_secs)
            }
            "--skew-floor-ms" => {
                a.skew_floor_ms = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(a.skew_floor_ms)
            }
            "--skew-ceil-ms" => {
                a.skew_ceil_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.skew_ceil_ms)
            }
            _ => {}
        }
    }
    a
}

fn parse_pin(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// =====================================================================
// Real device chunk reader (Windows: GENERIC_READ; host: deterministic stub)
// =====================================================================

struct DeviceReader {
    src: sources::RawReadSource,
    counters: std::cell::RefCell<Counters>,
}
impl ChunkReader for DeviceReader {
    fn read_chunk(&self, _index: u64, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        // CP7A is bounded (~257 x 8 MiB reads, seconds): a direct blocking read
        // on a multi-thread runtime worker is acceptable. A CP7B-scale full-
        // device pass (hours) would need a Send-safe device-reader thread — the
        // Windows raw HANDLE is not `Send`, so `spawn_blocking` is not free here.
        let mut c = self.counters.borrow_mut();
        self.src.read_bytes_at(offset, len, &mut c)
    }
}

// =====================================================================
// Real Worker-HTTPS data plane behind the stream::DataPlane trait
// =====================================================================

struct RealDataPlane {
    client: DataPlaneClient,
    auth: AgentTransferAuthorization,
    transfer_uuid: uuid::Uuid,
    chunk_size: u64,
}
impl DataPlane for RealDataPlane {
    async fn discover_resume(&mut self) -> ResumeStatus {
        let proof = match self
            .auth
            .create_proof_now(TransferOperation::ResumeDiscovery, None)
        {
            Ok(p) => p,
            Err(e) => return ResumeStatus::Fatal(format!("resume proof: {e}")),
        };
        match self
            .client
            .discover_resume(self.auth.token(), self.transfer_uuid, &proof)
            .await
        {
            Ok(ResumeOutcome::Approved(m)) => {
                if m.chunk_size as u64 != self.chunk_size {
                    return ResumeStatus::Fatal(format!(
                        "manifest chunk_size {} != expected {}",
                        m.chunk_size, self.chunk_size
                    ));
                }
                if m.sealed {
                    return ResumeStatus::Fatal("manifest already sealed mid-pass".into());
                }
                ResumeStatus::Ok(
                    m.held_chunks
                        .into_iter()
                        .map(|h| (h.chunk_index, h.digest))
                        .collect(),
                )
            }
            Ok(ResumeOutcome::AuthorizationDenied) => ResumeStatus::AuthDenied,
            Ok(ResumeOutcome::Malformed) => ResumeStatus::Fatal("resume malformed".into()),
            Ok(ResumeOutcome::Unexpected { status }) => {
                ResumeStatus::Fatal(format!("resume unexpected status {status}"))
            }
            Err(e) => ResumeStatus::Transient(format!("{e}")),
        }
    }

    async fn put_chunk(&mut self, index: u64, digest_wire: &str, bytes: &[u8]) -> PutStatus {
        let proof = match self
            .auth
            .create_proof_now(TransferOperation::ChunkUpload, Some(index))
        {
            Ok(p) => p,
            Err(e) => return PutStatus::Fatal(format!("chunk proof: {e}")),
        };
        match self
            .client
            .put_chunk(
                self.auth.token(),
                self.transfer_uuid,
                index,
                digest_wire,
                &proof,
                bytes.to_vec(),
            )
            .await
        {
            Ok(PutChunkOutcome::Accepted { .. }) => PutStatus::Accepted,
            Ok(PutChunkOutcome::AlreadyHeld { .. }) => PutStatus::AlreadyHeld,
            Ok(PutChunkOutcome::DigestMismatch) => PutStatus::DigestMismatch,
            Ok(PutChunkOutcome::ChunkIdentityConflict) => PutStatus::IdentityConflict,
            Ok(PutChunkOutcome::TransferNotContinuable) => PutStatus::NotContinuable,
            Ok(PutChunkOutcome::ChunkTooLarge) => PutStatus::Fatal("413 CHUNK_TOO_LARGE".into()),
            Ok(PutChunkOutcome::AuthorizationDenied) => PutStatus::AuthDenied,
            Ok(PutChunkOutcome::Malformed) => PutStatus::Fatal("400 MALFORMED_REQUEST".into()),
            Ok(PutChunkOutcome::Unexpected { status }) => {
                PutStatus::Fatal(format!("unexpected PUT status {status}"))
            }
            Err(e) => PutStatus::Transient(format!("{e}")),
        }
    }
}

// =====================================================================
// WSS helpers
// =====================================================================

/// Sends one `TransferAuthorizationRequest` for a freshly generated ephemeral
/// key and returns the matching grant material. Used for the initial grant and
/// for every post-interruption fresh grant.
async fn obtain_grant<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    log: &Log,
    action_id: ProtocolId,
    transfer_uuid: uuid::Uuid,
) -> Result<(AgentProofKey, String, String), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let proof_key = AgentProofKey::generate();
    let transfer_pid = ProtocolId::from_uuid(transfer_uuid)
        .map_err(|e| format!("bad transfer uuid: {e}"))?;
    let req = TransferAuthorizationRequestMessage::new(
        action_id,
        transfer_pid,
        proof_key.public_key_wire(),
    );
    ws.send(Message::text(
        encode(&AgentProtocolMessage::TransferAuthorizationRequest(req)).unwrap(),
    ))
    .await
    .map_err(|e| format!("send auth request: {e}"))?;
    log.emit("info", "cp7a.transfer_auth.request_sent", &[]);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() >= deadline {
            return Err("timed out waiting for TransferAuthorizationGrant".into());
        }
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => match decode(&t) {
                Ok(AgentProtocolMessage::TransferAuthorizationGrant(g)) => {
                    return Ok((proof_key, g.body.token.clone(), g.body.data_plane_base_url.clone()));
                }
                Ok(AgentProtocolMessage::TransferAuthorizationDenied(_)) => {
                    return Err("TransferAuthorizationDenied".into());
                }
                Ok(other) => log.emit(
                    "info",
                    "cp7a.wss.frame",
                    &[("kind", s(format!("{other:?}").split_whitespace().next().unwrap_or("?")))],
                ),
                Err(_) => {}
            },
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(format!("wss recv error: {e}")),
            Ok(None) => return Err("wss closed".into()),
            Err(_) => {}
        }
    }
}

/// CP7A exit codes.
mod exit {
    pub const PASS: i32 = 0;
    pub const ENUMERATION: i32 = 61;
    pub const COORD: i32 = 62;
    pub const WSS_AUTH: i32 = 63;
    pub const NO_DISPATCH: i32 = 64;
    pub const BAD_DISPATCH: i32 = 65;
    pub const NO_GRANT: i32 = 66;
    pub const RESOLVER: i32 = 67;
    pub const DEVICE: i32 = 68;
    pub const CLOCK_PREFLIGHT: i32 = 69;
    pub const STREAM_FATAL: i32 = 70;
    pub const SEAL_ARTIFACT_FAILED: i32 = 71;
    pub const SEAL_VERDICT_UNOBSERVABLE: i32 = 72; // N1
    pub const SEAL_ABANDONED: i32 = 73;
}

async fn run(log: &Log, args: &Args, counters: &mut Counters) -> i32 {
    // ---- 1. fresh source epoch -----------------------------------------
    let epoch_src = sources::enumerate();
    let obs_id = epoch_src.observation_id.clone();
    if obs_id.len() != 43 || epoch_src.sources.len() != 2 {
        log.emit(
            "error",
            "cp7a.epoch.bad",
            &[
                ("observation_id_len", V::U(obs_id.len() as u64)),
                ("source_count", V::U(epoch_src.sources.len() as u64)),
            ],
        );
        return exit::ENUMERATION;
    }
    log.emit(
        "info",
        "cp7a.epoch",
        &[
            ("authority.source_observation_id", s(&obs_id)),
            ("source_count", V::U(2)),
        ],
    );
    for (i, src) in epoch_src.sources.iter().enumerate() {
        log.emit(
            "info",
            "cp7a.epoch.source",
            &[
                ("index", V::U(i as u64)),
                ("authority.agent_source_id", s(&src.agent_source_id)),
                ("evidence_only.local_locator", s(&src.local_locator)),
                ("evidence_only.model", s(&src.product)),
                ("evidence_only.serial", s(&src.serial)),
                ("evidence_only.bus_type", s(&src.bus_type)),
            ],
        );
    }
    let epoch = CurrentEpoch::new(
        obs_id.clone(),
        epoch_src
            .sources
            .iter()
            .map(|s| EpochEntry {
                agent_source_id: s.agent_source_id.clone(),
                local_locator: s.local_locator.clone(),
            })
            .collect(),
    );
    // The resolver's epoch identity is exactly the minted observation id.
    if epoch.observation_id() != obs_id {
        log.emit("error", "cp7a.epoch.identity_mismatch", &[]);
        return exit::ENUMERATION;
    }
    if epoch.has_duplicate_agent_source_ids() {
        log.emit("error", "cp7a.epoch.ambiguous", &[]);
        return exit::ENUMERATION;
    }
    let matched: Vec<&sources::LocalSource> = epoch_src
        .sources
        .iter()
        .filter(|x| x.product.contains(&args.select_model_substr))
        .collect();
    if matched.len() != 1 {
        log.emit(
            "error",
            "cp7a.operator_selection.ambiguous",
            &[("match_count", V::U(matched.len() as u64))],
        );
        return exit::ENUMERATION;
    }
    let sel_asid = matched[0].agent_source_id.clone();
    let sel_locator = matched[0].local_locator.clone();
    log.emit(
        "info",
        "cp7a.operator_selection",
        &[
            ("basis", s("operator predicate over LOCAL HARDWARE EVIDENCE (model substring) — NOT cross-boundary authority")),
            ("matched.evidence_only.model", s(&matched[0].product)),
            ("matched.evidence_only.local_locator", s(&sel_locator)),
            ("resulting.authority.agent_source_id", s(&sel_asid)),
        ],
    );

    // ---- 2. lab-only coordination + Server-UTC ACK -------------------
    let coord_line = format!(
        r#"{{"cp7_coord":"source_selection","source_observation_id":"{}","selected_agent_source_id":"{}"}}"#,
        esc(&obs_id),
        esc(&sel_asid)
    );
    let server_utc_ms: i64 = match coord_roundtrip(&args.coord, &coord_line) {
        Ok(v) => v,
        Err(e) => {
            log.emit("error", "cp7a.coord.failed", &[("error", s(e))]);
            return exit::COORD;
        }
    };
    let agent_now_ms = now_ms() as i64;
    let skew_ms = agent_now_ms - server_utc_ms;
    log.emit(
        "info",
        "cp7a.coord.ok",
        &[
            ("source_observation_id", s(&obs_id)),
            ("selected_agent_source_id", s(&sel_asid)),
            ("server_utc_ms", V::I(server_utc_ms)),
            ("agent_now_ms", V::I(agent_now_ms)),
            ("skew_ms", V::I(skew_ms)),
        ],
    );

    // ---- 3. ASYMMETRIC clock pre-flight gate (before any device access) -
    if skew_ms < args.skew_floor_ms || skew_ms > args.skew_ceil_ms {
        log.emit(
            "error",
            "cp7a.clock.preflight_failed",
            &[
                ("skew_ms", V::I(skew_ms)),
                ("gate_floor_ms", V::I(args.skew_floor_ms)),
                ("gate_ceil_ms", V::I(args.skew_ceil_ms)),
                ("note", s("agent clock is outside the safe asymmetric window; realign the lab WinPE clock and re-run. Proof freshness is NOT widened.")),
            ],
        );
        return exit::CLOCK_PREFLIGHT;
    }
    log.emit(
        "info",
        "cp7a.clock.preflight_passed",
        &[("skew_ms", V::I(skew_ms))],
    );

    // ---- 4. pinned WSS + Agent auth --------------------------------
    let Some(pin) = parse_pin(&args.pin_hex) else {
        log.emit("error", "cp7a.pin.bad", &[]);
        return exit::WSS_AUTH;
    };
    let fingerprint = ServerCertFingerprint::from_sha256_digest(pin);
    let wss_addr: SocketAddr = match args.wss.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => {
            log.emit("error", "cp7a.wss.bad_addr", &[]);
            return exit::WSS_AUTH;
        }
    };
    let mut ws = match connect_pinned_wss(wss_addr, "bamep-agent", fingerprint).await {
        Ok(w) => w,
        Err(e) => {
            log.emit("error", "cp7a.wss.failed", &[("error", s(format!("{e}")))]);
            return exit::WSS_AUTH;
        }
    };
    log.emit("info", "cp7a.wss.established", &[("addr", s(args.wss.clone()))]);

    let credential = match std::fs::read_to_string(&args.credential_file) {
        Ok(c) => c.trim().to_string(),
        Err(e) => {
            log.emit(
                "error",
                "cp7a.credential.unreadable",
                &[("error", s(e.to_string()))],
            );
            return exit::WSS_AUTH;
        }
    };
    let session_id = match authenticate(&mut ws, &credential).await {
        Ok(SimulatorHandshakeOutcome::Established(m)) => {
            let sid = format!("{:?}", m.body.session_id);
            log.emit(
                "info",
                "cp7a.auth.session_established",
                &[("session_id", s(&sid))],
            );
            sid
        }
        Ok(SimulatorHandshakeOutcome::Rejected(_)) => {
            log.emit("error", "cp7a.auth.rejected", &[]);
            return exit::WSS_AUTH;
        }
        Err(e) => {
            log.emit("error", "cp7a.auth.error", &[("error", s(format!("{e}")))]);
            return exit::WSS_AUTH;
        }
    };

    // ---- 5. InventoryReport carrying this exact fresh epoch --------
    let mut inv = serde_json::Map::new();
    inv.insert("probe".into(), serde_json::json!(PROBE_NAME));
    inv.insert("probe_version".into(), serde_json::json!(PROBE_VERSION));
    inv.insert(
        "host".into(),
        serde_json::json!({
            "os": std::env::var("OS").unwrap_or_default(),
            "computername": std::env::var("COMPUTERNAME").unwrap_or_default(),
        }),
    );
    inv.insert(
        "capture_source_observation_id".into(),
        serde_json::json!(obs_id),
    );
    inv.insert(
        "capturable_sources".into(),
        serde_json::json!(epoch_src
            .sources
            .iter()
            .map(|x| serde_json::json!({ "agent_source_id": x.agent_source_id }))
            .collect::<Vec<_>>()),
    );
    if let Err(e) = send_inventory_report(&mut ws, inv).await {
        log.emit(
            "error",
            "cp7a.inventory.send_failed",
            &[("error", s(format!("{e}")))],
        );
        return exit::WSS_AUTH;
    }
    log.emit(
        "info",
        "cp7a.inventory.report_sent",
        &[("capture_source_observation_id", s(&obs_id))],
    );

    // ---- 6. wait for the harness to dispatch the M1 action --------
    let dispatch = {
        let deadline = Instant::now() + DISPATCH_WAIT;
        loop {
            if Instant::now() >= deadline {
                log.emit("error", "cp7a.dispatch.timeout", &[]);
                return exit::NO_DISPATCH;
            }
            let frame = match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
                Ok(Some(Ok(f))) => f,
                Ok(Some(Err(e))) => {
                    log.emit("error", "cp7a.wss.recv_error", &[("error", s(e.to_string()))]);
                    return exit::NO_DISPATCH;
                }
                Ok(None) => {
                    log.emit("error", "cp7a.wss.closed", &[]);
                    return exit::NO_DISPATCH;
                }
                Err(_) => continue,
            };
            let Message::Text(text) = frame else { continue };
            match decode(&text) {
                Ok(AgentProtocolMessage::ActionDispatch(d)) => break d,
                Ok(AgentProtocolMessage::ProtocolError(_)) => log.emit(
                    "info",
                    "cp7a.wss.barrier_protocol_error",
                    &[("note", s("expected inventory barrier — tolerated"))],
                ),
                Ok(other) => log.emit(
                    "info",
                    "cp7a.wss.frame",
                    &[("kind", s(format!("{other:?}").split_whitespace().next().unwrap_or("?")))],
                ),
                Err(e) => log.emit("warn", "cp7a.wss.decode_failed", &[("error", s(e.to_string()))]),
            }
        }
    };

    let action_id = dispatch.body.action_id;
    let action_type = dispatch.body.action_type.clone();
    if action_type != "bamep.m1.data-plane-transfer" {
        log.emit(
            "error",
            "cp7a.dispatch.wrong_action",
            &[("action_type", s(&action_type))],
        );
        return exit::NO_DISPATCH;
    }
    let p = &dispatch.body.parameters;
    let transfer_id_s = p
        .get("transfer_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let artifact_id_s = p
        .get("artifact_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let dispatch_chunk_size = p.get("chunk_size").and_then(|v| v.as_u64()).unwrap_or(0);
    let (Ok(transfer_uuid), Ok(artifact_uuid)) = (
        transfer_id_s.parse::<uuid::Uuid>(),
        artifact_id_s.parse::<uuid::Uuid>(),
    ) else {
        log.emit(
            "error",
            "cp7a.dispatch.bad_ids",
            &[("transfer_id", s(&transfer_id_s)), ("artifact_id", s(&artifact_id_s))],
        );
        return exit::BAD_DISPATCH;
    };
    let chunk_size = if dispatch_chunk_size > 0 {
        dispatch_chunk_size
    } else {
        args.chunk_size
    };
    if chunk_size != args.chunk_size {
        log.emit(
            "error",
            "cp7a.dispatch.chunk_size_mismatch",
            &[("dispatch", V::U(chunk_size)), ("expected", V::U(args.chunk_size))],
        );
        return exit::BAD_DISPATCH;
    }
    log.emit(
        "info",
        "cp7a.dispatch.received",
        &[
            ("action_type", s(&action_type)),
            ("transfer_id", s(&transfer_id_s)),
            ("artifact_id", s(&artifact_id_s)),
            ("chunk_size", V::U(chunk_size)),
        ],
    );

    // ---- 7. ActionAck{Accepted} -> Attempt InProgress -----------
    ws.send(Message::text(
        encode(&AgentProtocolMessage::ActionAck(ActionAckMessage::accepted(action_id))).unwrap(),
    ))
    .await
    .ok();
    log.emit("info", "cp7a.action.ack_sent", &[("outcome", s("Accepted"))]);

    // ---- 8. initial TransferAuthorizationGrant -----------------
    let (mut proof_key, mut token, base_url) =
        match obtain_grant(&mut ws, log, action_id, transfer_uuid).await {
            Ok(v) => v,
            Err(e) => {
                log.emit("error", "cp7a.transfer_auth.failed", &[("error", s(e))]);
                return exit::NO_GRANT;
            }
        };
    log.emit(
        "info",
        "cp7a.transfer_auth.grant_received",
        &[("data_plane_base_url", s(&base_url))],
    );

    // ---- 9. resolver: (obs_id, agent_source_id) -> local SSD ----
    counters.resolution_attempt_count += 1;
    let resolved = match epoch.resolve(&obs_id, &sel_asid) {
        Ok(r) => {
            counters.resolution_success_count += 1;
            r
        }
        Err(e) => {
            log.emit("error", "cp7a.resolve.failed", &[("detail", s(format!("{e:?}")))]);
            return exit::RESOLVER;
        }
    };
    if resolved.local_locator != sel_locator {
        log.emit("error", "cp7a.resolve.locator_mismatch", &[]);
        return exit::RESOLVER;
    }
    log.emit(
        "info",
        "cp7a.resolve.current",
        &[
            ("authority.source_observation_id", s(&obs_id)),
            ("authority.agent_source_id", s(&sel_asid)),
            ("evidence_only.resolved_local_locator", s(&resolved.local_locator)),
        ],
    );

    // ---- 10. open device GENERIC_READ, single-pass streaming ----
    let src = match sources::RawReadSource::open(&resolved.local_locator, counters) {
        Ok(s) => s,
        Err(e) => {
            log.emit("error", "cp7a.source.open_failed", &[("error", s(e))]);
            return exit::DEVICE;
        }
    };
    log.emit(
        "info",
        "cp7a.source.opened",
        &[
            ("evidence_only.opened_locator", s(src.locator())),
            ("desired_access", s("GENERIC_READ")),
            ("generic_write_requested", V::B(src.generic_write_requested())),
        ],
    );
    let reader = DeviceReader {
        src,
        counters: std::cell::RefCell::new(Counters::default()),
    };

    let mut state = match StreamState::new(args.prefix_bytes, chunk_size) {
        Ok(st) => st,
        Err(e) => {
            log.emit("error", "cp7a.stream.bad_plan", &[("error", s(e))]);
            return exit::STREAM_FATAL;
        }
    };
    log.emit(
        "info",
        "cp7a.stream.plan",
        &[
            ("prefix_bytes", V::U(args.prefix_bytes)),
            ("chunk_size", V::U(chunk_size)),
            ("chunk_count", V::U(state.chunk_count())),
            ("final_chunk_bytes", V::U(state.expected_len(state.chunk_count() - 1))),
            ("capture_extent", s("bounded_prefix_pressure — NOT a complete PhysicalDrive0 capture")),
        ],
    );

    let mut dp = RealDataPlane {
        client: DataPlaneClient::connect(&base_url, fingerprint)
            .map(|c| c.with_request_timeout(Duration::from_secs(args.seal_timeout_secs)))
            .unwrap_or_else(|e| {
                log.emit("error", "cp7a.dataplane.connect_failed", &[("error", s(format!("{e}")))]);
                std::process::exit(exit::DEVICE);
            }),
        auth: AgentTransferAuthorization::new(
            proof_key,
            token.clone(),
            transfer_uuid,
            artifact_uuid,
            DataPlaneTransferDirection::AgentToServer,
            base_url.clone(),
        ),
        transfer_uuid,
        chunk_size,
    };

    let mut suspensions = 0u32;
    loop {
        let mut last_logged_chunks = 0u64;
        let outcome = run_stream_pass(&mut state, &reader, &mut dp, &mut |tick: ProgressTick| {
            // Log a progress line only every ~16 newly-held chunks (and always
            // the last), to keep the CP7A NDJSON readable at 257 chunks.
            if tick.held_chunks == 257
                || tick.held_chunks / 16 != last_logged_chunks / 16
            {
                last_logged_chunks = tick.held_chunks;
                log.emit(
                    "info",
                    "cp7a.stream.progress",
                    &[
                        ("durably_held_bytes", V::U(tick.held_bytes)),
                        ("held_chunks", V::U(tick.held_chunks)),
                    ],
                );
            }
        })
        .await;
        match outcome {
            Ok(PassOutcome::Complete) => {
                log.emit(
                    "info",
                    "cp7a.stream.complete",
                    &[
                        ("held_chunks", V::U(state.chunk_count())),
                        ("device_read_count", V::U(reader.counters.borrow().data_read_count)),
                    ],
                );
                break;
            }
            Ok(PassOutcome::SuspendedNeedsAuthorization)
            | Ok(PassOutcome::SuspendedDataPlaneUnreachable) => {
                suspensions += 1;
                log.emit(
                    "warn",
                    "cp7a.stream.suspended",
                    &[
                        ("suspension_count", V::U(suspensions as u64)),
                        ("held_chunks", V::U(state.held_count())),
                        ("note", s("controlled interruption OR transient loss; re-acquiring a fresh grant and resuming")),
                    ],
                );
                if suspensions > MAX_OUTER_SUSPENSIONS {
                    log.emit("error", "cp7a.stream.too_many_suspensions", &[]);
                    return exit::STREAM_FATAL;
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
                match obtain_grant(&mut ws, log, action_id, transfer_uuid).await {
                    Ok((k, t, _url)) => {
                        proof_key = k;
                        token = t;
                        dp.auth = AgentTransferAuthorization::new(
                            proof_key,
                            token.clone(),
                            transfer_uuid,
                            artifact_uuid,
                            DataPlaneTransferDirection::AgentToServer,
                            base_url.clone(),
                        );
                        log.emit("info", "cp7a.stream.reauthorized", &[]);
                    }
                    Err(e) => {
                        log.emit("error", "cp7a.stream.reauth_failed", &[("error", s(e))]);
                        return exit::STREAM_FATAL;
                    }
                }
            }
            Err(StreamError::ChunkVerificationFailed { index }) => {
                log.emit(
                    "error",
                    "cp7a.stream.chunk_verification_failed",
                    &[("chunk_index", V::U(index))],
                );
                send_action_result(
                    &mut ws,
                    log,
                    action_id,
                    ActionResultOutcome::Failed,
                    "CHUNK_VERIFICATION_FAILED",
                    artifact_uuid,
                )
                .await;
                return exit::STREAM_FATAL;
            }
            Err(StreamError::Fatal(m)) => {
                log.emit("error", "cp7a.stream.fatal", &[("detail", s(m))]);
                send_action_result(
                    &mut ws,
                    log,
                    action_id,
                    ActionResultOutcome::Failed,
                    "TRANSFER_ABANDONED",
                    artifact_uuid,
                )
                .await;
                return exit::STREAM_FATAL;
            }
        }
    }

    // independent rolling full-Artifact digest over the exact bounded bytes.
    let artifact_digest_wire = match state.finish_digest() {
        Some(d) => d,
        None => {
            log.emit("error", "cp7a.stream.incomplete_hash", &[]);
            return exit::STREAM_FATAL;
        }
    };
    log.emit(
        "info",
        "cp7a.artifact_digest.rolling",
        &[
            ("artifact_digest_wire", s(&artifact_digest_wire)),
            ("total_bytes_hashed", V::U(args.prefix_bytes)),
            ("chunk_count", V::U(state.chunk_count())),
        ],
    );

    // ---- 11. seal + uncertain-delivery handling ---------------
    let chunk_count = state.chunk_count();
    let seal = finalize_seal(
        &mut ws,
        log,
        &mut dp,
        action_id,
        transfer_uuid,
        chunk_count,
        &artifact_digest_wire,
    )
    .await;

    match seal {
        SealFinal::Verified => {
            send_action_result(
                &mut ws,
                log,
                action_id,
                ActionResultOutcome::Succeeded,
                "TRANSFER_VERIFIED",
                artifact_uuid,
            )
            .await;
            log.emit(
                "info",
                "cp7a.verdict",
                &[
                    ("cp7a_pass", V::B(true)),
                    ("session_id", s(&session_id)),
                    ("artifact_id", s(artifact_uuid.to_string())),
                    ("artifact_status", s("Verified")),
                    ("suspensions", V::U(suspensions as u64)),
                    ("action_result", s("Succeeded/TRANSFER_VERIFIED")),
                    ("label", s("BOUNDED PREFIX PRESSURE — NOT walkthrough D, NOT complete capture, NOT Outcome A")),
                ],
            );
            exit::PASS
        }
        SealFinal::ArtifactFailed => {
            send_action_result(
                &mut ws,
                log,
                action_id,
                ActionResultOutcome::Failed,
                "ARTIFACT_VERIFICATION_FAILED",
                artifact_uuid,
            )
            .await;
            log.emit(
                "error",
                "cp7a.verdict",
                &[
                    ("cp7a_pass", V::B(false)),
                    ("artifact_status", s("Failed")),
                    ("action_result", s("Failed/ARTIFACT_VERIFICATION_FAILED")),
                    ("note", s("honest verification failure — Worker reconstruction digest != declared digest")),
                ],
            );
            exit::SEAL_ARTIFACT_FAILED
        }
        SealFinal::VerdictUnobservable => {
            // N1: the Artifact is durably terminal but a lost seal response left
            // the data plane returning generic 401. Do NOT send TRANSFER_VERIFIED
            // (no authoritative evidence in hand) and do NOT send
            // TRANSFER_ABANDONED (it may well have succeeded). Harness inspects
            // artifacts.state out-of-band for the Spike record.
            log.emit(
                "error",
                "cp7a.verdict",
                &[
                    ("cp7a_pass", V::B(false)),
                    ("seal_outcome", s("terminal_verdict_unobservable_via_data_plane")),
                    ("finding", s("N1 — M1 has no Agent-facing terminal-seal verdict channel after a lost seal response")),
                    ("action_result", s("NONE — deliberately not emitted")),
                ],
            );
            exit::SEAL_VERDICT_UNOBSERVABLE
        }
        SealFinal::Abandoned(reason) => {
            send_action_result(
                &mut ws,
                log,
                action_id,
                ActionResultOutcome::Failed,
                "TRANSFER_ABANDONED",
                artifact_uuid,
            )
            .await;
            log.emit(
                "error",
                "cp7a.verdict",
                &[
                    ("cp7a_pass", V::B(false)),
                    ("seal_outcome", s("abandoned")),
                    ("reason", s(reason)),
                    ("action_result", s("Failed/TRANSFER_ABANDONED")),
                ],
            );
            exit::SEAL_ABANDONED
        }
    }
}

enum SealFinal {
    Verified,
    ArtifactFailed,
    VerdictUnobservable,
    Abandoned(String),
}

/// Seals the manifest and resolves uncertain-delivery per the CURRENT M1
/// behavior (no invented status endpoint, no widened proof window).
async fn finalize_seal<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    log: &Log,
    dp: &mut RealDataPlane,
    action_id: ProtocolId,
    transfer_uuid: uuid::Uuid,
    chunk_count: u64,
    artifact_digest_wire: &str,
) -> SealFinal
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    for attempt in 0..8u32 {
        let started = Instant::now();
        let proof = match dp
            .auth
            .create_proof_now(TransferOperation::SealManifest, None)
        {
            Ok(p) => p,
            Err(e) => return SealFinal::Abandoned(format!("seal proof: {e}")),
        };
        let res = dp
            .client
            .seal(
                dp.auth.token(),
                transfer_uuid,
                &proof,
                chunk_count,
                artifact_digest_wire,
            )
            .await;
        log.emit(
            "info",
            "cp7a.seal.attempt",
            &[
                ("attempt", V::U(attempt as u64)),
                ("elapsed_ms", V::U(started.elapsed().as_millis() as u64)),
                ("outcome", s(format!("{res:?}"))),
            ],
        );
        match res {
            Ok(SealOutcome::Completed {
                artifact_id,
                artifact_status,
            }) => {
                if artifact_id != dp.auth.artifact_id() {
                    return SealFinal::Abandoned(format!(
                        "sealed artifact_id {artifact_id} != dispatched {}",
                        dp.auth.artifact_id()
                    ));
                }
                return match artifact_status {
                    SealArtifactStatus::Verified => SealFinal::Verified,
                    SealArtifactStatus::Failed => SealFinal::ArtifactFailed,
                };
            }
            Ok(SealOutcome::IncompleteManifest) => {
                return SealFinal::Abandoned("seal 409 INCOMPLETE_MANIFEST".into())
            }
            Ok(SealOutcome::ManifestAlreadySealed) => {
                return SealFinal::Abandoned(
                    "seal 409 MANIFEST_ALREADY_SEALED (a different (chunk_count,digest) is durably sealed)"
                        .into(),
                )
            }
            Ok(SealOutcome::Malformed) => {
                return SealFinal::Abandoned("seal 400 MALFORMED_REQUEST".into())
            }
            Ok(SealOutcome::Unexpected { status }) => {
                return SealFinal::Abandoned(format!("seal unexpected status {status}"))
            }
            Ok(SealOutcome::AuthorizationDenied) | Err(_) => {
                // Uncertain: fresh grant, then probe durable state via resume.
                log.emit("warn", "cp7a.seal.uncertain", &[("attempt", V::U(attempt as u64))]);
                tokio::time::sleep(Duration::from_millis(1200)).await;
                match obtain_grant(ws, log, action_id, transfer_uuid).await {
                    Ok((k, t, url)) => {
                        dp.auth = AgentTransferAuthorization::new(
                            k,
                            t,
                            transfer_uuid,
                            dp.auth.artifact_id(),
                            DataPlaneTransferDirection::AgentToServer,
                            url,
                        );
                    }
                    Err(e) => {
                        return SealFinal::Abandoned(format!("seal re-grant failed: {e}"))
                    }
                }
                match dp.discover_resume().await {
                    ResumeStatus::Ok(_) => {
                        // Artifact not terminal (Incomplete or PendingVerification
                        // — resume is eligible for both). Case A/B: retry seal
                        // with the SAME tuple.
                        log.emit("info", "cp7a.seal.resume_ok_retrying", &[]);
                        continue;
                    }
                    ResumeStatus::Fatal(m) if m.contains("already sealed") => {
                        // PendingVerification path: our RealDataPlane treats a
                        // sealed manifest as Fatal for the pass, but here it is
                        // the idempotent crash-recovery retry — try seal again.
                        log.emit("info", "cp7a.seal.pending_verification_retry", &[]);
                        continue;
                    }
                    ResumeStatus::AuthDenied => {
                        // A FRESH, otherwise-valid grant is still denied every
                        // data-plane operation -> the Artifact is durably
                        // terminal (Verified or Failed) and the verdict is not
                        // observable through the existing protocol (finding N1).
                        log.emit(
                            "error",
                            "cp7a.seal.verdict_unobservable",
                            &[("note", s("fresh valid grant still 401 on resume -> Artifact durably terminal; verdict not observable via data plane (N1)"))],
                        );
                        return SealFinal::VerdictUnobservable;
                    }
                    ResumeStatus::Transient(m) => {
                        log.emit("warn", "cp7a.seal.resume_transient", &[("error", s(m))]);
                        continue;
                    }
                    ResumeStatus::Fatal(m) => {
                        return SealFinal::Abandoned(format!("seal resume fatal: {m}"))
                    }
                }
            }
        }
    }
    SealFinal::Abandoned("seal retry budget exhausted (all attempts uncertain)".into())
}

async fn send_action_result<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    log: &Log,
    action_id: ProtocolId,
    outcome: ActionResultOutcome,
    code: &str,
    artifact_uuid: uuid::Uuid,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut detail = serde_json::Map::new();
    detail.insert("code".into(), serde_json::Value::String(code.into()));
    detail.insert(
        "artifact_id".into(),
        serde_json::Value::String(artifact_uuid.to_string()),
    );
    let msg = ActionResultMessage::new(action_id, outcome, detail);
    match encode(&AgentProtocolMessage::ActionResult(msg)) {
        Ok(wire) => {
            let _ = ws.send(Message::text(wire)).await;
            log.emit(
                "info",
                "cp7a.action_result.sent",
                &[("outcome", s(format!("{outcome:?}"))), ("code", s(code))],
            );
        }
        Err(e) => log.emit("error", "cp7a.action_result.encode_failed", &[("error", s(e.to_string()))]),
    }
    // Give the gateway a moment to consume it before the session ends.
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// One coordination round-trip: send the selection line, read one JSON ACK line
/// carrying `server_utc_ms`.
fn coord_roundtrip(coord: &str, line: &str) -> Result<i64, String> {
    let addr = coord
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| format!("bad coord addr {coord}"))?;
    let mut st = TcpStream::connect_timeout(&addr, NET_TIMEOUT).map_err(|e| format!("connect: {e}"))?;
    st.set_write_timeout(Some(NET_TIMEOUT)).ok();
    st.set_read_timeout(Some(NET_TIMEOUT)).ok();
    st.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    st.flush().ok();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match st.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > 4096 {
                    return Err("coord ACK line too long".into());
                }
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("coord ACK not JSON: {e}"))?;
    if v.get("cp7_coord_ack").and_then(|x| x.as_bool()) != Some(true) {
        return Err(format!("coord ACK missing cp7_coord_ack: {text}"));
    }
    v.get("server_utc_ms")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| format!("coord ACK missing server_utc_ms: {text}"))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let log = Log::new();
    let args = parse_args();
    log.emit(
        "info",
        "probe.start",
        &[
            ("probe_version", s(PROBE_VERSION)),
            ("git_short", s(env!("PROBE_GIT_SHORT"))),
            ("build_epoch_secs", s(env!("PROBE_BUILD_EPOCH_SECS"))),
            ("rustc", s(env!("PROBE_RUSTC_VERSION"))),
            ("target_triple", s(env!("PROBE_TARGET_TRIPLE"))),
            ("std_os", s(std::env::consts::OS)),
            ("std_arch", s(std::env::consts::ARCH)),
            ("computername", s(std::env::var("COMPUTERNAME").unwrap_or_else(|_| "<unset>".into()))),
            ("wss", s(&args.wss)),
            ("coord", s(&args.coord)),
            ("prefix_bytes", V::U(args.prefix_bytes)),
            ("chunk_size", V::U(args.chunk_size)),
            ("seal_timeout_secs", V::U(args.seal_timeout_secs)),
            ("skew_gate_ms", s(format!("[{}, {}]", args.skew_floor_ms, args.skew_ceil_ms))),
        ],
    );

    let mut counters = Counters::default();
    let exit = run(&log, &args, &mut counters).await;

    log.emit(
        "info",
        "probe.end",
        &[
            ("cp7a_exitcode", V::U(exit as u64)),
            ("cp7a_pass", V::B(exit == 0)),
            ("data_device_open_count", V::U(counters.data_device_open_count)),
            ("data_read_count", V::U(counters.data_read_count)),
        ],
    );
    write_local(&log);
    flush_sink(&log, &args.sink);
    write_local(&log);

    println!();
    println!("CP7A_EXITCODE={exit}");
    let _ = std::io::stdout().flush();
    std::process::exit(exit);
}
