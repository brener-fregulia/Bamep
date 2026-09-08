//! Issue #63 Stage 3 — the WinPE matrix-runner loop.
//!
//! Reached ONLY via `bamep-i63-runner --matrix ...` (intercepted in `main`
//! before `parse_args`, so the committed Stage-1 invocation
//! `bamep-i63-runner --mode <physical|host-smoke> ...` is byte-for-byte
//! unchanged and its regression suite stays green).
//!
//!   `--matrix`            -> prints `STAGE2_MATRIX_RUNNER_NOT_ARMED` and exits 0.
//!   `--matrix --arm ...`  -> the Stage-3 physical matrix loop (below). The
//!                            Stage-3 lab supervisor bakes `--arm` into the
//!                            derived WinPE bootstrap only after its own
//!                            host-side preflight.
//!
//! Armed loop, per case (one probe process == one transfer case):
//!   1. `next_case`  from the Stage-2 matrix coordinator (typed Case JSON);
//!   2. re-check / re-align the WinPE UTC clock OUTSIDE the measured wall (the
//!      PROVEN Stage-1 `SetSystemTime` path — NOT redesigned);
//!   3. `case_ready`;
//!   4. launch the Issue-63 transfer probe with explicit typed argv (chunk size
//!      + extent propagated verbatim; the probe re-checks them in its own
//!      agreement gate);
//!   5. observe the probe exit code + its `probe.case_result` line;
//!   6. on a verified Artifact -> `case_started` + `case_completed{result}`;
//!      on ANY other outcome -> `case_failed` and STOP (no retry).
//!
//! The runner opens NO `\\.\PhysicalDrive*` handle and issues NO IOCTL — the
//! probe owns all source access. This module performs NO deliberate fault
//! injection.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::{clock, pure};

// ---------------------------------------------------------------------------
// committed Stage-2 helpers (unchanged surface; unit-tested)
// ---------------------------------------------------------------------------

/// Pure config for building the probe argv.
pub struct MatrixRunnerConfig {
    pub probe_path: String,
    /// Probe `--coord`: the #61-shaped Stage-3 harness coord endpoint (Server
    /// UTC + `source_selection` -> fresh per-case Transfer lineage).
    pub coord: String,
    /// Probe `--sink`: the matrix coordinator's probe-evidence sink.
    pub sink: String,
    pub wss: String,
    pub pin_hex: String,
    /// Probe `--runtime-credential-out`: where the probe persists the rotated
    /// runtime credential for the NEXT per-case process.
    pub runtime_credential_out: String,
    pub select_model_substr: String,
    pub seal_timeout_secs: u64,
}

/// The minimal case fields the runner forwards to the probe / the coordinator.
pub struct PlannedCase {
    pub run_id: String,
    pub case_id: String,
    pub phase: String,
    pub cycle: Option<u64>,
    pub slot: Option<u64>,
    pub chunk_size_bytes: u64,
    pub extent_bytes: u64,
    pub expected_chunk_count: u64,
}

pub fn parse_case(json: &str) -> Result<PlannedCase, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("case JSON: {e}"))?;
    let get_str = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(String::from)
            .ok_or_else(|| format!("case missing string field {k}"))
    };
    let get_u64 = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_u64())
            .ok_or_else(|| format!("case missing u64 field {k}"))
    };
    Ok(PlannedCase {
        run_id: get_str("run_id")?,
        case_id: get_str("case_id")?,
        phase: get_str("phase")?,
        cycle: v.get("cycle").and_then(|x| x.as_u64()),
        slot: v.get("slot").and_then(|x| x.as_u64()),
        chunk_size_bytes: get_u64("chunk_size_bytes")?,
        extent_bytes: get_u64("extent_bytes")?,
        expected_chunk_count: get_u64("expected_chunk_count")?,
    })
}

/// The exact probe argv for `case`. `auth_credential_file` is the first-contact
/// enrollment credential for case 0 and the rotated runtime credential
/// thereafter. Chunk size / extent are propagated verbatim.
pub fn probe_argv(
    cfg: &MatrixRunnerConfig,
    case: &PlannedCase,
    auth_credential_file: &str,
) -> Vec<String> {
    vec![
        cfg.probe_path.clone(),
        "--coord".into(),
        cfg.coord.clone(),
        "--sink".into(),
        cfg.sink.clone(),
        "--wss".into(),
        cfg.wss.clone(),
        "--pin".into(),
        cfg.pin_hex.clone(),
        "--auth-credential-file".into(),
        auth_credential_file.to_string(),
        "--runtime-credential-out".into(),
        cfg.runtime_credential_out.clone(),
        "--select-model-substr".into(),
        cfg.select_model_substr.clone(),
        "--seal-timeout-secs".into(),
        cfg.seal_timeout_secs.to_string(),
        "--chunk-size".into(),
        case.chunk_size_bytes.to_string(),
        "--extent-bytes".into(),
        case.extent_bytes.to_string(),
        "--run-id".into(),
        case.run_id.clone(),
        "--case-id".into(),
        case.case_id.clone(),
    ]
}

/// The `--matrix` (NOT armed) body: explain and exit 0. No network, no clock
/// change, no device access, no transfer.
pub fn run_not_armed() -> i32 {
    println!("STAGE2_MATRIX_RUNNER_NOT_ARMED");
    println!(
        "bamep-i63-runner --matrix: the Stage-3 physical matrix loop is present but NOT armed. \
         Pass `--matrix --arm ...` (the Stage-3 lab supervisor does this in the derived WinPE \
         bootstrap ONLY after host-side preflight). Stage-1 behaviour is unchanged. \
         PHYSICAL MATRIX NOT ARMED."
    );
    0
}

// ---------------------------------------------------------------------------
// armed loop
// ---------------------------------------------------------------------------

struct MatrixArgs {
    matrix_coord: String,
    probe_path: String,
    harness_coord: String,
    harness_wss: String,
    sink: String,
    pin_hex: String,
    enroll_cred: String,
    runtime_cred: String,
    select_model_substr: String,
    skew_floor_ms: i64,
    skew_ceil_ms: i64,
    net_wait_secs: u64,
    seal_timeout_secs: u64,
    local_evidence: String,
}

