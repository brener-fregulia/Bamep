//! Issue #61 CP4 + CP5 — WinPE-native Agent-local source-authority pressure
//! (CP4) and read-only physical byte access (CP5).
//!
//! THROWAWAY Spike artifact. NOT the Bamep Agent, NOT `crates/agent`, NOT a
//! production Agent architecture. Two independent logical checkpoints, one
//! process, one WinPE session:
//!
//!   CP4 — build THIS boot's source-observation epoch; resolve the current
//!         tuple to exactly the operator-selected SSD; reject stale/unknown
//!         SourceReferences BEFORE any GENERIC_READ device handle is opened;
//!         no fallback. Prints CP4_EXITCODE.
//!   CP5 — only if CP4 PASSED: open the resolved source with GENERIC_READ
//!         (never GENERIC_WRITE), obtain the exact device length via read-only
//!         IOCTLs, perform small bounded reproducible raw reads. Prints
//!         CP5_EXITCODE.
//!
//! CP4/CP5 are probe-local Spike evidence. They do NOT prove that production
//! `bamep.m2.endpoint-capture-transfer` or a product `SOURCE_REFERENCE_STALE`
//! exists — no product component resolves an authoritative `SourceReference`.
//!
//! No authenticated WSS session is used: CP4/CP5 are pure local operations.
//! Evidence goes to stderr + a local NDJSON file + one TCP line to the lab
//! sink, so the operator only needs to report the two final status lines.

mod resolver;
mod sources;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use resolver::{CurrentEpoch, EpochEntry, ResolveError};
use sources::Counters;

const PROBE_NAME: &str = env!("CARGO_PKG_NAME");
const PROBE_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_SINK: &str = "192.168.99.1:9099";
const NET_TIMEOUT: Duration = Duration::from_secs(5);

// CP3 prior-epoch tuples (owner-supplied). Both are now stale relative to a
// fresh enumeration: a #60/#61 probe invocation mints a NEW epoch and never
// persists an old one (RF-3: no cross-process/cross-boot physical identity).
const CP3_CURRENT_OBS: &str = "fzSWUDJdAIdbvKkHa5UzXWp8ssDdr-blMbFHcUzUEVM";
const CP3_CURRENT_ASID: &str = "7bGA10ahvEZcXtM5W7O0CtXc";
const CP3_STALE_OBS: &str = "L9mQpz0PIeoDXIsBibiDXmtSsvecsG1qdBG1GRoMa20";
const CP3_STALE_ASID: &str = "z9CY-nubpHT9tTnNjAPCbwPj";

enum V {
    S(String),
    I(i64),
    U(u64),
    B(bool),
}
fn s(v: impl Into<String>) -> V {
    V::S(v.into())
}

