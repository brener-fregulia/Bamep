//! Issue #60 Physical Integration Spike — WinPE-native Bamep probe.
//!
//! THROWAWAY Spike artifact. This is NOT the Bamep Agent and defines no
//! production Agent architecture. Its only job is to produce honest,
//! machine-readable physical evidence, checkpoint by checkpoint.
//!
//! Checkpoint 2 scope (this version): prove a Bamep-owned native x86-64
//! executable actually starts inside the #53-validated WinPE, prints its
//! build identity, can observe basic networking, and emits errors visibly.
//! No TLS, no WSS, no Agent Protocol yet.
//!
//! Evidence sinks, all attempted, each failure itself logged, none fatal
//! except a failed local-file write:
//!   1. stderr            — immediate human diagnostic;
//!   2. local NDJSON file — `%TEMP%\bamep-winpe-probe.ndjson` (cwd fallback);
//!   3. network NDJSON    — one TCP connection to the lab evidence sink.
//!
//! Every emitted line is one JSON object (NDJSON).
//!
//! Checkpoint 3 adds an optional `--wss <addr> --pin <sha256-hex>` step:
//! after the Checkpoint 2 observations, the probe crosses the real Agent
//! Protocol v1 transport boundary — explicit TLS 1.3 (pinned Server leaf
//! certificate) followed by the WebSocket upgrade — against the
//! physical-integration harness. No authentication yet.

mod pinned;
mod sources;

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROBE_NAME: &str = env!("CARGO_PKG_NAME");
const PROBE_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_SINK: &str = "192.168.99.1:9099";
const NET_TIMEOUT: Duration = Duration::from_secs(5);

/// One structured field value. Kept tiny on purpose.
enum F {
    S(String),
    I(i64),
    B(bool),
}

