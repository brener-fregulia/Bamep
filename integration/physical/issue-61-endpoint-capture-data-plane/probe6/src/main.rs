//! Issue #61 CP6 — WinPE-native one-chunk physical data-plane probe.
//!
//! THROWAWAY Spike. NOT the Bamep Agent, NOT crates/agent, NOT a production
//! Agent architecture. One process, one WinPE session, fresh coherent lineage:
//!
//!   enumerate -> mint one fresh source epoch -> operator-local SSD selection
//!   -> lab-only coord event (source_observation_id + selected agent_source_id)
//!   -> pinned TLS 1.3 / WSS -> real Agent Protocol v1 authentication
//!   -> InventoryReport carrying that exact epoch
//!   -> (harness persists the InventoryRevision, then dispatches the M1 action)
//!   -> ActionDispatch (bamep.m1.data-plane-transfer) -> ActionAck{Accepted}
//!   -> TransferAuthorizationRequest -> TransferAuthorizationGrant
//!   -> resolver: (obs_id, agent_source_id) -> local SSD locator
//!   -> CreateFileW(GENERIC_READ) -> raw chunk 0 = exactly 8 MiB -> local SHA-256
//!   -> PUT /api/data/v1/transfers/{id}/chunks/0 (real Worker HTTPS)
//!   -> idempotent retry with a FRESH request proof
//!   -> STOP.  No seal, no reconstruct, no verify, no ActionResult{Succeeded}.
//!
//! Reuses the EXISTING M1 reference components from bamep-simulator
//! (connect_pinned_wss / authenticate / send_inventory_report / DataPlaneClient
//! / AgentProofKey / AgentTransferAuthorization / proof transcript). The action
//! is bamep.m1.data-plane-transfer — NOT bamep.m2.endpoint-capture-transfer.

mod resolver;
mod sources;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64_ct::b64url_nopad;
use bamep_agent_protocol::{
    decode, encode, ActionAckMessage, AgentProtocolMessage, ProtocolId,
    TransferAuthorizationRequestMessage,
};
use bamep_simulator::{
    authenticate, connect_pinned_wss, send_inventory_report, AgentProofKey,
    AgentTransferAuthorization, DataPlaneClient, DataPlaneTransferDirection, PutChunkOutcome,
    ServerCertFingerprint, SimulatorHandshakeOutcome, TransferOperation,
};
use futures_util::{SinkExt, StreamExt};
use resolver::{CurrentEpoch, EpochEntry};
use sha2::{Digest, Sha256};
use sources::Counters;
use tokio_tungstenite::tungstenite::Message;

const PROBE_NAME: &str = env!("CARGO_PKG_NAME");
const PROBE_VERSION: &str = env!("CARGO_PKG_VERSION");
const NET_TIMEOUT: Duration = Duration::from_secs(8);
const DISPATCH_WAIT: Duration = Duration::from_secs(120);

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

