//! Issue #63 Stage 1 lab coordinator (Fedora side). THROWAWAY Spike plumbing.
//!
//! Two plain-TCP listeners on the isolated #53 lab link:
//!   * coord (default 192.168.99.1:9206) — one line in, one JSON line out
//!     carrying the Server's current UTC in epoch milliseconds. This is the
//!     lab-only reference the WinPE runner aligns its system clock to; it is
//!     NOT production time synchronisation and issues no proof / capability.
//!   * sink (default 192.168.99.1:9299) — the runner connects and writes its
//!     cumulative NDJSON event snapshot; the coordinator de-dups by `seq`,
//!     appends new lines to the evidence file, and feeds the pure Stage-1
//!     state machine.
//!
//! It prints deterministic machine-readable lines (`STAGE1_EVENT ...`,
//! `STAGE1_PHYSICAL_PASS`, `STAGE1_PHYSICAL_FAIL ...`) so the launcher and the
//! owner never have to grep human prose. NO TLS, NO auth, NO database, NO
//! Worker, NO transfer. Not the Bamep control plane.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bamep_i63_stage1_coordinator::state::{self, Mode, Observed, Stage1Machine, Verdict};

/// After a terminal verdict the coordinator keeps accepting sink connections for
/// this long so the runner's trailing flush (e.g. `winpe.runner_end`) still
/// lands in the evidence file, then writes the verdict marker and exits.
const DRAIN_SECS: u64 = 5;

struct Cfg {
    coord_addr: String,
    sink_addr: String,
    evidence: String,
    run_id: String,
    mode: Mode,
    /// Optional path: on a terminal verdict the coordinator writes a short
    /// marker here (`physical_pass` / `physical_fail` / `host_smoke_pass` /
    /// `host_smoke_fail`) so the launcher can tell an expected terminal exit
    /// from a crash. Absent ⇒ no file written (standalone / test use).
    verdict_file: Option<String>,
}

