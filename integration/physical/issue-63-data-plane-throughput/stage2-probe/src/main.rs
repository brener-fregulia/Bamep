//! Issue #63 Stage 2 — WinPE-native transfer probe. THROWAWAY Spike.
//!
//! PROVENANCE: adapted from
//! `integration/physical/issue-61-endpoint-capture-data-plane/probe7/src/main.rs`
//! (Issue #61 CP7A, closed). Removed: the CP7A Gate-4 deterministic
//! fault-injection checkpoint, the A/B/C/D seal-mode elaboration, and the
//! bounded-prefix "pressure" framing. Added: the Issue-63 source-safety
//! predicate gate (`bamep_i63_stage2_engine::safety`) and the exact chunk-size
//! agreement gate. #61 is not modified.
//!
//! One process invocation = ONE Issue-63 transfer case:
//!
//!   enumerate -> mint fresh source epoch -> operator-local selection
//!   -> lab coord line + Server-UTC ACK -> asymmetric clock pre-flight
//!   -> pinned TLS 1.3 / WSS -> Agent auth -> InventoryReport
//!   -> ActionDispatch (bamep.m1.data-plane-transfer) -> ActionAck{Accepted}
//!   -> TransferAuthorizationGrant
//!   -> resolver: (obs_id, agent_source_id) -> local locator
//!   -> GENERIC_READ open + 3-IOCTL device length
//!   -> ISSUE-63 SOURCE-SAFETY PREDICATE  (fail-closed; Reject => ZERO bulk reads)
//!   -> exact chunk-size agreement gate
//!   -> SINGLE-PASS streaming read of the bounded extent (2048 MiB)
//!        * each chunk read AT MOST ONCE, hashed EXACTLY ONCE, ascending order
//!   -> real sender-constrained Worker HTTPS PUTs (serial, fresh connection)
//!   -> POST /seal -> Worker full-Artifact reconstruction -> Artifact::Verified
//!   -> ActionResult{Succeeded, TRANSFER_VERIFIED} -> exit.
//!
//! ZERO deliberate fault injection. Nothing here arms a physical matrix.

mod resolver;
mod safety;
mod sources;
mod stream;

