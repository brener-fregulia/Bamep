//! Issue #65 — throughput-first endpoint-capture streaming probe. THROWAWAY Spike.
//!
//! WinPE-native (`x86_64-pc-windows-msvc`, static CRT). LAB-ONLY. NOT the Bamep
//! Agent, NOT `crates/agent`, NOT `bamepd`, NOT a production transfer path.
//!
//! The SIMPLEST physical capture byte path that can answer the product question
//! (#65): is `MiniPC source -> network -> server file` limited by a physical
//! resource, or by Bamep software?
//!
//!   fresh same-boot source-observation epoch (THIS process)
//!     -> opaque agent_source_id -> local resolver -> resolved local locator
//!     -> GENERIC_READ open + 3-IOCTL device-length agreement
//!     -> ISSUE-63 SOURCE-SAFETY PREDICATE (fail-closed; Reject => ZERO bulk reads)
//!     -> dedicated producer thread: bounded 2 GiB SEQUENTIAL read of the source
//!        (+ incremental SHA-256 on that same thread) into a small bounded queue
//!     -> foreground: drain the queue into ONE continuous TCP stream (one
//!        connection) to the Fedora sink, which writes ONE sequential file and
//!        does ONE final fsync.
//!
//! NOT here (deliberately, per #65): per-chunk files / per-chunk fsync /
//! per-chunk DB commit / per-chunk ACK barrier / resume / retry / TLS / auth /
//! protocol framing beyond a single JSON preamble line / Artifact lifecycle /
//! benchmark matrix. If this reaches the acceptance range it is proof the
//! mechanism is worth hardening — not a production implementation.
//!
//! The producer/consumer split is the ONE non-trivial choice: a single serial
//! read->hash->write loop would leave the NIC idle during every read+hash and
//! impose an artificial software ceiling — exactly what #65 says Bamep must not
//! do. Two threads + a 4-deep queue keep the single TCP stream continuously fed.

mod resolver;
mod sources;

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use bamep_i63_stage2_engine::safety::{
    evaluate_predicate, ResolvedSource, SafetyRequest, SafetyVerdict,
};
use resolver::{CurrentEpoch, EpochEntry};
use sha2::{Digest, Sha256};
use sources::Counters;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Fixed bounded read extent — #65 says 2 GiB is sufficient.
const EXTENT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Sequential source read block handed to the queue. 2 GiB / 8 MiB = 256 blocks,
/// exact (no partial final block).
const READ_BLOCK: u64 = 8 * 1024 * 1024;
/// Bounded in-flight queue depth (buffers). 4 * 8 MiB = 32 MiB max in flight.
const QUEUE_DEPTH: usize = 4;

mod exit {
    pub const PASS: i32 = 0;
    pub const BAD_ARGS: i32 = 2;
    pub const ENUMERATION: i32 = 61;
    pub const RESOLVER: i32 = 67;
    pub const DEVICE: i32 = 68;
    pub const SAFETY_REJECTED: i32 = 74;
    pub const CONNECT: i32 = 62;
    pub const STREAM_FATAL: i32 = 70;
    pub const DIGEST_MISMATCH: i32 = 71;
    pub const INCOMPLETE: i32 = 72;
}