fn parse() -> Cfg {
    let mut coord_addr = "192.168.99.1:9206".to_string();
    let mut sink_addr = "192.168.99.1:9299".to_string();
    let mut evidence = "stage1-events.ndjson".to_string();
    let mut run_id = "stage1".to_string();
    let mut verdict_file: Option<String> = None;
    // `--mode` is MANDATORY and never inferred: a host/simulated run must be
    // unable to reach STAGE1_PHYSICAL_PASS.
    let mut mode: Option<Mode> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--verdict-file" => verdict_file = it.next(),
            "--coord-addr" => coord_addr = it.next().unwrap_or(coord_addr),
            "--sink-addr" => sink_addr = it.next().unwrap_or(sink_addr),
            "--evidence" => evidence = it.next().unwrap_or(evidence),
            "--run-id" => run_id = it.next().unwrap_or(run_id),
            "--mode" => {
                let raw = it.next().unwrap_or_default();
                mode = Some(Mode::parse(&raw).unwrap_or_else(|| {
                    eprintln!("coordinator: --mode must be 'physical' or 'host-smoke' (got {raw:?})");
                    std::process::exit(2);
                }));
            }
            "-h" | "--help" => {
                println!(
                    "bamep-i63-stage1-coordinator --mode <physical|host-smoke> \
                     [--coord-addr IP:PORT] [--sink-addr IP:PORT] \
                     [--evidence FILE] [--run-id ID] [--verdict-file FILE]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("coordinator: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    let mode = mode.unwrap_or_else(|| {
        eprintln!("coordinator: --mode <physical|host-smoke> is MANDATORY (never inferred)");
        std::process::exit(2);
    });
    Cfg { coord_addr, sink_addr, evidence, run_id, mode, verdict_file }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct Shared {
    machine: Stage1Machine,
    seen_seq: BTreeSet<u64>,
    evidence: String,
    run_id: String,
    mode: Mode,
    verdict_file: Option<String>,
    last_progress: usize,
    done: bool,
    /// Set once, when the drain-then-exit timer thread has been spawned.
    terminal_spawned: bool,
    utc_requests: u64,
}

/// Stage-2 sub-commands, intercepted BEFORE `parse()` so Stage-1 invocation
/// (`--mode <physical|host-smoke> ...`) behaves EXACTLY as before.
fn stage2_subcommand() -> Option<i32> {
    match std::env::args().nth(1).as_deref() {
        Some("--matrix-selftest") => Some(matrix_selftest()),
        Some("--stage4-selftest") => Some(stage4_selftest()),
        Some("--window8-selftest") => Some(window8_selftest()),
        Some("--batch8-selftest") => Some(batch8_selftest()),
        Some("--stage4") => {
            // Issue #63 Stage 4: `--stage4 --arm ...` runs the ARMED networked
            // 64 MiB serial-vs-prep-ahead micro-matrix authority (the Stage-4
            // lab supervisor passes `--arm` only after host-side preflight).
            if std::env::args().any(|a| a == "--arm") {
                let cfg = bamep_i63_stage1_coordinator::stage4_net::parse_cfg();
                bamep_i63_stage1_coordinator::stage4_net::run(cfg); // -> !
            }
            println!("STAGE4 NOT ARMED");
            println!(
                "coordinator: the Stage-4 micro-matrix (2 warm-up + 8 measured S/P transfers, \
                 64 MiB only) is armed ONLY behind `--stage4 --arm` and started ONLY by \
                 run-stage4-lab.sh after host-side preflight."
            );
            Some(0)
        }
        Some("--matrix") => {
            // Stage 3: `--matrix --arm ...` runs the ARMED networked matrix
            // authority (the Stage-3 lab supervisor passes `--arm` only after
            // its own host-side preflight). Without `--arm` this is inert.
            if std::env::args().any(|a| a == "--arm") {
                let cfg = bamep_i63_stage1_coordinator::matrix_net::parse_cfg();
                bamep_i63_stage1_coordinator::matrix_net::run(cfg); // -> !
            }
            println!("PHYSICAL MATRIX NOT ARMED");
            println!(
                "coordinator: the Stage-2 typed matrix coordinator (src/matrix.rs) is built and \
                 unit-tested. The Stage-3 ARMED networked wiring lives behind `--matrix --arm` and \
                 is started ONLY by run-stage3-lab.sh after host-side preflight. There is \
                 deliberately no casual command that starts the 36 physical transfers."
            );
            println!("Run `--matrix-selftest` for the deterministic in-memory sequencing check.");
            Some(0)
        }
        _ => None,
    }
}

/// In-memory: walk the full 36-case plan through the typed lab operations with
/// stub results, proving deterministic ordering + stop-on-failure + aggregation.
/// NO socket, NO transfer, NO device.
fn matrix_selftest() -> i32 {
    use bamep_i63_stage1_coordinator::matrix::{LabRequest, LabResponse, MatrixCoordinator};
    use bamep_i63_stage2_engine::result::{CaseResult, ConnectionCount};

    let run_id = "i63s2-matrix-selftest";
    let mut c = match MatrixCoordinator::new(run_id, 120 * bamep_i63_stage2_engine::GIB) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("matrix-selftest: coordinator did not start: {e:?}");
            return 1;
        }
    };
    println!("MATRIX_SELFTEST budget={:?}", c.budget());

    let mut handled = 0usize;
    loop {
        match c.handle(LabRequest::NextCase) {
            LabResponse::Case(case) => {
                let (b_mib, b_mb) = CaseResult::rates(case.extent_bytes, 60_000.0);
                let (v_mib, v_mb) = CaseResult::rates(case.extent_bytes, 66_000.0);
                let result = CaseResult {
                    run_id: case.run_id.clone(),
                    case_id: case.case_id.clone(),
                    phase: case.phase,
                    cycle: case.cycle,
                    slot: case.slot,
                    chunk_size_bytes: case.chunk_size_bytes,
                    extent_bytes: case.extent_bytes,
                    chunk_count: case.expected_chunk_count,
                    transfer_id: Some(format!("stub-t-{handled}")),
                    artifact_id: Some(format!("stub-a-{handled}")),
                    source_safety_verdict: "stub".into(),
                    clock_skew_verdict: "stub".into(),
                    bulk_stream_wall_ms: 60_000.0,
                    bulk_stream_mib_s: b_mib,
                    bulk_stream_mb_s: b_mb,
                    verified_transfer_wall_ms: 66_000.0,
                    verified_transfer_mib_s: v_mib,
                    verified_transfer_mb_s: v_mb,
                    resume_ms: 3.0,
                    seal_d2_ms: 5_000.0,
                    read_ms: 0.0,
                    chunk_sha_ms: 0.0,
                    rolling_sha_ms: 0.0,
                    proof_ms: 0.0,
                    put_ack_ms: 0.0,
                    connection_count: ConnectionCount::by_construction(case.expected_chunk_count),
                    final_artifact_status: "Verified".into(),
                    case_status: "completed".into(),
                };
                let cid = case.case_id.clone();
                c.handle(LabRequest::CaseReady { case_id: cid.clone() });
                c.handle(LabRequest::CaseStarted { case_id: cid.clone() });
                match c.handle(LabRequest::CaseCompleted {
                    case_id: cid.clone(),
                    result: Box::new(result),
                }) {
                    LabResponse::Ack => handled += 1,
                    other => {
                        eprintln!("matrix-selftest: case {cid} not acked: {other:?}");
                        return 1;
                    }
                }
            }
            LabResponse::MatrixCompleted { completed, analysis } => {
                println!(
                    "MATRIX_SELFTEST_COMPLETE handled={handled} completed={completed} \
                     measured_sizes={} paired_ratios={} excluded_unverified={}",
                    analysis.sizes.len(),
                    analysis.paired.len(),
                    analysis.excluded_unverified.len()
                );
                if completed == 36 && c.matrix_succeeded() {
                    println!("MATRIX_SELFTEST_PASS");
                    return 0;
                }
                eprintln!("matrix-selftest: expected 36 completed, got {completed}");
                return 1;
            }
            other => {
                eprintln!("matrix-selftest: unexpected {other:?}");
                return 1;
            }
        }
    }
}

/// In-memory: build the deterministic Issue #63 Stage-4 10-case plan, run
/// `analyse_s4` over stub Verified results, and assert the S-vs-P shape. NO
/// socket, NO transfer, NO device.
fn stage4_selftest() -> i32 {
    use bamep_i63_stage2_engine::stage4::{analyse_s4, S4CaseResult, S4Mode, S4Plan};
    let plan = match S4Plan::build("i63s4-selftest") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("stage4-selftest: plan did not build: {e:?}");
            return 1;
        }
    };
    if plan.cases.len() != 10 || plan.warmups().count() != 2 || plan.measured().count() != 8 {
        eprintln!("stage4-selftest: plan shape wrong");
        return 1;
    }
    let results: Vec<S4CaseResult> = plan
        .cases
        .iter()
        .map(|c| {
            // prep-ahead deterministically faster in the stub.
            let (b, v) = if c.mode == S4Mode::PrepAhead2 {
                (40_000.0, 46_000.0)
            } else {
                (52_000.0, 58_000.0)
            };
            S4CaseResult {
                run_id: c.run_id.clone(),
                case_id: c.case_id.clone(),
                mode: c.mode,
                phase: c.phase,
                cycle: c.cycle,
                slot: c.slot,
                chunk_size_bytes: c.chunk_size_bytes,
                extent_bytes: c.extent_bytes,
                chunk_count: c.expected_chunk_count,
                transfer_id: Some(format!("stub-t-{}", c.case_id)),
                artifact_id: Some("stub-a".into()),
                bulk_stream_wall_ms: b,
                verified_transfer_wall_ms: v,
                resume_ms: 3.0,
                seal_d2_ms: 5_000.0,
                read_ms: 5_700.0,
                chunk_sha_ms: 5_400.0,
                rolling_sha_ms: 6_100.0,
                proof_ms: 6.0,
                put_ack_ms: 33_000.0,
                prepared_buffer_peak: if c.mode == S4Mode::PrepAhead2 { 2 } else { 0 },
                device_read_count: 32,
                put_window: 0,
                put_started_count: 0,
                put_completed_count: 0,
                peak_puts_in_flight: 0,
                put_starts_ascending: false,
                final_artifact_status: "Verified".into(),
                case_status: "completed".into(),
            }
        })
        .collect();
    let a = analyse_s4(&results);
    let (Some(s), Some(p)) = (a.serial.as_ref(), a.prep_ahead.as_ref()) else {
        eprintln!("stage4-selftest: missing a mode summary");
        return 1;
    };
    println!(
        "STAGE4_SELFTEST plan=10 warmups=2 measured=8 extent={} chunk=64MiB chunks=32 \
         serial_n={} prep_ahead_n={} paired_series={} excluded={}",
        plan.cases[0].extent_bytes,
        s.n,
        p.n,
        a.paired.len(),
        a.excluded_unverified.len()
    );
    if s.n == 4 && p.n == 4 && a.paired.len() == 2 && a.excluded_unverified.is_empty() {
        println!("STAGE4_SELFTEST_PASS");
        0
    } else {
        eprintln!("stage4-selftest: expected serial_n=4 prep_ahead_n=4 paired=2 excluded=0");
        1
    }
}

