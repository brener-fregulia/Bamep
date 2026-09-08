//! Issue #63 **Stage 4** — the ARMED networked wiring for the 64 MiB
//! serial-vs-prep-ahead micro-matrix. THROWAWAY Spike.
//!
//! Reached ONLY from `coordinator --stage4 --arm ...` (the Stage-4 lab
//! supervisor passes `--arm` after its own host-side preflight). `--stage4`
//! WITHOUT `--arm` prints `STAGE4 NOT ARMED` and exits.
//!
//! One JSON object per line in, one JSON object per line out, one request per
//! connection (the WinPE runner opens a fresh connection per request):
//!
//!   {"op":"server_utc"}                    -> {"server_utc_ms":N}
//!   {"op":"next_case"}                     -> {"case":<S4Case>}
//!                                          | {"matrix_completed":{"completed":N}}
//!                                          | {"halt":"<reason>"}
//!   {"op":"case_ready","case_id":".."}     -> {"ack":true} | {"halt":".."}
//!   {"op":"case_started","case_id":".."}   -> {"ack":true} | {"halt":".."}
//!   {"op":"case_completed","case_id":"..","result":<S4CaseResult>}
//!                                          -> {"ack":true} | {"halt":".."}
//!   {"op":"case_failed","case_id":"..","reason":"..","contaminated":bool,
//!    "result":<S4CaseResult?>}             -> {"halt":".."}
//!
//! On the first terminal outcome (10 cases completed, or any halt) it writes
//! `analysis.json` (the S-vs-P paired analysis + — if a Worker PUT timing file
//! was given and covers the measured PUTs — the Worker decomposition) and a
//! one-word `--verdict-file` marker:
//!
//!   `stage4_pass`     — 10/10 completed `Verified` AND Worker timing evidence
//!                       covers every measured PUT;
//!   `stage4_invalid`  — 10/10 completed but the Worker timing evidence is
//!                       missing / short (Q2 cannot be answered);
//!   `stage4_fail`     — any case failed / was contaminated / non-`Verified`.
//!
//! It NEVER opens a device, runs a transfer, connects to PostgreSQL, or speaks
//! the Agent/Worker protocols. The real #61-shaped Server/Worker services are a
//! SEPARATE process (`stage4-harness`).

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use bamep_i63_stage2_engine::stage4::{
    analyse_s4, analyse_w8, analyse_worker_decomp, S4Mode, S4Plan, S4CaseResult, WorkerDecomp,
    WorkerPutRecord,
};

const DRAIN_SECS: u64 = 5;

pub struct Cfg {
    pub matrix_addr: String,
    pub sink_addr: String,
    pub evidence_dir: PathBuf,
    pub run_id: String,
    pub verdict_file: Option<String>,
    /// The env-gated Worker PUT timing NDJSON file (`BAMEP_I63_WORKER_PUT_TIMING`).
    /// Read best-effort at terminal; absence/shortfall => `stage4_invalid`.
    pub worker_timing_file: Option<PathBuf>,
    /// Issue #63 window_8 candidate: serve the 5-case P-vs-W plan
    /// (`S4Plan::build_window8`) instead of the 10-case S-vs-P plan, and write
    /// the P-vs-W analysis at terminal. Same wire protocol, same markers.
    pub window8: bool,
    pub batch8: bool,
}

