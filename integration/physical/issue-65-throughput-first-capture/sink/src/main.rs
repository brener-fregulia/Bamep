//! Issue #65 — Fedora-side capture sink. THROWAWAY Spike plumbing.
//!
//! NOT the Bamep data plane / Worker / control plane. ONE plain-TCP listener on
//! the isolated lab link. Per connection:
//!
//!   read ONE JSON preamble line  {"i65":"capture","label":..,"extent_bytes":N}
//!     -> stream exactly N bytes into ONE fresh sequential destination file
//!        (`O_TRUNC`; a `BufWriter`, incremental SHA-256)
//!     -> ONE final `fsync` (`File::sync_all`)
//!     -> reply with ONE JSON measurement line
//!     -> drop the file (+ verify it is gone), accept the next connection.
//!
//! Measurement boundary (single clock, sink-side, authoritative for #65):
//!   t0   = first byte of the payload received
//!   t1   = last payload byte written to the page cache
//!   t2   = fsync returned
//!   mb_s / mib_s use (t2 - t0): the durable capture wall.
//!
//! Destination selector (Issue #65 per-disk comparison): pass `--dest-dir DIR`
//! one or more times; the sink runs `--count` connections against
//! `DIR/<filename>` for the FIRST dir, then the next, etc. (total = count *
//! ndirs). With no `--dest-dir` it uses the single `--dest PATH` exactly as
//! before. The only thing that varies between groups is the destination
//! filesystem.
//!
//! NO TLS, NO auth, NO DB, NO per-chunk file/fsync/commit/ACK, NO retry.

use std::io::{BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const RECV_BUF: usize = 8 * 1024 * 1024;
const DEFAULT_EXTENT: u64 = 2 * 1024 * 1024 * 1024;

struct Cfg {
    listen: String,
    dest: String,
    dest_dirs: Vec<String>,
    filename: String,
    /// Connections PER destination (per `--dest-dir`, or total when none given).
    count: u64,
    default_extent: u64,
    digest: bool,
    /// Diagnostic only (default off): drain the socket but do NOT write the
    /// destination file — isolates whether destination storage is the limiter.
    discard: bool,
}

fn parse() -> Cfg {
    let mut c = Cfg {
        listen: "192.168.99.1:9265".into(),
        dest: "/var/tmp/bamep-i65-capture.bin".into(),
        dest_dirs: Vec::new(),
        filename: "capture.bin".into(),
        count: 3,
        default_extent: DEFAULT_EXTENT,
        digest: true,
        discard: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--listen" => c.listen = it.next().unwrap_or(c.listen),
            "--dest" => c.dest = it.next().unwrap_or(c.dest),
            "--dest-dir" => {
                if let Some(d) = it.next() {
                    c.dest_dirs.push(d);
                }
            }
            "--filename" => c.filename = it.next().unwrap_or(c.filename),
            "--count" => c.count = it.next().and_then(|v| v.parse().ok()).unwrap_or(c.count),
            "--extent-bytes" => {
                c.default_extent = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(c.default_extent)
            }
            "--no-digest" => c.digest = false,
            "--digest" => c.digest = true,
            "--discard" => c.discard = true,
            "-h" | "--help" => {
                println!(
                    "bamep-i65-capture-sink --listen IP:PORT [--dest PATH | --dest-dir DIR ...] [--filename NAME] [--count N] [--extent-bytes N] [--no-digest] [--discard]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("sink: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    c
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn main() {
    let cfg = parse();

    // Build the connection schedule: one destination path per expected
    // connection, grouped by --dest-dir in the given order.
    let schedule: Vec<PathBuf> = if cfg.dest_dirs.is_empty() {
        vec![PathBuf::from(&cfg.dest); cfg.count as usize]
    } else {
        let mut v = Vec::with_capacity(cfg.dest_dirs.len() * cfg.count as usize);
        for d in &cfg.dest_dirs {
            let p = Path::new(d).join(&cfg.filename);
            for _ in 0..cfg.count {
                v.push(p.clone());
            }
        }
        v
    };

    let listener = TcpListener::bind(&cfg.listen).unwrap_or_else(|e| {
        eprintln!("sink: bind {}: {e}", cfg.listen);
        std::process::exit(1);
    });
    println!(
        "I65_SINK_LISTENING listen={} count_per_dest={} default_extent_bytes={} digest={} discard={} total_connections={} dests=[{}]",
        cfg.listen,
        cfg.count,
        cfg.default_extent,
        cfg.digest,
        cfg.discard,
        schedule.len(),
        if cfg.dest_dirs.is_empty() {
            cfg.dest.clone()
        } else {
            cfg.dest_dirs.join(", ")
        }
    );
    let _ = std::io::stdout().flush();

    let mut received = 0usize;
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let idx = received;
                let dest_path = &schedule[idx];
                handle(s, dest_path, idx + 1, schedule.len(), &cfg);
                received += 1;
                if received >= schedule.len() {
                    break;
                }
            }
            Err(e) => eprintln!("sink: accept: {e}"),
        }
    }

    // Best-effort: leave nothing behind on any destination.
    let mut seen = Vec::new();
    for p in &schedule {
        if !seen.contains(&p) {
            seen.push(p);
            let _ = std::fs::remove_file(p);
        }
    }
    println!("I65_SINK_DONE received={received}");
    let _ = std::io::stdout().flush();
}

/// Read the one-line JSON preamble byte-by-byte (bounded). Returns the raw line.
fn read_preamble(s: &mut TcpStream) -> Result<String, String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match s.read(&mut b) {
            Ok(0) => return Err("connection closed before preamble".into()),
            Ok(_) => {
                if b[0] == b'\n' {
                    break;
                }
                line.push(b[0]);
                if line.len() > 8192 {
                    return Err("preamble line too long".into());
                }
            }
            Err(e) => return Err(format!("preamble read: {e}")),
        }
    }
    String::from_utf8(line).map_err(|_| "preamble not utf-8".into())
}