mod exit {
    pub const DONE: i32 = 0;
    pub const BAD_ARGS: i32 = 2;
    pub const NET_NOT_READY: i32 = 20;
    pub const CLOCK_FAILED: i32 = 30;
    pub const MATRIX_HALTED: i32 = 12;
    pub const COORD_PROTOCOL: i32 = 13;
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct Log {
    started: Instant,
    seq: u64,
    local: String,
    buf: String,
}
impl Log {
    fn new(local: &str) -> Self {
        Self {
            started: Instant::now(),
            seq: 0,
            local: local.to_string(),
            buf: String::new(),
        }
    }
    fn emit(&mut self, level: &str, event: &str, extra: Value) {
        self.seq += 1;
        let line = json!({
            "ts_ms": now_ms(),
            "seq": self.seq,
            "elapsed_ms": self.started.elapsed().as_millis() as u64,
            "level": level,
            "event": event,
            "stage": "stage3",
            "component": "matrix-runner",
            "detail": extra,
        })
        .to_string();
        eprintln!("{line}");
        let _ = std::io::stderr().flush();
        self.buf.push_str(&line);
        self.buf.push('\n');
        // best-effort local persistence after every line
        for p in [self.local.as_str(), "X:\\bamep-i63-stage3-runner.ndjson", "bamep-i63-stage3-runner.ndjson"] {
            if !p.is_empty() && std::fs::write(p, &self.buf).is_ok() {
                break;
            }
        }
    }
}

fn parse_matrix_args() -> Result<MatrixArgs, String> {
    let mut a = MatrixArgs {
        matrix_coord: "192.168.99.1:9210".into(),
        probe_path: "X:\\Windows\\System32\\bamep-i63-stage2-probe.exe".into(),
        harness_coord: "192.168.99.1:9206".into(),
        harness_wss: "192.168.99.1:8443".into(),
        sink: "192.168.99.1:9299".into(),
        pin_hex: String::new(),
        enroll_cred: "X:\\bamep-i63-enroll.cred".into(),
        runtime_cred: "X:\\bamep-i63-runtime.cred".into(),
        select_model_substr: "256GB".into(),
        skew_floor_ms: -2000,
        skew_ceil_ms: 2000,
        net_wait_secs: 180,
        seal_timeout_secs: 300,
        local_evidence: "X:\\bamep-i63-stage3-runner.ndjson".into(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut next = || it.next().ok_or_else(|| format!("{x} needs a value"));
        match x.as_str() {
            "--matrix" | "--arm" | "--stage4" => {}
            "--matrix-coord" => a.matrix_coord = next()?,
            "--probe" => a.probe_path = next()?,
            "--coord" => a.harness_coord = next()?,
            "--wss" => a.harness_wss = next()?,
            "--sink" => a.sink = next()?,
            "--pin" => a.pin_hex = next()?,
            "--enroll-credential-file" => a.enroll_cred = next()?,
            "--runtime-credential-file" => a.runtime_cred = next()?,
            "--select-model-substr" => a.select_model_substr = next()?,
            "--skew-floor-ms" => {
                a.skew_floor_ms = next()?.parse().map_err(|_| "bad --skew-floor-ms".to_string())?
            }
            "--skew-ceil-ms" => {
                a.skew_ceil_ms = next()?.parse().map_err(|_| "bad --skew-ceil-ms".to_string())?
            }
            "--net-wait-secs" => {
                a.net_wait_secs = next()?.parse().map_err(|_| "bad --net-wait-secs".to_string())?
            }
            "--seal-timeout-secs" => {
                a.seal_timeout_secs =
                    next()?.parse().map_err(|_| "bad --seal-timeout-secs".to_string())?
            }
            "--local-evidence" => a.local_evidence = next()?,
            other => return Err(format!("unknown --matrix argument {other:?}")),
        }
    }
    if a.pin_hex.trim().len() != 64 {
        return Err("--pin must be a 64-hex Server leaf fingerprint".into());
    }
    if a.skew_floor_ms > 0 || a.skew_ceil_ms < 0 || a.skew_ceil_ms - a.skew_floor_ms > 60_000 {
        return Err(format!(
            "implausible skew window [{}, {}] ms",
            a.skew_floor_ms, a.skew_ceil_ms
        ));
    }
    Ok(a)
}

fn resolve(addr: &str) -> Result<std::net::SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {addr}"))
}

/// One JSON-line request/response round-trip to the matrix coordinator.
fn matrix_rpc(addr: &str, req: &Value) -> Result<Value, String> {
    let sa = resolve(addr)?;
    let mut st = TcpStream::connect_timeout(&sa, Duration::from_secs(8))
        .map_err(|e| format!("connect {addr}: {e}"))?;
    st.set_read_timeout(Some(Duration::from_secs(30))).ok();
    st.set_write_timeout(Some(Duration::from_secs(15))).ok();
    st.write_all(format!("{req}\n").as_bytes())
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
                if buf.len() > 1 << 20 {
                    return Err("matrix response line too long".into());
                }
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    serde_json::from_slice(&buf).map_err(|e| format!("matrix response not JSON: {e}"))
}

fn wait_for_network(log: &mut Log, addr: &str, secs: u64) -> Result<(), String> {
    let sa = resolve(addr)?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        if let Ok(s) = TcpStream::connect_timeout(&sa, Duration::from_secs(3)) {
            let _ = s.shutdown(std::net::Shutdown::Both);
            log.emit("info", "stage3.network_ready", json!({ "attempts": attempts }));
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("no TCP path to {addr} after {attempts} attempts"));
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn server_utc(addr: &str) -> Result<i64, String> {
    let r = matrix_rpc(addr, &json!({ "op": "server_utc" }))?;
    r.get("server_utc_ms")
        .and_then(|x| x.as_i64())
        .filter(|m| *m > 0)
        .ok_or_else(|| format!("no server_utc_ms in {r}"))
}

/// Align (or on a repeat call, re-check and re-align if needed) the WinPE UTC
/// clock to the Server. ALWAYS outside the measured transfer wall.
fn align_clock(log: &mut Log, a: &MatrixArgs, first: bool) -> Result<i64, String> {
    let target = server_utc(&a.matrix_coord)?;
    let before = clock::now_unix_ms();
    let skew_before = before - target;
    if !first
        && pure::classify_skew(skew_before, a.skew_floor_ms, a.skew_ceil_ms) == pure::SkewVerdict::InBound
    {
        log.emit(
            "info",
            "stage3.clock_still_in_bound",
            json!({ "skew_ms": skew_before }),
        );
        return Ok(skew_before);
    }
    let align = clock::align_to_unix_ms(target);
    if align.filetime_conv_failed {
        return Err(format!(
            "FileTimeToSystemTime failed win32_error={}",
            align.filetime_conv_win32_error
        ));
    }
    if !align.set_ok {
        return Err(format!(
            "SetSystemTime returned FALSE win32_error={} (likely SeSystemtimePrivilege not held; \
             STOP and report — no fallback)",
            align.win32_error
        ));
    }
    let target2 = server_utc(&a.matrix_coord)?;
    let after = clock::now_unix_ms();
    let skew_after = after - target2;
    let (y, mo, d, h, mi, s, ms) = align.readback;
    match pure::classify_skew(skew_after, a.skew_floor_ms, a.skew_ceil_ms) {
        pure::SkewVerdict::InBound => {
            log.emit(
                "info",
                "stage3.clock_aligned",
                json!({
                    "method": clock::BACKEND,
                    "skew_before_ms": skew_before,
                    "skew_after_ms": skew_after,
                    "readback_utc": pure::iso8601_utc(y, mo, d, h, mi, s, ms),
                }),
            );
            Ok(skew_after)
        }
        v => Err(format!(
            "residual skew {skew_after} ms still out of [{}, {}] after SetSystemTime ({v:?})",
            a.skew_floor_ms, a.skew_ceil_ms
        )),
    }
}

fn extract_probe_exit(output: &str) -> Option<i32> {
    output
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("BAMEP_I63_STAGE2_PROBE_EXITCODE="))
        .and_then(|n| n.trim().parse().ok())
}

fn extract_case_result(output: &str) -> Option<Value> {
    // The probe emits one `"event":"probe.case_result"` NDJSON line (the full
    // one, from `emit_full_result`). Take the LAST such line.
    output
        .lines()
        .rev()
        .filter(|l| l.contains(r#""event":"probe.case_result""#))
        .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
}

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}
fn u64f(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// Merge the probe's `probe.case_result` line + the plan `case` into a full
/// `bamep_i63_stage2_engine::result::CaseResult`-shaped JSON object.
fn build_case_result(case: &PlannedCase, pr: Option<&Value>, probe_exit: i32) -> Value {
    let extent = case.extent_bytes as f64;
    let mib_s = |wall: f64| if wall > 0.0 { (extent / (1024.0 * 1024.0)) / (wall / 1000.0) } else { 0.0 };
    let mb_s = |wall: f64| if wall > 0.0 { (extent / 1_000_000.0) / (wall / 1000.0) } else { 0.0 };

    let (
        transfer_id, artifact_id, safety, clock, b_wall, v_wall, resume_ms, seal_d2_ms, chunks,
        read_ms, chunk_sha_ms, rolling_sha_ms, proof_ms, put_ack_ms, artifact_status, case_status,
    ) = match pr {
        Some(v) => (
            v.get("transfer_id").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(String::from),
            v.get("artifact_id").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(String::from),
            v.get("source_safety_verdict").and_then(|x| x.as_str()).unwrap_or("unknown").to_string(),
            v.get("clock_skew_verdict").and_then(|x| x.as_str()).unwrap_or("unknown").to_string(),
            num(v, "bulk_stream_wall_ms"),
            num(v, "verified_transfer_wall_ms"),
            num(v, "resume_ms"),
            num(v, "seal_d2_ms"),
            {
                let c = u64f(v, "chunk_count");
                if c > 0 { c } else { case.expected_chunk_count }
            },
            num(v, "read_ms"),
            num(v, "chunk_sha_ms"),
            num(v, "rolling_sha_ms"),
            num(v, "proof_ms"),
            num(v, "put_ack_ms"),
            v.get("final_artifact_status").and_then(|x| x.as_str()).unwrap_or("none").to_string(),
            v.get("case_status").and_then(|x| x.as_str()).unwrap_or("failed:no_probe_result").to_string(),
        ),
        None => (
            None, None, "unknown".into(), "unknown".into(), 0.0, 0.0, 0.0, 0.0,
            case.expected_chunk_count, 0.0, 0.0, 0.0, 0.0, 0.0, "none".into(),
            format!("failed:no_probe_result(exit={probe_exit})"),
        ),
    };

    json!({
        "run_id": case.run_id,
        "case_id": case.case_id,
        "phase": case.phase,
        "cycle": case.cycle,
        "slot": case.slot,
        "chunk_size_bytes": case.chunk_size_bytes,
        "extent_bytes": case.extent_bytes,
        "chunk_count": chunks,
        "transfer_id": transfer_id,
        "artifact_id": artifact_id,
        "source_safety_verdict": safety,
        "clock_skew_verdict": clock,
        "bulk_stream_wall_ms": b_wall,
        "bulk_stream_mib_s": mib_s(b_wall),
        "bulk_stream_mb_s": mb_s(b_wall),
        "verified_transfer_wall_ms": v_wall,
        "verified_transfer_mib_s": mib_s(v_wall),
        "verified_transfer_mb_s": mb_s(v_wall),
        "resume_ms": resume_ms,
        "seal_d2_ms": seal_d2_ms,
        "read_ms": read_ms,
        "chunk_sha_ms": chunk_sha_ms,
        "rolling_sha_ms": rolling_sha_ms,
        "proof_ms": proof_ms,
        "put_ack_ms": put_ack_ms,
        "connection_count": {
            "resume_requests": 1,
            "chunk_puts": chunks,
            "seal_requests": 1,
            "expected_total": chunks + 2,
            "observed_total": Value::Null,
        },
        "final_artifact_status": artifact_status,
        "case_status": case_status,
    })
}

fn run_probe(log: &mut Log, argv: &[String]) -> (i32, String) {
    log.emit(
        "info",
        "stage3.probe_launch",
        json!({ "argv_redacted": argv.iter().map(|a| if a.len() == 64 { "<pin>".into() } else { a.clone() }).collect::<Vec<_>>() }),
    );
    let out = match Command::new(&argv[0]).args(&argv[1..]).output() {
        Ok(o) => o,
        Err(e) => {
            log.emit("error", "stage3.probe_spawn_failed", json!({ "error": e.to_string() }));
            return (-1, format!("probe spawn failed: {e}"));
        }
    };
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push('\n');
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    let code = out.status.code().unwrap_or(-1);
    // echo the probe's tail so the operator sees it in the bootstrap window
    for l in combined.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
        println!("  [probe] {l}");
    }
    (code, combined)
}

/// Entry point for `bamep-i63-runner --matrix --arm ...`.
pub fn run_armed() -> i32 {
    println!("STAGE3_MATRIX_RUNNER_ARMED");
    let a = match parse_matrix_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bamep-i63-runner --matrix --arm: FATAL: {e}");
            return exit::BAD_ARGS;
        }
    };
    let mut log = Log::new(&a.local_evidence);
    log.emit(
        "info",
        "stage3.runner_start",
        json!({
            "clock_backend": clock::BACKEND,
            "matrix_coord": a.matrix_coord,
            "harness_coord": a.harness_coord,
            "harness_wss": a.harness_wss,
            "probe": a.probe_path,
            "skew_window_ms": [a.skew_floor_ms, a.skew_ceil_ms],
        }),
    );

