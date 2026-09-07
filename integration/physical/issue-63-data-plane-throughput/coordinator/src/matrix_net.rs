//! Issue #63 Stage 3 — the ARMED networked wiring around the pure Stage-2
//! [`crate::matrix::MatrixCoordinator`]. THROWAWAY Spike.
//!
//! This is the only place in the coordinator that opens a matrix TCP listener.
//! It is reached ONLY from `coordinator --matrix --arm ...` (the Stage-3 lab
//! supervisor passes `--arm` after its own host-side preflight). `--matrix`
//! WITHOUT `--arm` still prints `PHYSICAL MATRIX NOT ARMED` and exits.
//!
//! Protocol: one JSON object per line in, one JSON object per line out, one
//! request per connection (the WinPE matrix runner opens a fresh connection per
//! request, exactly like the Stage-1 `coord` round-trip).
//!
//!   {"op":"server_utc"}
//!       -> {"server_utc_ms":N}
//!   {"op":"next_case"}
//!       -> {"case":<Case>} | {"matrix_completed":{"completed":N}} | {"halt":"<reason>"}
//!   {"op":"case_ready","case_id":"..."}    -> {"ack":true} | {"error":"..."} | {"halt":"..."}
//!   {"op":"case_started","case_id":"..."}  -> {"ack":true} | {"error":"..."} | {"halt":"..."}
//!   {"op":"case_completed","case_id":"...","result":<CaseResult>}
//!       -> {"ack":true} | {"halt":"..."}
//!   {"op":"case_failed","case_id":"...","reason":"...","contaminated":bool}
//!       -> {"halt":"..."}
//!
//! On the FIRST terminal outcome (all 36 cases completed, or any halt) it writes
//! `analysis.json` + the one-word `--verdict-file` marker (`matrix_pass` /
//! `matrix_fail`), prints `STAGE3_MATRIX_TERMINAL marker=...`, drains for a few
//! seconds so a trailing probe sink flush still lands, then exits (0 pass / 10
//! fail). This mirrors the Stage-1 coordinator lifecycle.
//!
//! It NEVER opens a device, runs a transfer, connects to PostgreSQL, or speaks
//! the Agent/Worker protocols. The real #61-shaped Server/Worker services are a
//! SEPARATE process (`stage3-harness`); this coordinator is only the typed
//! case authority + the probe evidence sink.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use bamep_i63_stage2_engine::analysis::analyse;
use bamep_i63_stage2_engine::result::CaseResult;

use crate::matrix::{LabRequest, LabResponse, MatrixCoordinator, MatrixStartError};

const DRAIN_SECS: u64 = 5;

pub struct Cfg {
    pub matrix_addr: String,
    pub sink_addr: String,
    pub evidence_dir: PathBuf,
    pub run_id: String,
    pub verdict_file: Option<String>,
    /// Free bytes observed by the supervisor under the Worker storage root; the
    /// pure disk-budget gate fails closed below the Stage-3 threshold.
    pub observed_free_bytes: u64,
}