pub fn parse_cfg() -> Cfg {
    let mut matrix_addr = "192.168.99.1:9210".to_string();
    let mut sink_addr = "192.168.99.1:9299".to_string();
    let mut evidence_dir = PathBuf::from("stage4-evidence");
    let mut run_id = "i63s4".to_string();
    let mut verdict_file: Option<String> = None;
    let mut worker_timing_file: Option<PathBuf> = None;
    let mut window8 = false;
    let mut batch8 = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--stage4" | "--arm" => {}
            "--window8" => window8 = true,
            "--batch8" => batch8 = true,
            "--matrix-addr" => matrix_addr = it.next().unwrap_or(matrix_addr),
            "--sink-addr" => sink_addr = it.next().unwrap_or(sink_addr),
            "--evidence-dir" => {
                evidence_dir = PathBuf::from(it.next().unwrap_or_else(|| "stage4-evidence".into()))
            }
            "--run-id" => run_id = it.next().unwrap_or(run_id),
            "--verdict-file" => verdict_file = it.next(),
            "--worker-timing-file" => worker_timing_file = it.next().map(PathBuf::from),
            other => {
                eprintln!("coordinator --stage4 --arm: unknown argument {other:?}");
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
        worker_timing_file,
        window8,
        batch8,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Per-case lifecycle position (strictly linear; a `case_failed` latches Bad).
#[derive(Clone, Copy, PartialEq)]
enum CasePos {
    HandedOut,
    Ready,
    Started,
    Completed,
}

struct State {
    plan: S4Plan,
    next_index: usize,
    /// (case_id, position) of the case currently in flight.
    current: Option<(String, CasePos)>,
    results: Vec<S4CaseResult>,
    completed: usize,
    halt: Option<String>,
    done: bool,
    terminal_spawned: bool,
    evidence_dir: PathBuf,
    run_id: String,
    verdict_file: Option<String>,
    worker_timing_file: Option<PathBuf>,
    window8: bool,
    batch8: bool,
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
            "ts_ms": now_ms(), "seq": self.events_written, "run_id": self.run_id,
            "kind": kind, "detail": detail,
        });
        self.append("coordinator-events.ndjson", &line.to_string());
    }

}

/// Read + parse the Worker PUT timing NDJSON (best effort).
fn read_worker_records(path: &Option<PathBuf>) -> Vec<WorkerPutRecord> {
    let Some(path) = path else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen: BTreeSet<(String, u64)> = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<WorkerPutRecord>(line) {
            if seen.insert((r.transfer_id.clone(), r.chunk_index)) {
                out.push(r);
            }
        }
    }
    out
}

/// The Worker decomposition, split by mode via the case results' transfer_ids.
/// Returns `(overall, per_mode)` where `per_mode` carries one entry per mode
/// that appears in the MEASURED results (record count + decomposition).
fn worker_decomp(
    results: &[S4CaseResult],
    records: &[WorkerPutRecord],
) -> (WorkerDecomp, Vec<(S4Mode, usize, WorkerDecomp)>) {
    let mode_of = |tid: &str| -> Option<S4Mode> {
        results
            .iter()
            .find(|r| r.is_measured() && r.transfer_id.as_deref() == Some(tid))
            .map(|r| r.mode)
    };
    let mut by_mode: Vec<(S4Mode, Vec<WorkerPutRecord>)> = Vec::new();
    let mut overall = Vec::new();
    for rec in records {
        let Some(mode) = mode_of(&rec.transfer_id) else { continue };
        overall.push(rec.clone());
        match by_mode.iter_mut().find(|(m, _)| *m == mode) {
            Some((_, v)) => v.push(rec.clone()),
            None => by_mode.push((mode, vec![rec.clone()])),
        }
    }
    let per_mode = by_mode
        .into_iter()
        .map(|(m, v)| (m, v.len(), analyse_worker_decomp(&v)))
        .collect();
    (analyse_worker_decomp(&overall), per_mode)
}

fn spawn_drain_then_exit(marker: &'static str, verdict_file: Option<String>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(DRAIN_SECS));
        if let Some(path) = &verdict_file {
            let _ = std::fs::write(path, marker);
        }
        println!("STAGE4_MATRIX_TERMINAL marker={marker} drained_secs={DRAIN_SECS}");
        let _ = std::io::stdout().flush();
        std::process::exit(if marker == "stage4_pass" { 0 } else { 10 });
    });
}