fn esc(i: &str) -> String {
    let mut o = String::with_capacity(i.len() + 2);
    for c in i.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

struct Args {
    sink: String,
    select_model_substr: String,
    label: String,
    extent_bytes: u64,
    digest: bool,
    self_check: bool,
    /// `--self-check` only: also run ONE real streamed transfer against `--sink`
    /// (host wiring check). Off by default so `--self-check` alone needs no sink.
    loopback: bool,
    /// Bounded wait for the sink TCP endpoint to become reachable (WinPE DHCP
    /// can lag `wpeinit` by tens of seconds). This is initial reachability
    /// only — NOT transfer resume/retry: once the stream starts, a failure is
    /// fatal.
    connect_wait_secs: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        sink: "192.168.99.1:9265".into(),
        select_model_substr: "256GB".into(),
        label: "unlabeled".into(),
        extent_bytes: EXTENT_BYTES,
        digest: true,
        self_check: false,
        loopback: false,
        connect_wait_secs: 60,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--sink" => a.sink = it.next().ok_or("--sink needs a value")?,
            "--select-model-substr" => {
                a.select_model_substr = it.next().ok_or("--select-model-substr needs a value")?
            }
            "--label" => a.label = it.next().ok_or("--label needs a value")?,
            "--extent-bytes" => {
                a.extent_bytes = it
                    .next()
                    .ok_or("--extent-bytes needs a value")?
                    .parse()
                    .map_err(|_| "bad --extent-bytes")?
            }
            "--digest" => a.digest = true,
            "--no-digest" => a.digest = false,
            "--self-check" => a.self_check = true,
            "--loopback" => a.loopback = true,
            "--connect-wait-secs" => {
                a.connect_wait_secs = it
                    .next()
                    .ok_or("--connect-wait-secs needs a value")?
                    .parse()
                    .map_err(|_| "bad --connect-wait-secs")?
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if a.extent_bytes == 0 || !a.extent_bytes.is_multiple_of(READ_BLOCK) {
        return Err(format!(
            "--extent-bytes must be a non-zero multiple of {READ_BLOCK}"
        ));
    }
    Ok(a)
}

/// One resolved + safety-gated source ready for bulk reads.
#[derive(Debug)]
struct GatedSource {
    locator: String,
    device_length_bytes: u64,
}

/// enumerate -> select -> resolve -> GENERIC_READ open + length -> Issue-63
/// source-safety predicate. Returns `Ok` only on `Accept`; a `Reject` (or any
/// earlier failure) returns `Err(exit_code)` and NO bulk read has happened.
fn resolve_and_gate(args: &Args) -> Result<GatedSource, i32> {
    let epoch_src = sources::enumerate();
    let obs_id = epoch_src.observation_id.clone();
    if obs_id.len() != 43 || epoch_src.sources.is_empty() {
        eprintln!("i65: bad source epoch (obs_id_len={})", obs_id.len());
        return Err(exit::ENUMERATION);
    }
    for (i, s) in epoch_src.sources.iter().enumerate() {
        eprintln!(
            "i65: source[{i}] authority.agent_source_id={} evidence_only.locator={} evidence_only.model={:?}",
            s.agent_source_id, s.local_locator, s.product
        );
    }

    let matched: Vec<&sources::LocalSource> = epoch_src
        .sources
        .iter()
        .filter(|x| x.product.contains(&args.select_model_substr))
        .collect();
    if matched.len() != 1 {
        eprintln!(
            "i65: operator selection ambiguous: {} sources match model substring {:?}",
            matched.len(),
            args.select_model_substr
        );
        return Err(exit::ENUMERATION);
    }
    let sel_asid = matched[0].agent_source_id.clone();
    let sel_source = matched[0];

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
        eprintln!("i65: ambiguous epoch (duplicate agent_source_id)");
        return Err(exit::RESOLVER);
    }
    let resolved = match epoch.resolve(&obs_id, &sel_asid) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("i65: resolver failed: {e:?}");
            return Err(exit::RESOLVER);
        }
    };

    let mut counters = Counters::default();
    let src = match sources::RawReadSource::open(&resolved.local_locator, &mut counters) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("i65: source open (GENERIC_READ) failed: {e}");
            return Err(exit::DEVICE);
        }
    };
    let device_length = src.device_length();
    eprintln!(
        "i65: source opened desired_access=GENERIC_READ generic_write_requested={} device_length_authoritative={:?} bulk_read_count={}",
        src.generic_write_requested(),
        device_length.authoritative(),
        counters.data_read_count
    );

    let rs = ResolvedSource {
        agent_source_id: resolved.agent_source_id.clone(),
        source_observation_id: obs_id.clone(),
        local_locator: resolved.local_locator.clone(),
        model: sel_source.product.clone(),
        serial: {
            let s = sel_source.serial.trim();
            (!s.is_empty()).then(|| s.to_string())
        },
        device_length_bytes: device_length.authoritative(),
    };
    let req = SafetyRequest {
        current_observation_id: &obs_id,
        selected_agent_source_ids: std::slice::from_ref(&sel_asid),
        resolved: Some(&rs),
        requested_extent_bytes: args.extent_bytes,
    };
    // `src` is dropped here (handle closed). The producer thread opens its OWN
    // GENERIC_READ handle to the same safety-PASSED locator — the exact
    // owner-reviewed pattern from Issue-63 Stage 4. ZERO bulk reads have
    // happened on any path to this point.
    drop(src);
    match evaluate_predicate(&req) {
        SafetyVerdict::Accept {
            locator,
            device_length_bytes,
        } => {
            eprintln!(
                "i65: SOURCE-SAFETY PREDICATE = ACCEPT locator={locator} device_length_bytes={device_length_bytes} extent_bytes={} bulk_read_count={}",
                args.extent_bytes, counters.data_read_count
            );
            Ok(GatedSource {
                locator,
                device_length_bytes,
            })
        }
        SafetyVerdict::Reject(reason) => {
            eprintln!(
                "i65: SOURCE-SAFETY PREDICATE = REJECT reason={reason:?} — fail-closed, NO bulk source read, nothing sent"
            );
            Err(exit::SAFETY_REJECTED)
        }
    }
}