enum V {
    S(String),
    U(u64),
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
        l.push_str(&format!(r#""ts_ms":{},"seq":{seq},"elapsed_ms":{}"#, now_ms(), self.started.elapsed().as_millis()));
        l.push_str(&format!(r#","level":"{}","event":"{}","probe":"{}""#, esc(level), esc(event), esc(PROBE_NAME)));
        for (k, v) in fields {
            match v {
                V::S(x) => l.push_str(&format!(r#","{}":"{}""#, esc(k), esc(x))),
                V::U(x) => l.push_str(&format!(r#","{}":{}"#, esc(k), x)),
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
    let dir = std::env::var("TEMP").or_else(|_| std::env::var("TMP")).unwrap_or_else(|_| ".".into());
    for p in [format!("{dir}\\bamep-issue61-cp6-probe.ndjson"), "bamep-issue61-cp6-probe.ndjson".into()] {
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
}
fn parse_args() -> Args {
    let mut a = Args {
        sink: "192.168.99.1:9099".into(),
        coord: "192.168.99.1:9106".into(),
        wss: "192.168.99.1:8443".into(),
        pin_hex: String::new(),
        credential_file: String::new(),
        select_model_substr: "256GB".into(),
        chunk_size: 8 * 1024 * 1024,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--sink" => a.sink = it.next().unwrap_or(a.sink),
            "--coord" => a.coord = it.next().unwrap_or(a.coord),
            "--wss" => a.wss = it.next().unwrap_or(a.wss),
            "--pin" => a.pin_hex = it.next().unwrap_or_default(),
            "--auth-credential-file" => a.credential_file = it.next().unwrap_or_default(),
            "--select-model-substr" => a.select_model_substr = it.next().unwrap_or(a.select_model_substr),
            "--chunk-size" => a.chunk_size = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.chunk_size),
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

fn sha256_wire(bytes: &[u8]) -> String {
    b64url_nopad(&Sha256::digest(bytes))
}
fn hexs(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// CP6 exit codes: 0 PASS · 61 enumeration/selection · 62 coord send · 63 WSS/auth
/// · 64 no ActionDispatch · 65 wrong action_type · 66 no grant · 67 resolver
/// · 68 GENERIC_READ open/read · 69 chunk PUT not Accepted · 70 retry not AlreadyHeld.
async fn run(log: &Log, args: &Args, counters: &mut Counters) -> i32 {
    // ---- 1. fresh source epoch -----------------------------------------
    let epoch_src = sources::enumerate();
    let obs_id = epoch_src.observation_id.clone();
    if obs_id.len() != 43 || epoch_src.sources.len() != 2 {
        log.emit("error", "cp6.epoch.bad", &[("observation_id_len", V::U(obs_id.len() as u64)), ("source_count", V::U(epoch_src.sources.len() as u64))]);
        return 61;
    }
    log.emit("info", "cp6.epoch", &[("authority.source_observation_id", s(&obs_id)), ("source_count", V::U(2))]);
    for (i, src) in epoch_src.sources.iter().enumerate() {
        log.emit("info", "cp6.epoch.source", &[
            ("index", V::U(i as u64)),
            ("authority.agent_source_id", s(&src.agent_source_id)),
            ("evidence_only.local_locator", s(&src.local_locator)),
            ("evidence_only.model", s(&src.product)),
            ("evidence_only.serial", s(&src.serial)),
            ("evidence_only.bus_type", s(&src.bus_type)),
        ]);
    }
    debug_assert_eq!(
        {let e = CurrentEpoch::new(obs_id.clone(), Vec::new()); e.observation_id().to_string()},
        obs_id
    );
    let epoch = CurrentEpoch::new(obs_id.clone(), epoch_src.sources.iter().map(|s| EpochEntry {
        agent_source_id: s.agent_source_id.clone(),
        local_locator: s.local_locator.clone(),
    }).collect());
    if epoch.has_duplicate_agent_source_ids() {
        log.emit("error", "cp6.epoch.ambiguous", &[]);
        return 61;
    }
    // operator predicate over LOCAL HARDWARE EVIDENCE only
    let matched: Vec<&sources::LocalSource> = epoch_src.sources.iter().filter(|x| x.product.contains(&args.select_model_substr)).collect();
    if matched.len() != 1 {
        log.emit("error", "cp6.operator_selection.ambiguous", &[("match_count", V::U(matched.len() as u64))]);
        return 61;
    }
    let sel_asid = matched[0].agent_source_id.clone();
    let sel_locator = matched[0].local_locator.clone();
    log.emit("info", "cp6.operator_selection", &[
        ("basis", s("operator predicate over LOCAL HARDWARE EVIDENCE (model substring) — NOT cross-boundary authority")),
        ("matched.evidence_only.model", s(&matched[0].product)),
        ("matched.evidence_only.local_locator", s(&sel_locator)),
        ("resulting.authority.agent_source_id", s(&sel_asid)),
    ]);

    // ---- 2. lab-only coordination event (fixture orchestration) --------
    // NO PhysicalDriveN / model / serial. Just the two opaque values the
    // harness needs to correlate the InventoryRevision it is about to persist.
    let coord_line = format!(
        r#"{{"cp6_coord":"source_selection","source_observation_id":"{}","selected_agent_source_id":"{}"}}"#,
        esc(&obs_id), esc(&sel_asid)
    );
    match args.coord.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(addr) => match TcpStream::connect_timeout(&addr, NET_TIMEOUT) {
            Ok(mut st) => {
                let _ = st.set_write_timeout(Some(NET_TIMEOUT));
                if st.write_all(format!("{coord_line}\n").as_bytes()).is_ok() {
                    let _ = st.flush();
                    log.emit("info", "cp6.coord.sent", &[("coord", s(&args.coord)), ("source_observation_id", s(&obs_id)), ("selected_agent_source_id", s(&sel_asid))]);
                } else {
                    log.emit("error", "cp6.coord.write_failed", &[]);
                    return 62;
                }
            }
            Err(e) => {
                log.emit("error", "cp6.coord.connect_failed", &[("error", s(e.to_string()))]);
                return 62;
            }
        },
        None => {
            log.emit("error", "cp6.coord.bad_addr", &[("coord", s(&args.coord))]);
            return 62;
        }
    }

    // ---- 3. pinned WSS + Agent auth -----------------------------------
    let Some(pin) = parse_pin(&args.pin_hex) else {
        log.emit("error", "cp6.pin.bad", &[]);
        return 63;
    };
    let fingerprint = ServerCertFingerprint::from_sha256_digest(pin);
    let wss_addr: SocketAddr = match args.wss.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => {
            log.emit("error", "cp6.wss.bad_addr", &[]);
            return 63;
        }
    };
    let mut ws = match connect_pinned_wss(wss_addr, "bamep-agent", fingerprint).await {
        Ok(w) => w,
        Err(e) => {
            log.emit("error", "cp6.wss.failed", &[("error", s(format!("{e}")))]);
            return 63;
        }
    };
    log.emit("info", "cp6.wss.established", &[("addr", s(args.wss.clone())), ("pin_sha256", s(&args.pin_hex))]);

    let credential = match std::fs::read_to_string(&args.credential_file) {
        Ok(c) => c.trim().to_string(),
        Err(e) => {
            log.emit("error", "cp6.credential.unreadable", &[("error", s(e.to_string()))]);
            return 63;
        }
    };
    let session_id = match authenticate(&mut ws, &credential).await {
        Ok(SimulatorHandshakeOutcome::Established(m)) => {
            let sid = format!("{:?}", m.body.session_id);
            log.emit("info", "cp6.auth.session_established", &[("session_id", s(&sid)), ("credential_len", V::U(credential.len() as u64))]);
            sid
        }
        Ok(SimulatorHandshakeOutcome::Rejected(_)) => {
            log.emit("error", "cp6.auth.rejected", &[]);
            return 63;
        }
        Err(e) => {
            log.emit("error", "cp6.auth.error", &[("error", s(format!("{e}")))]);
            return 63;
        }
    };

    // ---- 4. InventoryReport carrying this exact fresh epoch -----------
    let mut inv = serde_json::Map::new();
    inv.insert("probe".into(), serde_json::json!(PROBE_NAME));
    inv.insert("probe_version".into(), serde_json::json!(PROBE_VERSION));
    inv.insert("host".into(), serde_json::json!({
        "os": std::env::var("OS").unwrap_or_default(),
        "computername": std::env::var("COMPUTERNAME").unwrap_or_default(),
    }));
    inv.insert("capture_source_observation_id".into(), serde_json::json!(obs_id));
    inv.insert("capturable_sources".into(), serde_json::json!(
        epoch_src.sources.iter().map(|x| serde_json::json!({ "agent_source_id": x.agent_source_id })).collect::<Vec<_>>()
    ));
    if let Err(e) = send_inventory_report(&mut ws, inv).await {
        log.emit("error", "cp6.inventory.send_failed", &[("error", s(format!("{e}")))]);
        return 63;
    }
    log.emit("info", "cp6.inventory.report_sent", &[("capture_source_observation_id", s(&obs_id)), ("capturable_sources_count", V::U(2))]);

    // ---- 5. wait for the harness to dispatch the M1 action -----------
    let dispatch = {
        let deadline = Instant::now() + DISPATCH_WAIT;
        loop {
            if Instant::now() >= deadline {
                log.emit("error", "cp6.dispatch.timeout", &[("waited_ms", V::U(DISPATCH_WAIT.as_millis() as u64))]);
                return 64;
            }
            let frame = match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
                Ok(Some(Ok(f))) => f,
                Ok(Some(Err(e))) => {
                    log.emit("error", "cp6.wss.recv_error", &[("error", s(e.to_string()))]);
                    return 64;
                }
                Ok(None) => {
                    log.emit("error", "cp6.wss.closed", &[]);
                    return 64;
                }
                Err(_) => continue, // 5s idle: keep waiting until DISPATCH_WAIT
            };
            let Message::Text(text) = frame else { continue };
            match decode(&text) {
                Ok(AgentProtocolMessage::ActionDispatch(d)) => break d,
                Ok(AgentProtocolMessage::ProtocolError(_)) => {
                    log.emit("info", "cp6.wss.barrier_protocol_error", &[("note", s("expected inventory barrier — tolerated"))]);
                }
                Ok(other) => log.emit("info", "cp6.wss.frame", &[("kind", s(format!("{other:?}").split_whitespace().next().unwrap_or("?").to_string()))]),
                Err(e) => log.emit("warn", "cp6.wss.decode_failed", &[("error", s(e.to_string()))]),
            }
        }
    };

    let action_id = dispatch.body.action_id;
    let action_type = dispatch.body.action_type.clone();
    if action_type != "bamep.m1.data-plane-transfer" {
        log.emit("error", "cp6.dispatch.wrong_action", &[("action_type", s(&action_type))]);
        return 65;
    }
    let p = &dispatch.body.parameters;
    let transfer_id_s = p.get("transfer_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let artifact_id_s = p.get("artifact_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let chunk_size = p.get("chunk_size").and_then(|v| v.as_u64()).unwrap_or(0);
    let (Ok(transfer_uuid), Ok(artifact_uuid)) = (transfer_id_s.parse::<uuid::Uuid>(), artifact_id_s.parse::<uuid::Uuid>()) else {
        log.emit("error", "cp6.dispatch.bad_ids", &[("transfer_id", s(&transfer_id_s)), ("artifact_id", s(&artifact_id_s))]);
        return 65;
    };
    log.emit("info", "cp6.dispatch.received", &[
        ("action_type", s(&action_type)),
        ("action_id", s(format!("{action_id:?}"))),
        ("transfer_id", s(&transfer_id_s)),
        ("artifact_id", s(&artifact_id_s)),
        ("chunk_size", V::U(chunk_size)),
        ("digest_algorithm", s(p.get("digest_algorithm").and_then(|v| v.as_str()).unwrap_or("").to_string())),
        ("direction", s(p.get("direction").and_then(|v| v.as_str()).unwrap_or("").to_string())),
    ]);
    let effective_chunk_size = if chunk_size > 0 { chunk_size } else { args.chunk_size };

    // ---- 6. ActionAck{Accepted} -> Attempt InProgress ---------------
    let ack = ActionAckMessage::accepted(action_id);
    ws.send(Message::text(encode(&AgentProtocolMessage::ActionAck(ack)).unwrap())).await.ok();
    log.emit("info", "cp6.action.ack_sent", &[("outcome", s("Accepted"))]);

    // ---- 7. TransferAuthorizationRequest -> Grant -------------------
    let proof_key = AgentProofKey::generate();
    let transfer_pid = ProtocolId::from_uuid(transfer_uuid).unwrap();
    let req = TransferAuthorizationRequestMessage::new(action_id, transfer_pid, proof_key.public_key_wire());
    ws.send(Message::text(encode(&AgentProtocolMessage::TransferAuthorizationRequest(req)).unwrap())).await.ok();
    log.emit("info", "cp6.transfer_auth.request_sent", &[("proof_public_key_len", V::U(proof_key.public_key_wire().len() as u64))]);

    let grant = {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if Instant::now() >= deadline {
                log.emit("error", "cp6.transfer_auth.timeout", &[]);
                return 66;
            }
            match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => match decode(&t) {
                    Ok(AgentProtocolMessage::TransferAuthorizationGrant(g)) => break g,
                    Ok(AgentProtocolMessage::TransferAuthorizationDenied(_)) => {
                        log.emit("error", "cp6.transfer_auth.denied", &[]);
                        return 66;
                    }
                    Ok(other) => log.emit("info", "cp6.wss.frame", &[("kind", s(format!("{other:?}").split_whitespace().next().unwrap_or("?").to_string()))]),
                    Err(_) => {}
                },
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => {
                    log.emit("error", "cp6.wss.recv_error", &[("error", s(e.to_string()))]);
                    return 66;
                }
                Ok(None) => {
                    log.emit("error", "cp6.wss.closed", &[]);
                    return 66;
                }
                Err(_) => {}
            }
        }
    };
    let token = grant.body.token.clone();
    let base_url = grant.body.data_plane_base_url.clone();
    log.emit("info", "cp6.transfer_auth.grant_received", &[
        ("token_len", V::U(token.len() as u64)),
        ("data_plane_base_url", s(&base_url)),
    ]);

    // ---- 8. resolver: (obs_id, agent_source_id) -> local SSD --------
    counters.resolution_attempt_count += 1;
    let resolved = match epoch.resolve(&obs_id, &sel_asid) {
        Ok(r) => {
            counters.resolution_success_count += 1;
            r
        }
        Err(e) => {
            log.emit("error", "cp6.resolve.failed", &[("detail", s(format!("{e:?}")))]);
            return 67;
        }
    };
    let locator_ok = resolved.local_locator == sel_locator;
    log.emit("info", "cp6.resolve.current", &[
        ("authority.source_observation_id", s(&obs_id)),
        ("authority.agent_source_id", s(&sel_asid)),
        ("evidence_only.resolved_local_locator", s(&resolved.local_locator)),
        ("locator_matches_operator_selection", V::B(locator_ok)),
    ]);
    if !locator_ok {
        return 67;
    }

    // ---- 9. GENERIC_READ raw chunk 0 = exactly 8 MiB ---------------
    let src = match sources::RawReadSource::open(&resolved.local_locator, counters) {
        Ok(s) => s,
        Err(e) => {
            log.emit("error", "cp6.source.open_failed", &[("error", s(e))]);
            return 68;
        }
    };
    log.emit("info", "cp6.source.opened", &[
        ("evidence_only.opened_locator", s(src.locator())),
        ("desired_access", s("GENERIC_READ")),
        ("generic_write_requested", V::B(src.generic_write_requested())),
    ]);
    let rr = src.read_at("chunk-0", 0, effective_chunk_size, counters);
    if !rr.ok || rr.actual_len != effective_chunk_size {
        log.emit("error", "cp6.source.read_failed", &[("actual_len", V::U(rr.actual_len)), ("requested_len", V::U(effective_chunk_size))]);
        return 68;
    }
    // reconstruct the exact bytes for upload (a second identical read)
    let chunk_bytes = {
        // The RawReadSource hashes but does not return bytes; re-open a fresh
        // read that returns the buffer for the PUT. On WinPE this is a second
        // GENERIC_READ of the same 8 MiB range.
        src.read_bytes_at(0, effective_chunk_size, counters)
    };
    let chunk_bytes = match chunk_bytes {
        Ok(b) => b,
        Err(e) => {
            log.emit("error", "cp6.source.read_bytes_failed", &[("error", s(e))]);
            return 68;
        }
    };
    let local_sha_hex = hexs(&Sha256::digest(&chunk_bytes));
    let local_sha_wire = sha256_wire(&chunk_bytes);
    if local_sha_hex != rr.sha256_hex {
        log.emit("error", "cp6.source.read_nondeterministic", &[("first", s(&rr.sha256_hex)), ("second", s(&local_sha_hex))]);
        return 68;
    }
    log.emit("info", "cp6.source_read", &[
        ("chunk_index", V::U(0)),
        ("offset", V::U(0)),
        ("length", V::U(effective_chunk_size)),
        ("sha256_hex", s(&local_sha_hex)),
        ("sha256_wire", s(&local_sha_wire)),
    ]);

    // ---- 10. PUT chunk 0 to the real Worker HTTPS -----------------
    let client = match DataPlaneClient::connect(&base_url, fingerprint) {
        Ok(c) => c,
        Err(e) => {
            log.emit("error", "cp6.dataplane.connect_failed", &[("error", s(format!("{e}")))]);
            return 69;
        }
    };
    let auth = AgentTransferAuthorization::new(
        proof_key,
        token,
        transfer_uuid,
        artifact_uuid,
        DataPlaneTransferDirection::AgentToServer,
        base_url.clone(),
    );
    let proof1 = auth.create_proof_now(TransferOperation::ChunkUpload, Some(0)).expect("proof");
    let put1 = match client.put_chunk(auth.token(), transfer_uuid, 0, &local_sha_wire, &proof1, chunk_bytes.clone()).await {
        Ok(o) => o,
        Err(e) => {
            log.emit("error", "cp6.chunk_put.transport_failed", &[("error", s(format!("{e}")))]);
            return 69;
        }
    };
    log.emit("info", "cp6.chunk_put", &[("chunk_index", V::U(0)), ("result", s(format!("{put1:?}")))]);
    if !matches!(put1, PutChunkOutcome::Accepted { chunk_index: 0 }) {
        return 69;
    }

    // ---- 11. idempotent retry with a FRESH request proof ---------
    let proof2 = auth.create_proof_now(TransferOperation::ChunkUpload, Some(0)).expect("proof");
    let fresh_proof = proof2.proof_id_wire != proof1.proof_id_wire;
    let put2 = match client.put_chunk(auth.token(), transfer_uuid, 0, &local_sha_wire, &proof2, chunk_bytes).await {
        Ok(o) => o,
        Err(e) => {
            log.emit("error", "cp6.chunk_retry.transport_failed", &[("error", s(format!("{e}")))]);
            return 70;
        }
    };
    log.emit("info", "cp6.chunk_retry", &[
        ("chunk_index", V::U(0)),
        ("result", s(format!("{put2:?}"))),
        ("fresh_request_proof", V::B(fresh_proof)),
    ]);
    if !matches!(put2, PutChunkOutcome::AlreadyHeld { chunk_index: 0 }) {
        return 70;
    }

    log.emit("info", "cp6.verdict", &[
        ("cp6_pass", V::B(true)),
        ("session_id", s(&session_id)),
        ("chunk0_accepted", V::B(true)),
        ("retry_already_held", V::B(true)),
        ("fresh_request_proof_on_retry", V::B(fresh_proof)),
        ("sealed", V::B(false)),
        ("action_result_sent", V::B(false)),
        ("data_device_open_count", V::U(counters.data_device_open_count)),
        ("data_read_count", V::U(counters.data_read_count)),
        ("label", s("probe-local Spike evidence — one M1 chunk; NOT bamep.m2.endpoint-capture-transfer")),
    ]);
    0
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let log = Log::new();
    let args = parse_args();
    log.emit("info", "probe.start", &[
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
        ("chunk_size", V::U(args.chunk_size)),
    ]);

    let mut counters = Counters::default();
    let exit = run(&log, &args, &mut counters).await;

    log.emit("info", "probe.end", &[
        ("cp6_exitcode", V::U(exit as u64)),
        ("cp6_pass", V::B(exit == 0)),
        ("data_device_open_count", V::U(counters.data_device_open_count)),
        ("data_read_count", V::U(counters.data_read_count)),
    ]);
    write_local(&log);
    flush_sink(&log, &args.sink);
    write_local(&log);

    println!();
    println!("CP6_EXITCODE={exit}");
    let _ = std::io::stdout().flush();
    std::process::exit(exit);
}