pub fn parse_cfg() -> Cfg {
    let mut matrix_addr = "192.168.99.1:9210".to_string();
    let mut sink_addr = "192.168.99.1:9299".to_string();
    let mut evidence_dir = PathBuf::from("stage3-evidence");
    let mut run_id = "i63s3".to_string();
    let mut verdict_file: Option<String> = None;
    let mut observed_free_bytes: u64 = 0;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--matrix" | "--arm" => {}
            "--matrix-addr" => matrix_addr = it.next().unwrap_or(matrix_addr),
            "--sink-addr" => sink_addr = it.next().unwrap_or(sink_addr),
            "--evidence-dir" => {
                evidence_dir = PathBuf::from(it.next().unwrap_or_else(|| "stage3-evidence".into()))
            }
            "--run-id" => run_id = it.next().unwrap_or(run_id),
            "--verdict-file" => verdict_file = it.next(),
            "--observed-free-bytes" => {
                observed_free_bytes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| {
                        eprintln!("coordinator --matrix --arm: --observed-free-bytes needs a u64");
                        std::process::exit(2);
                    })
            }
            other => {
                eprintln!("coordinator --matrix --arm: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    Cfg {
        matrix_addr,
        sink_addr,
        evidence_dir,
        run_id,
        verdict_file,
        observed_free_bytes,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct State {
    coord: MatrixCoordinator,
    evidence_dir: PathBuf,
    run_id: String,
    verdict_file: Option<String>,
    done: bool,
    terminal_spawned: bool,
    events_written: u64,
}

impl State {
    fn append(&self, name: &str, line: &str) {
        let p = self.evidence_dir.join(name);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{line}");
        }
    }

    fn record_event(&mut self, kind: &str, detail: Value) {
        self.events_written += 1;
        let line = json!({
            "ts_ms": now_ms(),
            "seq": self.events_written,
            "run_id": self.run_id,
            "kind": kind,
            "detail": detail,
        });
        self.append("coordinator-events.ndjson", &line.to_string());
    }
}

/// The one expected exit path: on the first terminal outcome, write the verdict
/// marker, drain briefly, then exit.
fn spawn_drain_then_exit(marker: &'static str, verdict_file: Option<String>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(DRAIN_SECS));
        if let Some(path) = &verdict_file {
            let _ = std::fs::write(path, marker);
        }
        println!("STAGE3_MATRIX_TERMINAL marker={marker} drained_secs={DRAIN_SECS}");
        let _ = std::io::stdout().flush();
        std::process::exit(if marker.ends_with("pass") { 0 } else { 10 });
    });
}

fn maybe_go_terminal(state: &mut State, marker: &'static str) {
    if state.terminal_spawned {
        return;
    }
    state.terminal_spawned = true;
    state.done = true;
    // Final analysis over every recorded CaseResult.
    let a = analyse(state.coord.results());
    let analysis_json = serde_json::to_string_pretty(&a).unwrap_or_else(|_| "{}".into());
    let _ = std::fs::write(state.evidence_dir.join("analysis.json"), analysis_json);
    state.record_event(
        "matrix_terminal",
        json!({
            "marker": marker,
            "completed": state.coord.results().len(),
            "matrix_succeeded": state.coord.matrix_succeeded(),
        }),
    );
    spawn_drain_then_exit(marker, state.verdict_file.clone());
}

fn write_plan(state: &State) {
    // Snapshot the deterministic 36-case plan for the run directory.
    let plan = &state.coord;
    let cases: Vec<Value> = plan
        .plan_cases()
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .collect();
    let doc = json!({ "run_id": state.run_id, "cases": cases });
    let _ = std::fs::write(
        state.evidence_dir.join("matrix-plan.json"),
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into()),
    );
}