/// The producer thread: opens its own GENERIC_READ handle to `locator`, streams
/// `extent_bytes` in ascending `READ_BLOCK` slices into `tx`, hashing each slice
/// exactly once (in order) if `want_digest`. Returns the final digest.
fn spawn_producer(
    locator: String,
    extent_bytes: u64,
    want_digest: bool,
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
) -> std::io::Result<std::thread::JoinHandle<Result<Option<[u8; 32]>, String>>> {
    std::thread::Builder::new()
        .name("i65-source".into())
        .spawn(move || -> Result<Option<[u8; 32]>, String> {
            let mut c = Counters::default();
            let src = sources::RawReadSource::open(&locator, &mut c)
                .map_err(|e| format!("producer: open source: {e}"))?;
            let mut hasher = want_digest.then(Sha256::new);
            let mut offset = 0u64;
            while offset < extent_bytes {
                let len = READ_BLOCK.min(extent_bytes - offset);
                let buf = src
                    .read_bytes_at(offset, len, &mut c)
                    .map_err(|e| format!("producer: read at {offset}: {e}"))?;
                if buf.len() as u64 != len {
                    return Err(format!(
                        "producer: short read at {offset}: got {} want {len}",
                        buf.len()
                    ));
                }
                if let Some(h) = hasher.as_mut() {
                    h.update(&buf);
                }
                if tx.send(buf).is_err() {
                    return Err("producer: foreground consumer hung up".into());
                }
                offset += len;
            }
            Ok(hasher.map(|h| {
                let d = h.finalize();
                let mut o = [0u8; 32];
                o.copy_from_slice(&d);
                o
            }))
        })
}

