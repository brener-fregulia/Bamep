//! Issue #63 Stage 1 — WinPE-native runner. THROWAWAY Spike. LAB-ONLY.
//!
//! NOT the Bamep Agent, NOT `crates/agent`, NOT the `bamepd` composition root.
//! One process, one WinPE session, launched automatically by an injected
//! `winpeshl.ini` (the Phase-9d `boot.wim` is NOT modified — the files are
//! overlaid into `X:\Windows\System32` by wimboot at boot time). Its ONLY job
//! is to prove the Stage-1 automation primitives:
//!
//!   forward the bootstrap's `winpe.booted` / `winpe.wpeinit_complete`
//!   -> wait (bounded) for the network
//!   -> reach the Fedora coordinator
//!   -> obtain the Server's current UTC
//!   -> align the WinPE SYSTEM clock with `SetSystemTime` (UTC)
//!   -> read the clock back + re-check the skew against a STRICT bound
//!   -> report `stage1.ready`
//!   -> STOP (leave the CMD open for the operator).
//!
//! It opens NO `\\.\PhysicalDrive*` handle, issues NO IOCTL, performs NO
//! transfer, and writes only a local `X:\` NDJSON evidence file. If
//! `SetSystemTime` returns FALSE it records the exact Win32 error and fails
//! closed WITHOUT inventing a registry/timezone fallback (per the Stage-1
//! contract).

mod matrix;
mod pure;

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const NET_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const COORD_IO_TIMEOUT: Duration = Duration::from_secs(8);

mod exit {
    pub const PASS: i32 = 0;
    pub const BAD_ARGS: i32 = 2;
    pub const NET_NOT_READY: i32 = 20;
    pub const COORD_UTC_FAILED: i32 = 21;
    /// The bootstrap's on-disk `X:\` evidence is absent / incomplete / corrupt.
    /// The runner NEVER synthesises `winpe.booted` / `winpe.wpeinit_complete`.
    pub const BOOTSTRAP_EVIDENCE_MISSING: i32 = 22;
    pub const SETSYSTEMTIME_FAILED: i32 = 30;
    pub const RESIDUAL_SKEW_OUT_OF_BOUND: i32 = 31;
    pub const SINK_UNREACHABLE_FINAL: i32 = 40;
}

// ---------------------------------------------------------------------------
// tiny structured logger (buffer + cumulative flush, same idiom as #61)
// ---------------------------------------------------------------------------