/// Handle one matrix-runner request line; return the JSON response line.
fn handle_request(state: &Arc<Mutex<State>>, raw: &str) -> String {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("bad request json: {e}") }).to_string(),
    };
    let op = v.get("op").and_then(|x| x.as_str()).unwrap_or("");
    let mut g = state.lock().unwrap();

    if g.done && op != "server_utc" {
        return json!({ "halt": "matrix already terminal" }).to_string();
    }

    match op {
        "server_utc" => json!({ "server_utc_ms": now_ms() }).to_string(),

        "next_case" => {
            g.record_event("next_case", json!({}));
            match g.coord.handle(LabRequest::NextCase) {
                LabResponse::Case(case) => {
                    let case_json = serde_json::to_value(&*case).unwrap_or(Value::Null);
                    g.record_event("case_handed_out", json!({ "case_id": case.case_id }));
                    json!({ "case": case_json }).to_string()
                }
                LabResponse::MatrixCompleted { completed, .. } => {
                    g.record_event("matrix_completed", json!({ "completed": completed }));
                    let marker = if g.coord.matrix_succeeded() {
                        "matrix_pass"
                    } else {
                        "matrix_fail"
                    };
                    let resp = json!({ "matrix_completed": { "completed": completed } }).to_string();
                    maybe_go_terminal(&mut g, marker);
                    resp
                }
                LabResponse::MatrixHalted(halt) => {
                    let reason = format!("{halt:?}");
                    g.record_event("matrix_halted", json!({ "reason": reason }));
                    let resp = json!({ "halt": reason }).to_string();
                    maybe_go_terminal(&mut g, "matrix_fail");
                    resp
                }
                LabResponse::Rejected(m) => json!({ "error": m }).to_string(),
                LabResponse::Ack => json!({ "error": "unexpected Ack for next_case" }).to_string(),
            }
        }

        "case_ready" | "case_started" => {
            let case_id = v.get("case_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let req = if op == "case_ready" {
                LabRequest::CaseReady { case_id: case_id.clone() }
            } else {
                LabRequest::CaseStarted { case_id: case_id.clone() }
            };
            g.record_event(op, json!({ "case_id": case_id }));
            match g.coord.handle(req) {
                LabResponse::Ack => json!({ "ack": true }).to_string(),
                LabResponse::MatrixHalted(halt) => {
                    let reason = format!("{halt:?}");
                    let resp = json!({ "halt": reason }).to_string();
                    maybe_go_terminal(&mut g, "matrix_fail");
                    resp
                }
                LabResponse::Rejected(m) => json!({ "error": m }).to_string(),
                other => json!({ "error": format!("unexpected {other:?}") }).to_string(),
            }
        }

        "case_completed" => {
            let case_id = v.get("case_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let result: CaseResult = match v.get("result").cloned().map(serde_json::from_value) {
                Some(Ok(r)) => r,
                Some(Err(e)) => {
                    return json!({ "error": format!("bad CaseResult: {e}") }).to_string()
                }
                None => return json!({ "error": "case_completed needs result" }).to_string(),
            };
            g.append("case-results.ndjson", &result.to_ndjson_line());
            g.record_event(
                "case_completed",
                json!({
                    "case_id": case_id,
                    "case_status": result.case_status,
                    "artifact": result.final_artifact_status,
                    "verified_transfer_wall_ms": result.verified_transfer_wall_ms,
                    "bulk_stream_wall_ms": result.bulk_stream_wall_ms,
                }),
            );
            match g.coord.handle(LabRequest::CaseCompleted {
                case_id,
                result: Box::new(result),
            }) {
                LabResponse::Ack => json!({ "ack": true }).to_string(),
                LabResponse::MatrixHalted(halt) => {
                    let reason = format!("{halt:?}");
                    let resp = json!({ "halt": reason }).to_string();
                    maybe_go_terminal(&mut g, "matrix_fail");
                    resp
                }
                LabResponse::Rejected(m) => json!({ "error": m }).to_string(),
                other => json!({ "error": format!("unexpected {other:?}") }).to_string(),
            }
        }

        "case_failed" => {
            let case_id = v.get("case_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let reason = v
                .get("reason")
                .and_then(|x| x.as_str())
                .unwrap_or("unspecified")
                .to_string();
            let contaminated = v
                .get("contaminated")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            // A failed / contaminated case still carries a CaseResult when the
            // probe produced one (e.g. contaminated-but-Verified). Preserve it —
            // evidence must not be lost just because the matrix stops here.
            if let Some(Ok(r)) = v
                .get("result")
                .cloned()
                .map(serde_json::from_value::<CaseResult>)
            {
                g.append("case-results.ndjson", &r.to_ndjson_line());
            }
            g.record_event(
                "case_failed",
                json!({ "case_id": case_id, "reason": reason, "contaminated": contaminated }),
            );
            let resp = match g.coord.handle(LabRequest::CaseFailed {
                case_id,
                reason,
                contaminated,
            }) {
                LabResponse::MatrixHalted(halt) => json!({ "halt": format!("{halt:?}") }).to_string(),
                LabResponse::Rejected(m) => json!({ "error": m }).to_string(),
                other => json!({ "halt": format!("{other:?}") }).to_string(),
            };
            maybe_go_terminal(&mut g, "matrix_fail");
            resp
        }

        other => json!({ "error": format!("unknown op {other:?}") }).to_string(),
    }
}

fn handle_conn(mut s: TcpStream, state: &Arc<Mutex<State>>) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(15)));
    let mut reader = BufReader::new(match s.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let resp = handle_request(state, line.trim());
    let _ = s.write_all(resp.as_bytes());
    let _ = s.write_all(b"\n");
    let _ = s.flush();
}