/// In-memory: build the deterministic Issue #63 window_8 5-case plan, run
/// `analyse_w8` over stub Verified results, and assert the P-vs-W shape +
/// window invariants. NO socket, NO transfer, NO device.
fn window8_selftest() -> i32 {
    use bamep_i63_stage2_engine::stage4::{
        analyse_w8, S4CaseResult, S4Mode, S4Plan, W8_PUT_WINDOW, W8_TOTAL_CASES,
    };
    let plan = match S4Plan::build_window8("i63w8-selftest") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("window8-selftest: plan did not build: {e:?}");
            return 1;
        }
    };
    if plan.cases.len() != W8_TOTAL_CASES || plan.warmups().count() != 1 || plan.measured().count() != 4 {
        eprintln!("window8-selftest: plan shape wrong");
        return 1;
    }
    let results: Vec<S4CaseResult> = plan
        .cases
        .iter()
        .map(|c| {
            let window = c.mode == S4Mode::PrepAheadWindow8;
            // window_8 deterministically faster in the stub.
            let (b, v) = if window { (21_000.0, 27_000.0) } else { (66_000.0, 72_000.0) };
            S4CaseResult {
                run_id: c.run_id.clone(),
                case_id: c.case_id.clone(),
                mode: c.mode,
                phase: c.phase,
                cycle: c.cycle,
                slot: c.slot,
                chunk_size_bytes: c.chunk_size_bytes,
                extent_bytes: c.extent_bytes,
                chunk_count: c.expected_chunk_count,
                transfer_id: Some(format!("stub-t-{}", c.case_id)),
                artifact_id: Some("stub-a".into()),
                bulk_stream_wall_ms: b,
                verified_transfer_wall_ms: v,
                resume_ms: 3.0,
                seal_d2_ms: 5_000.0,
                read_ms: 5_700.0,
                chunk_sha_ms: 5_400.0,
                rolling_sha_ms: 6_100.0,
                proof_ms: 6.0,
                put_ack_ms: if window { 90_000.0 } else { 33_000.0 },
                prepared_buffer_peak: if window { 9 } else { 2 },
                device_read_count: 32,
                put_window: if window { W8_PUT_WINDOW } else { 0 },
                put_started_count: if window { 32 } else { 0 },
                put_completed_count: if window { 32 } else { 0 },
                peak_puts_in_flight: if window { 8 } else { 0 },
                put_starts_ascending: window,
                final_artifact_status: "Verified".into(),
                case_status: "completed".into(),
            }
        })
        .collect();
    let a = analyse_w8(&results);
    let (Some(p), Some(w)) = (a.prep_ahead.as_ref(), a.window8.as_ref()) else {
        eprintln!("window8-selftest: missing a mode summary");
        return 1;
    };
    println!(
        "WINDOW8_SELFTEST plan=5 warmups=1 measured=4 chunk=64MiB chunks=32 put_window={} \
         prep_ahead_n={} window8_n={} paired_series={} invariants_held={} excluded={}",
        a.put_window,
        p.n,
        w.n,
        a.paired.len(),
        a.window_invariants_held,
        a.excluded_unverified.len()
    );
    if p.n == 2
        && w.n == 2
        && a.paired.len() == 2
        && a.window_invariants_held
        && a.excluded_unverified.is_empty()
        && a.window8_median_bulk_mb_s > 100.0
    {
        println!("WINDOW8_SELFTEST_PASS");
        0
    } else {
        eprintln!("window8-selftest: expected prep_n=2 window_n=2 paired=2 invariants=held excluded=0");
        1
    }
}