fn maybe_go_terminal(st: &mut State) {
    if st.terminal_spawned {
        return;
    }
    st.terminal_spawned = true;
    st.done = true;

    let records = read_worker_records(&st.worker_timing_file);
    let (overall, per_mode) = worker_decomp(&st.results, &records);

    // The Worker timing must cover every measured PUT of EVERY planned mode.
    let mut expected_modes: Vec<(S4Mode, u64)> = Vec::new();
    for c in st.plan.measured() {
        match expected_modes.iter_mut().find(|(m, _)| *m == c.mode) {
            Some((_, n)) => *n += c.expected_chunk_count,
            None => expected_modes.push((c.mode, c.expected_chunk_count)),
        }
    }
    let records_for = |mode: S4Mode| -> u64 {
        per_mode
            .iter()
            .find(|(m, _, _)| *m == mode)
            .map(|(_, n, _)| *n as u64)
            .unwrap_or(0)
    };
    let worker_ok = expected_modes
        .iter()
        .all(|(mode, expect)| records_for(*mode) >= *expect);

    let all_completed = st.halt.is_none() && st.completed == st.plan.cases.len();
    let marker: &'static str = if !all_completed {
        "stage4_fail"
    } else if !worker_ok {
        "stage4_invalid"
    } else {
        "stage4_pass"
    };

    let decomp_by_mode: Vec<Value> = per_mode
        .iter()
        .map(|(m, n, d)| json!({ "mode": m.wire(), "records": n, "decomposition": d }))
        .collect();
    let coverage: Vec<Value> = expected_modes
        .iter()
        .map(|(m, expect)| {
            json!({ "mode": m.wire(), "expected": expect, "records": records_for(*m) })
        })
        .collect();
    let worker_decomposition = json!({
        "coverage": coverage,
        "covers_measured_puts": worker_ok,
        "overall": overall,
        "by_mode": decomp_by_mode,
    });

    let mut analysis = if st.batch8 {
        batch8_analysis(st, &records, worker_decomposition, marker)
    } else if st.window8 {
        let p_vs_w = analyse_w8(&st.results);
        json!({
            "run_id": st.run_id,
            "candidate": "prep_ahead_window_8",
            "verdict": marker,
            "completed": st.completed,
            "total_cases": st.plan.cases.len(),
            "halt": st.halt,
            "p_vs_w": p_vs_w,
            "worker_decomposition": worker_decomposition,
            "question": "does window_8 break the per-chunk PUT throughput ceiling? W decomposition intervals OVERLAP ACROSS PUTs — never sum per-PUT times against wall-clock",
        })
    } else {
        let s_vs_p = analyse_s4(&st.results);
        json!({
            "run_id": st.run_id,
            "verdict": marker,
            "completed": st.completed,
            "total_cases": st.plan.cases.len(),
            "halt": st.halt,
            "s_vs_p": s_vs_p,
            "worker_decomposition": worker_decomposition,
            "q1": "compare median prep_ahead bulk MiB/s vs median serial; paired P/S ratios (n=4, no significance)",
            "q2": "see worker_decomposition; body_pump_ms & staging_worker_ms OVERLAP",
        })
    };
    let marker = if st.batch8 && marker == "stage4_pass"
        && analysis["contamination"].as_array().is_some_and(|c| !c.is_empty()) {
        analysis["verdict"] = json!("stage4_invalid");
        "stage4_invalid"
    } else { marker };
    let _ = std::fs::write(
        st.evidence_dir.join("analysis.json"),
        serde_json::to_string_pretty(&analysis).unwrap_or_else(|_| "{}".into()),
    );
    st.record_event(
        "stage4_terminal",
        json!({ "marker": marker, "completed": st.completed, "worker_ok": worker_ok }),
    );
    spawn_drain_then_exit(marker, st.verdict_file.clone());
}