use std::future::Future;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bamep_agent_protocol::{
    decode, encode, ActionAckMessage, ActionResultMessage, ActionResultOutcome,
    AgentProtocolMessage, ProtocolId, TransferAuthorizationRequestMessage,
};
use bamep_i63_stage2_engine::matrix::{
    expected_chunk_count, verify_chunk_agreement, ChunkAgreement, EXTENT_BYTES,
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

use stream::{
    run_stream_pass, run_stream_pass_prep_ahead, run_stream_pass_window8, ChunkReader, DataPlane,
    PassOutcome, ProgressTick, PutStatus, ResumeStatus, StreamError, StreamEvent, StreamState,
    WindowedPutLauncher,
};

const PROBE_NAME: &str = env!("CARGO_PKG_NAME");
const PROBE_VERSION: &str = env!("CARGO_PKG_VERSION");
const NET_TIMEOUT: Duration = Duration::from_secs(8);
const DISPATCH_WAIT: Duration = Duration::from_secs(120);
const CHUNK_SIZE_DEFAULT: u64 = 8 * 1024 * 1024;
const SKEW_FLOOR_MS_DEFAULT: i64 = -60_000;
const SKEW_CEIL_MS_DEFAULT: i64 = 10_000;
const SEAL_TIMEOUT_SECS_DEFAULT: u64 = 240;
const MAX_OUTER_SUSPENSIONS: u32 = 12;

pub(crate) mod base64_ct {
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
        format!("{dir}\\bamep-i63-stage2-probe.ndjson"),
        "bamep-i63-stage2-probe.ndjson".into(),
    ] {
        if std::fs::write(&p, format!("{}\n", log.snapshot())).is_ok() {
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

/// Issue #63 Stage 4 — which single-pass streaming algorithm the probe runs.
/// `Serial` is the already-proven Stage-3 default; `PrepAhead2` is the
/// throwaway depth-2 prep-ahead pipeline (`run_stream_pass_prep_ahead`);
/// `PrepAheadWindow8` is the window_8 solution candidate: the SAME prep-ahead
/// source pipeline + up to 8 concurrent chunk PUTs, per-PUT Worker durability
/// semantics unchanged (`run_stream_pass_window8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamMode {
    Serial,
    PrepAhead2,
    PrepAheadWindow8,
    PrepAheadWindow8Batch8,
}
impl StreamMode {
    fn wire(self) -> &'static str {
        match self {
            StreamMode::Serial => "serial",
            StreamMode::PrepAhead2 => "prep_ahead_2",
            StreamMode::PrepAheadWindow8 => "prep_ahead_window_8",
            StreamMode::PrepAheadWindow8Batch8 => "prep_ahead_window_8_batch_8",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "serial" => Some(StreamMode::Serial),
            "prep_ahead_2" | "prep-ahead-2" => Some(StreamMode::PrepAhead2),
            "prep_ahead_window_8" | "prep-ahead-window-8" | "window_8" => {
                Some(StreamMode::PrepAheadWindow8)
            }
            "prep_ahead_window_8_batch_8" | "batch_8" => Some(StreamMode::PrepAheadWindow8Batch8),
            _ => None,
        }
    }
}

struct Args {
    sink: String,
    coord: String,
    wss: String,
    pin_hex: String,
    credential_file: String,
    select_model_substr: String,
    /// Stage-4 stream algorithm. Defaults to `Serial` so every existing
    /// (Stage-1/2/3) invocation is byte-for-byte unchanged.
    mode: StreamMode,
    chunk_size: u64,
    extent_bytes: u64,
    seal_timeout_secs: u64,
    skew_floor_ms: i64,
    skew_ceil_ms: i64,
    /// The plan case id, carried through to the result line for correlation.
    case_id: String,
    run_id: String,
    /// If non-empty: after a successful WSS auth, write the freshly issued
    /// `runtime_credential` (from `SessionEstablished`) here so the NEXT
    /// per-case probe process can reuse it (ADR-0012 runtime-credential
    /// rotation). Stage-3 matrix runner: case 0 authenticates with the
    /// first-contact enrollment credential; cases 1..35 with the rotated
    /// runtime credential this wrote.
    runtime_credential_out: String,
}
fn parse_args() -> Args {
    let mut a = Args {
        sink: "192.168.99.1:9299".into(),
        coord: "192.168.99.1:9206".into(),
        wss: "192.168.99.1:8443".into(),
        pin_hex: String::new(),
        credential_file: String::new(),
        select_model_substr: "256GB".into(),
        mode: StreamMode::Serial,
        chunk_size: CHUNK_SIZE_DEFAULT,
        extent_bytes: EXTENT_BYTES,
        seal_timeout_secs: SEAL_TIMEOUT_SECS_DEFAULT,
        skew_floor_ms: SKEW_FLOOR_MS_DEFAULT,
        skew_ceil_ms: SKEW_CEIL_MS_DEFAULT,
        case_id: "i63s2-probe-case".into(),
        run_id: "i63s2-probe".into(),
        runtime_credential_out: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let num = |it: &mut dyn Iterator<Item = String>| it.next().and_then(|v| v.parse().ok());
        match x.as_str() {
            "--sink" => a.sink = it.next().unwrap_or(a.sink),
            "--coord" => a.coord = it.next().unwrap_or(a.coord),
            "--wss" => a.wss = it.next().unwrap_or(a.wss),
            "--pin" => a.pin_hex = it.next().unwrap_or_default(),
            "--auth-credential-file" => a.credential_file = it.next().unwrap_or_default(),
            "--select-model-substr" => {
                a.select_model_substr = it.next().unwrap_or(a.select_model_substr)
            }
            "--chunk-size" => a.chunk_size = num(&mut it).unwrap_or(a.chunk_size),
            "--extent-bytes" => a.extent_bytes = num(&mut it).unwrap_or(a.extent_bytes),
            "--seal-timeout-secs" => a.seal_timeout_secs = num(&mut it).unwrap_or(a.seal_timeout_secs),
            "--skew-floor-ms" => {
                a.skew_floor_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.skew_floor_ms)
            }
            "--skew-ceil-ms" => {
                a.skew_ceil_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.skew_ceil_ms)
            }
            "--mode" => {
                let raw = it.next().unwrap_or_default();
                match StreamMode::parse(&raw) {
                    Some(m) => a.mode = m,
                    None => {
                        eprintln!("bad --mode {raw:?} (want: serial | prep_ahead_2 | prep_ahead_window_8)");
                        std::process::exit(exit::BAD_ARGS);
                    }
                }
            }
            "--case-id" => a.case_id = it.next().unwrap_or(a.case_id),
            "--run-id" => a.run_id = it.next().unwrap_or(a.run_id),
            "--runtime-credential-out" => {
                a.runtime_credential_out = it.next().unwrap_or_default()
            }
            "--self-check" => { /* handled in main before this */ }
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

mod exit {
    pub const PASS: i32 = 0;
    pub const BAD_ARGS: i32 = 2;
    pub const ENUMERATION: i32 = 61;
    pub const COORD: i32 = 62;
    pub const WSS_AUTH: i32 = 63;
    pub const NO_DISPATCH: i32 = 64;
    pub const BAD_DISPATCH: i32 = 65;
    pub const NO_GRANT: i32 = 66;
    pub const RESOLVER: i32 = 67;
    pub const DEVICE: i32 = 68;
    pub const CLOCK_PREFLIGHT: i32 = 69;
    pub const SAFETY_REJECTED: i32 = 74;
    pub const CHUNK_AGREEMENT: i32 = 75;
    pub const STREAM_FATAL: i32 = 70;
    pub const SEAL_ARTIFACT_FAILED: i32 = 71;
    pub const SEAL_ABANDONED: i32 = 73;
}

// ---- real device chunk reader (Windows: GENERIC_READ; host: stub) ----

struct DeviceReader {
    src: sources::RawReadSource,
    counters: std::cell::RefCell<Counters>,
}
impl ChunkReader for DeviceReader {
    fn read_chunk(&self, _index: u64, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        let mut c = self.counters.borrow_mut();
        self.src.read_bytes_at(offset, len, &mut c)
    }
}

// ---- real Worker-HTTPS data plane behind the stream::DataPlane trait ----

struct RealDataPlane {
    client: DataPlaneClient,
    auth: AgentTransferAuthorization,
    transfer_uuid: uuid::Uuid,
    chunk_size: u64,
    /// The chunk_size the Server's manifest reported on the last resume — the
    /// "Server Transfer chunk_size" input to the agreement gate.
    observed_manifest_chunk_size: Option<u64>,
    /// Aggregate per-chunk proof-mint + PUT/ACK nanoseconds (Stage-3 CaseResult
    /// `proof_ms` / `put_ack_ms`; the Stage-2 schema already carries both).
    proof_ns: std::cell::Cell<u128>,
    put_ack_ns: std::cell::Cell<u128>,
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
                self.observed_manifest_chunk_size = Some(m.chunk_size as u64);
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
        let t_proof = Instant::now();
        let proof = match self
            .auth
            .create_proof_now(TransferOperation::ChunkUpload, Some(index))
        {
            Ok(p) => p,
            Err(e) => return PutStatus::Fatal(format!("chunk proof: {e}")),
        };
        self.proof_ns.set(self.proof_ns.get() + t_proof.elapsed().as_nanos());
        let t_put = Instant::now();
        let outcome = self
            .client
            .put_chunk(
                self.auth.token(),
                self.transfer_uuid,
                index,
                digest_wire,
                &proof,
                bytes.to_vec(),
            )
            .await;
        self.put_ack_ns.set(self.put_ack_ns.get() + t_put.elapsed().as_nanos());
        match outcome
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

// ---- window_8 candidate: bounded-concurrent-PUT launcher ------------------

/// The `stream::WindowedPutLauncher` for the real Worker HTTPS data plane.
/// Borrows `auth` ONLY to synchronously mint each PUT's proof on the
/// foreground (`AgentTransferAuthorization::create_proof_now` takes `&self`,
/// so this is safe without any interior synchronization); the returned future
/// captures only owned, immutable copies (`token`, `transfer_uuid`, the
/// already-minted `proof`, `base_url`, the `Copy` `fingerprint`) — never
/// `auth` itself — so the future's `'static` bound holds despite `auth` being
/// borrowed. Keeps the SAME fresh-TCP/TLS-per-request semantics `DataPlaneClient`
/// already uses; `DataPlaneClient` is not `Clone`, and per the Spike's
/// authorization a fresh per-PUT client is an acceptable candidate simplification
/// (persistent-connection pooling is not a variable of this candidate).
struct RealWindowLauncher<'a> {
    auth: &'a AgentTransferAuthorization,
    transfer_uuid: uuid::Uuid,
    base_url: String,
    fingerprint: ServerCertFingerprint,
    request_timeout: Duration,
    /// Foreground-only (proof minting is synchronous and sequential) —
    /// a plain accumulator is safe.
    proof_ns: std::cell::Cell<u128>,
    /// Written concurrently from multiple in-flight PUT futures — needs a
    /// real atomic. Nanoseconds fit comfortably in a `u64`.
    put_ack_ns: Arc<AtomicU64>,
}
impl WindowedPutLauncher for RealWindowLauncher<'_> {
    fn start_put(
        &mut self,
        index: u64,
        digest_wire: String,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = PutStatus> + Send>> {
        let t_proof = Instant::now();
        let proof = match self
            .auth
            .create_proof_now(TransferOperation::ChunkUpload, Some(index))
        {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { PutStatus::Fatal(format!("chunk proof: {e}")) }),
        };
        self.proof_ns.set(self.proof_ns.get() + t_proof.elapsed().as_nanos());

        let token = self.auth.token().to_string();
        let transfer_uuid = self.transfer_uuid;
        let base_url = self.base_url.clone();
        let fingerprint = self.fingerprint;
        let request_timeout = self.request_timeout;
        let put_ack_ns = Arc::clone(&self.put_ack_ns);

        Box::pin(async move {
            let client = match DataPlaneClient::connect(&base_url, fingerprint) {
                Ok(c) => c.with_request_timeout(request_timeout),
                Err(e) => return PutStatus::Fatal(format!("window_8: connect: {e}")),
            };
            let t_put = Instant::now();
            let outcome = client
                .put_chunk(&token, transfer_uuid, index, &digest_wire, &proof, bytes)
                .await;
            put_ack_ns.fetch_add(t_put.elapsed().as_nanos() as u64, Ordering::Relaxed);
            match outcome {
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
        })
    }
}

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
    let transfer_pid =
        ProtocolId::from_uuid(transfer_uuid).map_err(|e| format!("bad transfer uuid: {e}"))?;
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

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() >= deadline {
            return Err("timed out waiting for TransferAuthorizationGrant".into());
        }
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => match decode(&t) {
                Ok(AgentProtocolMessage::TransferAuthorizationGrant(g)) => {
                    log.emit("info", "probe.transfer_auth.grant_received", &[]);
                    return Ok((
                        proof_key,
                        g.body.token.clone(),
                        g.body.data_plane_base_url.clone(),
                    ));
                }
                Ok(AgentProtocolMessage::TransferAuthorizationDenied(_)) => {
                    return Err("TransferAuthorizationDenied".into());
                }
                _ => {}
            },
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(format!("wss recv error: {e}")),
            Ok(None) => return Err("wss closed".into()),
            Err(_) => {}
        }
    }
}