fn main() {
    if let Some(code) = stage2_subcommand() {
        std::process::exit(code);
    }
    let cfg = parse();

    let coord = TcpListener::bind(&cfg.coord_addr).unwrap_or_else(|e| {
        eprintln!("coordinator: bind coord {}: {e}", cfg.coord_addr);
        std::process::exit(1);
    });
    let sink = TcpListener::bind(&cfg.sink_addr).unwrap_or_else(|e| {
        eprintln!("coordinator: bind sink {}: {e}", cfg.sink_addr);
        std::process::exit(1);
    });

    // Truncate/create the evidence file up front so its presence is a reliable
    // readiness signal for the launcher.
    if let Err(e) = std::fs::write(&cfg.evidence, b"") {
        eprintln!("coordinator: cannot create evidence file {}: {e}", cfg.evidence);
        std::process::exit(1);
    }

    let shared = Arc::new(Mutex::new(Shared {
        machine: Stage1Machine::new(cfg.mode),
        seen_seq: BTreeSet::new(),
        evidence: cfg.evidence.clone(),
        run_id: cfg.run_id.clone(),
        mode: cfg.mode,
        verdict_file: cfg.verdict_file.clone(),
        last_progress: 0,
        done: false,
        terminal_spawned: false,
        utc_requests: 0,
    }));

    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            for stream in coord.incoming().flatten() {
                let _ = handle_coord(stream, &shared);
            }
        });
    }

    println!(
        "STAGE1_COORDINATOR_LISTENING mode={} coord={} sink={} evidence={} run_id={}",
        cfg.mode.as_str(), cfg.coord_addr, cfg.sink_addr, cfg.evidence, cfg.run_id
    );
    println!("STAGE1_EXPECTED_CHAIN {}", state::EXPECTED.join(" -> "));
    let _ = std::io::stdout().flush();

    // The sink loop NEVER breaks on its own. When `handle_sink` reaches a
    // terminal verdict it spawns a one-shot drain-then-exit thread (below): the
    // listener keeps accepting for `DRAIN_SECS` so the runner's trailing flush
    // still lands, then the process writes its verdict marker and exits. This
    // is the ONLY expected exit path.
    for stream in sink.incoming().flatten() {
        handle_sink(stream, &shared);
    }
}

