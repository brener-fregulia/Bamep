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

fn main() {
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