    if let Err(e) = wait_for_network(&mut log, &a.matrix_coord, a.net_wait_secs) {
        log.emit("error", "stage3.network_unreachable", json!({ "detail": e }));
        return exit::NET_NOT_READY;
    }

    if let Err(e) = align_clock(&mut log, &a, true) {
        log.emit("error", "stage3.clock_alignment_failed", json!({ "detail": e }));
        return exit::CLOCK_FAILED;
    }

    let cfg = MatrixRunnerConfig {
        probe_path: a.probe_path.clone(),
        coord: a.harness_coord.clone(),
        sink: a.sink.clone(),
        wss: a.harness_wss.clone(),
        pin_hex: a.pin_hex.clone(),
        runtime_credential_out: a.runtime_cred.clone(),
        select_model_substr: a.select_model_substr.clone(),
        seal_timeout_secs: a.seal_timeout_secs,
    };

    let mut case_index = 0usize;
    loop {
        let resp = match matrix_rpc(&a.matrix_coord, &json!({ "op": "next_case" })) {
            Ok(r) => r,
            Err(e) => {
                log.emit("error", "stage3.next_case_rpc_failed", json!({ "detail": e }));
                return exit::COORD_PROTOCOL;
            }
        };
        if let Some(mc) = resp.get("matrix_completed") {
            let completed = mc.get("completed").and_then(|x| x.as_u64()).unwrap_or(0);
            log.emit("info", "stage3.matrix_completed", json!({ "completed": completed }));
            println!("STAGE3_MATRIX_RUNNER_DONE completed={completed}");
            return exit::DONE;
        }
        if let Some(h) = resp.get("halt") {
            log.emit("error", "stage3.matrix_halted", json!({ "reason": h }));
            println!("STAGE3_MATRIX_RUNNER_HALTED reason={h}");
            return exit::MATRIX_HALTED;
        }
        let case_json = match resp.get("case") {
            Some(c) => c.to_string(),
            None => {
                log.emit("error", "stage3.next_case_unexpected", json!({ "resp": resp }));
                return exit::COORD_PROTOCOL;
            }
        };
        let case = match parse_case(&case_json) {
            Ok(c) => c,
            Err(e) => {
                log.emit("error", "stage3.case_parse_failed", json!({ "detail": e }));
                return exit::COORD_PROTOCOL;
            }
        };
        log.emit(
            "info",
            "stage3.case_received",
            json!({
                "case_index": case_index,
                "case_id": case.case_id,
                "phase": case.phase,
                "chunk_size_bytes": case.chunk_size_bytes,
                "expected_chunk_count": case.expected_chunk_count,
            }),
        );

        // clock re-check / re-align OUTSIDE the measured wall
        if let Err(e) = align_clock(&mut log, &a, false) {
            log.emit("error", "stage3.clock_recheck_failed", json!({ "detail": e }));
            let _ = matrix_rpc(
                &a.matrix_coord,
                &json!({ "op": "case_ready", "case_id": case.case_id }),
            );
            let _ = matrix_rpc(
                &a.matrix_coord,
                &json!({ "op": "case_failed", "case_id": case.case_id, "reason": format!("clock recheck failed: {e}"), "contaminated": false }),
            );
            println!("STAGE3_MATRIX_RUNNER_HALTED reason=clock_recheck_failed");
            return exit::CLOCK_FAILED;
        }

        let ready = matrix_rpc(
            &a.matrix_coord,
            &json!({ "op": "case_ready", "case_id": case.case_id }),
        );
        match ready {
            Ok(r) if r.get("ack") == Some(&Value::Bool(true)) => {}
            Ok(r) if r.get("halt").is_some() => {
                log.emit("error", "stage3.case_ready_halt", json!({ "resp": r }));
                println!("STAGE3_MATRIX_RUNNER_HALTED reason=case_ready");
                return exit::MATRIX_HALTED;
            }
            other => {
                log.emit("error", "stage3.case_ready_failed", json!({ "resp": format!("{other:?}") }));
                return exit::COORD_PROTOCOL;
            }
        }

        let auth_cred = if case_index == 0 { &a.enroll_cred } else { &a.runtime_cred };
        let argv = probe_argv(&cfg, &case, auth_cred);
        let (probe_exit, output) = run_probe(&mut log, &argv);
        let pr = extract_case_result(&output);
        let real_exit = extract_probe_exit(&output).unwrap_or(probe_exit);
        let result = build_case_result(&case, pr.as_ref(), real_exit);
        let case_status = result["case_status"].as_str().unwrap_or("").to_string();
        let artifact_status = result["final_artifact_status"].as_str().unwrap_or("").to_string();
        let verified = real_exit == 0 && case_status == "completed" && artifact_status == "Verified";
        log.emit(
            "info",
            "stage3.probe_observed",
            json!({
                "case_id": case.case_id,
                "probe_exit": real_exit,
                "case_status": case_status,
                "final_artifact_status": artifact_status,
                "verified": verified,
            }),
        );

        if verified {
            for op in ["case_started"] {
                let r = matrix_rpc(
                    &a.matrix_coord,
                    &json!({ "op": op, "case_id": case.case_id }),
                );
                if !matches!(&r, Ok(v) if v.get("ack") == Some(&Value::Bool(true))) {
                    log.emit("error", "stage3.case_started_failed", json!({ "resp": format!("{r:?}") }));
                    return exit::COORD_PROTOCOL;
                }
            }
            let done = matrix_rpc(
                &a.matrix_coord,
                &json!({ "op": "case_completed", "case_id": case.case_id, "result": result }),
            );
            match done {
                Ok(r) if r.get("ack") == Some(&Value::Bool(true)) => {
                    log.emit("info", "stage3.case_completed", json!({ "case_id": case.case_id }));
                    case_index += 1;
                }
                Ok(r) if r.get("halt").is_some() => {
                    log.emit("error", "stage3.case_completed_halt", json!({ "resp": r }));
                    println!("STAGE3_MATRIX_RUNNER_HALTED reason=case_completed");
                    return exit::MATRIX_HALTED;
                }
                other => {
                    log.emit("error", "stage3.case_completed_failed", json!({ "resp": format!("{other:?}") }));
                    return exit::COORD_PROTOCOL;
                }
            }
        } else {
            let contaminated = case_status == "contaminated";
            let reason = format!(
                "probe exit {real_exit}; case_status={case_status}; artifact={artifact_status}"
            );
            let _ = matrix_rpc(
                &a.matrix_coord,
                &json!({ "op": "case_failed", "case_id": case.case_id, "reason": reason, "contaminated": contaminated, "result": result }),
            );
            log.emit(
                "error",
                "stage3.case_failed",
                json!({ "case_id": case.case_id, "reason": reason, "contaminated": contaminated }),
            );
            println!("STAGE3_MATRIX_RUNNER_HALTED reason=case_failed");
            return exit::MATRIX_HALTED;
        }
    }
}