fn coord_roundtrip(coord: &str, line: &str) -> Result<i64, String> {
    let addr = coord
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| format!("bad coord addr {coord}"))?;
    let mut st =
        TcpStream::connect_timeout(&addr, NET_TIMEOUT).map_err(|e| format!("connect: {e}"))?;
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
    // Accept either the Stage-1 (`stage1_coord_ack`) or CP7 (`cp7_coord_ack`)
    // ACK shape — the lab coordinator answers with the Server's current UTC.
    let ok = v.get("cp7_coord_ack").and_then(|x| x.as_bool()) == Some(true)
        || v.get("stage1_coord_ack").and_then(|x| x.as_bool()) == Some(true);
    if !ok {
        return Err(format!("coord ACK missing *_coord_ack: {text}"));
    }
    v.get("server_utc_ms")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| format!("coord ACK missing server_utc_ms: {text}"))
}

// ---- seal ----------------------------------------------------------

enum SealFinal {
    Verified,
    ArtifactFailed,
    Abandoned(String),
}

async fn finalize_seal(
    dp: &mut RealDataPlane,
    log: &Log,
    transfer_uuid: uuid::Uuid,
    chunk_count: u64,
    artifact_digest_wire: &str,
) -> SealFinal {
    for attempt in 0..6u32 {
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
            .seal(dp.auth.token(), transfer_uuid, &proof, chunk_count, artifact_digest_wire)
            .await;
        log.emit(
            "info",
            "probe.seal.attempt",
            &[
                ("attempt", V::U(attempt as u64)),
                ("elapsed_ms", V::U(started.elapsed().as_millis() as u64)),
                ("outcome", s(format!("{res:?}"))),
            ],
        );
        match res {
            Ok(SealOutcome::Completed { artifact_status, .. }) => {
                return match artifact_status {
                    SealArtifactStatus::Verified => SealFinal::Verified,
                    SealArtifactStatus::Failed => SealFinal::ArtifactFailed,
                }
            }
            Ok(SealOutcome::IncompleteManifest) => {
                return SealFinal::Abandoned("seal 409 INCOMPLETE_MANIFEST".into())
            }
            Ok(SealOutcome::ManifestAlreadySealed) => {
                return SealFinal::Abandoned("seal 409 MANIFEST_ALREADY_SEALED".into())
            }
            Ok(SealOutcome::Malformed) => {
                return SealFinal::Abandoned("seal 400 MALFORMED_REQUEST".into())
            }
            Ok(SealOutcome::Unexpected { status }) => {
                return SealFinal::Abandoned(format!("seal unexpected status {status}"))
            }
            Ok(SealOutcome::AuthorizationDenied) | Err(_) => {
                tokio::time::sleep(Duration::from_millis(800)).await;
                continue;
            }
        }
    }
    SealFinal::Abandoned("seal retry budget exhausted".into())
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
    if let Ok(wire) = encode(&AgentProtocolMessage::ActionResult(ActionResultMessage::new(
        action_id, outcome, detail,
    ))) {
        let _ = ws.send(Message::text(wire)).await;
        log.emit(
            "info",
            "probe.action_result.sent",
            &[("outcome", s(format!("{outcome:?}"))), ("code", s(code))],
        );
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

// ---- run ----------------------------------------------------------

async fn run(log: &Log, args: &Args, counters: &mut Counters) -> i32 {
    // ---- 0. plan arithmetic (pure, before anything) ----
    let expected_chunks = match expected_chunk_count(args.extent_bytes, args.chunk_size) {
        Ok(n) => n,
        Err(e) => {
            log.emit("error", "probe.plan.bad_arithmetic", &[("detail", s(format!("{e:?}")))]);
            return exit::BAD_ARGS;
        }
    };
    log.emit(
        "info",
        "probe.plan",
        &[
            ("run_id", s(&args.run_id)),
            ("case_id", s(&args.case_id)),
            ("mode", s(args.mode.wire())),
            ("extent_bytes", V::U(args.extent_bytes)),
            ("chunk_size", V::U(args.chunk_size)),
            ("expected_chunk_count", V::U(expected_chunks)),
        ],
    );

    // ---- 1. fresh source epoch ----
    let epoch_src = sources::enumerate();
    let obs_id = epoch_src.observation_id.clone();
    if obs_id.len() != 43 || epoch_src.sources.is_empty() {
        log.emit("error", "probe.epoch.bad", &[("observation_id_len", V::U(obs_id.len() as u64))]);
        return exit::ENUMERATION;
    }
    for (i, src) in epoch_src.sources.iter().enumerate() {
        log.emit(
            "info",
            "probe.epoch.source",
            &[
                ("index", V::U(i as u64)),
                ("authority.agent_source_id", s(&src.agent_source_id)),
                ("evidence_only.local_locator", s(&src.local_locator)),
                ("evidence_only.model", s(&src.product)),
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
    if epoch.has_duplicate_agent_source_ids() {
        log.emit("error", "probe.epoch.ambiguous", &[]);
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
            "probe.operator_selection.ambiguous",
            &[("match_count", V::U(matched.len() as u64))],
        );
        return exit::ENUMERATION;
    }
    let sel_asid = matched[0].agent_source_id.clone();
    let sel_locator = matched[0].local_locator.clone();
    let sel_source = matched[0].clone();

    // ---- 2. lab coord + Server-UTC ACK ----
    // The `chunk_size` is carried so the Stage-3 harness creates this case's
    // fresh Transfer lineage with the exact plan chunk size (the harness
    // ignores it for the Stage-1/#61 single-transfer shapes).
    let coord_line = format!(
        r#"{{"cp7_coord":"source_selection","source_observation_id":"{}","selected_agent_source_id":"{}","chunk_size":{},"case_id":"{}"}}"#,
        esc(&obs_id),
        esc(&sel_asid),
        args.chunk_size,
        esc(&args.case_id)
    );
    let server_utc_ms = match coord_roundtrip(&args.coord, &coord_line) {
        Ok(v) => v,
        Err(e) => {
            log.emit("error", "probe.coord.failed", &[("error", s(e))]);
            return exit::COORD;
        }
    };
    let agent_now_ms = now_ms() as i64;
    let skew_ms = agent_now_ms - server_utc_ms;
    log.emit(
        "info",
        "probe.coord.ok",
        &[("server_utc_ms", V::I(server_utc_ms)), ("skew_ms", V::I(skew_ms))],
    );

    // ---- 3. asymmetric clock pre-flight (before any device access) ----
    if skew_ms < args.skew_floor_ms || skew_ms > args.skew_ceil_ms {
        log.emit(
            "error",
            "probe.clock.preflight_failed",
            &[
                ("skew_ms", V::I(skew_ms)),
                ("gate", s(format!("[{}, {}]", args.skew_floor_ms, args.skew_ceil_ms))),
            ],
        );
        return exit::CLOCK_PREFLIGHT;
    }
    let clock_verdict = format!("in_bound(skew_ms={skew_ms})");

    // ---- 4. pinned WSS + auth ----
    let Some(pin) = parse_pin(&args.pin_hex) else {
        log.emit("error", "probe.pin.bad", &[]);
        return exit::WSS_AUTH;
    };
    let fingerprint = ServerCertFingerprint::from_sha256_digest(pin);
    let Some(wss_addr) = args.wss.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        log.emit("error", "probe.wss.bad_addr", &[]);
        return exit::WSS_AUTH;
    };
    let mut ws: tokio_tungstenite::WebSocketStream<_> =
        match connect_pinned_wss(wss_addr as SocketAddr, "bamep-agent", fingerprint).await {
            Ok(w) => w,
            Err(e) => {
                log.emit("error", "probe.wss.failed", &[("error", s(format!("{e}")))]);
                return exit::WSS_AUTH;
            }
        };
    log.emit("info", "probe.wss.established", &[]);

    let credential = match std::fs::read_to_string(&args.credential_file) {
        Ok(c) => c.trim().to_string(),
        Err(e) => {
            log.emit("error", "probe.credential.unreadable", &[("error", s(e.to_string()))]);
            return exit::WSS_AUTH;
        }
    };
    match authenticate(&mut ws, &credential).await {
        Ok(SimulatorHandshakeOutcome::Established(est)) => {
            log.emit("info", "probe.auth.session_established", &[]);
            // Persist the freshly issued runtime credential so the NEXT
            // per-case probe process can authenticate without re-redeeming the
            // (single-use) first-contact credential. Never logged.
            if !args.runtime_credential_out.is_empty() {
                match std::fs::write(
                    &args.runtime_credential_out,
                    est.body.runtime_credential.as_bytes(),
                ) {
                    Ok(()) => log.emit(
                        "info",
                        "probe.auth.runtime_credential_persisted",
                        &[("path", s(&args.runtime_credential_out))],
                    ),
                    Err(e) => {
                        log.emit(
                            "error",
                            "probe.auth.runtime_credential_persist_failed",
                            &[("error", s(e.to_string()))],
                        );
                        return exit::WSS_AUTH;
                    }
                }
            }
        }
        Ok(SimulatorHandshakeOutcome::Rejected(_)) => {
            log.emit("error", "probe.auth.rejected", &[]);
            return exit::WSS_AUTH;
        }
        Err(e) => {
            log.emit("error", "probe.auth.error", &[("error", s(format!("{e}")))]);
            return exit::WSS_AUTH;
        }
    }

    // ---- 5. InventoryReport carrying this fresh epoch ----
    let mut inv = serde_json::Map::new();
    inv.insert("probe".into(), serde_json::json!(PROBE_NAME));
    inv.insert("capture_source_observation_id".into(), serde_json::json!(obs_id));
    inv.insert(
        "capturable_sources".into(),
        serde_json::json!(epoch_src
            .sources
            .iter()
            .map(|x| serde_json::json!({ "agent_source_id": x.agent_source_id }))
            .collect::<Vec<_>>()),
    );
    if let Err(e) = send_inventory_report(&mut ws, inv).await {
        log.emit("error", "probe.inventory.send_failed", &[("error", s(format!("{e}")))]);
        return exit::WSS_AUTH;
    }

    // ---- 6. wait for ActionDispatch ----
    let dispatch = {
        let deadline = Instant::now() + DISPATCH_WAIT;
        loop {
            if Instant::now() >= deadline {
                log.emit("error", "probe.dispatch.timeout", &[]);
                return exit::NO_DISPATCH;
            }
            let frame = match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
                Ok(Some(Ok(f))) => f,
                Ok(Some(Err(e))) => {
                    log.emit("error", "probe.wss.recv_error", &[("error", s(e.to_string()))]);
                    return exit::NO_DISPATCH;
                }
                Ok(None) => {
                    log.emit("error", "probe.wss.closed", &[]);
                    return exit::NO_DISPATCH;
                }
                Err(_) => continue,
            };
            let Message::Text(text) = frame else { continue };
            match decode(&text) {
                Ok(AgentProtocolMessage::ActionDispatch(d)) => break d,
                _ => continue,
            }
        }
    };
    let action_id = dispatch.body.action_id;
    if dispatch.body.action_type != "bamep.m1.data-plane-transfer" {
        log.emit(
            "error",
            "probe.dispatch.wrong_action",
            &[("action_type", s(&dispatch.body.action_type))],
        );
        return exit::NO_DISPATCH;
    }
    let p = &dispatch.body.parameters;
    let transfer_id_s = p.get("transfer_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let artifact_id_s = p.get("artifact_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let dispatch_chunk_size = p.get("chunk_size").and_then(|v| v.as_u64()).unwrap_or(0);
    let (Ok(transfer_uuid), Ok(artifact_uuid)) =
        (transfer_id_s.parse::<uuid::Uuid>(), artifact_id_s.parse::<uuid::Uuid>())
    else {
        log.emit("error", "probe.dispatch.bad_ids", &[]);
        return exit::BAD_DISPATCH;
    };
    let action_dispatch_chunk_size = if dispatch_chunk_size > 0 {
        dispatch_chunk_size
    } else {
        args.chunk_size
    };
    log.emit(
        "info",
        "probe.dispatch.received",
        &[
            ("transfer_id", s(&transfer_id_s)),
            ("artifact_id", s(&artifact_id_s)),
            ("action_dispatch_chunk_size", V::U(action_dispatch_chunk_size)),
        ],
    );

    // ---- 7. ActionAck{Accepted} ----
    ws.send(Message::text(
        encode(&AgentProtocolMessage::ActionAck(ActionAckMessage::accepted(action_id))).unwrap(),
    ))
    .await
    .ok();

    // ---- 8. initial grant ----
    let (proof_key, token, base_url) = match obtain_grant(&mut ws, log, action_id, transfer_uuid).await
    {
        Ok(v) => v,
        Err(e) => {
            log.emit("error", "probe.transfer_auth.failed", &[("error", s(e))]);
            return exit::NO_GRANT;
        }
    };

    // ---- 9. resolver ----
    counters.resolution_attempt_count += 1;
    let resolved = match epoch.resolve(&obs_id, &sel_asid) {
        Ok(r) => {
            counters.resolution_success_count += 1;
            r
        }
        Err(e) => {
            log.emit("error", "probe.resolve.failed", &[("detail", s(format!("{e:?}")))]);
            return exit::RESOLVER;
        }
    };
    if resolved.local_locator != sel_locator {
        log.emit("error", "probe.resolve.locator_mismatch", &[]);
        return exit::RESOLVER;
    }

    // ---- 10. GENERIC_READ open + 3-IOCTL device length (permitted pre-predicate) ----
    let src = match sources::RawReadSource::open(&resolved.local_locator, counters) {
        Ok(s) => s,
        Err(e) => {
            log.emit("error", "probe.source.open_failed", &[("error", s(e))]);
            return exit::DEVICE;
        }
    };
    let device_length = src.device_length();
    log.emit(
        "info",
        "probe.source.opened",
        &[
            ("desired_access", s("GENERIC_READ")),
            ("generic_write_requested", V::B(src.generic_write_requested())),
            ("device_length_authoritative", V::I(
                device_length.authoritative().map(|n| n as i64).unwrap_or(-1),
            )),
            ("bulk_read_count", V::U(counters.data_read_count)),
        ],
    );

    // ---- 11. ISSUE-63 SOURCE-SAFETY PREDICATE (fail-closed; Reject => ZERO bulk reads) ----
    let safety = safety::evaluate(
        &obs_id,
        std::slice::from_ref(&sel_asid),
        Some(&resolved),
        Some(&sel_source),
        Some(&device_length),
        args.extent_bytes,
    );
    let safety_verdict_token = match &safety {
        safety::GateOutcome::Accept { locator, device_length_bytes } => {
            log.emit(
                "info",
                "probe.safety.accept",
                &[
                    ("evidence_only.locator", s(locator)),
                    ("device_length_bytes", V::U(*device_length_bytes)),
                    ("bulk_read_count", V::U(counters.data_read_count)),
                ],
            );
            "accept".to_string()
        }
        safety::GateOutcome::Reject { token } => {
            log.emit(
                "error",
                "probe.safety.reject",
                &[
                    ("reason", s(token)),
                    ("bulk_read_count", V::U(counters.data_read_count)),
                    ("note", s("fail-closed: NO bulk source read performed")),
                ],
            );
            emit_result_line(
                log, args, expected_chunks, None, None,
                &format!("reject:{token}"), &clock_verdict, "SafetyRejected", "failed:source_safety",
            );
            return exit::SAFETY_REJECTED;
        }
    };

    // ---- 12. exact chunk-size agreement gate (all sources must agree) ----
    // The Server Transfer chunk_size is confirmed by RealDataPlane on the first
    // resume; here we gate the plan / dispatch / probe values up front and then
    // let the manifest check inside the stream confirm the Server value.
    let agreement = ChunkAgreement {
        plan_extent_bytes: args.extent_bytes,
        plan_chunk_size_bytes: args.chunk_size,
        plan_expected_chunk_count: expected_chunks,
        // Confirmed again from the manifest during the stream; seeded here with
        // the dispatch value so a plan/dispatch disagreement fails before bytes.
        server_transfer_chunk_size: action_dispatch_chunk_size,
        action_dispatch_chunk_size,
        probe_chunk_size: args.chunk_size,
    };
    let agreed = if args.extent_bytes == EXTENT_BYTES {
        verify_chunk_agreement(&agreement)
    } else {
        bamep_i63_stage2_engine::matrix::verify_chunk_agreement_at_extent(
            args.extent_bytes,
            &agreement,
        )
    };
    let agreed_chunks = match agreed {
        Ok(n) => n,
        Err(e) => {
            log.emit("error", "probe.chunk_agreement.failed", &[("detail", s(format!("{e:?}")))]);
            emit_result_line(
                log, args, expected_chunks, None, None,
                &safety_verdict_token, &clock_verdict, "ChunkAgreementFailed", "failed:chunk_agreement",
            );
            return exit::CHUNK_AGREEMENT;
        }
    };

    // ---- 13. single-pass stream ----
    let reader = DeviceReader {
        src,
        counters: std::cell::RefCell::new(Counters::default()),
    };
    let mut state = match StreamState::new(args.extent_bytes, args.chunk_size) {
        Ok(st) => st,
        Err(e) => {
            log.emit("error", "probe.stream.bad_plan", &[("error", s(e))]);
            return exit::STREAM_FATAL;
        }
    };
    if state.chunk_count() != agreed_chunks {
        log.emit(
            "error",
            "probe.stream.chunk_count_mismatch",
            &[("stream", V::U(state.chunk_count())), ("agreed", V::U(agreed_chunks))],
        );
        return exit::CHUNK_AGREEMENT;
    }

    let mut dp = RealDataPlane {
        client: match DataPlaneClient::connect(&base_url, fingerprint) {
            Ok(c) => c.with_request_timeout(Duration::from_secs(args.seal_timeout_secs)),
            Err(e) => {
                log.emit("error", "probe.dataplane.connect_failed", &[("error", s(format!("{e}")))]);
                return exit::DEVICE;
            }
        },
        auth: AgentTransferAuthorization::new(
            proof_key,
            token,
            transfer_uuid,
            artifact_uuid,
            DataPlaneTransferDirection::AgentToServer,
            base_url.clone(),
        ),
        transfer_uuid,
        chunk_size: args.chunk_size,
        observed_manifest_chunk_size: None,
        proof_ns: std::cell::Cell::new(0),
        put_ack_ns: std::cell::Cell::new(0),
    };

    // ---- measurement boundary B start (before resume / stream) ----
    let wall_b = Instant::now();
    let mut resume_ms_total = 0.0f64;
    let mut wall_a: Option<Instant> = None;
    let mut bulk_stream_wall_ms = 0.0f64;
    let mut contamination: Option<String> = None;
    let mut suspensions = 0u32;

    'outer: loop {
        let mut on_progress = |t: ProgressTick| {
            if t.held_chunks == state_chunk_count_snapshot(agreed_chunks)
                || t.held_chunks.is_multiple_of(16)
            {
                log.emit(
                    "info",
                    "probe.stream.progress",
                    &[("held_chunks", V::U(t.held_chunks)), ("held_bytes", V::U(t.held_bytes))],
                );
            }
        };
        let mut on_lifecycle = |e: StreamEvent| match e {
            StreamEvent::ResumeBegin => {}
            StreamEvent::ResumeResult { outcome, held_chunks, .. } => log.emit(
                "info",
                "probe.resume.result",
                &[("outcome", s(outcome)), ("held_chunks", V::U(held_chunks))],
            ),
            StreamEvent::ResumeReconciled { held_count, .. } => {
                if wall_a.is_none() {
                    wall_a = Some(Instant::now());
                }
                log.emit("info", "probe.resume.reconciled", &[("held_count", V::U(held_count))]);
            }
            StreamEvent::PutAuthDenied { chunk_index } => {
                contamination.get_or_insert(format!("put_auth_denied@{chunk_index}"));
                log.emit("warn", "probe.CONTAMINATION.put_auth_denied", &[("chunk_index", V::U(chunk_index))]);
            }
            StreamEvent::PutTransient { chunk_index, local_attempt, detail } => {
                contamination.get_or_insert(format!("put_transient@{chunk_index}"));
                log.emit(
                    "warn",
                    "probe.CONTAMINATION.put_transient",
                    &[("chunk_index", V::U(chunk_index)), ("local_attempt", V::U(local_attempt as u64)), ("detail", s(detail))],
                );
            }
            StreamEvent::ResumeTransient { local_attempt, detail } => {
                contamination.get_or_insert("resume_transient".to_string());
                log.emit(
                    "warn",
                    "probe.CONTAMINATION.resume_transient",
                    &[("local_attempt", V::U(local_attempt as u64)), ("detail", s(detail))],
                );
            }
        };

        let t_resume = Instant::now();
        let outcome = match args.mode {
            StreamMode::Serial => {
                run_stream_pass(&mut state, &reader, &mut dp, &mut on_progress, &mut on_lifecycle)
                    .await
            }
            StreamMode::PrepAhead2 => {
                // The producer thread opens its OWN GENERIC_READ-only handle to
                // the already-resolved + already-safety-PASSED locator (a fresh
                // closure per `'outer` iteration; `run_stream_pass_prep_ahead`
                // takes it `FnOnce`). No handle and no hasher crosses a thread
                // boundary; no `unsafe`.
                let locator = resolved.local_locator.clone();
                let factory = move || -> Result<DeviceReader, String> {
                    let mut c = Counters::default();
                    let src = sources::RawReadSource::open(&locator, &mut c)?;
                    Ok(DeviceReader {
                        src,
                        counters: std::cell::RefCell::new(c),
                    })
                };
                run_stream_pass_prep_ahead(
                    &mut state,
                    factory,
                    &mut dp,
                    &mut on_progress,
                    &mut on_lifecycle,
                )
                .await
            }
            StreamMode::PrepAheadWindow8 | StreamMode::PrepAheadWindow8Batch8 => {
                // Resume is fetched HERE (not inside the driver) so the
                // driver can immutably borrow `dp.auth` for the whole pass —
                // see `RealWindowLauncher`'s doc comment for why that borrow
                // is `'static`-safe despite the concurrent PUT futures.
                let resume = dp.discover_resume().await;
                let locator = resolved.local_locator.clone();
                let factory = move || -> Result<DeviceReader, String> {
                    let mut c = Counters::default();
                    let src = sources::RawReadSource::open(&locator, &mut c)?;
                    Ok(DeviceReader {
                        src,
                        counters: std::cell::RefCell::new(c),
                    })
                };
                let mut launcher = RealWindowLauncher {
                    auth: &dp.auth,
                    transfer_uuid,
                    base_url: base_url.clone(),
                    fingerprint,
                    request_timeout: Duration::from_secs(args.seal_timeout_secs),
                    proof_ns: std::cell::Cell::new(0),
                    put_ack_ns: Arc::new(AtomicU64::new(0)),
                };
                let result = run_stream_pass_window8(
                    &mut state,
                    factory,
                    resume,
                    &mut launcher,
                    &mut on_progress,
                    &mut on_lifecycle,
                )
                .await;
                // Fold the launcher's aggregates into `dp`'s — both are Cell
                // `.set`/`.get` (shared access), so this needs no mutable
                // borrow of `dp` and does not conflict with `launcher.auth`.
                dp.proof_ns.set(dp.proof_ns.get() + launcher.proof_ns.get());
                dp.put_ack_ns
                    .set(dp.put_ack_ns.get() + launcher.put_ack_ns.load(Ordering::Relaxed) as u128);
                result
            }
        };
        resume_ms_total += t_resume.elapsed().as_secs_f64() * 1000.0;

        match outcome {
            Ok(PassOutcome::Complete) => {
                if let Some(a) = wall_a {
                    bulk_stream_wall_ms = a.elapsed().as_secs_f64() * 1000.0;
                }
                let device_read_count = match args.mode {
                    StreamMode::Serial => reader.counters.borrow().data_read_count,
                    // In prep-ahead / window_8 the producer thread owns the
                    // reads; the foreground `reader` is unused. The producer's
                    // read log is the authoritative per-pass read count.
                    StreamMode::PrepAhead2 | StreamMode::PrepAheadWindow8 | StreamMode::PrepAheadWindow8Batch8 => {
                        state.producer_read_log().len() as u64
                    }
                };
                log.emit(
                    "info",
                    "probe.stream.complete",
                    &[
                        ("held_chunks", V::U(state.chunk_count())),
                        ("device_read_count", V::U(device_read_count)),
                        ("prepared_buffer_peak", V::U(state.prepared_peak())),
                        ("observed_manifest_chunk_size", V::I(
                            dp.observed_manifest_chunk_size.map(|n| n as i64).unwrap_or(-1),
                        )),
                    ],
                );
                if matches!(args.mode, StreamMode::PrepAheadWindow8 | StreamMode::PrepAheadWindow8Batch8) {
                    // Raw evidence only (not part of the engine's parsed
                    // schema): the exact PUT start/completion order proof.
                    log.emit(
                        "info",
                        "probe.window8.put_order_evidence",
                        &[
                            ("put_window", V::U(state.put_window())),
                            ("peak_puts_in_flight", V::U(state.peak_puts_in_flight())),
                            ("put_starts_ascending", V::B(state.put_starts_ascending())),
                            ("put_start_order", s(format!("{:?}", state.put_start_order()))),
                            ("put_completion_order", s(format!("{:?}", state.put_completion_order()))),
                        ],
                    );
                }
                break 'outer;
            }
            Ok(_) => {
                suspensions += 1;
                contamination.get_or_insert("stream_suspension".to_string());
                log.emit("warn", "probe.CONTAMINATION.stream_suspended", &[("suspension_count", V::U(suspensions as u64))]);
                if suspensions > MAX_OUTER_SUSPENSIONS {
                    log.emit("error", "probe.stream.too_many_suspensions", &[]);
                    return exit::STREAM_FATAL;
                }
                tokio::time::sleep(Duration::from_millis(1200)).await;
                match obtain_grant(&mut ws, log, action_id, transfer_uuid).await {
                    Ok((k, t, _)) => {
                        dp.auth = AgentTransferAuthorization::new(
                            k, t, transfer_uuid, artifact_uuid,
                            DataPlaneTransferDirection::AgentToServer, base_url.clone(),
                        );
                    }
                    Err(e) => {
                        log.emit("error", "probe.stream.reauth_failed", &[("error", s(e))]);
                        return exit::STREAM_FATAL;
                    }
                }
            }
            Err(StreamError::ChunkVerificationFailed { index }) => {
                log.emit("error", "probe.stream.chunk_verification_failed", &[("chunk_index", V::U(index))]);
                send_action_result(&mut ws, log, action_id, ActionResultOutcome::Failed, "CHUNK_VERIFICATION_FAILED", artifact_uuid).await;
                emit_result_line(log, args, agreed_chunks, Some(transfer_uuid), Some(artifact_uuid), &safety_verdict_token, &clock_verdict, "ChunkVerificationFailed", "failed:chunk_verification");
                return exit::STREAM_FATAL;
            }
            Err(StreamError::Fatal(m)) => {
                log.emit("error", "probe.stream.fatal", &[("detail", s(m))]);
                send_action_result(&mut ws, log, action_id, ActionResultOutcome::Failed, "TRANSFER_ABANDONED", artifact_uuid).await;
                emit_result_line(log, args, agreed_chunks, Some(transfer_uuid), Some(artifact_uuid), &safety_verdict_token, &clock_verdict, "StreamFatal", "failed:stream_fatal");
                return exit::STREAM_FATAL;
            }
        }
    }

    // confirm the Server manifest chunk_size agreed (belt-and-suspenders).
    if let Some(observed) = dp.observed_manifest_chunk_size {
        if observed != args.chunk_size {
            log.emit(
                "error",
                "probe.chunk_agreement.server_manifest_mismatch",
                &[("observed", V::U(observed)), ("expected", V::U(args.chunk_size))],
            );
            return exit::CHUNK_AGREEMENT;
        }
    }

    let artifact_digest_wire = match state.finish_digest() {
        Some(d) => d,
        None => {
            log.emit("error", "probe.stream.incomplete_hash", &[]);
            return exit::STREAM_FATAL;
        }
    };

    // ---- 14. seal + verify ----
    let t_seal = Instant::now();
    let seal = finalize_seal(&mut dp, log, transfer_uuid, state.chunk_count(), &artifact_digest_wire).await;
    let seal_d2_ms = t_seal.elapsed().as_secs_f64() * 1000.0;
    let verified_transfer_wall_ms = wall_b.elapsed().as_secs_f64() * 1000.0;

    match seal {
        SealFinal::Verified => {
            send_action_result(&mut ws, log, action_id, ActionResultOutcome::Succeeded, "TRANSFER_VERIFIED", artifact_uuid).await;
            let case_status = if let Some(c) = &contamination {
                log.emit("warn", "probe.verdict.contaminated", &[("detail", s(c))]);
                "contaminated"
            } else {
                "completed"
            };
            let ts = state.timings();
            let ns_ms = |n: u128| (n as f64 / 1_000_000.0) as i64;
            let device_read_count = match args.mode {
                StreamMode::Serial => reader.counters.borrow().data_read_count,
                StreamMode::PrepAhead2 | StreamMode::PrepAheadWindow8 | StreamMode::PrepAheadWindow8Batch8 => {
                    state.producer_read_log().len() as u64
                }
            };
            emit_full_result(
                log, args, state.chunk_count(), transfer_uuid, artifact_uuid,
                &safety_verdict_token, &clock_verdict,
                bulk_stream_wall_ms, verified_transfer_wall_ms, resume_ms_total, seal_d2_ms,
                device_read_count, state.prepared_peak(),
                state.put_window(), state.put_started_count(), state.put_completed_count(),
                state.peak_puts_in_flight(), state.put_starts_ascending(),
                "Verified", case_status,
                &[
                    ("read_ms", ns_ms(ts.read_ns)),
                    ("chunk_sha_ms", ns_ms(ts.chunk_sha_ns)),
                    ("rolling_sha_ms", ns_ms(ts.rolling_sha_ns)),
                    ("proof_ms", ns_ms(dp.proof_ns.get())),
                    ("put_ack_ms", ns_ms(dp.put_ack_ns.get())),
                ],
            );
            log.emit(
                "info",
                "probe.verdict",
                &[
                    ("probe_pass", V::B(case_status == "completed")),
                    ("artifact_status", s("Verified")),
                    ("case_status", s(case_status)),
                    ("suspensions", V::U(suspensions as u64)),
                ],
            );
            if case_status == "completed" { exit::PASS } else { exit::STREAM_FATAL }
        }
        SealFinal::ArtifactFailed => {
            send_action_result(&mut ws, log, action_id, ActionResultOutcome::Failed, "ARTIFACT_VERIFICATION_FAILED", artifact_uuid).await;
            emit_result_line(log, args, state.chunk_count(), Some(transfer_uuid), Some(artifact_uuid), &safety_verdict_token, &clock_verdict, "Failed", "failed:artifact_verification");
            exit::SEAL_ARTIFACT_FAILED
        }
        SealFinal::Abandoned(reason) => {
            send_action_result(&mut ws, log, action_id, ActionResultOutcome::Failed, "TRANSFER_ABANDONED", artifact_uuid).await;
            log.emit("error", "probe.seal.abandoned", &[("reason", s(reason))]);
            emit_result_line(log, args, state.chunk_count(), Some(transfer_uuid), Some(artifact_uuid), &safety_verdict_token, &clock_verdict, "Abandoned", "failed:seal_abandoned");
            exit::SEAL_ABANDONED
        }
    }
}

fn state_chunk_count_snapshot(n: u64) -> u64 {
    n
}

// ---- structured result line (CaseResult-shaped JSON) ----------------

#[allow(clippy::too_many_arguments)]
fn emit_full_result(
    log: &Log,
    args: &Args,
    chunk_count: u64,
    transfer_uuid: uuid::Uuid,
    artifact_uuid: uuid::Uuid,
    safety_verdict: &str,
    clock_verdict: &str,
    bulk_stream_wall_ms: f64,
    verified_transfer_wall_ms: f64,
    resume_ms: f64,
    seal_d2_ms: f64,
    device_read_count: u64,
    prepared_buffer_peak: u64,
    // window_8 candidate fields — 0/false for serial / prep_ahead_2 (the
    // engine's `S4CaseResult` serde-defaults them identically).
    put_window: u64,
    put_started_count: u64,
    put_completed_count: u64,
    peak_puts_in_flight: u64,
    put_starts_ascending: bool,
    final_artifact_status: &str,
    case_status: &str,
    extra_ms: &[(&str, i64)],
) {
    let mib_s = |wall_ms: f64| {
        if wall_ms <= 0.0 {
            0.0
        } else {
            (args.extent_bytes as f64 / (1024.0 * 1024.0)) / (wall_ms / 1000.0)
        }
    };
    let mut fields: Vec<(&str, V)> = vec![
        ("run_id", s(&args.run_id)),
        ("case_id", s(&args.case_id)),
        ("mode", s(args.mode.wire())),
        ("chunk_size_bytes", V::U(args.chunk_size)),
        ("extent_bytes", V::U(args.extent_bytes)),
        ("chunk_count", V::U(chunk_count)),
        ("device_read_count", V::U(device_read_count)),
        ("prepared_buffer_peak", V::U(prepared_buffer_peak)),
        ("put_window", V::U(put_window)),
        ("put_started_count", V::U(put_started_count)),
        ("put_completed_count", V::U(put_completed_count)),
        ("peak_puts_in_flight", V::U(peak_puts_in_flight)),
        ("put_starts_ascending", V::B(put_starts_ascending)),
        ("transfer_id", s(transfer_uuid.to_string())),
        ("artifact_id", s(artifact_uuid.to_string())),
        ("source_safety_verdict", s(safety_verdict)),
        ("clock_skew_verdict", s(clock_verdict)),
        ("bulk_stream_wall_ms", V::I(bulk_stream_wall_ms as i64)),
        ("bulk_stream_mib_s", V::I(mib_s(bulk_stream_wall_ms) as i64)),
        ("verified_transfer_wall_ms", V::I(verified_transfer_wall_ms as i64)),
        ("verified_transfer_mib_s", V::I(mib_s(verified_transfer_wall_ms) as i64)),
        ("resume_ms", V::I(resume_ms as i64)),
        ("seal_d2_ms", V::I(seal_d2_ms as i64)),
        ("connection_count_expected", V::U(chunk_count + 2)),
        ("final_artifact_status", s(final_artifact_status)),
        ("case_status", s(case_status)),
    ];
    for (k, v) in extra_ms {
        fields.push((k, V::I(*v)));
    }
    log.emit("info", "probe.case_result", &fields);
}

#[allow(clippy::too_many_arguments)]
fn emit_result_line(
    log: &Log,
    args: &Args,
    chunk_count: u64,
    transfer_uuid: Option<uuid::Uuid>,
    artifact_uuid: Option<uuid::Uuid>,
    safety_verdict: &str,
    clock_verdict: &str,
    final_artifact_status: &str,
    case_status: &str,
) {
    log.emit(
        "info",
        "probe.case_result",
        &[
            ("run_id", s(&args.run_id)),
            ("case_id", s(&args.case_id)),
            ("mode", s(args.mode.wire())),
            ("chunk_size_bytes", V::U(args.chunk_size)),
            ("extent_bytes", V::U(args.extent_bytes)),
            ("chunk_count", V::U(chunk_count)),
            ("transfer_id", s(transfer_uuid.map(|u| u.to_string()).unwrap_or_default())),
            ("artifact_id", s(artifact_uuid.map(|u| u.to_string()).unwrap_or_default())),
            ("source_safety_verdict", s(safety_verdict)),
            ("clock_skew_verdict", s(clock_verdict)),
            ("final_artifact_status", s(final_artifact_status)),
            ("case_status", s(case_status)),
        ],
    );
}

/// `--self-check`: host-only structural check (no network, no real device).
/// Proves the safety predicate + chunk arithmetic wiring builds and the
/// non-Windows stub source Accepts. NOT a transfer.
fn self_check() -> i32 {
    let epoch = sources::enumerate();
    let ssd: Vec<&sources::LocalSource> = epoch
        .sources
        .iter()
        .filter(|x| x.product.contains("256GB"))
        .collect();
    if ssd.len() != 1 {
        eprintln!("self-check: expected exactly one stub SSD, got {}", ssd.len());
        return 1;
    }
    let sel = ssd[0];
    let cur = CurrentEpoch::new(
        epoch.observation_id.clone(),
        epoch
            .sources
            .iter()
            .map(|s| EpochEntry {
                agent_source_id: s.agent_source_id.clone(),
                local_locator: s.local_locator.clone(),
            })
            .collect(),
    );
    let resolved = cur
        .resolve(&epoch.observation_id, &sel.agent_source_id)
        .expect("resolver must map the stub tuple");
    let mut counters = Counters::default();
    let src = sources::RawReadSource::open(&resolved.local_locator, &mut counters).expect("stub open");
    let dl = src.device_length();

    // Accept path
    match safety::evaluate(
        &epoch.observation_id,
        std::slice::from_ref(&sel.agent_source_id),
        Some(&resolved),
        Some(sel),
        Some(&dl),
        EXTENT_BYTES,
    ) {
        safety::GateOutcome::Accept { .. } => {}
        safety::GateOutcome::Reject { token } => {
            eprintln!("self-check: stub SSD unexpectedly rejected: {token}");
            return 1;
        }
    }
    // Reject path: a device-path "selection" without an authority tuple.
    match safety::evaluate(
        &epoch.observation_id,
        &[r"\\.\PhysicalDrive0".to_string()],
        None,
        None,
        Some(&dl),
        EXTENT_BYTES,
    ) {
        safety::GateOutcome::Reject { .. } => {}
        safety::GateOutcome::Accept { .. } => {
            eprintln!("self-check: PhysicalDrive0-without-tuple was NOT rejected");
            return 1;
        }
    }
    // bulk_read_count must still be 0 — no bulk read anywhere in self-check.
    if counters.data_read_count != 0 {
        eprintln!("self-check: bulk_read_count = {} (must be 0)", counters.data_read_count);
        return 1;
    }
    // chunk arithmetic
    for (mib, want) in [(8u64, 256u64), (16, 128), (32, 64), (64, 32)] {
        assert_eq!(expected_chunk_count(EXTENT_BYTES, mib * 1024 * 1024).unwrap(), want);
    }
    println!("PROBE_SELF_CHECK_PASS (Accept + Reject paths, bulk_read_count=0, chunk arithmetic)");
    0
}

/// `--pipeline-check`: host synthetic SERIAL vs PREP-AHEAD comparison over the
/// non-Windows stub source + an in-process fake data plane. NO network, NO real
/// device. Proves: (1) the probe's real `run_stream_pass_prep_ahead` call path
/// (mode branch → reader factory → dedicated producer thread → `DeviceReader`
/// second open → `sync_channel`) works; (2) the prep-ahead full-Artifact digest
/// is bit-identical to serial over the same bounded source; (3) the depth-2
/// buffer bound holds (`prepared_buffer_peak == 2`); (4) every chunk is read
/// once, ascending.
fn pipeline_check() -> i32 {
    // Run on a fresh OS thread: `main` is already inside a `#[tokio::main]`
    // runtime and this check builds its own.
    std::thread::spawn(pipeline_check_inner).join().unwrap_or(1)
}

fn pipeline_check_inner() -> i32 {
    use stream::{run_stream_pass, run_stream_pass_prep_ahead, PassOutcome, StreamState};

    // A minimal in-process data plane: accepts every well-formed chunk once.
    struct FakeDp {
        held: std::collections::BTreeMap<u64, String>,
    }
    impl stream::DataPlane for FakeDp {
        async fn discover_resume(&mut self) -> stream::ResumeStatus {
            stream::ResumeStatus::Ok(vec![])
        }
        async fn put_chunk(&mut self, index: u64, digest_wire: &str, bytes: &[u8]) -> stream::PutStatus {
            if sha256_wire(bytes) != digest_wire {
                return stream::PutStatus::DigestMismatch;
            }
            match self.held.insert(index, digest_wire.to_string()) {
                None => stream::PutStatus::Accepted,
                Some(_) => stream::PutStatus::AlreadyHeld,
            }
        }
    }

    let locator = {
        let epoch = sources::enumerate();
        epoch
            .sources
            .iter()
            .find(|s| s.product.contains("256GB"))
            .map(|s| s.local_locator.clone())
            .expect("stub SSD")
    };
    // small bounded extent so the stub `pattern()` generation stays fast
    let chunk = 1u64 << 20; // 1 MiB
    let extent = 6 * chunk + 12345; // short final chunk

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("rt");

    // serial
    let serial_digest = rt.block_on(async {
        let mut c = Counters::default();
        let src = sources::RawReadSource::open(&locator, &mut c).expect("open");
        let reader = DeviceReader { src, counters: std::cell::RefCell::new(c) };
        let mut state = StreamState::new(extent, chunk).unwrap();
        let mut dp = FakeDp { held: Default::default() };
        let out = run_stream_pass(&mut state, &reader, &mut dp, &mut |_| {}, &mut |_| {})
            .await
            .expect("serial pass");
        assert_eq!(out, PassOutcome::Complete);
        assert_eq!(state.prepared_peak(), 0, "serial never uses prep buffers");
        state.finish_digest().expect("serial digest")
    });

    // prep-ahead
    let (prep_digest, peak, read_log_ok) = rt.block_on(async {
        let mut state = StreamState::new(extent, chunk).unwrap();
        let mut dp = FakeDp { held: Default::default() };
        let loc = locator.clone();
        let factory = move || -> Result<DeviceReader, String> {
            let mut c = Counters::default();
            let src = sources::RawReadSource::open(&loc, &mut c)?;
            Ok(DeviceReader { src, counters: std::cell::RefCell::new(c) })
        };
        let out =
            run_stream_pass_prep_ahead(&mut state, factory, &mut dp, &mut |_| {}, &mut |_| {})
                .await
                .expect("prep-ahead pass");
        assert_eq!(out, PassOutcome::Complete);
        let n = state.chunk_count();
        let want: Vec<u64> = (0..n).collect();
        (
            state.finish_digest().expect("prep digest"),
            state.prepared_peak(),
            state.producer_read_log() == &want[..],
        )
    });

    if serial_digest != prep_digest {
        eprintln!("pipeline-check: prep-ahead digest != serial digest");
        return 1;
    }
    if peak != 2 {
        eprintln!("pipeline-check: prepared_buffer_peak = {peak} (want 2)");
        return 1;
    }
    if !read_log_ok {
        eprintln!("pipeline-check: producer read log not 0..N ascending");
        return 1;
    }
    println!(
        "PROBE_PIPELINE_CHECK_PASS (serial digest == prep-ahead digest; prepared_buffer_peak=2; \
         each chunk read once ascending)"
    );
    0
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("--self-check") {
        std::process::exit(self_check());
    }
    if std::env::args().nth(1).as_deref() == Some("--pipeline-check") {
        std::process::exit(pipeline_check());
    }
    let log = Log::new();
    let args = parse_args();
    log.emit(
        "info",
        "probe.start",
        &[
            ("probe_version", s(PROBE_VERSION)),
            ("git_short", s(env!("PROBE_GIT_SHORT"))),
            ("target_triple", s(env!("PROBE_TARGET_TRIPLE"))),
            ("std_os", s(std::env::consts::OS)),
            ("run_id", s(&args.run_id)),
            ("case_id", s(&args.case_id)),
        ],
    );
    let mut counters = Counters::default();
    let code = run(&log, &args, &mut counters).await;
    log.emit(
        "info",
        "probe.end",
        &[
            ("exitcode", V::U(code as u64)),
            ("pass", V::B(code == exit::PASS)),
            ("data_device_open_count", V::U(counters.data_device_open_count)),
            ("data_read_count", V::U(counters.data_read_count)),
        ],
    );
    write_local(&log);
    flush_sink(&log, &args.sink);
    println!();
    println!("BAMEP_I63_STAGE2_PROBE_EXITCODE={code}");
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

#[test]
fn batch8_mode_keeps_candidate_wire_identity() {
    let mode = StreamMode::parse("prep_ahead_window_8_batch_8").unwrap();
    assert_eq!(mode, StreamMode::PrepAheadWindow8Batch8);
    assert_eq!(mode.wire(), "prep_ahead_window_8_batch_8");
    assert_eq!(StreamMode::parse("window_8"), Some(StreamMode::PrepAheadWindow8));
}