/// The probe evidence sink: each probe connection writes its cumulative NDJSON
/// snapshot; we de-dup by `seq` within THIS run and append fresh lines to
/// `probe-evidence.ndjson`.
fn run_sink(listener: TcpListener, state: Arc<Mutex<State>>) {
    let seen: Arc<Mutex<BTreeSet<u64>>> = Arc::new(Mutex::new(BTreeSet::new()));
    for stream in listener.incoming().flatten() {
        let state = Arc::clone(&state);
        let seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut s = stream;
            let _ = s.set_read_timeout(Some(Duration::from_secs(20)));
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            let _ = s.write_all(b"ok\n");
            let mut fresh = 0u64;
            for raw in buf.lines() {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(raw) {
                    if let Some(seq) = v.get("seq").and_then(|x| x.as_u64()) {
                        if !seen.lock().unwrap().insert(seq) {
                            continue;
                        }
                    }
                }
                fresh += 1;
                state.lock().unwrap().append("probe-evidence.ndjson", raw);
            }
            if fresh > 0 {
                state
                    .lock()
                    .unwrap()
                    .record_event("probe_sink_ingest", json!({ "fresh_lines": fresh }));
            }
        });
    }
}

/// Entry point for `coordinator --matrix --arm ...`.
pub fn run(cfg: Cfg) -> ! {
    std::fs::create_dir_all(&cfg.evidence_dir).unwrap_or_else(|e| {
        eprintln!("coordinator --matrix --arm: cannot create evidence dir: {e}");
        std::process::exit(1);
    });

    let coord = match MatrixCoordinator::new(&cfg.run_id, cfg.observed_free_bytes) {
        Ok(c) => c,
        Err(MatrixStartError::Plan(e)) => {
            eprintln!("STAGE3_MATRIX_START_FAIL plan_arithmetic={e:?}");
            std::process::exit(1);
        }
        Err(MatrixStartError::Budget(v)) => {
            eprintln!("STAGE3_MATRIX_START_FAIL disk_budget={v:?}");
            std::process::exit(1);
        }
    };
    println!(
        "STAGE3_MATRIX_ARMED run_id={} matrix={} sink={} budget={:?}",
        cfg.run_id,
        cfg.matrix_addr,
        cfg.sink_addr,
        coord.budget()
    );

    let matrix = TcpListener::bind(&cfg.matrix_addr).unwrap_or_else(|e| {
        eprintln!("coordinator --matrix --arm: bind matrix {}: {e}", cfg.matrix_addr);
        std::process::exit(1);
    });
    let sink = TcpListener::bind(&cfg.sink_addr).unwrap_or_else(|e| {
        eprintln!("coordinator --matrix --arm: bind sink {}: {e}", cfg.sink_addr);
        std::process::exit(1);
    });

    let state = Arc::new(Mutex::new(State {
        coord,
        evidence_dir: cfg.evidence_dir.clone(),
        run_id: cfg.run_id.clone(),
        verdict_file: cfg.verdict_file.clone(),
        done: false,
        terminal_spawned: false,
        events_written: 0,
    }));
    {
        let g = state.lock().unwrap();
        write_plan(&g);
        // truncate the streaming evidence files so their presence is a signal
        for f in ["coordinator-events.ndjson", "case-results.ndjson", "probe-evidence.ndjson"] {
            let _ = std::fs::write(g.evidence_dir.join(f), b"");
        }
    }

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || run_sink(sink, state));
    }

    println!(
        "STAGE3_MATRIX_LISTENING matrix={} sink={} evidence_dir={}",
        cfg.matrix_addr,
        cfg.sink_addr,
        cfg.evidence_dir.display()
    );
    let _ = std::io::stdout().flush();

    for stream in matrix.incoming().flatten() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle_conn(stream, &state));
    }
    // The accept loop never breaks on its own; the drain-then-exit thread owns
    // the one expected exit.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamep_i63_stage2_engine::result::ConnectionCount;
    use bamep_i63_stage2_engine::GIB;

    fn state_for(dir: &std::path::Path) -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(State {
            coord: MatrixCoordinator::new("i63s3-net-test", 120 * GIB).unwrap(),
            evidence_dir: dir.to_path_buf(),
            run_id: "i63s3-net-test".into(),
            verdict_file: None,
            done: false,
            terminal_spawned: true, // block the real process::exit path in tests
            events_written: 0,
        }))
    }

    fn ok_result_json(case: &Value) -> Value {
        let extent = case["extent_bytes"].as_u64().unwrap();
        let chunks = case["expected_chunk_count"].as_u64().unwrap();
        let (b_mib, b_mb) = CaseResult::rates(extent, 60_000.0);
        let (v_mib, v_mb) = CaseResult::rates(extent, 66_000.0);
        json!({
            "run_id": case["run_id"], "case_id": case["case_id"], "phase": case["phase"],
            "cycle": case["cycle"], "slot": case["slot"],
            "chunk_size_bytes": case["chunk_size_bytes"], "extent_bytes": extent,
            "chunk_count": chunks, "transfer_id": "t", "artifact_id": "a",
            "source_safety_verdict": "accept", "clock_skew_verdict": "in_bound(skew_after_ms=-6)",
            "bulk_stream_wall_ms": 60_000.0, "bulk_stream_mib_s": b_mib, "bulk_stream_mb_s": b_mb,
            "verified_transfer_wall_ms": 66_000.0, "verified_transfer_mib_s": v_mib,
            "verified_transfer_mb_s": v_mb, "resume_ms": 3.0, "seal_d2_ms": 5000.0,
            "read_ms": 1.0, "chunk_sha_ms": 1.0, "rolling_sha_ms": 1.0, "proof_ms": 1.0,
            "put_ack_ms": 1.0,
            "connection_count": serde_json::to_value(ConnectionCount::by_construction(chunks)).unwrap(),
            "final_artifact_status": "Verified", "case_status": "completed",
        })
    }

    #[test]
    fn full_36_case_line_protocol_walkthrough_reaches_matrix_completed() {
        let dir = std::env::temp_dir().join(format!("i63s3net-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = state_for(&dir);
        let mut handled = 0;
        loop {
            let r = handle_request(&st, r#"{"op":"next_case"}"#);
            let v: Value = serde_json::from_str(&r).unwrap();
            if let Some(mc) = v.get("matrix_completed") {
                assert_eq!(mc["completed"].as_u64().unwrap(), 36);
                break;
            }
            let case = v.get("case").expect("a case").clone();
            let cid = case["case_id"].as_str().unwrap();
            let ready = handle_request(
                &st,
                &json!({ "op": "case_ready", "case_id": cid }).to_string(),
            );
            assert_eq!(serde_json::from_str::<Value>(&ready).unwrap()["ack"], json!(true));
            let started = handle_request(
                &st,
                &json!({ "op": "case_started", "case_id": cid }).to_string(),
            );
            assert_eq!(serde_json::from_str::<Value>(&started).unwrap()["ack"], json!(true));
            let done = handle_request(
                &st,
                &json!({ "op": "case_completed", "case_id": cid, "result": ok_result_json(&case) })
                    .to_string(),
            );
            assert_eq!(serde_json::from_str::<Value>(&done).unwrap()["ack"], json!(true));
            handled += 1;
        }
        assert_eq!(handled, 36);
        assert!(st.lock().unwrap().coord.matrix_succeeded());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_case_failed_halts_the_matrix_and_no_more_cases_are_handed_out() {
        let dir = std::env::temp_dir().join(format!("i63s3net-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = state_for(&dir);
        let r = handle_request(&st, r#"{"op":"next_case"}"#);
        let case = serde_json::from_str::<Value>(&r).unwrap()["case"].clone();
        let cid = case["case_id"].as_str().unwrap().to_string();
        handle_request(&st, &json!({ "op": "case_ready", "case_id": cid }).to_string());
        let failed = handle_request(
            &st,
            &json!({ "op": "case_failed", "case_id": cid, "reason": "digest mismatch", "contaminated": false })
                .to_string(),
        );
        assert!(serde_json::from_str::<Value>(&failed).unwrap().get("halt").is_some());
        let next = handle_request(&st, r#"{"op":"next_case"}"#);
        assert!(serde_json::from_str::<Value>(&next).unwrap().get("halt").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_utc_is_answered_even_after_terminal() {
        let dir = std::env::temp_dir().join(format!("i63s3net-utc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = state_for(&dir);
        st.lock().unwrap().done = true;
        let r = handle_request(&st, r#"{"op":"server_utc"}"#);
        assert!(serde_json::from_str::<Value>(&r).unwrap()["server_utc_ms"].as_i64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