enum F {
    S(String),
    I(i64),
    U(u64),
    B(bool),
}
fn fs(x: impl Into<String>) -> F {
    F::S(x.into())
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct Log {
    started: Instant,
    seq: Mutex<u64>,
    buf: Mutex<Vec<String>>,
    run_id: String,
    mode: String,
}
impl Log {
    fn new(run_id: &str, mode: &str) -> Self {
        Self {
            mode: mode.to_string(),
            started: Instant::now(),
            seq: Mutex::new(0),
            buf: Mutex::new(Vec::new()),
            run_id: run_id.to_string(),
        }
    }
    fn emit(&self, level: &str, event: &str, fields: &[(&str, F)]) {
        let seq = {
            let mut g = self.seq.lock().unwrap();
            *g += 1;
            *g
        };
        let mut l = format!(
            r#"{{"ts_ms":{},"seq":{seq},"elapsed_ms":{},"level":"{}","event":"{}","stage":"stage1","mode":"{}","run_id":"{}","runner":"{}""#,
            now_ms(),
            self.started.elapsed().as_millis(),
            esc(level),
            esc(event),
            esc(&self.mode),
            esc(&self.run_id),
            esc(NAME),
        );
        for (k, v) in fields {
            match v {
                F::S(x) => l.push_str(&format!(r#","{}":"{}""#, esc(k), esc(x))),
                F::I(x) => l.push_str(&format!(r#","{}":{}"#, esc(k), x)),
                F::U(x) => l.push_str(&format!(r#","{}":{}"#, esc(k), x)),
                F::B(x) => l.push_str(&format!(r#","{}":{}"#, esc(k), x)),
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
    /// Cumulative snapshot to the Fedora sink (idempotent — the coordinator
    /// de-dups by `seq`). Best effort; a failure is logged locally, never fatal
    /// until the final flush.
    fn flush_sink(&self, sink: &str) -> bool {
        let Some(addr) = sink.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            return false;
        };
        let Ok(mut st) = TcpStream::connect_timeout(&addr, NET_PROBE_TIMEOUT) else {
            return false;
        };
        let _ = st.set_write_timeout(Some(COORD_IO_TIMEOUT));
        let _ = st.set_read_timeout(Some(Duration::from_millis(500)));
        if st
            .write_all(format!("{}\n", self.snapshot()).as_bytes())
            .is_err()
        {
            return false;
        }
        let _ = st.flush();
        let _ = st.shutdown(std::net::Shutdown::Write);
        let _ = st.read(&mut [0u8; 16]);
        true
    }
    fn write_local(&self) {
        let dir = std::env::var("TEMP")
            .or_else(|_| std::env::var("TMP"))
            .unwrap_or_else(|_| ".".into());
        for p in [
            format!("{dir}\\bamep-i63-stage1-runner.ndjson"),
            "X:\\bamep-i63-stage1-runner.ndjson".into(),
            "bamep-i63-stage1-runner.ndjson".into(),
        ] {
            if std::fs::write(&p, format!("{}\n", self.snapshot())).is_ok() {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// args
// ---------------------------------------------------------------------------

struct Args {
    run_id: String,
    /// `physical` (baked into the derived boot bootstrap) or `host-smoke`
    /// (host wiring test). Stamped on every event; the coordinator cross-checks
    /// it and refuses `STAGE1_PHYSICAL_PASS` for anything but `physical`.
    mode: String,
    coord: String,
    sink: String,
    bootstrap_events: String,
    skew_floor_ms: i64,
    skew_ceil_ms: i64,
    net_wait_secs: u64,
}
fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        run_id: "stage1".into(),
        mode: String::new(),
        coord: "192.168.99.1:9206".into(),
        sink: "192.168.99.1:9299".into(),
        bootstrap_events: "X:\\bamep-i63-events.ndjson".into(),
        skew_floor_ms: -2000,
        skew_ceil_ms: 2000,
        net_wait_secs: 120,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut next = || it.next().ok_or_else(|| format!("{x} needs a value"));
        match x.as_str() {
            "--run-id" => a.run_id = next()?,
            "--mode" => a.mode = next()?,
            "--coord" => a.coord = next()?,
            "--sink" => a.sink = next()?,
            "--bootstrap-events" => a.bootstrap_events = next()?,
            "--skew-floor-ms" => {
                a.skew_floor_ms = next()?.parse().map_err(|_| "bad --skew-floor-ms".to_string())?
            }
            "--skew-ceil-ms" => {
                a.skew_ceil_ms = next()?.parse().map_err(|_| "bad --skew-ceil-ms".to_string())?
            }
            "--net-wait-secs" => {
                a.net_wait_secs = next()?.parse().map_err(|_| "bad --net-wait-secs".to_string())?
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    match a.mode.as_str() {
        "physical" | "host-smoke" => {}
        "" => return Err("--mode <physical|host-smoke> is MANDATORY".into()),
        other => return Err(format!("--mode must be 'physical' or 'host-smoke' (got {other:?})")),
    }
    if a.skew_floor_ms > 0 || a.skew_ceil_ms < 0 || a.skew_ceil_ms - a.skew_floor_ms > 60_000 {
        return Err(format!(
            "implausible skew window [{}, {}] ms (Stage 1 wants a strict bound)",
            a.skew_floor_ms, a.skew_ceil_ms
        ));
    }
    Ok(a)
}

// ---------------------------------------------------------------------------
// clock backend: real Win32 on WinPE, no-op stub on the dev host
// ---------------------------------------------------------------------------

struct AlignResult {
    set_ok: bool,
    win32_error: u32,
    readback: (u16, u16, u16, u16, u16, u16, u16),
    filetime_conv_failed: bool,
    filetime_conv_win32_error: u32,
}

#[cfg(windows)]
mod clock {
    use super::{pure, AlignResult};
    use windows_sys::Win32::Foundation::{GetLastError, FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::SystemInformation::{
        GetSystemTime, GetSystemTimeAsFileTime, SetSystemTime,
    };
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;

    pub const BACKEND: &str = "win32-setsystemtime-utc";

    pub fn now_unix_ms() -> i64 {
        let mut ft = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        unsafe { GetSystemTimeAsFileTime(&mut ft) };
        let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
        pure::filetime_ticks_to_unix_ms(ticks)
    }

    /// Convert `target_unix_ms` to a UTC `SYSTEMTIME`, call `SetSystemTime`,
    /// then read the clock back with `GetSystemTime`. Never manipulates the
    /// token / privileges — if the call fails we surface the raw Win32 error.
    pub fn align_to_unix_ms(target_unix_ms: i64) -> AlignResult {
        let ticks = pure::unix_ms_to_filetime_ticks(target_unix_ms);
        let ft = FILETIME {
            dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
        if unsafe { FileTimeToSystemTime(&ft, &mut st) } == 0 {
            return AlignResult {
                set_ok: false,
                win32_error: 0,
                readback: (0, 0, 0, 0, 0, 0, 0),
                filetime_conv_failed: true,
                filetime_conv_win32_error: unsafe { GetLastError() },
            };
        }
        let set_ok = unsafe { SetSystemTime(&st) } != 0;
        let win32_error = if set_ok { 0 } else { unsafe { GetLastError() } };

        let mut rb: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetSystemTime(&mut rb) };
        AlignResult {
            set_ok,
            win32_error,
            readback: (
                rb.wYear,
                rb.wMonth,
                rb.wDay,
                rb.wHour,
                rb.wMinute,
                rb.wSecond,
                rb.wMilliseconds,
            ),
            filetime_conv_failed: false,
            filetime_conv_win32_error: 0,
        }
    }
}

#[cfg(not(windows))]
mod clock {
    use super::AlignResult;

    pub const BACKEND: &str = "host-stub-noop";

    pub fn now_unix_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Dev-host stub: does NOT touch the host clock (that would need root and be
    /// wrong on a dev box). Reports success so `--dry-run` style host exercises
    /// of the whole flow work; the REAL `SetSystemTime` path is Windows-only and
    /// is only ever proven on the MiniPC.
    pub fn align_to_unix_ms(_target_unix_ms: i64) -> AlignResult {
        AlignResult {
            set_ok: true,
            win32_error: 0,
            readback: (0, 0, 0, 0, 0, 0, 0),
            filetime_conv_failed: false,
            filetime_conv_win32_error: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// network helpers
// ---------------------------------------------------------------------------

fn resolve(addr: &str) -> Result<std::net::SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {addr}"))
}

/// Bounded wait until a TCP connection to the coordinator succeeds.
fn wait_for_network(log: &Log, coord: &str, deadline: Instant) -> Result<u64, String> {
    let sa = resolve(coord)?;
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        match TcpStream::connect_timeout(&sa, NET_PROBE_TIMEOUT) {
            Ok(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
                return Ok(attempts);
            }
            Err(_) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "no TCP path to coordinator {coord} after {attempts} attempts"
                    ));
                }
                std::thread::sleep(Duration::from_millis(1000));
                if attempts % 5 == 0 {
                    log.emit(
                        "info",
                        "winpe.network_waiting",
                        &[("attempts", F::U(attempts)), ("target", fs(coord))],
                    );
                }
            }
        }
    }
}

/// One coordinator round-trip: send a request line, read one JSON line back,
/// return `server_utc_ms`.
fn coord_utc(coord: &str, run_id: &str) -> Result<i64, String> {
    let sa = resolve(coord)?;
    let mut st =
        TcpStream::connect_timeout(&sa, COORD_IO_TIMEOUT).map_err(|e| format!("connect: {e}"))?;
    st.set_write_timeout(Some(COORD_IO_TIMEOUT)).ok();
    st.set_read_timeout(Some(COORD_IO_TIMEOUT)).ok();
    let req = format!("{{\"stage1\":\"utc_request\",\"run_id\":\"{}\"}}\n", esc(run_id));
    st.write_all(req.as_bytes())
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
    pure::parse_server_utc_ms(&String::from_utf8_lossy(&buf))
}

// ---------------------------------------------------------------------------
// bootstrap-event forwarding
// ---------------------------------------------------------------------------

/// Forward — VERBATIM — the two milestones the injected bootstrap `.cmd` wrote to
/// `X:\bamep-i63-events.ndjson` before it ran `wpeinit` and before it launched
/// us. The runner NEVER synthesises `winpe.booted` / `winpe.wpeinit_complete`:
/// if the on-disk evidence is absent/incomplete/corrupt/out-of-order this emits
/// the distinct `winpe.bootstrap_evidence_missing` failure event and returns
/// `Err` so the caller fails closed (no PHYSICAL_PASS is then possible).
fn forward_bootstrap_events(log: &Log, path: &str) -> Result<(), ()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    match pure::classify_bootstrap_evidence(&content) {
        pure::BootstrapEvidence::Ok(milestones) => {
            for m in milestones {
                let mut fields: Vec<(&str, F)> = vec![
                    ("origin", fs("bootstrap-forwarded")),
                    ("raw", fs(&m.raw)),
                ];
                if let Some(ts) = &m.local_ts {
                    fields.push(("bootstrap_local_ts", fs(ts)));
                }
                // preserve the milestone's own event name exactly
                let name: &str = &m.event;
                log.emit("info", name, &fields);
            }
            Ok(())
        }
        pure::BootstrapEvidence::Missing { reason } => {
            log.emit(
                "error",
                "winpe.bootstrap_evidence_missing",
                &[
                    ("path", fs(path)),
                    ("detail", fs(reason)),
                    (
                        "note",
                        fs("the injected bootstrap.cmd did not leave usable winpe.booted/winpe.wpeinit_complete evidence on X:\\; the runner does NOT synthesise them — failing closed"),
                    ),
                ],
            );
            Err(())
        }
    }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

fn run(log: &Log, args: &Args) -> i32 {
    log.emit(
        "info",
        "winpe.runner_start",
        &[
            ("version", fs(VERSION)),
            ("git_short", fs(env!("I63_GIT_SHORT"))),
            ("build_epoch_secs", fs(env!("I63_BUILD_EPOCH_SECS"))),
            ("rustc", fs(env!("I63_RUSTC_VERSION"))),
            ("target_triple", fs(env!("I63_TARGET_TRIPLE"))),
            ("clock_backend", fs(clock::BACKEND)),
            ("declared_mode", fs(&args.mode)),
            ("coord", fs(&args.coord)),
            ("sink", fs(&args.sink)),
            (
                "skew_window_ms",
                fs(format!("[{}, {}]", args.skew_floor_ms, args.skew_ceil_ms)),
            ),
            ("computername", fs(std::env::var("COMPUTERNAME").unwrap_or_default())),
        ],
    );

    // The two boot milestones must be OBSERVED (forwarded VERBATIM from the
    // bootstrap's on-disk X:\ evidence), never synthesised. Read + classify the
    // file now (local, no network); if it is unusable we still finish the
    // network wait so the coordinator SEES the failure, then exit non-zero.
    let bootstrap_ok = forward_bootstrap_events(log, &args.bootstrap_events).is_ok();

    // ---- network readiness -------------------------------------------------
    let deadline = Instant::now() + Duration::from_secs(args.net_wait_secs);
    match wait_for_network(log, &args.coord, deadline) {
        Ok(attempts) => log.emit(
            "info",
            "winpe.network_ready",
            &[("attempts", F::U(attempts)), ("coord", fs(&args.coord))],
        ),
        Err(e) => {
            log.emit("error", "winpe.network_unreachable", &[("detail", fs(e))]);
            log.emit("error", "stage1.failed", &[("reason", fs("network_not_ready"))]);
            let _ = log.flush_sink(&args.sink);
            log.write_local();
            return exit::NET_NOT_READY;
        }
    }
    let _ = log.flush_sink(&args.sink);

    if !bootstrap_ok {
        log.emit("error", "stage1.failed", &[("reason", fs("bootstrap_evidence_missing"))]);
        let _ = log.flush_sink(&args.sink);
        log.write_local();
        return exit::BOOTSTRAP_EVIDENCE_MISSING;
    }

    // ---- runner readiness (local evidence writable + sink reachable) ------
    let sink_reachable = log.flush_sink(&args.sink);
    log.emit(
        "info",
        "winpe.runner_ready",
        &[
            ("sink_reachable", F::B(sink_reachable)),
            ("bootstrap_events_path", fs(&args.bootstrap_events)),
        ],
    );
    let _ = log.flush_sink(&args.sink);

    // ---- Server UTC ------------------------------------------------------
    let server_utc_ms = match coord_utc(&args.coord, &args.run_id) {
        Ok(v) => v,
        Err(e) => {
            log.emit("error", "winpe.server_utc_failed", &[("detail", fs(e))]);
            log.emit("error", "stage1.failed", &[("reason", fs("coord_utc_failed"))]);
            let _ = log.flush_sink(&args.sink);
            log.write_local();
            return exit::COORD_UTC_FAILED;
        }
    };
    let agent_before = clock::now_unix_ms();
    let skew_before = agent_before - server_utc_ms;
    log.emit(
        "info",
        "winpe.server_utc_received",
        &[
            ("server_utc_ms", F::I(server_utc_ms)),
            ("agent_now_ms", F::I(agent_before)),
            ("skew_before_ms", F::I(skew_before)),
        ],
    );
    let _ = log.flush_sink(&args.sink);

    // ---- automatic clock alignment ------------------------------------
    log.emit(
        "info",
        "winpe.clock_alignment_attempted",
        &[
            ("method", fs(clock::BACKEND)),
            ("target_server_utc_ms", F::I(server_utc_ms)),
            ("skew_before_ms", F::I(skew_before)),
        ],
    );
    let _ = log.flush_sink(&args.sink);

    let align = clock::align_to_unix_ms(server_utc_ms);

    if align.filetime_conv_failed {
        log.emit(
            "error",
            "winpe.clock_alignment_failed",
            &[
                ("phase", fs("FileTimeToSystemTime")),
                ("win32_error", F::U(align.filetime_conv_win32_error as u64)),
                ("note", fs("could not convert Server UTC to SYSTEMTIME; STOP and report this Win32 error")),
            ],
        );
        log.emit("error", "stage1.failed", &[("reason", fs("filetime_conv_failed"))]);
        let _ = log.flush_sink(&args.sink);
        log.write_local();
        return exit::SETSYSTEMTIME_FAILED;
    }

    if !align.set_ok {
        log.emit(
            "error",
            "winpe.clock_alignment_failed",
            &[
                ("phase", fs("SetSystemTime")),
                ("set_ok", F::B(false)),
                ("win32_error", F::U(align.win32_error as u64)),
                ("note", fs("SetSystemTime returned FALSE. Per the Stage-1 contract: STOP and report this EXACT Win32 error before any fallback. Likely SeSystemtimePrivilege is not held by this WinPE token — do NOT add token/privilege or registry/timezone manipulation without owner review.")),
            ],
        );
        log.emit("error", "stage1.failed", &[("reason", fs("setsystemtime_false"))]);
        let _ = log.flush_sink(&args.sink);
        log.write_local();
        return exit::SETSYSTEMTIME_FAILED;
    }

    let (y, mo, d, h, mi, s, ms) = align.readback;
    let readback_iso = pure::iso8601_utc(y, mo, d, h, mi, s, ms);

    // fresh Server UTC for the residual-skew re-check
    let server_utc_2 = match coord_utc(&args.coord, &args.run_id) {
        Ok(v) => v,
        Err(e) => {
            log.emit(
                "error",
                "winpe.server_utc_failed",
                &[("phase", fs("post_align_recheck")), ("detail", fs(e))],
            );
            log.emit("error", "stage1.failed", &[("reason", fs("coord_utc_failed_recheck"))]);
            let _ = log.flush_sink(&args.sink);
            log.write_local();
            return exit::COORD_UTC_FAILED;
        }
    };
    let agent_after = clock::now_unix_ms();
    let skew_after = agent_after - server_utc_2;

    match pure::classify_skew(skew_after, args.skew_floor_ms, args.skew_ceil_ms) {
        pure::SkewVerdict::InBound => {
            log.emit(
                "info",
                "winpe.clock_aligned",
                &[
                    ("method", fs(clock::BACKEND)),
                    ("set_ok", F::B(true)),
                    ("win32_error", F::U(0)),
                    ("readback_utc", fs(&readback_iso)),
                    ("skew_before_ms", F::I(skew_before)),
                    ("skew_after_ms", F::I(skew_after)),
                    ("skew_floor_ms", F::I(args.skew_floor_ms)),
                    ("skew_ceil_ms", F::I(args.skew_ceil_ms)),
                ],
            );
        }
        verdict => {
            log.emit(
                "error",
                "winpe.clock_alignment_failed",
                &[
                    ("phase", fs("residual_skew_recheck")),
                    ("verdict", fs(format!("{verdict:?}"))),
                    ("readback_utc", fs(&readback_iso)),
                    ("skew_before_ms", F::I(skew_before)),
                    ("skew_after_ms", F::I(skew_after)),
                    ("skew_floor_ms", F::I(args.skew_floor_ms)),
                    ("skew_ceil_ms", F::I(args.skew_ceil_ms)),
                    ("note", fs("clock still outside the strict Stage-1 bound after SetSystemTime; failing closed, NO fallback")),
                ],
            );
            log.emit("error", "stage1.failed", &[("reason", fs("residual_skew_out_of_bound"))]);
            let _ = log.flush_sink(&args.sink);
            log.write_local();
            return exit::RESIDUAL_SKEW_OUT_OF_BOUND;
        }
    }
    let _ = log.flush_sink(&args.sink);

    // ---- READY ---------------------------------------------------------
    log.emit(
        "info",
        "stage1.ready",
        &[
            ("skew_after_ms", F::I(skew_after)),
            ("readback_utc", fs(&readback_iso)),
            (
                "note",
                fs("derived PXE/WinPE runtime + automatic WinPE start + automatic UTC clock alignment proven. NO source-content handle opened, NO Transfer, NO Artifact, NO matrix. Phase-9d boot.wim was not modified."),
            ),
        ],
    );
    log.write_local();
    if !log.flush_sink(&args.sink) {
        log.emit(
            "warn",
            "stage1.sink_unreachable_final",
            &[("note", fs("stage1.ready reached but the final sink flush failed; local X:\\ NDJSON holds the full evidence"))],
        );
        log.write_local();
        return exit::SINK_UNREACHABLE_FINAL;
    }
    exit::PASS
}

fn main() {
    // `--matrix` subcommand, intercepted BEFORE `parse_args` so the committed
    // Stage-1 invocation is completely unaffected. `--matrix --arm` runs the
    // Stage-3 physical matrix loop; `--matrix` alone is inert.
    if std::env::args().nth(1).as_deref() == Some("--matrix") {
        if std::env::args().any(|x| x == "--arm") {
            std::process::exit(matrix::run_armed());
        }
        std::process::exit(matrix::run_not_armed());
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bamep-i63-runner: FATAL: {e}");
            println!();
            println!("BAMEP_I63_STAGE1_EXITCODE={}", exit::BAD_ARGS);
            std::process::exit(exit::BAD_ARGS);
        }
    };
    let log = Log::new(&args.run_id, &args.mode);
    let code = run(&log, &args);
    log.emit(
        "info",
        "winpe.runner_end",
        &[("exitcode", F::I(code as i64)), ("pass", F::B(code == exit::PASS))],
    );
    log.write_local();
    let _ = log.flush_sink(&args.sink);

    println!();
    println!("BAMEP_I63_STAGE1_EXITCODE={code}");
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