fn s(v: impl Into<String>) -> F {
    F::S(v.into())
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

struct Log {
    started: Instant,
    seq: Mutex<u64>,
    buffer: Mutex<Vec<String>>,
}

impl Log {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            seq: Mutex::new(0),
            buffer: Mutex::new(Vec::new()),
        }
    }

    fn emit(&self, level: &str, event: &str, fields: &[(&str, F)]) {
        let seq = {
            let mut g = self.seq.lock().unwrap();
            *g += 1;
            *g
        };
        let mut line = String::new();
        line.push('{');
        line.push_str(&format!(r#""ts_ms":{}"#, now_millis()));
        line.push_str(&format!(r#","seq":{seq}"#));
        line.push_str(&format!(
            r#","elapsed_ms":{}"#,
            self.started.elapsed().as_millis()
        ));
        line.push_str(&format!(r#","level":"{}""#, json_escape(level)));
        line.push_str(&format!(r#","event":"{}""#, json_escape(event)));
        line.push_str(&format!(r#","probe":"{}""#, json_escape(PROBE_NAME)));
        for (k, v) in fields {
            match v {
                F::S(x) => line.push_str(&format!(r#","{}":"{}""#, json_escape(k), json_escape(x))),
                F::I(x) => line.push_str(&format!(r#","{}":{}"#, json_escape(k), x)),
                F::B(x) => line.push_str(&format!(r#","{}":{}"#, json_escape(k), x)),
            }
        }
        line.push('}');

        eprintln!("{line}");
        let _ = std::io::stderr().flush();
        self.buffer.lock().unwrap().push(line);
    }

    fn snapshot(&self) -> String {
        let mut body = self.buffer.lock().unwrap().join("\n");
        body.push('\n');
        body
    }
}

fn resolve_addr(raw: &str) -> Option<std::net::SocketAddr> {
    raw.to_socket_addrs().ok().and_then(|mut it| it.next())
}

/// Observe local networking without any Win32 binding: which local address
/// the OS would use to reach the sink, and whether a TCP connection to it
/// completes. Returns the live stream on success so the caller can reuse it.
fn observe_network(log: &Log, sink: std::net::SocketAddr) -> Option<TcpStream> {
    match UdpSocket::bind("0.0.0.0:0").and_then(|u| {
        u.connect(sink)?;
        u.local_addr()
    }) {
        Ok(local) => log.emit(
            "info",
            "net.route_probe",
            &[
                ("sink", s(sink.to_string())),
                ("local_addr", s(local.to_string())),
                ("local_ip", s(local.ip().to_string())),
            ],
        ),
        Err(e) => log.emit(
            "warn",
            "net.route_probe.failed",
            &[("sink", s(sink.to_string())), ("error", s(e.to_string()))],
        ),
    }

    match TcpStream::connect_timeout(&sink, NET_TIMEOUT) {
        Ok(stream) => {
            let local = stream
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            log.emit(
                "info",
                "net.tcp_connect.ok",
                &[("local_addr", s(local)), ("peer_addr", s(peer))],
            );
            Some(stream)
        }
        Err(e) => {
            log.emit(
                "warn",
                "net.tcp_connect.failed",
                &[
                    ("sink", s(sink.to_string())),
                    ("error", s(e.to_string())),
                    ("kind", s(format!("{:?}", e.kind()))),
                ],
            );
            None
        }
    }
}

fn capture_ipconfig(log: &Log) {
    match std::process::Command::new("ipconfig").arg("/all").output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            for raw_line in text.lines() {
                let trimmed = raw_line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                log.emit("info", "net.ipconfig.line", &[("text", s(trimmed))]);
            }
            log.emit(
                "info",
                "net.ipconfig.done",
                &[("exit_ok", F::B(out.status.success()))],
            );
        }
        Err(e) => log.emit(
            "warn",
            "net.ipconfig.failed",
            &[("error", s(e.to_string()))],
        ),
    }
}

fn write_local_file(log: &Log) -> bool {
    let name = "bamep-winpe-probe.ndjson";
    let mut candidates = Vec::new();
    if let Ok(tmp) = std::env::var("TEMP") {
        candidates.push(std::path::PathBuf::from(tmp).join(name));
    }
    if let Ok(tmp) = std::env::var("TMP") {
        candidates.push(std::path::PathBuf::from(tmp).join(name));
    }
    candidates.push(std::path::PathBuf::from(name));

    let body = log.snapshot();
    for path in candidates {
        match std::fs::File::create(&path).and_then(|mut f| f.write_all(body.as_bytes())) {
            Ok(()) => {
                log.emit(
                    "info",
                    "sink.file.ok",
                    &[("path", s(path.display().to_string()))],
                );
                return true;
            }
            Err(e) => log.emit(
                "warn",
                "sink.file.failed",
                &[
                    ("path", s(path.display().to_string())),
                    ("error", s(e.to_string())),
                ],
            ),
        }
    }
    false
}

fn ship_over_network(log: &Log, mut stream: TcpStream) {
    let body = log.snapshot();
    let _ = stream.set_write_timeout(Some(NET_TIMEOUT));
    let _ = stream.set_read_timeout(Some(NET_TIMEOUT));
    match stream.write_all(body.as_bytes()).and_then(|()| stream.flush()) {
        Ok(()) => {
            let _ = stream.shutdown(Shutdown::Write);
            let mut ack = String::new();
            let _ = stream.take(512).read_to_string(&mut ack);
            log.emit(
                "info",
                "sink.network.ok",
                &[
                    ("bytes", F::I(body.len() as i64)),
                    ("ack", s(ack.trim())),
                ],
            );
        }
        Err(e) => log.emit(
            "warn",
            "sink.network.failed",
            &[("error", s(e.to_string()))],
        ),
    }
}

/// Cross the real Agent Protocol v1 transport boundary: explicit pinned
/// TLS 1.3, then the WebSocket upgrade, against the physical-integration
/// harness. Pre-authentication transport proof only (Checkpoint 3).
fn cross_wss_boundary(
    log: &Log,
    addr_raw: &str,
    pin_hex: &str,
    auth_cred_file: Option<&str>,
    inventory_mode: bool,
) -> bool {
    let pin = match pinned::parse_pin_hex(pin_hex) {
        Some(p) => p,
        None => {
            log.emit("error", "wss.bad_pin", &[("pin", s(pin_hex))]);
            return false;
        }
    };
    let addr: SocketAddr = match resolve_addr(addr_raw) {
        Some(a) => a,
        None => {
            log.emit("error", "wss.addr_unresolved", &[("addr", s(addr_raw))]);
            return false;
        }
    };
    log.emit(
        "info",
        "wss.begin",
        &[("addr", s(addr.to_string())), ("pin_sha256", s(pin_hex))],
    );

    let tcp = match TcpStream::connect_timeout(&addr, NET_TIMEOUT) {
        Ok(t) => t,
        Err(e) => {
            log.emit(
                "error",
                "wss.tcp_failed",
                &[("error", s(e.to_string())), ("kind", s(format!("{:?}", e.kind())))],
            );
            return false;
        }
    };
    let _ = tcp.set_read_timeout(Some(NET_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(NET_TIMEOUT));
    log.emit(
        "info",
        "wss.tcp_connected",
        &[
            ("local_addr", s(tcp.local_addr().map(|a| a.to_string()).unwrap_or_default())),
            ("peer_addr", s(addr.to_string())),
        ],
    );

    let config = match pinned::pinned_tls13_client_config(pin) {
        Ok(c) => c,
        Err(e) => {
            log.emit("error", "wss.tls_config_failed", &[("error", s(e.to_string()))]);
            return false;
        }
    };
    let server_name = rustls::pki_types::ServerName::try_from("bamep-agent").unwrap();
    let conn = match rustls::ClientConnection::new(Arc::new(config), server_name) {
        Ok(c) => c,
        Err(e) => {
            log.emit("error", "wss.tls_client_init_failed", &[("error", s(e.to_string()))]);
            return false;
        }
    };
    let mut tls = rustls::StreamOwned::new(conn, tcp);

    // TLS 1.3 must fully terminate before the WebSocket upgrade is attempted.
    if let Err(e) = tls.conn.complete_io(&mut tls.sock) {
        log.emit(
            "error",
            "wss.tls_handshake_failed",
            &[("error", s(e.to_string())), ("kind", s(format!("{:?}", e.kind())))],
        );
        return false;
    }
    let version = format!("{:?}", tls.conn.protocol_version());
    let suite = format!(
        "{:?}",
        tls.conn.negotiated_cipher_suite().map(|c| c.suite())
    );
    log.emit(
        "info",
        "wss.tls_handshake_ok",
        &[("negotiated_version", s(version)), ("cipher_suite", s(suite))],
    );

    let (mut ws, resp) = match tungstenite::client("wss://bamep-agent/", tls) {
        Ok(pair) => pair,
        Err(e) => {
            log.emit("error", "wss.upgrade_failed", &[("error", s(e.to_string()))]);
            return false;
        }
    };
    log.emit(
        "info",
        "wss.upgraded",
        &[("http_status", F::I(resp.status().as_u16() as i64))],
    );

    let ok = match auth_cred_file {
        Some(path) => authenticate(log, &mut ws, path),
        None => {
            let payload = r#"{"probe":"cp3-hello","note":"pre-auth transport proof only"}"#;
            if let Err(e) = ws.send(tungstenite::Message::text(payload)) {
                log.emit("error", "wss.send_failed", &[("error", s(e.to_string()))]);
                false
            } else {
                log.emit("info", "wss.sent", &[("bytes", F::I(payload.len() as i64))]);
                match ws.read() {
                    Ok(msg) => {
                        let text = msg.to_text().unwrap_or("<non-text>").to_string();
                        log.emit("info", "wss.recv", &[("text", s(text))]);
                        true
                    }
                    Err(e) => {
                        log.emit("warn", "wss.recv_failed", &[("error", s(e.to_string()))]);
                        false
                    }
                }
            }
        }
    };

    let ok = if ok && inventory_mode {
        run_inventory_reports(log, &mut ws)
    } else {
        ok
    };

    let _ = ws.close(None);
    let _ = ws.flush();
    log.emit("info", "wss.done", &[("ok", F::B(ok))]);
    ok
}

/// Checkpoint 6: over the authenticated session, drive the #59 inventory-on-
/// change / continuity-epoch behaviour:
///   1. report inventory carrying source-epoch A;
///   2. report the identical inventory again (must NOT create a revision);
///   3. mint a fresh source-observation epoch B (same disks) and report —
///      the JSON changes, so a new InventoryRevisionId must be created.
/// Each report is followed by a deliberately malformed frame used as an
/// in-order barrier: its `ProtocolError` reply can only arrive after the
/// Server finished processing the preceding report.
fn run_inventory_reports<S: Read + Write>(log: &Log, ws: &mut tungstenite::WebSocket<S>) -> bool {
    use bamep_agent_protocol::{encode, AgentProtocolMessage, InventoryReportMessage};

    const MALFORMED_BARRIER: &str = r#"{"type":"InventoryReport","inventory":[]}"#;

    let mut send_report = |tag: &str, map: serde_json::Map<String, serde_json::Value>| -> bool {
        let obs = map
            .get("capture_source_observation_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let count = map
            .get("capturable_sources")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let wire = match encode(&AgentProtocolMessage::InventoryReport(
            InventoryReportMessage::new(map),
        )) {
            Ok(w) => w,
            Err(e) => {
                log.emit("error", "inventory.encode_failed", &[("tag", s(tag)), ("error", s(e.to_string()))]);
                return false;
            }
        };
        if let Err(e) = ws.send(tungstenite::Message::text(wire)) {
            log.emit("error", "inventory.send_failed", &[("tag", s(tag)), ("error", s(e.to_string()))]);
            return false;
        }
        log.emit(
            "info",
            "inventory.report_sent",
            &[
                ("tag", s(tag)),
                ("capture_source_observation_id", s(obs)),
                ("capturable_sources_count", F::I(count as i64)),
            ],
        );
        // Barrier.
        if ws.send(tungstenite::Message::text(MALFORMED_BARRIER)).is_err() {
            return false;
        }
        match ws.read() {
            Ok(m) => {
                let t = m.to_text().unwrap_or("").to_string();
                let got = t.contains("ProtocolError");
                log.emit(
                    "info",
                    "inventory.barrier_ack",
                    &[("tag", s(tag)), ("protocol_error", F::B(got)), ("text", s(t))],
                );
                got
            }
            Err(e) => {
                log.emit("error", "inventory.barrier_no_reply", &[("tag", s(tag)), ("error", s(e.to_string()))]);
                false
            }
        }
    };

    let epoch_a = sources::enumerate();
    log_source_epoch(log, &epoch_a, "A");
    if !epoch_selectable(log, &epoch_a, "A") {
        return false;
    }
    let inv_a = build_inventory_map(&epoch_a);

    let mut ok = send_report("epoch_a_first", inv_a.clone());
    ok &= send_report("epoch_a_repeat_unchanged", inv_a);

    let epoch_b = sources::enumerate();
    log_source_epoch(log, &epoch_b, "B");
    if !epoch_selectable(log, &epoch_b, "B") {
        return false;
    }
    let inv_b = build_inventory_map(&epoch_b);
    ok &= send_report("epoch_b_fresh", inv_b);

    log.emit("info", "inventory.reports_done", &[("all_barriers_ok", F::B(ok))]);
    ok
}

/// #59 RF-4 fail-closed gate: an epoch whose `capturable_sources` contains a
/// duplicate `agent_source_id` yields **no** selectable cross-boundary source
/// projection. The probe must not build or send an `InventoryReport` for it;
/// `--inventory-report` then fails with the ordinary failure semantics
/// (probe exit code 5). This is a local structural guard — it does not, and
/// this Spike did not, physically exercise a malformed-duplicate consumer
/// path (Issue #60 CP7 remains NOT EXERCISED).
fn epoch_selectable(log: &Log, epoch: &sources::SourceEpoch, tag: &str) -> bool {
    if epoch.duplicate_ids.is_empty() {
        return true;
    }
    log.emit(
        "error",
        "inventory.aborted_duplicate_source_ids",
        &[
            ("epoch", s(tag)),
            ("duplicate_ids", s(epoch.duplicate_ids.join(","))),
            ("note", s("#59 RF-4: no selectable cross-boundary projection; InventoryReport not sent")),
        ],
    );
    false
}

/// Builds the reported `InventoryReport.inventory` object: stable host facts
/// plus the #59 normative source fragment. Only the source fragment changes
/// between epoch A and epoch B.
fn build_inventory_map(epoch: &sources::SourceEpoch) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::{json, Value};
    let mut m = serde_json::Map::new();
    m.insert("probe".into(), json!("bamep-winpe-probe"));
    m.insert("probe_version".into(), json!(PROBE_VERSION));
    m.insert(
        "host".into(),
        json!({
            "computername": std::env::var("COMPUTERNAME").unwrap_or_default(),
            "os": std::env::var("OS").unwrap_or_default(),
        }),
    );
    m.insert(
        "capture_source_observation_id".into(),
        Value::String(epoch.observation_id.clone()),
    );
    m.insert(
        "capturable_sources".into(),
        Value::Array(
            epoch
                .sources
                .iter()
                .map(|src| json!({ "agent_source_id": src.agent_source_id }))
                .collect(),
        ),
    );
    m
}

fn log_source_epoch(log: &Log, epoch: &sources::SourceEpoch, tag: &str) {
    for (i, src) in epoch.sources.iter().enumerate() {
        // LOCAL LAB EVIDENCE ONLY — never a cross-boundary identity.
        log.emit(
            "info",
            "source.local_evidence",
            &[
                ("epoch", s(tag)),
                ("index", F::I(i as i64)),
                ("agent_source_id", s(&src.agent_source_id)),
                ("local_locator", s(&src.local_locator)),
                ("size_bytes", F::I(src.size_bytes as i64)),
                ("vendor", s(&src.vendor)),
                ("product", s(&src.product)),
                ("serial", s(&src.serial)),
                ("bus_type", s(&src.bus_type)),
                ("removable", F::B(src.removable)),
            ],
        );
    }
    if !epoch.duplicate_ids.is_empty() {
        log.emit(
            "error",
            "source.duplicate_fail_closed",
            &[
                ("epoch", s(tag)),
                ("duplicate_ids", s(epoch.duplicate_ids.join(","))),
                ("note", s("ambiguous projection; no SourceReference selectable (#59 RF-4)")),
            ],
        );
    }
    // When an epoch is ambiguous (#59 RF-4), do not even render a
    // cross-boundary fragment for it — it is not a selectable projection.
    let fragment = if epoch.duplicate_ids.is_empty() {
        s(epoch.capture_fragment_json())
    } else {
        s("<withheld: duplicate agent_source_id — no selectable projection>")
    };
    log.emit(
        "info",
        "source.capture_epoch",
        &[
            ("epoch", s(tag)),
            ("capture_source_observation_id", s(&epoch.observation_id)),
            ("observation_id_len", F::I(epoch.observation_id.len() as i64)),
            ("capturable_sources_count", F::I(epoch.sources.len() as i64)),
            ("duplicates", F::B(!epoch.duplicate_ids.is_empty())),
            ("capture_fragment_json", fragment),
        ],
    );
}

/// Checkpoint 4: send a real Agent Protocol v1 `AuthRequest` carrying the
/// disposable enrollment credential and interpret the framed response.
/// Returns true only on `SessionEstablished`.
fn authenticate<S: Read + Write>(
    log: &Log,
    ws: &mut tungstenite::WebSocket<S>,
    cred_path: &str,
) -> bool {
    use bamep_agent_protocol::{decode, encode, AgentProtocolMessage, AuthRequestMessage};

    let wire = match std::fs::read_to_string(cred_path) {
        Ok(c) => c.trim().to_string(),
        Err(e) => {
            log.emit(
                "error",
                "auth.credential_file_unreadable",
                &[("path", s(cred_path)), ("error", s(e.to_string()))],
            );
            return false;
        }
    };
    if wire.is_empty() {
        log.emit("error", "auth.credential_file_empty", &[("path", s(cred_path))]);
        return false;
    }

    let credential_len = wire.len() as i64;
    let request = AuthRequestMessage::new(wire);
    let request_id = request.envelope.message_id;
    let encoded = match encode(&AgentProtocolMessage::AuthRequest(request)) {
        Ok(e) => e,
        Err(e) => {
            log.emit("error", "auth.encode_failed", &[("error", s(e.to_string()))]);
            return false;
        }
    };
    // Never log `encoded` — it carries the bearer credential.
    if let Err(e) = ws.send(tungstenite::Message::text(encoded)) {
        log.emit("error", "auth.send_failed", &[("error", s(e.to_string()))]);
        return false;
    }
    log.emit("info", "auth.request_sent", &[("credential_len", F::I(credential_len))]);

    let frame = match ws.read() {
        Ok(f) => f,
        Err(e) => {
            log.emit("error", "auth.no_response", &[("error", s(e.to_string()))]);
            return false;
        }
    };
    let text = match frame.to_text() {
        Ok(t) => t.to_string(),
        Err(_) => {
            log.emit("error", "auth.non_text_response", &[]);
            return false;
        }
    };
    match decode(&text) {
        Ok(AgentProtocolMessage::SessionEstablished(m)) => {
            let correlation_ok = m.envelope.correlation_id == Some(request_id);
            let v1 = m.envelope.protocol_version.is_v1();
            log.emit(
                "info",
                "auth.session_established",
                &[
                    ("session_id", s(format!("{:?}", m.body.session_id))),
                    ("credential_expires_at", s(format!("{:?}", m.body.credential_expires_at))),
                    ("correlation_ok", F::B(correlation_ok)),
                    ("protocol_v1", F::B(v1)),
                ],
            );
            correlation_ok && v1
        }
        Ok(AgentProtocolMessage::AuthError(m)) => {
            log.emit("warn", "auth.rejected", &[("reason", s(m.body.reason))]);
            false
        }
        Ok(other) => {
            log.emit(
                "error",
                "auth.unexpected_message",
                &[("type", s(format!("{other:?}")))],
            );
            false
        }
        Err(e) => {
            log.emit("error", "auth.decode_failed", &[("error", s(e.to_string()))]);
            false
        }
    }
}

/// Checkpoint 5: standalone read-only physical capture-source enumeration
/// under #59 (one epoch, no inventory report).
fn enumerate_capture_sources(log: &Log) {
    let epoch = sources::enumerate();
    log_source_epoch(log, &epoch, "standalone");
}

fn main() {
    let log = std::sync::Arc::new(Log::new());

    {
        let log = std::sync::Arc::clone(&log);
        std::panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "unknown".into());
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|x| x.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            log.emit(
                "error",
                "probe.panic",
                &[("location", s(loc)), ("message", s(msg))],
            );
            let _ = write_local_file(&log);
        }));
    }

    // Args: [SINK_ADDR] [--sink ADDR] [--wss ADDR --pin SHA256_HEX]
    // Back-compatible with Checkpoint 2's single positional sink address.
    let mut sink_raw = std::env::var("BAMEP_PROBE_SINK").unwrap_or_else(|_| DEFAULT_SINK.to_string());
    let mut wss_addr: Option<String> = None;
    let mut pin_hex: Option<String> = None;
    let mut auth_cred_file: Option<String> = None;
    let mut enumerate_sources = false;
    let mut inventory_report_mode = false;
    let mut positional_seen = false;
    let mut unknown_args: Vec<String> = Vec::new();
    {
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--wss" => wss_addr = it.next(),
                "--pin" => pin_hex = it.next(),
                "--auth-credential-file" => auth_cred_file = it.next(),
                "--enumerate-sources" => enumerate_sources = true,
                "--inventory-report" => inventory_report_mode = true,
                "--sink" => {
                    if let Some(v) = it.next() {
                        sink_raw = v;
                    }
                }
                other if !other.starts_with("--") && !positional_seen => {
                    sink_raw = other.to_string();
                    positional_seen = true;
                }
                other => unknown_args.push(other.to_string()),
            }
        }
    }
    let do_wss = wss_addr.is_some() && pin_hex.is_some();

    let build_epoch: i64 = env!("PROBE_BUILD_EPOCH_SECS").parse().unwrap_or(0);
    log.emit(
        "info",
        "probe.start",
        &[
            ("probe_version", s(PROBE_VERSION)),
            ("git_short", s(env!("PROBE_GIT_SHORT"))),
            ("build_epoch_secs", F::I(build_epoch)),
            ("rustc", s(env!("PROBE_RUSTC_VERSION"))),
            ("target_triple", s(env!("PROBE_TARGET_TRIPLE"))),
            ("build_profile", s(env!("PROBE_PROFILE"))),
            ("crt_static", F::B(cfg!(target_feature = "crt-static"))),
            ("std_os", s(std::env::consts::OS)),
            ("std_arch", s(std::env::consts::ARCH)),
            ("std_family", s(std::env::consts::FAMILY)),
            ("pid", F::I(std::process::id() as i64)),
            ("sink_arg", s(&sink_raw)),
            ("wss_arg", s(wss_addr.clone().unwrap_or_else(|| "<none>".into()))),
            ("do_wss", F::B(do_wss)),
        ],
    );
    for a in &unknown_args {
        log.emit("warn", "args.unknown", &[("arg", s(a))]);
    }

    let getenv = |k: &str| std::env::var(k).unwrap_or_else(|_| "<unset>".into());
    log.emit(
        "info",
        "env.host",
        &[
            ("computername", s(getenv("COMPUTERNAME"))),
            ("username", s(getenv("USERNAME"))),
            ("os_env", s(getenv("OS"))),
            ("processor_arch", s(getenv("PROCESSOR_ARCHITECTURE"))),
            ("system_root", s(getenv("SystemRoot"))),
            ("temp", s(getenv("TEMP"))),
            (
                "cwd",
                s(std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()),
            ),
        ],
    );

    capture_ipconfig(&log);

    let stream = match resolve_addr(&sink_raw) {
        Some(addr) => observe_network(&log, addr),
        None => {
            log.emit("warn", "net.sink_unresolved", &[("sink_arg", s(&sink_raw))]);
            None
        }
    };

    let wss_ok = if do_wss {
        cross_wss_boundary(
            &log,
            wss_addr.as_deref().unwrap(),
            pin_hex.as_deref().unwrap(),
            auth_cred_file.as_deref(),
            inventory_report_mode,
        )
    } else {
        if wss_addr.is_some() || pin_hex.is_some() {
            log.emit(
                "warn",
                "wss.skipped_incomplete_args",
                &[("need", s("both --wss and --pin"))],
            );
        }
        true
    };

    let do_auth = do_wss && auth_cred_file.is_some();

    if enumerate_sources && !inventory_report_mode {
        enumerate_capture_sources(&log);
    }

    let file_ok = write_local_file(&log);
    // 0 ok; 2 local-file sink failed; 3 TLS/WSS transport crossing failed;
    // 4 authentication attempted but not established; 5 inventory-report
    // sequence did not complete cleanly.
    let exit_code: i32 = if !file_ok {
        2
    } else if do_auth && !wss_ok && inventory_report_mode {
        5
    } else if do_auth && !wss_ok {
        4
    } else if do_wss && !wss_ok {
        3
    } else {
        0
    };
    log.emit(
        "info",
        "probe.end",
        &[
            ("file_ok", F::B(file_ok)),
            ("wss_attempted", F::B(do_wss)),
            ("auth_attempted", F::B(do_auth)),
            ("inventory_report_mode", F::B(inventory_report_mode)),
            ("wss_ok", F::B(wss_ok)),
            ("exit_code", F::I(exit_code as i64)),
        ],
    );

    // Ship the full record (incl. probe.end) to the sink, then rewrite the
    // local file so it also carries the trailing sink result.
    if let Some(stream) = stream {
        ship_over_network(&log, stream);
    }
    let _ = write_local_file(&log);
    std::process::exit(exit_code);
}