fn batch8_analysis(st: &State, records: &[WorkerPutRecord], worker: Value, marker: &str) -> Value {
    use bamep_i63_stage2_engine::analysis::median;
    let measured: Vec<_> = st.results.iter().filter(|r| r.is_measured()).collect();
    let batches: Vec<Value> = std::fs::read_to_string(st.evidence_dir.join("worker-batch-timing.ndjson"))
        .unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let mut contamination = Vec::new();
    if marker != "stage4_pass" { contamination.push(format!("terminal:{marker}")); }
    if let Some(h) = &st.halt { contamination.push(h.clone()); }
    for r in &st.results {
        if !r.is_completed_and_verified() || r.device_read_count != 32 || r.put_window != 8
            || r.put_started_count != 32 || r.put_completed_count != 32 || !r.put_starts_ascending
            || r.peak_puts_in_flight <= 1 || r.peak_puts_in_flight > 8 || r.prepared_buffer_peak > 9 {
            contamination.push(format!("case_invariant:{}", r.case_id));
        }
    }
    if batches.len() != 16 || st.results.iter().any(|r| (0..4).any(|n| batches.iter().filter(|b|
        b["transfer_id"].as_str() == r.transfer_id.as_deref() && b["batch_number"] == n
        && b["first_chunk_index"] == n*8 && b["last_chunk_index"] == n*8+7
        && b["member_count"] == 8 && b["outcome"] == "durable").count() != 1)) {
        contamination.push("batch_timing_missing_or_failed".into());
    }
    let raw_puts: Vec<Value> = st.worker_timing_file.as_ref().and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    if st.worker_timing_file.is_some() && (raw_puts.len() != 128 || st.results.iter().any(|r|
        (0..32).any(|n| raw_puts.iter().filter(|p| p["transfer_id"].as_str() == r.transfer_id.as_deref()
            && p["chunk_index"] == n && p["outcome"] == "accepted").count() != 1))) {
        contamination.push("put_timing_duplicate_missing_or_failed".into());
    }
    let errors = records.iter().filter(|r| r.outcome.contains("control_error")).count();
    if errors > 0 { contamination.push("control_error".into()); }
    let mib = median(measured.iter().map(|r| r.bulk_mib_s()).collect());
    let mb = median(measured.iter().map(|r| r.bulk_mb_s()).collect());
    let classification = if !contamination.is_empty() || measured.len() != 3 { "D — NO USEFUL SOLUTION" }
        else if mb >= 100.0 { "A — TARGET MET" } else if mib >= 80.0 { "B — MAJOR IMPROVEMENT" }
        else if mib >= 60.0 { "C — MATERIAL BUT INSUFFICIENT" } else { "D — NO USEFUL SOLUTION" };
    let distributions: Vec<Value> = ["file_sync_sum_ns", "dir_fsync_ns", "batch_finalize_total_ns"].into_iter().map(|key| {
        let mut samples: Vec<f64> = batches.iter().filter(|b| measured.iter().any(|r| r.transfer_id.as_deref() == b["transfer_id"].as_str()))
            .filter_map(|b| b[key].as_u64()).map(|n| n as f64 / 1e6).collect();
        samples.sort_by(f64::total_cmp);
        json!({"name":key,"n":samples.len(),"median_ms":median(samples.clone()),"samples_ms":samples})
    }).collect();
    let root = st.evidence_dir.parent().unwrap_or(&st.evidence_dir);
    json!({"run_id":st.run_id,"candidate":"prep_ahead_window_8_batch_8", "verdict":marker,"classification":classification,
        "base_git_head":std::fs::read_to_string(root.join("repo-head.txt")).unwrap_or_default().trim(),
        "worktree_status":std::fs::read_to_string(root.join("repo-status.txt")).unwrap_or_default(),
        "case_outcomes":st.plan.cases.iter().map(|c|json!({"case_id":c.case_id,"result":st.results.iter().find(|r|r.case_id==c.case_id),"outcome":st.results.iter().find(|r|r.case_id==c.case_id).map(|r|r.case_status.as_str()).unwrap_or("not_completed")})).collect::<Vec<_>>(),
        "measured_bulk_walls_ms":measured.iter().map(|r|r.bulk_stream_wall_ms).collect::<Vec<_>>(),
        "measured_verified_walls_ms":measured.iter().map(|r|r.verified_transfer_wall_ms).collect::<Vec<_>>(),
        "candidate_median_mib_s":mib,"candidate_median_decimal_mb_s":mb,
        "median_2gib_bulk_seconds":median(measured.iter().map(|r|r.bulk_stream_wall_ms/1000.0).collect()),
        "ratio_vs_stage4_prep_ahead_2":mib/30.7199,"historical_baseline_run":"i63s4-20260907T214609",
        "peak_put_concurrency":st.results.iter().map(|r|r.peak_puts_in_flight).max(),
        "prepared_payload_buffer_peak":st.results.iter().map(|r|r.prepared_buffer_peak).max(),
        "source_read_counts":st.results.iter().map(|r|r.device_read_count).collect::<Vec<_>>(),
        "worker_decomposition":worker,"batch_distributions":distributions,"control_error_count":errors,
        "artifact_verified":measured.iter().filter(|r|r.final_artifact_status=="Verified").count(),
        "contamination":contamination,"note":"Intervals overlap across PUTs; do not add them. Descriptive comparison only; no significance claim."})
}