fn esc(input: &str) -> String {
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

fn now_ms() -> u128 {
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

    fn emit(&self, level: &str, event: &str, fields: &[(&str, V)]) {
        let seq = {
            let mut g = self.seq.lock().unwrap();
            *g += 1;
            *g
        };
        let mut line = String::new();
        line.push('{');
        line.push_str(&format!(r#""ts_ms":{}"#, now_ms()));
        line.push_str(&format!(r#","seq":{seq}"#));
        line.push_str(&format!(r#","elapsed_ms":{}"#, self.started.elapsed().as_millis()));
        line.push_str(&format!(r#","level":"{}""#, esc(level)));
        line.push_str(&format!(r#","event":"{}""#, esc(event)));
        line.push_str(&format!(r#","probe":"{}""#, esc(PROBE_NAME)));
        for (k, v) in fields {
            match v {
                V::S(x) => line.push_str(&format!(r#","{}":"{}""#, esc(k), esc(x))),
                V::I(x) => line.push_str(&format!(r#","{}":{}"#, esc(k), x)),
                V::U(x) => line.push_str(&format!(r#","{}":{}"#, esc(k), x)),
                V::B(x) => line.push_str(&format!(r#","{}":{}"#, esc(k), x)),
            }
        }
        line.push('}');
        eprintln!("{line}");
        let _ = std::io::stderr().flush();
        self.buffer.lock().unwrap().push(line);
    }

    fn snapshot(&self) -> String {
        self.buffer.lock().unwrap().join("\n")
    }
}

fn write_local_file(log: &Log) -> bool {
    let dir = std::env::var("TEMP")
        .or_else(|_| std::env::var("TMP"))
        .unwrap_or_else(|_| ".".into());
    let path = format!("{dir}\\bamep-issue61-cp45-probe.ndjson");
    let alt = "bamep-issue61-cp45-probe.ndjson";
    let body = log.snapshot();
    for p in [path.as_str(), alt] {
        if std::fs::write(p, format!("{body}\n")).is_ok() {
            log.emit("info", "sink.file.ok", &[("path", s(p))]);
            return true;
        }
    }
    log.emit("warn", "sink.file.failed", &[]);
    false
}

fn flush_to_sink(log: &Log, sink: &str) {
    let addr: Option<SocketAddr> = sink.to_socket_addrs().ok().and_then(|mut a| a.next());
    let Some(addr) = addr else {
        log.emit("warn", "sink.net.bad_addr", &[("sink", s(sink))]);
        return;
    };
    match TcpStream::connect_timeout(&addr, NET_TIMEOUT) {
        Ok(mut stream) => {
            let _ = stream.set_write_timeout(Some(NET_TIMEOUT));
            let body = format!("{}\n", log.snapshot());
            if stream.write_all(body.as_bytes()).is_ok() {
                let _ = stream.flush();
                let mut sink_buf = [0u8; 64];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.read(&mut sink_buf);
                log.emit("info", "sink.net.ok", &[("bytes", V::U(body.len() as u64))]);
            } else {
                log.emit("warn", "sink.net.write_failed", &[]);
            }
        }
        Err(e) => log.emit("warn", "sink.net.connect_failed", &[("error", s(e.to_string()))]),
    }
}

struct Args {
    sink: String,
    select_model_substr: String,
    diag_range_bytes: u64,
    run_cp5: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        sink: DEFAULT_SINK.to_string(),
        select_model_substr: "256GB".to_string(),
        diag_range_bytes: 4 * 1024 * 1024,
        run_cp5: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sink" => a.sink = it.next().unwrap_or(a.sink),
            "--select-model-substr" => {
                a.select_model_substr = it.next().unwrap_or(a.select_model_substr)
            }
            "--diag-range-bytes" => {
                a.diag_range_bytes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(a.diag_range_bytes)
            }
            "--no-cp5" => a.run_cp5 = false,
            _ => {}
        }
    }
    a
}

fn resolve_err_str(e: &ResolveError) -> (&'static str, String) {
    match e {
        ResolveError::AmbiguousEpoch { duplicate_agent_source_ids } => (
            "AmbiguousEpoch",
            format!("duplicate agent_source_id(s): {duplicate_agent_source_ids:?}"),
        ),
        ResolveError::StaleObservationEpoch { presented, current } => (
            "StaleObservationEpoch",
            format!("presented={presented} current={current}"),
        ),
        ResolveError::UnknownAgentSourceId { agent_source_id } => (
            "UnknownAgentSourceId",
            format!("agent_source_id={agent_source_id}"),
        ),
        ResolveError::AmbiguousAgentSourceId { agent_source_id, count } => (
            "AmbiguousAgentSourceId",
            format!("agent_source_id={agent_source_id} count={count}"),
        ),
    }
}

fn align_down(v: u64, a: u64) -> u64 {
    v - (v % a)
}
fn align_up(v: u64, a: u64) -> u64 {
    align_down(v + a - 1, a)
}

fn main() {
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
            ("system_root", s(std::env::var("SystemRoot").unwrap_or_else(|_| "<unset>".into()))),
            ("select_model_substr", s(&args.select_model_substr)),
            ("diag_range_bytes", V::U(args.diag_range_bytes)),
        ],
    );

    let mut counters = Counters::default();

    // ============================ CP4 ============================
    let cp4 = run_cp4(&log, &args, &mut counters);
    let cp4_exit = match &cp4 {
        Ok(_) => 0,
        Err(code) => *code,
    };
    let cp4_pass = cp4_exit == 0;
    log.emit(
        "info",
        "cp4.final",
        &[
            ("cp4_exitcode", V::I(cp4_exit as i64)),
            ("cp4_pass", V::B(cp4_pass)),
            ("resolution_attempt_count", V::U(counters.resolution_attempt_count)),
            ("resolution_success_count", V::U(counters.resolution_success_count)),
            ("data_device_open_count", V::U(counters.data_device_open_count)),
            ("data_read_count", V::U(counters.data_read_count)),
        ],
    );

    // ============================ CP5 ============================
    let cp5_exit: i64 = match (&cp4, args.run_cp5) {
        (Err(_), _) => {
            log.emit("warn", "cp5.skipped", &[("reason", s("CP4 did not PASS"))]);
            -1 // SKIPPED
        }
        (Ok(_), false) => {
            log.emit("info", "cp5.skipped", &[("reason", s("--no-cp5"))]);
            -1
        }
        (Ok(success), true) => run_cp5(&log, &args, success, &mut counters) as i64,
    };
    let cp5_pass = cp5_exit == 0;

    // final unmistakable status pair
    let cp5_str = if cp5_exit < 0 { "SKIPPED".to_string() } else { cp5_exit.to_string() };
    log.emit(
        "info",
        "probe.end",
        &[
            ("cp4_exitcode", V::I(cp4_exit as i64)),
            ("cp5_exitcode", s(&cp5_str)),
            ("cp4_pass", V::B(cp4_pass)),
            ("cp5_pass", V::B(cp5_pass)),
            ("data_device_open_count", V::U(counters.data_device_open_count)),
            ("data_read_count", V::U(counters.data_read_count)),
        ],
    );

    let _ = write_local_file(&log);
    flush_to_sink(&log, &args.sink);
    let _ = write_local_file(&log);

    println!();
    println!("CP4_EXITCODE={cp4_exit}");
    println!("CP5_EXITCODE={cp5_str}");
    let _ = std::io::stdout().flush();

    // process exit: CP4 failure dominates; else CP5 status (SKIPPED -> 0 only
    // when it was --no-cp5 on a CP4 PASS)
    let code = if !cp4_pass {
        cp4_exit
    } else if cp5_exit < 0 {
        0
    } else {
        cp5_exit as i32
    };
    std::process::exit(code);
}

/// What CP4 established, for CP5 to reuse verbatim (SAME epoch, SAME selection,
/// SAME resolution — CP5 never re-enumerates).
struct Cp4Success {
    current_obs: String,
    selected_asid: String,
    resolved_locator: String,
    evidence_model: String,
    evidence_serial: String,
}

/// CP4 exit codes: 0 PASS, 40 resolver/instrumentation, 41 operator selection
/// ambiguous, 42 device opened during CP4, 43 stale/unknown resolved.
fn run_cp4(log: &Log, args: &Args, counters: &mut Counters) -> Result<Cp4Success, i32> {
    log.emit("info", "cp4.begin", &[("boundary", s("probe-local Agent source resolution — NOT product SOURCE_REFERENCE_STALE"))]);

    let epoch_src = sources::enumerate();
    let current_obs = epoch_src.observation_id.clone();
    log.emit(
        "info",
        "cp4.epoch.current",
        &[
            ("authority.source_observation_id", s(&current_obs)),
            ("observation_id_len", V::U(current_obs.len() as u64)),
            ("source_count", V::U(epoch_src.sources.len() as u64)),
        ],
    );
    for (i, src) in epoch_src.sources.iter().enumerate() {
        log.emit(
            "info",
            "cp4.epoch.source",
            &[
                ("index", V::U(i as u64)),
                ("authority.agent_source_id", s(&src.agent_source_id)),
                ("evidence_only.local_locator", s(&src.local_locator)),
                ("evidence_only.model", s(&src.product)),
                ("evidence_only.vendor", s(&src.vendor)),
                ("evidence_only.serial", s(&src.serial)),
                ("evidence_only.bus_type", s(&src.bus_type)),
                ("evidence_only.size_bytes", V::U(src.size_bytes)),
                ("evidence_only.removable", V::B(src.removable)),
            ],
        );
    }

    let entries: Vec<EpochEntry> = epoch_src
        .sources
        .iter()
        .map(|s| EpochEntry {
            agent_source_id: s.agent_source_id.clone(),
            local_locator: s.local_locator.clone(),
        })
        .collect();
    let epoch = CurrentEpoch::new(current_obs.clone(), entries);
    if epoch.has_duplicate_agent_source_ids() {
        log.emit("error", "cp4.epoch.ambiguous", &[("detail", s("duplicate agent_source_id in this epoch — fail closed"))]);
        return Err(40);
    }

    // ---- operator selection: LOCAL HARDWARE EVIDENCE only, never authority ----
    let matched: Vec<&sources::LocalSource> = epoch_src
        .sources
        .iter()
        .filter(|src| src.product.contains(&args.select_model_substr))
        .collect();
    if matched.len() != 1 {
        log.emit(
            "error",
            "cp4.operator_selection.ambiguous",
            &[
                ("select_model_substr", s(&args.select_model_substr)),
                ("match_count", V::U(matched.len() as u64)),
            ],
        );
        return Err(41);
    }
    let selected_asid = matched[0].agent_source_id.clone();
    let selected_locator = matched[0].local_locator.clone();
    log.emit(
        "info",
        "cp4.operator_selection",
        &[
            ("basis", s("operator predicate over LOCAL HARDWARE EVIDENCE (model substring) — NOT cross-boundary authority")),
            ("matched.evidence_only.model", s(&matched[0].product)),
            ("matched.evidence_only.local_locator", s(&selected_locator)),
            ("resulting.authority.agent_source_id", s(&selected_asid)),
        ],
    );

    // ---- CP4 HAPPY: current tuple resolves exactly once ----
    counters.resolution_attempt_count += 1;
    debug_assert_eq!(epoch.observation_id(), current_obs);
    let happy = epoch.resolve(epoch.observation_id(), &selected_asid);
    let happy_ok_locator_match = match &happy {
        Ok(r) => {
            counters.resolution_success_count += 1;
            let m = r.local_locator == selected_locator;
            log.emit(
                "info",
                "cp4.resolve.current",
                &[
                    ("result", s("RESOLVED")),
                    ("authority.source_observation_id", s(&current_obs)),
                    ("authority.agent_source_id", s(&selected_asid)),
                    ("evidence_only.resolved_local_locator", s(&r.local_locator)),
                    ("locator_matches_operator_selection", V::B(m)),
                    ("STOP", s("evaluating CP4 — NOT opening GENERIC_READ")),
                ],
            );
            m
        }
        Err(e) => {
            let (k, d) = resolve_err_str(e);
            log.emit("error", "cp4.resolve.current", &[("result", s("UNEXPECTED_REJECT")), ("kind", s(k)), ("detail", s(d))]);
            false
        }
    };

    // Instrumentation invariant: no data handle may have been opened yet.
    if counters.data_device_open_count != 0 || counters.data_read_count != 0 {
        log.emit("error", "cp4.instrumentation.violated", &[("data_device_open_count", V::U(counters.data_device_open_count)), ("data_read_count", V::U(counters.data_read_count))]);
        return Err(42);
    }

    // ---- CP4 PRESSURE: stale / unknown references must fail closed ----
    let pressure: [(&str, &str, &str); 3] = [
        ("cp3_prior_current_epoch", CP3_CURRENT_OBS, CP3_CURRENT_ASID),
        ("cp3_prior_stale_epoch", CP3_STALE_OBS, CP3_STALE_ASID),
        ("unknown_asid_in_current_epoch", current_obs.as_str(), "cp4-deliberately-unknown-agent-source-id"),
    ];
    let mut all_rejected = true;
    for (label, obs, asid) in pressure {
        counters.resolution_attempt_count += 1;
        match epoch.resolve(obs, asid) {
            Err(e) => {
                let (k, d) = resolve_err_str(&e);
                log.emit(
                    "info",
                    "cp4.resolve.pressure",
                    &[
                        ("case", s(label)),
                        ("presented.source_observation_id", s(obs)),
                        ("presented.agent_source_id", s(asid)),
                        ("result", s("REJECTED_FAIL_CLOSED")),
                        ("kind", s(k)),
                        ("detail", s(d)),
                        ("data_device_open_count", V::U(counters.data_device_open_count)),
                        ("data_read_count", V::U(counters.data_read_count)),
                    ],
                );
            }
            Ok(r) => {
                all_rejected = false;
                log.emit(
                    "error",
                    "cp4.resolve.pressure",
                    &[
                        ("case", s(label)),
                        ("result", s("RESOLVED_SHOULD_NOT_HAVE")),
                        ("evidence_only.resolved_local_locator", s(&r.local_locator)),
                    ],
                );
            }
        }
    }

    // ---- CP4 gate ----
    let no_device_touched = counters.data_device_open_count == 0 && counters.data_read_count == 0;
    let cp4_pass = happy_ok_locator_match
        && counters.resolution_success_count == 1
        && all_rejected
        && no_device_touched;

    log.emit(
        "info",
        "cp4.verdict",
        &[
            ("cp4_pass", V::B(cp4_pass)),
            ("happy_resolved_to_expected_ssd", V::B(happy_ok_locator_match)),
            ("resolution_success_count", V::U(counters.resolution_success_count)),
            ("all_pressure_rejected", V::B(all_rejected)),
            ("data_device_open_count", V::U(counters.data_device_open_count)),
            ("data_read_count", V::U(counters.data_read_count)),
            ("label", s("probe-local Spike evidence — NOT production M2 validation")),
        ],
    );

    if !cp4_pass {
        if !all_rejected {
            return Err(43);
        }
        return Err(40);
    }
    Ok(Cp4Success {
        current_obs,
        selected_asid,
        resolved_locator: selected_locator,
        evidence_model: matched[0].product.clone(),
        evidence_serial: matched[0].serial.clone(),
    })
}

/// CP5 exit codes: 0 PASS, 50 open failed, 51 length APIs disagree/none,
/// 52 device too small for the plan, 53 a bounded read failed, 54 a repeat
/// range hashed differently, 55 a read crossed device length.
fn run_cp5(log: &Log, args: &Args, success: &Cp4Success, counters: &mut Counters) -> i32 {
    // CP5 uses CP4's established epoch/selection/resolution VERBATIM — it never
    // re-enumerates (a fresh enumeration would mint a new epoch id, which is
    // exactly why CP4's stale cases were stale).
    log.emit(
        "info",
        "cp5.source",
        &[
            ("authority.source_observation_id", s(&success.current_obs)),
            ("authority.agent_source_id", s(&success.selected_asid)),
            ("evidence_only.local_locator", s(&success.resolved_locator)),
            ("evidence_only.model", s(&success.evidence_model)),
            ("evidence_only.serial", s(&success.evidence_serial)),
            ("note", s("locator came from CP4's resolver result, not a hardcoded PhysicalDrive path")),
        ],
    );

    // ---- open GENERIC_READ ----
    let src = match sources::RawReadSource::open(&success.resolved_locator, counters) {
        Ok(src) => src,
        Err(e) => {
            log.emit("error", "cp5.open.failed", &[("error", s(e))]);
            return 50;
        }
    };
    log.emit(
        "info",
        "cp5.open.ok",
        &[
            ("evidence_only.opened_locator", s(src.locator())),
            ("locator_equals_resolver_result", V::B(src.locator() == success.resolved_locator)),
            ("desired_access", s("GENERIC_READ")),
            ("generic_write_requested", V::B(src.generic_write_requested())),
            ("data_device_open_count", V::U(counters.data_device_open_count)),
        ],
    );

    // ---- device length via 3 read-only IOCTLs ----
    let dl = src.device_length();
    log.emit(
        "info",
        "cp5.length",
        &[
            ("ioctl_disk_get_length_info", match dl.get_length_info { Some(v) => V::U(v), None => s("null") }),
            ("ioctl_disk_get_drive_geometry_ex", match dl.drive_geometry_ex { Some(v) => V::U(v), None => s("null") }),
            ("ioctl_storage_read_capacity", match dl.storage_read_capacity { Some(v) => V::U(v), None => s("null") }),
            ("bytes_per_sector", match dl.bytes_per_sector { Some(v) => V::U(v as u64), None => s("null") }),
            ("apis_agree", V::B(dl.agree())),
        ],
    );
    let Some(dev_len) = dl.authoritative() else {
        log.emit("error", "cp5.length.unusable", &[("obtained", s(format!("{:?}", dl.obtained())))]);
        return 51;
    };
    let sector = dl.bytes_per_sector.unwrap_or(512).max(512) as u64;
    let align = sector.max(4096);
    let diag = align_up(args.diag_range_bytes.max(align), align);
    log.emit(
        "info",
        "cp5.plan",
        &[
            ("evidence_only.device_byte_length", V::U(dev_len)),
            ("evidence_only.device_gib", s(format!("{:.2}", dev_len as f64 / (1u64 << 30) as f64))),
            ("sector_size", V::U(sector)),
            ("read_alignment", V::U(align)),
            ("diag_range_bytes", V::U(diag)),
        ],
    );
    if dev_len < diag.saturating_mul(4) {
        log.emit("error", "cp5.plan.device_too_small", &[("dev_len", V::U(dev_len)), ("diag", V::U(diag))]);
        return 52;
    }

    // ---- bounded ranges: begin / middle / end / repeat begin / repeat middle ----
    let mid_off = align_down(dev_len / 2, align);
    let end_off = align_down(dev_len - diag, align);
    let end_len = dev_len - end_off; // sector-aligned, ends exactly at dev_len
    let ranges: [(&str, u64, u64); 5] = [
        ("begin", 0, diag),
        ("middle", mid_off, diag),
        ("end", end_off, end_len),
        ("repeat_begin", 0, diag),
        ("repeat_middle", mid_off, diag),
    ];

    let mut reads = Vec::new();
    let mut crossed = false;
    for (label, off, len) in ranges {
        let rr = src.read_at(label, off, len, counters);
        if rr.offset + rr.actual_len > dev_len {
            crossed = true;
        }
        log.emit(
            "info",
            "cp5.read",
            &[
                ("label", s(&rr.label)),
                ("offset", V::U(rr.offset)),
                ("requested_len", V::U(rr.requested_len)),
                ("actual_len", V::U(rr.actual_len)),
                ("sha256", s(&rr.sha256_hex)),
                ("ok", V::B(rr.ok)),
                ("elapsed_ms", V::U(rr.elapsed_ms as u64)),
                ("within_device_length", V::B(rr.offset + rr.actual_len <= dev_len)),
            ],
        );
        reads.push(rr);
    }

    let by = |l: &str| reads.iter().find(|r| r.label == l).unwrap();
    let begin = by("begin");
    let middle = by("middle");
    let end = by("end");
    let rbegin = by("repeat_begin");
    let rmiddle = by("repeat_middle");

    let reproducible_begin = begin.ok && rbegin.ok && begin.sha256_hex == rbegin.sha256_hex && !begin.sha256_hex.is_empty();
    let reproducible_middle = middle.ok && rmiddle.ok && middle.sha256_hex == rmiddle.sha256_hex;
    let end_ok = end.ok && end.actual_len == end_len && (end.offset + end.actual_len == dev_len);

    let cp5_pass = begin.ok
        && middle.ok
        && end_ok
        && reproducible_begin
        && reproducible_middle
        && !crossed
        && counters.data_read_count == 5
        && !src.generic_write_requested();

    log.emit(
        "info",
        "cp5.verdict",
        &[
            ("cp5_pass", V::B(cp5_pass)),
            ("begin_ok", V::B(begin.ok)),
            ("middle_ok", V::B(middle.ok)),
            ("end_ok_ends_at_device_length", V::B(end_ok)),
            ("begin_reproducible", V::B(reproducible_begin)),
            ("middle_reproducible", V::B(reproducible_middle)),
            ("no_read_crossed_device_length", V::B(!crossed)),
            ("data_read_count", V::U(counters.data_read_count)),
            ("generic_write_requested", V::B(src.generic_write_requested())),
            ("no_source_write_no_mount_no_repair", V::B(true)),
        ],
    );

    drop(src); // CloseHandle

    if !cp5_pass {
        if crossed {
            return 55;
        }
        if !reproducible_begin || !reproducible_middle {
            return 54;
        }
        return 53;
    }
    0
}