// ===================================================================
// Issue #63 STAGE 4 — 64 MiB serial-vs-prep-ahead micro-matrix loop.
//
// Reached ONLY via `bamep-i63-runner --stage4 --arm ...`. Same shape as the
// Stage-3 loop above, but: the case carries a `mode` (serial | prep_ahead_2)
// which is forwarded to the probe as `--mode`, the chunk size is always 64 MiB,
// and the per-case result is the self-contained `S4CaseResult` shape (NOT the
// Stage-3 `CaseResult`). The runner opens no device and injects no fault.
// ===================================================================

/// The Stage-4 case fields the runner forwards.
pub struct S4PlannedCase {
    pub run_id: String,
    pub case_id: String,
    pub phase: String,
    /// `"serial"` | `"prep_ahead_2"` — passed verbatim to the probe `--mode`.
    pub mode: String,
    pub cycle: Option<u64>,
    pub slot: Option<u64>,
    pub chunk_size_bytes: u64,
    pub extent_bytes: u64,
    pub expected_chunk_count: u64,
}

pub fn parse_s4_case(json: &str) -> Result<S4PlannedCase, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("s4 case JSON: {e}"))?;
    let get_str = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(String::from)
            .ok_or_else(|| format!("s4 case missing string field {k}"))
    };
    let get_u64 = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_u64())
            .ok_or_else(|| format!("s4 case missing u64 field {k}"))
    };
    let mode = get_str("mode")?;
    if mode != "serial" && mode != "prep_ahead_2" && mode != "prep_ahead_window_8" && mode != "prep_ahead_window_8_batch_8" {
        return Err(format!("s4 case has unknown mode {mode:?}"));
    }
    Ok(S4PlannedCase {
        run_id: get_str("run_id")?,
        case_id: get_str("case_id")?,
        phase: get_str("phase")?,
        mode,
        cycle: v.get("cycle").and_then(|x| x.as_u64()),
        slot: v.get("slot").and_then(|x| x.as_u64()),
        chunk_size_bytes: get_u64("chunk_size_bytes")?,
        extent_bytes: get_u64("extent_bytes")?,
        expected_chunk_count: get_u64("expected_chunk_count")?,
    })
}