fn write_plan(st: &State) {
    let cases: Vec<Value> = st
        .plan
        .cases
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .collect();
    let _ = std::fs::write(
        st.evidence_dir.join("matrix-plan.json"),
        serde_json::to_string_pretty(&json!({ "run_id": st.run_id, "cases": cases }))
            .unwrap_or_else(|_| "{}".into()),
    );
}

fn halt(st: &mut State, reason: String) -> String {
    if st.halt.is_none() {
        st.halt = Some(reason.clone());
    }
    let resp = json!({ "halt": st.halt.clone().unwrap_or(reason) }).to_string();
    maybe_go_terminal(st);
    resp
}

fn handle_request(state: &Arc<Mutex<State>>, raw: &str) -> String {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("bad request json: {e}") }).to_string(),
    };
    let op = v.get("op").and_then(|x| x.as_str()).unwrap_or("");
    let mut st = state.lock().unwrap();

    if op == "server_utc" {
        return json!({ "server_utc_ms": now_ms() }).to_string();
    }
    if st.done {
        return json!({ "halt": "stage4 already terminal" }).to_string();
    }

    match op {
        "next_case" => {
            if let Some(h) = st.halt.clone() {
                return json!({ "halt": h }).to_string();
            }
            if let Some((cid, pos)) = &st.current {
                if *pos != CasePos::Completed {
                    let cid = cid.clone();
                    return halt(&mut st, format!("next_case while {cid} still in flight"));
                }
            }
            if st.next_index >= st.plan.cases.len() {
                let completed = st.completed;
                st.record_event("matrix_completed", json!({ "completed": completed }));
                let resp = json!({ "matrix_completed": { "completed": completed } }).to_string();
                maybe_go_terminal(&mut st);
                return resp;
            }
            let case = st.plan.cases[st.next_index].clone();
            st.next_index += 1;
            st.current = Some((case.case_id.clone(), CasePos::HandedOut));
            st.record_event(
                "case_handed_out",
                json!({ "case_id": case.case_id, "mode": case.mode.wire() }),
            );
            json!({ "case": serde_json::to_value(&case).unwrap_or(Value::Null) }).to_string()
        }

        "case_ready" | "case_started" => {
            let case_id = v.get("case_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let want_from = if op == "case_ready" {
                CasePos::HandedOut
            } else {
                CasePos::Ready
            };
            let to = if op == "case_ready" {
                CasePos::Ready
            } else {
                CasePos::Started
            };
            match &st.current {
                Some((cid, pos)) if *cid == case_id && *pos == want_from => {
                    st.current = Some((case_id.clone(), to));
                    st.record_event(op, json!({ "case_id": case_id }));
                    json!({ "ack": true }).to_string()
                }
                _ => halt(&mut st, format!("{op} out of order for {case_id:?}")),
            }
        }

        "case_completed" => {
            let case_id = v.get("case_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let result: S4CaseResult = match v.get("result").cloned().map(serde_json::from_value) {
                Some(Ok(r)) => r,
                Some(Err(e)) => return json!({ "error": format!("bad S4CaseResult: {e}") }).to_string(),
                None => return json!({ "error": "case_completed needs result" }).to_string(),
            };
            match &st.current {
                Some((cid, CasePos::Started)) if *cid == case_id => {}
                _ => return halt(&mut st, format!("case_completed out of order for {case_id:?}")),
            }
            st.append("case-results.ndjson", &serde_json::to_string(&result).unwrap_or_default());
            let verified = result.is_completed_and_verified();
            st.record_event(
                "case_completed",
                json!({
                    "case_id": case_id, "mode": result.mode.wire(),
                    "case_status": result.case_status,
                    "artifact": result.final_artifact_status,
                    "bulk_stream_wall_ms": result.bulk_stream_wall_ms,
                    "verified_transfer_wall_ms": result.verified_transfer_wall_ms,
                    "prepared_buffer_peak": result.prepared_buffer_peak,
                }),
            );
            st.results.push(result);
            if !verified {
                return halt(&mut st, format!("{case_id} completed but not Verified"));
            }
            st.current = Some((case_id, CasePos::Completed));
            st.completed += 1;
            json!({ "ack": true }).to_string()
        }

        "case_failed" => {
            let case_id = v.get("case_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let reason = v.get("reason").and_then(|x| x.as_str()).unwrap_or("unspecified").to_string();
            let contaminated = v.get("contaminated").and_then(|x| x.as_bool()).unwrap_or(false);
            if let Some(Ok(r)) = v
                .get("result")
                .cloned()
                .map(serde_json::from_value::<S4CaseResult>)
            {
                st.append("case-results.ndjson", &serde_json::to_string(&r).unwrap_or_default());
                st.results.push(r);
            }
            st.record_event(
                "case_failed",
                json!({ "case_id": case_id, "reason": reason, "contaminated": contaminated }),
            );
            halt(
                &mut st,
                format!("case_failed {case_id}: {reason} (contaminated={contaminated})"),
            )
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

/// Entry point for `coordinator --stage4 --arm ...`.
pub fn run(cfg: Cfg) -> ! {
    std::fs::create_dir_all(&cfg.evidence_dir).unwrap_or_else(|e| {
        eprintln!("coordinator --stage4 --arm: cannot create evidence dir: {e}");
        std::process::exit(1);
    });

    let plan = if cfg.batch8 {
        S4Plan::build_batch8(&cfg.run_id)
    } else if cfg.window8 {
        S4Plan::build_window8(&cfg.run_id)
    } else {
        S4Plan::build(&cfg.run_id)
    }
    .unwrap_or_else(|e| {
        eprintln!("STAGE4_START_FAIL plan_arithmetic={e:?}");
        std::process::exit(1);
    });
    println!(
        "STAGE4_ARMED run_id={} cases={} plan={} matrix={} sink={}",
        cfg.run_id,
        plan.cases.len(),
        if cfg.batch8 { "prep_ahead_window_8_batch_8" } else if cfg.window8 { "window8_p_vs_w" } else { "stage4_s_vs_p" },
        cfg.matrix_addr,
        cfg.sink_addr
    );

    let matrix = TcpListener::bind(&cfg.matrix_addr).unwrap_or_else(|e| {
        eprintln!("coordinator --stage4 --arm: bind matrix {}: {e}", cfg.matrix_addr);
        std::process::exit(1);
    });
    let sink = TcpListener::bind(&cfg.sink_addr).unwrap_or_else(|e| {
        eprintln!("coordinator --stage4 --arm: bind sink {}: {e}", cfg.sink_addr);
        std::process::exit(1);
    });

    let state = Arc::new(Mutex::new(State {
        plan,
        next_index: 0,
        current: None,
        results: Vec::new(),
        completed: 0,
        halt: None,
        done: false,
        terminal_spawned: false,
        evidence_dir: cfg.evidence_dir.clone(),
        run_id: cfg.run_id.clone(),
        verdict_file: cfg.verdict_file.clone(),
        worker_timing_file: cfg.worker_timing_file.clone(),
        window8: cfg.window8,
        batch8: cfg.batch8,
        events_written: 0,
    }));
    {
        let g = state.lock().unwrap();
        write_plan(&g);
        for f in ["coordinator-events.ndjson", "case-results.ndjson", "probe-evidence.ndjson"] {
            let _ = std::fs::write(g.evidence_dir.join(f), b"");
        }
    }

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || run_sink(sink, state));
    }

    println!(
        "STAGE4_LISTENING matrix={} sink={} evidence_dir={}",
        cfg.matrix_addr,
        cfg.sink_addr,
        cfg.evidence_dir.display()
    );
    let _ = std::io::stdout().flush();

    for stream in matrix.incoming().flatten() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle_conn(stream, &state));
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_plan(dir: &std::path::Path, window8: bool) -> Arc<Mutex<State>> {
        let plan = if window8 {
            S4Plan::build_window8("i63w8-net-test").unwrap()
        } else {
            S4Plan::build("i63s4-net-test").unwrap()
        };
        Arc::new(Mutex::new(State {
            plan,
            next_index: 0,
            current: None,
            results: Vec::new(),
            completed: 0,
            halt: None,
            done: false,
            terminal_spawned: true, // block the real process::exit path in tests
            evidence_dir: dir.to_path_buf(),
            run_id: "i63s4-net-test".into(),
            verdict_file: None,
            worker_timing_file: None,
            window8,
            batch8: false,
            events_written: 0,
        }))
    }

    fn state_for(dir: &std::path::Path) -> Arc<Mutex<State>> {
        state_with_plan(dir, false)
    }

    fn ok_result_json(case: &Value) -> Value {
        let window = case["mode"] == "prep_ahead_window_8" || case["mode"] == "prep_ahead_window_8_batch_8";
        json!({
            "run_id": case["run_id"], "case_id": case["case_id"], "mode": case["mode"],
            "phase": case["phase"], "cycle": case["cycle"], "slot": case["slot"],
            "chunk_size_bytes": case["chunk_size_bytes"], "extent_bytes": case["extent_bytes"],
            "chunk_count": case["expected_chunk_count"],
            "transfer_id": format!("t-{}", case["case_id"].as_str().unwrap()),
            "artifact_id": "a",
            "bulk_stream_wall_ms": if window { 21000.0 } else { 52000.0 },
            "verified_transfer_wall_ms": if window { 27000.0 } else { 58000.0 },
            "resume_ms": 3.0, "seal_d2_ms": 5000.0, "read_ms": 5700.0, "chunk_sha_ms": 5400.0,
            "rolling_sha_ms": 6100.0, "proof_ms": 6.0, "put_ack_ms": 33000.0,
            "prepared_buffer_peak": if case["mode"] == "prep_ahead_2" { 2 } else if window { 9 } else { 0 },
            "device_read_count": 32,
            "put_window": if window { 8 } else { 0 },
            "put_started_count": if window { 32 } else { 0 },
            "put_completed_count": if window { 32 } else { 0 },
            "peak_puts_in_flight": if window { 8 } else { 0 },
            "put_starts_ascending": window,
            "final_artifact_status": "Verified", "case_status": "completed",
        })
    }

    #[test]
    fn batch8_analysis_requires_exact_durable_batch_coverage() {
        let dir = std::env::temp_dir().join(format!("i63b8-analysis-{}",std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = state_for(&dir);
        let mut st = state.lock().unwrap();
        st.batch8 = true;
        st.plan = S4Plan::build_batch8("b8test").unwrap();
        st.results = st.plan.cases.iter().map(|c| serde_json::from_value(ok_result_json(&serde_json::to_value(c).unwrap())).unwrap()).collect();
        let mut batches = Vec::new();
        for r in &st.results { for n in 0..4 { batches.push(json!({"transfer_id":r.transfer_id,"batch_number":n,"first_chunk_index":n*8,"last_chunk_index":n*8+7,"member_count":8,"outcome":"durable","file_sync_sum_ns":1000,"dir_fsync_ns":1000,"batch_finalize_total_ns":2000})); } }
        let write = |b: &Vec<Value>| std::fs::write(dir.join("worker-batch-timing.ndjson"),b.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")).unwrap();
        write(&batches);
        let good = batch8_analysis(&st,&[],json!({}),"stage4_pass");
        assert_eq!(good["classification"], "A — TARGET MET");
        assert_eq!(good["case_outcomes"].as_array().unwrap().len(),4);
        assert_eq!(good["measured_bulk_walls_ms"].as_array().unwrap().len(),3);
        batches[1] = batches[0].clone();
        write(&batches);
        assert_eq!(batch8_analysis(&st,&[],json!({}),"stage4_pass")["classification"],"D — NO USEFUL SOLUTION");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn full_10_case_walkthrough_reaches_matrix_completed() {
        let dir = std::env::temp_dir().join(format!("i63s4net-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = state_for(&dir);
        let mut handled = 0;
        loop {
            let r = handle_request(&st, r#"{"op":"next_case"}"#);
            let v: Value = serde_json::from_str(&r).unwrap();
            if let Some(mc) = v.get("matrix_completed") {
                assert_eq!(mc["completed"].as_u64().unwrap(), 10);
                break;
            }
            let case = v.get("case").expect("a case").clone();
            let cid = case["case_id"].as_str().unwrap();
            for op in ["case_ready", "case_started"] {
                let resp = handle_request(&st, &json!({ "op": op, "case_id": cid }).to_string());
                assert_eq!(serde_json::from_str::<Value>(&resp).unwrap()["ack"], json!(true), "{op}");
            }
            let done = handle_request(
                &st,
                &json!({ "op": "case_completed", "case_id": cid, "result": ok_result_json(&case) }).to_string(),
            );
            assert_eq!(serde_json::from_str::<Value>(&done).unwrap()["ack"], json!(true));
            handled += 1;
        }
        assert_eq!(handled, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window8_5_case_walkthrough_reaches_matrix_completed_with_pw_wp_order() {
        let dir = std::env::temp_dir().join(format!("i63w8net-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = state_with_plan(&dir, true);
        let mut modes: Vec<String> = Vec::new();
        loop {
            let r = handle_request(&st, r#"{"op":"next_case"}"#);
            let v: Value = serde_json::from_str(&r).unwrap();
            if let Some(mc) = v.get("matrix_completed") {
                assert_eq!(mc["completed"].as_u64().unwrap(), 5);
                break;
            }
            let case = v.get("case").expect("a case").clone();
            modes.push(case["mode"].as_str().unwrap().to_string());
            let cid = case["case_id"].as_str().unwrap();
            for op in ["case_ready", "case_started"] {
                let resp = handle_request(&st, &json!({ "op": op, "case_id": cid }).to_string());
                assert_eq!(serde_json::from_str::<Value>(&resp).unwrap()["ack"], json!(true));
            }
            let done = handle_request(
                &st,
                &json!({ "op": "case_completed", "case_id": cid, "result": ok_result_json(&case) }).to_string(),
            );
            assert_eq!(serde_json::from_str::<Value>(&done).unwrap()["ack"], json!(true));
        }
        assert_eq!(
            modes,
            vec![
                "prep_ahead_window_8", // warm-up
                "prep_ahead_2",
                "prep_ahead_window_8",
                "prep_ahead_window_8",
                "prep_ahead_2",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_verified_completion_halts_and_hands_out_no_more() {
        let dir = std::env::temp_dir().join(format!("i63s4net-nv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = state_for(&dir);
        let r = handle_request(&st, r#"{"op":"next_case"}"#);
        let case = serde_json::from_str::<Value>(&r).unwrap()["case"].clone();
        let cid = case["case_id"].as_str().unwrap().to_string();
        handle_request(&st, &json!({ "op": "case_ready", "case_id": cid }).to_string());
        handle_request(&st, &json!({ "op": "case_started", "case_id": cid }).to_string());
        let mut bad = ok_result_json(&case);
        bad["final_artifact_status"] = json!("Failed");
        bad["case_status"] = json!("failed:artifact_verification");
        let done = handle_request(
            &st,
            &json!({ "op": "case_completed", "case_id": cid, "result": bad }).to_string(),
        );
        assert!(serde_json::from_str::<Value>(&done).unwrap().get("halt").is_some());
        let next = handle_request(&st, r#"{"op":"next_case"}"#);
        assert!(serde_json::from_str::<Value>(&next).unwrap().get("halt").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_order_case_ready_halts() {
        let dir = std::env::temp_dir().join(format!("i63s4net-oo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = state_for(&dir);
        handle_request(&st, r#"{"op":"next_case"}"#);
        let resp = handle_request(&st, r#"{"op":"case_started","case_id":"nope"}"#);
        assert!(serde_json::from_str::<Value>(&resp).unwrap().get("halt").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_utc_answered_even_after_terminal() {
        let dir = std::env::temp_dir().join(format!("i63s4net-utc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = state_for(&dir);
        st.lock().unwrap().done = true;
        let r = handle_request(&st, r#"{"op":"server_utc"}"#);
        assert!(serde_json::from_str::<Value>(&r).unwrap()["server_utc_ms"].as_i64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn _mode_token_sanity() {
        assert_eq!(S4Mode::Serial.wire(), "serial");
    }
}