/// Spawned exactly once, from the terminal-verdict arm of `handle_sink`.
/// Keeps the process alive for `DRAIN_SECS` (trailing-flush drain), then writes
/// the verdict marker file and exits. `exit(0)` for a pass verdict, `exit(10)`
/// for a fail verdict — but the launcher keys off the FILE, not the code.
fn spawn_drain_then_exit(marker: &'static str, verdict_file: Option<String>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(DRAIN_SECS));
        if let Some(path) = &verdict_file {
            let _ = std::fs::write(path, marker);
        }
        println!("STAGE1_COORDINATOR_TERMINAL marker={marker} drained_secs={DRAIN_SECS}");
        let _ = std::io::stdout().flush();
        std::process::exit(if marker.ends_with("pass") { 0 } else { 10 });
    });
}

fn handle_coord(mut s: TcpStream, shared: &Arc<Mutex<Shared>>) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut reader = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let utc = now_ms();
    {
        let mut g = shared.lock().unwrap();
        g.utc_requests += 1;
        println!(
            "STAGE1_UTC_SERVED n={} peer={} server_utc_ms={}",
            g.utc_requests, peer, utc
        );
        let _ = std::io::stdout().flush();
    }
    let ack = format!("{{\"stage1_coord_ack\":true,\"server_utc_ms\":{utc}}}\n");
    s.write_all(ack.as_bytes())?;
    s.flush()?;
    Ok(())
}