/// The exact probe argv for a Stage-4 case — `probe_argv` plus `--mode`.
pub fn s4_probe_argv(
    cfg: &MatrixRunnerConfig,
    case: &S4PlannedCase,
    auth_credential_file: &str,
) -> Vec<String> {
    let base = PlannedCase {
        run_id: case.run_id.clone(),
        case_id: case.case_id.clone(),
        phase: case.phase.clone(),
        cycle: case.cycle,
        slot: case.slot,
        chunk_size_bytes: case.chunk_size_bytes,
        extent_bytes: case.extent_bytes,
        expected_chunk_count: case.expected_chunk_count,
    };
    let mut argv = probe_argv(cfg, &base, auth_credential_file);
    argv.push("--mode".into());
    argv.push(case.mode.clone());
    argv
}

/// Merge the probe's `probe.case_result` line + the plan case into an
/// `bamep_i63_stage2_engine::stage4::S4CaseResult`-shaped JSON object.
pub fn build_s4_case_result(case: &S4PlannedCase, pr: Option<&Value>, probe_exit: i32) -> Value {
    let (
        transfer_id,
        artifact_id,
        b_wall,
        v_wall,
        resume_ms,
        seal_d2_ms,
        chunks,
        read_ms,
        chunk_sha_ms,
        rolling_sha_ms,
        proof_ms,
        put_ack_ms,
        prepared_peak,
        device_reads,
        artifact_status,
        case_status,
    ) = match pr {
        Some(v) => (
            v.get("transfer_id").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(String::from),
            v.get("artifact_id").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(String::from),
            num(v, "bulk_stream_wall_ms"),
            num(v, "verified_transfer_wall_ms"),
            num(v, "resume_ms"),
            num(v, "seal_d2_ms"),
            {
                let c = u64f(v, "chunk_count");
                if c > 0 { c } else { case.expected_chunk_count }
            },
            num(v, "read_ms"),
            num(v, "chunk_sha_ms"),
            num(v, "rolling_sha_ms"),
            num(v, "proof_ms"),
            num(v, "put_ack_ms"),
            u64f(v, "prepared_buffer_peak"),
            u64f(v, "device_read_count"),
            v.get("final_artifact_status").and_then(|x| x.as_str()).unwrap_or("none").to_string(),
            v.get("case_status").and_then(|x| x.as_str()).unwrap_or("failed:no_probe_result").to_string(),
        ),
        None => (
            None, None, 0.0, 0.0, 0.0, 0.0, case.expected_chunk_count, 0.0, 0.0, 0.0, 0.0, 0.0,
            0, 0, "none".into(),
            format!("failed:no_probe_result(exit={probe_exit})"),
        ),
    };

    json!({
        "run_id": case.run_id,
        "case_id": case.case_id,
        "mode": case.mode,
        "phase": case.phase,
        "cycle": case.cycle,
        "slot": case.slot,
        "chunk_size_bytes": case.chunk_size_bytes,
        "extent_bytes": case.extent_bytes,
        "chunk_count": chunks,
        "transfer_id": transfer_id,
        "artifact_id": artifact_id,
        "bulk_stream_wall_ms": b_wall,
        "verified_transfer_wall_ms": v_wall,
        "resume_ms": resume_ms,
        "seal_d2_ms": seal_d2_ms,
        "read_ms": read_ms,
        "chunk_sha_ms": chunk_sha_ms,
        "rolling_sha_ms": rolling_sha_ms,
        "proof_ms": proof_ms,
        "put_ack_ms": put_ack_ms,
        "prepared_buffer_peak": prepared_peak,
        "device_read_count": device_reads,
        // window_8 candidate fields — 0/false for serial / prep_ahead_2 (the
        // engine's `S4CaseResult` serde-defaults them the same way).
        "put_window": pr.map(|v| u64f(v, "put_window")).unwrap_or(0),
        "put_started_count": pr.map(|v| u64f(v, "put_started_count")).unwrap_or(0),
        "put_completed_count": pr.map(|v| u64f(v, "put_completed_count")).unwrap_or(0),
        "peak_puts_in_flight": pr.map(|v| u64f(v, "peak_puts_in_flight")).unwrap_or(0),
        "put_starts_ascending": pr
            .and_then(|v| v.get("put_starts_ascending").and_then(|x| x.as_bool()))
            .unwrap_or(false),
        "final_artifact_status": artifact_status,
        "case_status": case_status,
    })
}

/// `--stage4` (NOT armed): explain + exit 0. No network, no clock change, no
/// device access, no transfer.
pub fn run_stage4_not_armed() -> i32 {
    println!("STAGE4_RUNNER_NOT_ARMED");
    println!(
        "bamep-i63-runner --stage4: the Stage-4 micro-matrix loop is present but NOT armed. \
         Pass `--stage4 --arm ...` (run-stage4-lab.sh does this in the derived WinPE bootstrap \
         ONLY after host-side preflight). STAGE4 NOT ARMED."
    );
    0
}