fn handle(mut s: TcpStream, dest_path: &Path, idx: usize, total: usize, cfg: &Cfg) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_nodelay(true);
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let dest_str = dest_path.display().to_string();
    let dest_dir = dest_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let preamble = match read_preamble(&mut s) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sink: {peer}: {e}");
            return;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&preamble) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sink: {peer}: preamble not JSON ({e}): {preamble:?}");
            return;
        }
    };
    let label = v
        .get("label")
        .and_then(|x| x.as_str())
        .unwrap_or("unlabeled")
        .to_string();
    let extent = v
        .get("extent_bytes")
        .and_then(|x| x.as_u64())
        .unwrap_or(cfg.default_extent);
    let want_digest = cfg.digest && v.get("digest").and_then(|x| x.as_bool()).unwrap_or(true);
    println!(
        "I65_SINK_ACCEPT conn={idx}/{total} peer={peer} label={label} dest_dir={dest_dir} extent_bytes={extent} digest={want_digest}"
    );
    let _ = std::io::stdout().flush();

    let mut w: Option<BufWriter<std::fs::File>> = if cfg.discard {
        None
    } else {
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dest_path)
        {
            Ok(f) => Some(BufWriter::with_capacity(RECV_BUF, f)),
            Err(e) => {
                eprintln!("sink: {peer}: open dest {dest_str}: {e}");
                return;
            }
        }
    };
    let mut hasher = want_digest.then(Sha256::new);
    let mut buf = vec![0u8; RECV_BUF];
    let mut got = 0u64;
    let mut t0: Option<Instant> = None;
    let mut short = false;

    while got < extent {
        let want = ((extent - got) as usize).min(buf.len());
        match s.read(&mut buf[..want]) {
            Ok(0) => {
                short = true;
                break;
            }
            Ok(n) => {
                if t0.is_none() {
                    t0 = Some(Instant::now());
                }
                if let Some(w) = w.as_mut() {
                    if let Err(e) = w.write_all(&buf[..n]) {
                        eprintln!("sink: {peer}: file write at {got}: {e}");
                        return;
                    }
                }
                if let Some(h) = hasher.as_mut() {
                    h.update(&buf[..n]);
                }
                got += n as u64;
            }
            Err(e) => {
                eprintln!("sink: {peer}: socket read at {got}: {e}");
                short = true;
                break;
            }
        }
    }
    let recv_done = Instant::now();

    if let Some(mut w) = w.take() {
        if let Err(e) = w.flush() {
            eprintln!("sink: {peer}: flush: {e}");
            return;
        }
        match w.into_inner() {
            Ok(file) => {
                if let Err(e) = file.sync_all() {
                    eprintln!("sink: {peer}: fsync: {e}");
                    return;
                }
            }
            Err(e) => {
                eprintln!("sink: {peer}: into_inner: {e}");
                return;
            }
        }
    }
    let fsync_done = Instant::now();

    let t0 = t0.unwrap_or(recv_done);
    let wall_recv = recv_done.duration_since(t0).as_secs_f64().max(1e-9);
    let wall_fsync = fsync_done.duration_since(t0).as_secs_f64().max(1e-9);
    let fsync_ms = fsync_done.duration_since(recv_done).as_secs_f64() * 1000.0;
    let complete = !short && got == extent;
    let digest_hex = hasher.map(|h| hex(&h.finalize())).unwrap_or_default();

    let line = format!(
        r#"{{"i65_sink_result":true,"conn":{idx},"label":"{label}","peer":"{peer}","dest_dir":"{dest_dir}","discard":{},"bytes":{got},"expected_bytes":{extent},"complete":{complete},"wall_ms_recv":{:.1},"wall_ms_to_fsync":{:.1},"fsync_ms":{:.1},"mb_s":{:.2},"mib_s":{:.2},"recv_mb_s":{:.2},"recv_mib_s":{:.2},"sha256_hex":"{digest_hex}"}}"#,
        cfg.discard,
        wall_recv * 1000.0,
        wall_fsync * 1000.0,
        fsync_ms,
        got as f64 / 1_000_000.0 / wall_fsync,
        got as f64 / 1_048_576.0 / wall_fsync,
        got as f64 / 1_000_000.0 / wall_recv,
        got as f64 / 1_048_576.0 / wall_recv,
    );
    let _ = s.write_all(line.as_bytes());
    let _ = s.write_all(b"\n");
    let _ = s.flush();
    println!("{line}");

    // Per-transfer cleanup: drop the throwaway capture file and VERIFY it is gone.
    if !cfg.discard {
        let rm = std::fs::remove_file(dest_path);
        let gone = !dest_path.exists();
        println!(
            "I65_SINK_CLEANUP conn={idx} dest={dest_str} remove_ok={} gone={gone}",
            rm.is_ok()
        );
        if !gone {
            eprintln!("sink: WARNING conn={idx}: {dest_str} still present after remove");
        }
    }
    let _ = std::io::stdout().flush();
}