fn handle_sink(mut s: TcpStream, shared: &Arc<Mutex<Shared>>) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(15)));
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    let _ = s.write_all(b"ok\n");

    if buf.trim().is_empty() {
        return;
    }

    let mut g = shared.lock().unwrap();
    let mut fresh = 0u64;
    for raw in buf.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        if let Some(seq) = v.get("seq").and_then(|x| x.as_u64()) {
            if !g.seen_seq.insert(seq) {
                continue;
            }
        }
        fresh += 1;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&g.evidence)
        {
            let _ = writeln!(f, "{raw}");
        }
        let Some(event) = v.get("event").and_then(|x| x.as_str()) else {
            continue;
        };
        if g.done {
            continue;
        }
        let obs = Observed {
            name: event.to_string(),
            mode: v.get("mode").and_then(|x| x.as_str()).map(String::from),
            origin: v.get("origin").and_then(|x| x.as_str()).map(String::from),
            clock_backend: v.get("clock_backend").and_then(|x| x.as_str()).map(String::from),
        };
        let mode = g.mode;
        // PASS token: STAGE1_PHYSICAL_PASS is ONLY ever printed from the
        // Verdict::PhysicalPass arm, which the state machine can only return
        // when the coordinator was started `--mode physical` AND every
        // evidence-integrity check held.
        let phys_tag = if mode == Mode::Physical { "PHYSICAL" } else { "HOST_SMOKE" };
        let verdict = g.machine.observe(&obs);
        match &verdict {
            Verdict::Pending { progress } => {
                if *progress != g.last_progress {
                    g.last_progress = *progress;
                    println!(
                        "STAGE1_EVENT {event} (progress {progress}/{})",
                        state::EXPECTED.len()
                    );
                }
            }
            Verdict::PhysicalPass => {
                g.done = true;
                let run_id = g.run_id.clone();
                let observed = g.machine.observed();
                println!("STAGE1_PHYSICAL_PASS run_id={run_id}");
                for e in observed {
                    println!("  [x] {e}");
                }
                println!(
                    "STAGE1_PASS_NOTE physical MiniPC: derived-boot + WinPE auto-start \
                     + automatic UTC clock alignment proven; NO source-content handle \
                     opened, NO transfer, NO artifact."
                );
            }
            Verdict::HostSmokePass => {
                g.done = true;
                let run_id = g.run_id.clone();
                println!("STAGE1_HOST_SMOKE_PASS run_id={run_id}");
                println!(
                    "STAGE1_HOST_SMOKE_NOTE host/simulated coordinator+runner wiring \
                     exercised; this is NOT physical evidence and is structurally \
                     incapable of a physical pass verdict."
                );
            }
            Verdict::Fail { reason } => {
                g.done = true;
                let run_id = g.run_id.clone();
                let missing = g.machine.missing();
                let violations = g.machine.integrity_violations().join(" | ");
                println!("STAGE1_{phys_tag}_FAIL run_id={run_id} reason={reason}");
                println!("STAGE1_FAIL_LINE {raw}");
                println!("STAGE1_FAIL_MISSING {}", missing.join(","));
                if !violations.is_empty() {
                    println!("STAGE1_FAIL_INTEGRITY {violations}");
                }
            }
        }
        // On the FIRST terminal verdict, arm the drain-then-exit timer. The sink
        // loop keeps accepting during the drain so a trailing runner flush still
        // lands; then the coordinator writes its verdict marker and exits — the
        // one expected exit path.
        if verdict.is_terminal() && !g.terminal_spawned {
            g.terminal_spawned = true;
            let marker = state::verdict_marker(&verdict, mode).expect("terminal verdict has a marker");
            let vf = g.verdict_file.clone();
            spawn_drain_then_exit(marker, vf);
        }
    }
    if fresh > 0 {
        println!("STAGE1_SINK_INGEST peer={peer} fresh_lines={fresh}");
    }
    let _ = std::io::stdout().flush();
}

fn batch8_selftest() -> i32 {
    use bamep_i63_stage2_engine::stage4::S4Plan;
    let p = S4Plan::build_batch8("i63b8-selftest").expect("fixed geometry");
    if p.cases.len() != 4 || p.warmups().count() != 1 || p.measured().count() != 3
        || p.cases.iter().enumerate().any(|(i,c)| c.case_id != format!("i63b8-selftest/B{i}")
            || c.mode.wire() != "prep_ahead_window_8_batch_8" || c.expected_chunk_count != 32) {
        return 1;
    }
    println!("BATCH8_SELFTEST_PASS cases=4 warmup=1 measured=3 chunk_mib=64 extent_mib=2048 put_window=8");
    0
}