/// One continuous streamed capture over a single fresh TCP connection.
fn run_capture(args: &Args, locator: &str) -> i32 {
    let addr = match args.sink.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => {
            eprintln!("i65: bad --sink address {:?}", args.sink);
            return exit::CONNECT;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(args.connect_wait_secs);
    let mut attempts = 0u64;
    let mut stream = loop {
        attempts += 1;
        match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(s) => break s,
            Err(e) => {
                if Instant::now() >= deadline {
                    eprintln!("i65: connect {} after {attempts} attempts: {e}", args.sink);
                    return exit::CONNECT;
                }
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    };
    if attempts > 1 {
        eprintln!("i65: sink reachable after {attempts} attempts");
    }
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(120)));

    let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_DEPTH);
    let producer = match spawn_producer(locator.to_string(), args.extent_bytes, args.digest, tx) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("i65: spawn producer: {e}");
            return exit::STREAM_FATAL;
        }
    };

    // One JSON preamble line, then the raw byte stream. This is the ONLY
    // framing — not a protocol.
    let header = format!(
        r#"{{"i65":"capture","label":"{}","extent_bytes":{},"digest":{}}}"#,
        esc(&args.label),
        args.extent_bytes,
        args.digest
    );
    if let Err(e) = stream
        .write_all(header.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
    {
        eprintln!("i65: write preamble: {e}");
        return exit::STREAM_FATAL;
    }

    let t0 = Instant::now();
    let mut sent = 0u64;
    while sent < args.extent_bytes {
        let buf = match rx.recv() {
            Ok(b) => b,
            Err(_) => {
                let perr = producer
                    .join()
                    .map(|r| r.err().unwrap_or_else(|| "producer ended early".into()))
                    .unwrap_or_else(|_| "producer panicked".into());
                eprintln!("i65: stream aborted: {perr}");
                return exit::STREAM_FATAL;
            }
        };
        if let Err(e) = stream.write_all(&buf) {
            eprintln!("i65: socket write at {sent}: {e}");
            let _ = producer.join();
            return exit::STREAM_FATAL;
        }
        sent += buf.len() as u64;
    }
    let _ = stream.flush();
    let client_wall = t0.elapsed();
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let digest = match producer.join() {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => {
            eprintln!("i65: producer error: {e}");
            return exit::STREAM_FATAL;
        }
        Err(_) => {
            eprintln!("i65: producer panicked");
            return exit::STREAM_FATAL;
        }
    };

    let mut sink_line = String::new();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(180)));
    let _ = stream.read_to_string(&mut sink_line);
    let sink_line = sink_line.trim().to_string();

    let secs = client_wall.as_secs_f64().max(1e-9);
    let client_mb_s = sent as f64 / 1_000_000.0 / secs;
    let client_mib_s = sent as f64 / 1_048_576.0 / secs;
    let client_digest_hex = digest.map(|d| hex(&d)).unwrap_or_default();

    println!("I65_SINK_LINE {sink_line}");
    println!(
        "I65_CLIENT_RESULT {{\"label\":\"{}\",\"bytes\":{sent},\"client_wall_ms\":{:.1},\"client_mb_s\":{:.2},\"client_mib_s\":{:.2},\"client_sha256_hex\":\"{}\",\"digest_enabled\":{}}}",
        esc(&args.label),
        client_wall.as_millis(),
        client_mb_s,
        client_mib_s,
        client_digest_hex,
        args.digest
    );

    // Basic byte-correctness: the sink echoes what it received + hashed.
    let sink_v: Option<SinkResult> = SinkResult::parse(&sink_line);
    match &sink_v {
        Some(sv) if !sv.complete || sv.bytes != sent => {
            eprintln!(
                "i65: INCOMPLETE at sink: complete={} sink_bytes={} client_bytes={sent}",
                sv.complete, sv.bytes
            );
            return exit::INCOMPLETE;
        }
        Some(sv) if args.digest && !client_digest_hex.is_empty() && !sv.sha256_hex.is_empty() => {
            if sv.sha256_hex.eq_ignore_ascii_case(&client_digest_hex) {
                println!(
                    "I65_DIGEST_MATCH label={} sha256={}",
                    args.label, client_digest_hex
                );
            } else {
                eprintln!(
                    "i65: DIGEST MISMATCH client={client_digest_hex} sink={}",
                    sv.sha256_hex
                );
                return exit::DIGEST_MISMATCH;
            }
        }
        Some(_) => {}
        None => {
            eprintln!("i65: sink returned no parseable result line: {sink_line:?}");
            return exit::INCOMPLETE;
        }
    }
    exit::PASS
}