/// Entry point for `bamep-i63-runner --stage4 --arm ...`.
pub fn run_stage4_armed() -> i32 {
    println!("STAGE4_RUNNER_ARMED");
    let a = match parse_matrix_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bamep-i63-runner --stage4 --arm: FATAL: {e}");
            return exit::BAD_ARGS;
        }
    };
    let mut log = Log::new(&a.local_evidence);
    log.emit(
        "info",
        "stage4.runner_start",
        json!({
            "clock_backend": clock::BACKEND,
            "matrix_coord": a.matrix_coord,
            "harness_coord": a.harness_coord,
            "harness_wss": a.harness_wss,
            "probe": a.probe_path,
            "skew_window_ms": [a.skew_floor_ms, a.skew_ceil_ms],
        }),
    );

    if let Err(e) = wait_for_network(&mut log, &a.matrix_coord, a.net_wait_secs) {
        log.emit("error", "stage4.network_unreachable", json!({ "detail": e }));
        return exit::NET_NOT_READY;
    }
    if let Err(e) = align_clock(&mut log, &a, true) {
        log.emit("error", "stage4.clock_alignment_failed", json!({ "detail": e }));
        return exit::CLOCK_FAILED;
    }

    let cfg = MatrixRunnerConfig {
        probe_path: a.probe_path.clone(),
        coord: a.harness_coord.clone(),
        sink: a.sink.clone(),
        wss: a.harness_wss.clone(),
        pin_hex: a.pin_hex.clone(),
        runtime_credential_out: a.runtime_cred.clone(),
        select_model_substr: a.select_model_substr.clone(),
        seal_timeout_secs: a.seal_timeout_secs,
    };

    let mut case_index = 0usize;
    loop {
        let resp = match matrix_rpc(&a.matrix_coord, &json!({ "op": "next_case" })) {
            Ok(r) => r,
            Err(e) => {
                log.emit("error", "stage4.next_case_rpc_failed", json!({ "detail": e }));
                return exit::COORD_PROTOCOL;
            }
        };
        if let Some(mc) = resp.get("matrix_completed") {
            let completed = mc.get("completed").and_then(|x| x.as_u64()).unwrap_or(0);
            log.emit("info", "stage4.matrix_completed", json!({ "completed": completed }));
            println!("STAGE4_RUNNER_DONE completed={completed}");
            return exit::DONE;
        }
        if let Some(h) = resp.get("halt") {
            log.emit("error", "stage4.matrix_halted", json!({ "reason": h }));
            println!("STAGE4_RUNNER_HALTED reason={h}");
            return exit::MATRIX_HALTED;
        }
        let case_json = match resp.get("case") {
            Some(c) => c.to_string(),
            None => {
                log.emit("error", "stage4.next_case_unexpected", json!({ "resp": resp }));
                return exit::COORD_PROTOCOL;
            }
        };
        let case = match parse_s4_case(&case_json) {
            Ok(c) => c,
            Err(e) => {
                log.emit("error", "stage4.case_parse_failed", json!({ "detail": e }));
                return exit::COORD_PROTOCOL;
            }
        };
        log.emit(
            "info",
            "stage4.case_received",
            json!({
                "case_index": case_index, "case_id": case.case_id, "phase": case.phase,
                "mode": case.mode, "expected_chunk_count": case.expected_chunk_count,
            }),
        );

        // clock re-check / re-align OUTSIDE the measured wall
        if let Err(e) = align_clock(&mut log, &a, false) {
            log.emit("error", "stage4.clock_recheck_failed", json!({ "detail": e }));
            let _ = matrix_rpc(&a.matrix_coord, &json!({ "op": "case_ready", "case_id": case.case_id }));
            let _ = matrix_rpc(
                &a.matrix_coord,
                &json!({ "op": "case_failed", "case_id": case.case_id, "reason": format!("clock recheck failed: {e}"), "contaminated": false }),
            );
            println!("STAGE4_RUNNER_HALTED reason=clock_recheck_failed");
            return exit::CLOCK_FAILED;
        }

        let ready = matrix_rpc(&a.matrix_coord, &json!({ "op": "case_ready", "case_id": case.case_id }));
        match ready {
            Ok(r) if r.get("ack") == Some(&Value::Bool(true)) => {}
            Ok(r) if r.get("halt").is_some() => {
                log.emit("error", "stage4.case_ready_halt", json!({ "resp": r }));
                println!("STAGE4_RUNNER_HALTED reason=case_ready");
                return exit::MATRIX_HALTED;
            }
            other => {
                log.emit("error", "stage4.case_ready_failed", json!({ "resp": format!("{other:?}") }));
                return exit::COORD_PROTOCOL;
            }
        }

        let auth_cred = if case_index == 0 { &a.enroll_cred } else { &a.runtime_cred };
        let argv = s4_probe_argv(&cfg, &case, auth_cred);
        let (probe_exit, output) = run_probe(&mut log, &argv);
        let pr = extract_case_result(&output);
        let real_exit = extract_probe_exit(&output).unwrap_or(probe_exit);
        let result = build_s4_case_result(&case, pr.as_ref(), real_exit);
        let case_status = result["case_status"].as_str().unwrap_or("").to_string();
        let artifact_status = result["final_artifact_status"].as_str().unwrap_or("").to_string();
        let verified = real_exit == 0 && case_status == "completed" && artifact_status == "Verified";
        log.emit(
            "info",
            "stage4.probe_observed",
            json!({
                "case_id": case.case_id, "mode": case.mode, "probe_exit": real_exit,
                "case_status": case_status, "final_artifact_status": artifact_status, "verified": verified,
            }),
        );

        if verified {
            let r = matrix_rpc(&a.matrix_coord, &json!({ "op": "case_started", "case_id": case.case_id }));
            if !matches!(&r, Ok(v) if v.get("ack") == Some(&Value::Bool(true))) {
                log.emit("error", "stage4.case_started_failed", json!({ "resp": format!("{r:?}") }));
                return exit::COORD_PROTOCOL;
            }
            let done = matrix_rpc(
                &a.matrix_coord,
                &json!({ "op": "case_completed", "case_id": case.case_id, "result": result }),
            );
            match done {
                Ok(r) if r.get("ack") == Some(&Value::Bool(true)) => {
                    log.emit("info", "stage4.case_completed", json!({ "case_id": case.case_id }));
                    case_index += 1;
                }
                Ok(r) if r.get("halt").is_some() => {
                    log.emit("error", "stage4.case_completed_halt", json!({ "resp": r }));
                    println!("STAGE4_RUNNER_HALTED reason=case_completed");
                    return exit::MATRIX_HALTED;
                }
                other => {
                    log.emit("error", "stage4.case_completed_failed", json!({ "resp": format!("{other:?}") }));
                    return exit::COORD_PROTOCOL;
                }
            }
        } else {
            let contaminated = case_status == "contaminated";
            let reason = format!(
                "probe exit {real_exit}; case_status={case_status}; artifact={artifact_status}"
            );
            let _ = matrix_rpc(
                &a.matrix_coord,
                &json!({ "op": "case_failed", "case_id": case.case_id, "reason": reason, "contaminated": contaminated, "result": result }),
            );
            log.emit("error", "stage4.case_failed", json!({ "case_id": case.case_id, "reason": reason, "contaminated": contaminated }));
            println!("STAGE4_RUNNER_HALTED reason=case_failed");
            return exit::MATRIX_HALTED;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASE: &str = r#"{
        "run_id":"i63s3-x","case_id":"i63s3-x/c3/s2/32mib","phase":"measured",
        "cycle":3,"slot":2,"chunk_size_bytes":33554432,"extent_bytes":2147483648,
        "expected_chunk_count":64
    }"#;

    fn cfg() -> MatrixRunnerConfig {
        MatrixRunnerConfig {
            probe_path: "X:\\bamep-i63-stage2-probe.exe".into(),
            coord: "192.168.99.1:9206".into(),
            sink: "192.168.99.1:9299".into(),
            wss: "192.168.99.1:8443".into(),
            pin_hex: "aa".repeat(32),
            runtime_credential_out: "X:\\rt.cred".into(),
            select_model_substr: "256GB".into(),
            seal_timeout_secs: 300,
        }
    }

    #[test]
    fn parse_case_reads_the_engine_case_shape_incl_phase_cycle_slot() {
        let c = parse_case(CASE).unwrap();
        assert_eq!(c.case_id, "i63s3-x/c3/s2/32mib");
        assert_eq!(c.phase, "measured");
        assert_eq!(c.cycle, Some(3));
        assert_eq!(c.slot, Some(2));
        assert_eq!(c.chunk_size_bytes, 33_554_432);
        assert_eq!(c.extent_bytes, 2_147_483_648);
        assert_eq!(c.expected_chunk_count, 64);
    }

    #[test]
    fn parse_case_warmup_has_no_cycle_or_slot() {
        let w = r#"{"run_id":"r","case_id":"r/warmup/08mib","phase":"warmup","cycle":null,"slot":null,"chunk_size_bytes":8388608,"extent_bytes":2147483648,"expected_chunk_count":256}"#;
        let c = parse_case(w).unwrap();
        assert_eq!(c.phase, "warmup");
        assert_eq!(c.cycle, None);
        assert_eq!(c.slot, None);
    }

    #[test]
    fn parse_case_fails_closed_on_missing_fields() {
        assert!(parse_case(r#"{"run_id":"x"}"#).is_err());
        assert!(parse_case("not json").is_err());
    }

    // ---- Stage 4 ----------------------------------------------------------

    const S4_CASE: &str = r#"{
        "run_id":"i63s4-x","case_id":"i63s4-x/c2/s1/prep_ahead_2","phase":"measured",
        "mode":"prep_ahead_2","cycle":2,"slot":1,
        "chunk_size_bytes":67108864,"extent_bytes":2147483648,"expected_chunk_count":32
    }"#;

    #[test]
    fn parse_s4_case_reads_mode_and_rejects_unknown_mode() {
        let c = parse_s4_case(S4_CASE).unwrap();
        assert_eq!(c.mode, "prep_ahead_2");
        assert_eq!(c.cycle, Some(2));
        assert_eq!(c.chunk_size_bytes, 67_108_864);
        assert_eq!(c.expected_chunk_count, 32);
        let bad = S4_CASE.replace("prep_ahead_2", "turbo");
        assert!(parse_s4_case(&bad).is_err());
        assert!(parse_s4_case(r#"{"run_id":"x"}"#).is_err());
    }

    #[test]
    fn s4_probe_argv_appends_mode_and_keeps_the_stage3_argv() {
        let c = parse_s4_case(S4_CASE).unwrap();
        let argv = s4_probe_argv(&cfg(), &c, "X:\\enroll.cred");
        let pos = |k: &str| argv.iter().position(|a| a == k).map(|i| argv[i + 1].clone());
        assert_eq!(pos("--mode").as_deref(), Some("prep_ahead_2"));
        assert_eq!(pos("--chunk-size").as_deref(), Some("67108864"));
        assert_eq!(pos("--extent-bytes").as_deref(), Some("2147483648"));
        assert_eq!(pos("--case-id").as_deref(), Some("i63s4-x/c2/s1/prep_ahead_2"));
        // exactly one --mode
        assert_eq!(argv.iter().filter(|a| *a == "--mode").count(), 1);
    }

    #[test]
    fn build_s4_case_result_shape_incl_mode_and_prepared_peak() {
        let c = parse_s4_case(S4_CASE).unwrap();
        let pr: Value = serde_json::from_str(
            r#"{"event":"probe.case_result","mode":"prep_ahead_2","chunk_count":32,
                "bulk_stream_wall_ms":40000,"verified_transfer_wall_ms":46000,"resume_ms":3,
                "seal_d2_ms":5000,"read_ms":5700,"chunk_sha_ms":5400,"rolling_sha_ms":6100,
                "proof_ms":6,"put_ack_ms":33000,"prepared_buffer_peak":2,"device_read_count":32,
                "transfer_id":"t","artifact_id":"a","final_artifact_status":"Verified",
                "case_status":"completed"}"#,
        )
        .unwrap();
        let r = build_s4_case_result(&c, Some(&pr), 0);
        assert_eq!(r["mode"], "prep_ahead_2");
        assert_eq!(r["chunk_count"], 32);
        assert_eq!(r["prepared_buffer_peak"], 2);
        assert_eq!(r["final_artifact_status"], "Verified");
        assert_eq!(r["cycle"], 2);
        // exact field set the engine's `S4CaseResult` deserialises (asserted
        // against real deserialisation in the engine's own test suite).
        for k in [
            "run_id", "case_id", "mode", "phase", "cycle", "slot", "chunk_size_bytes",
            "extent_bytes", "chunk_count", "transfer_id", "artifact_id", "bulk_stream_wall_ms",
            "verified_transfer_wall_ms", "resume_ms", "seal_d2_ms", "read_ms", "chunk_sha_ms",
            "rolling_sha_ms", "proof_ms", "put_ack_ms", "prepared_buffer_peak", "device_read_count",
            "put_window", "put_started_count", "put_completed_count", "peak_puts_in_flight",
            "put_starts_ascending", "final_artifact_status", "case_status",
        ] {
            assert!(r.get(k).is_some(), "missing S4CaseResult field {k}");
        }
        // non-window modes carry the defaulted window fields
        assert_eq!(r["put_window"], 0);
        assert_eq!(r["put_starts_ascending"], false);
    }

    // ---- window_8 candidate --------------------------------------------

    const W8_CASE: &str = r#"{
        "run_id":"i63w8-x","case_id":"i63w8-x/c1/s2/prep_ahead_window_8","phase":"measured",
        "mode":"prep_ahead_window_8","cycle":1,"slot":2,
        "chunk_size_bytes":67108864,"extent_bytes":2147483648,"expected_chunk_count":32
    }"#;

    #[test]
    fn batch8_mode_is_forwarded_without_changing_window_pipeline_arguments() {
        let c = parse_s4_case(&W8_CASE.replace("prep_ahead_window_8", "prep_ahead_window_8_batch_8")).unwrap();
        let argv = s4_probe_argv(&cfg(), &c, "X:\\rt.cred");
        let i = argv.iter().position(|a| a == "--mode").unwrap();
        assert_eq!(argv[i+1], "prep_ahead_window_8_batch_8");
        assert_eq!(c.chunk_size_bytes, 67108864);
    }

    #[test]
    fn parse_s4_case_accepts_the_window8_mode() {
        let c = parse_s4_case(W8_CASE).unwrap();
        assert_eq!(c.mode, "prep_ahead_window_8");
        let argv = s4_probe_argv(&cfg(), &c, "X:\\rt.cred");
        let pos = |k: &str| argv.iter().position(|a| a == k).map(|i| argv[i + 1].clone());
        assert_eq!(pos("--mode").as_deref(), Some("prep_ahead_window_8"));
    }

    #[test]
    fn build_s4_case_result_forwards_the_window8_fields() {
        let c = parse_s4_case(W8_CASE).unwrap();
        let pr: Value = serde_json::from_str(
            r#"{"event":"probe.case_result","mode":"prep_ahead_window_8","chunk_count":32,
                "bulk_stream_wall_ms":21000,"verified_transfer_wall_ms":27000,"resume_ms":3,
                "seal_d2_ms":5000,"read_ms":5700,"chunk_sha_ms":5400,"rolling_sha_ms":6100,
                "proof_ms":6,"put_ack_ms":90000,"prepared_buffer_peak":9,"device_read_count":32,
                "put_window":8,"put_started_count":32,"put_completed_count":32,
                "peak_puts_in_flight":8,"put_starts_ascending":true,
                "transfer_id":"t","artifact_id":"a","final_artifact_status":"Verified",
                "case_status":"completed"}"#,
        )
        .unwrap();
        let r = build_s4_case_result(&c, Some(&pr), 0);
        assert_eq!(r["mode"], "prep_ahead_window_8");
        assert_eq!(r["put_window"], 8);
        assert_eq!(r["put_started_count"], 32);
        assert_eq!(r["put_completed_count"], 32);
        assert_eq!(r["peak_puts_in_flight"], 8);
        assert_eq!(r["put_starts_ascending"], true);
        assert_eq!(r["prepared_buffer_peak"], 9);
    }

    #[test]
    fn build_s4_case_result_without_probe_line_is_a_failure_record() {
        let c = parse_s4_case(S4_CASE).unwrap();
        let r = build_s4_case_result(&c, None, 70);
        assert!(r["case_status"].as_str().unwrap().starts_with("failed:no_probe_result"));
        assert_eq!(r["final_artifact_status"], "none");
        assert_eq!(r["chunk_count"], 32);
    }

    #[test]
    fn stage4_not_armed_returns_zero() {
        assert_eq!(run_stage4_not_armed(), 0);
    }

    #[test]
    fn probe_argv_propagates_chunk_size_extent_creds_and_pin() {
        let c = parse_case(CASE).unwrap();
        let argv = probe_argv(&cfg(), &c, "X:\\enroll.cred");
        let pos = |k: &str| argv.iter().position(|a| a == k).map(|i| argv[i + 1].clone());
        assert_eq!(pos("--chunk-size").as_deref(), Some("33554432"));
        assert_eq!(pos("--extent-bytes").as_deref(), Some("2147483648"));
        assert_eq!(pos("--case-id").as_deref(), Some("i63s3-x/c3/s2/32mib"));
        assert_eq!(pos("--run-id").as_deref(), Some("i63s3-x"));
        assert_eq!(pos("--auth-credential-file").as_deref(), Some("X:\\enroll.cred"));
        assert_eq!(pos("--runtime-credential-out").as_deref(), Some("X:\\rt.cred"));
        assert_eq!(pos("--pin").as_deref(), Some("aa".repeat(32).as_str()));
    }

    #[test]
    fn not_armed_body_returns_zero_and_does_nothing() {
        assert_eq!(run_not_armed(), 0);
    }

    #[test]
    fn extract_probe_exit_and_case_result_from_mixed_output() {
        let out = concat!(
            "some noise\n",
            r#"{"ts_ms":1,"seq":9,"level":"info","event":"probe.case_result","case_id":"c1","chunk_count":64,"bulk_stream_wall_ms":60000,"verified_transfer_wall_ms":66000,"resume_ms":3,"seal_d2_ms":5000,"read_ms":12000,"chunk_sha_ms":4000,"rolling_sha_ms":4000,"proof_ms":20,"put_ack_ms":40000,"transfer_id":"t","artifact_id":"a","source_safety_verdict":"accept","clock_skew_verdict":"in_bound","final_artifact_status":"Verified","case_status":"completed"}"#,
            "\n\nBAMEP_I63_STAGE2_PROBE_EXITCODE=0\n",
        );
        assert_eq!(extract_probe_exit(out), Some(0));
        let pr = extract_case_result(out).unwrap();
        assert_eq!(pr["case_status"], "completed");
        let case = parse_case(CASE).unwrap();
        let r = build_case_result(&case, Some(&pr), 0);
        assert_eq!(r["phase"], "measured");
        assert_eq!(r["cycle"], 3);
        assert_eq!(r["chunk_count"], 64);
        assert_eq!(r["final_artifact_status"], "Verified");
        assert_eq!(r["connection_count"]["expected_total"], 66);
        assert!((r["verified_transfer_mib_s"].as_f64().unwrap() - 2048.0 / 66.0).abs() < 1.0);
    }

    #[test]
    fn build_case_result_without_a_probe_line_is_a_failure_record() {
        let case = parse_case(CASE).unwrap();
        let r = build_case_result(&case, None, 74);
        assert!(r["case_status"].as_str().unwrap().starts_with("failed:no_probe_result"));
        assert_eq!(r["final_artifact_status"], "none");
        assert_eq!(r["chunk_count"], 64);
    }
}