/// The handful of sink-result fields the probe cross-checks. Tiny hand parser —
/// no serde dependency in this crate.
struct SinkResult {
    bytes: u64,
    complete: bool,
    sha256_hex: String,
}
impl SinkResult {
    fn parse(line: &str) -> Option<Self> {
        if !line.contains("\"i65_sink_result\":true") {
            return None;
        }
        let num = |key: &str| -> Option<u64> {
            let k = format!("\"{key}\":");
            let start = line.find(&k)? + k.len();
            let rest = &line[start..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            rest[..end].parse().ok()
        };
        let str_field = |key: &str| -> String {
            let k = format!("\"{key}\":\"");
            match line.find(&k) {
                Some(i) => {
                    let rest = &line[i + k.len()..];
                    rest.find('"').map(|e| rest[..e].to_string()).unwrap_or_default()
                }
                None => String::new(),
            }
        };
        Some(SinkResult {
            bytes: num("bytes").unwrap_or(0),
            complete: line.contains("\"complete\":true"),
            sha256_hex: str_field("sha256_hex"),
        })
    }
}

/// Host-only wiring check: proves the safety Accept + Reject paths and, with
/// `--loopback`, one full streamed transfer against `--sink` using the
/// deterministic non-Windows stub source. NEVER touches a real device.
fn self_check(args: &Args) -> i32 {
    eprintln!("i65: --self-check ({} {})", NAME, VERSION);
    match resolve_and_gate(args) {
        Ok(g) => eprintln!(
            "i65: self-check ACCEPT locator={} device_length_bytes={}",
            g.locator, g.device_length_bytes
        ),
        Err(code) => {
            eprintln!("i65: self-check unexpectedly failed the Accept path (exit {code})");
            return 1;
        }
    }
    // Reject path: an impossible extent must fail closed.
    let bad = Args {
        sink: args.sink.clone(),
        select_model_substr: args.select_model_substr.clone(),
        label: args.label.clone(),
        extent_bytes: 512 * 1024 * 1024 * 1024, // 512 GiB > device length
        digest: args.digest,
        self_check: true,
        loopback: false,
        connect_wait_secs: args.connect_wait_secs,
    };
    match resolve_and_gate(&bad) {
        Err(exit::SAFETY_REJECTED) => eprintln!("i65: self-check REJECT path ok (extent > device)"),
        other => {
            eprintln!("i65: self-check REJECT path did NOT fail closed: {other:?}");
            return 1;
        }
    }
    // Optional loopback stream — one real transfer against --sink.
    if args.loopback {
        match resolve_and_gate(args) {
            Ok(g) => {
                let code = run_capture(args, &g.locator);
                if code != exit::PASS {
                    eprintln!("i65: self-check loopback stream failed (exit {code})");
                    return 1;
                }
                eprintln!("i65: self-check loopback stream ok");
            }
            Err(code) => {
                eprintln!("i65: self-check loopback re-gate failed (exit {code})");
                return 1;
            }
        }
    } else {
        eprintln!("i65: self-check: --loopback not set — safety checks only, no stream");
    }
    println!("I65_PROBE_SELFCHECK_PASS");
    0
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bamep-i65-capture-probe: FATAL: {e}");
            std::process::exit(exit::BAD_ARGS);
        }
    };
    eprintln!(
        "i65: {NAME} {VERSION} git={} target={} sink={} label={} extent_bytes={} digest={}",
        env!("I65_GIT_SHORT"),
        env!("I65_TARGET_TRIPLE"),
        args.sink,
        args.label,
        args.extent_bytes,
        args.digest
    );

    if args.self_check {
        std::process::exit(self_check(&args));
    }

    let gated = match resolve_and_gate(&args) {
        Ok(g) => g,
        Err(code) => {
            println!("I65_CAPTURE_ABORTED exit={code} label={}", args.label);
            std::process::exit(code);
        }
    };
    let code = run_capture(&args, &gated.locator);
    println!("BAMEP_I65_CAPTURE_EXITCODE={code} label={}", args.label);
    std::process::exit(code);
}
