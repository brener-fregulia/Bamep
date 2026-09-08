//! Bamep Issue #65 — plain-HTTP throughput CONTROL sink (Fedora side). THROWAWAY.
//!
//! Answers ONE question with the capture path untouched: can stock WinPE + its
//! NIC/driver + the physical 1 GbE link sustain >= 110 MB/s with a minimal
//! WinHTTP POST? This sink is the server half: it drains a POST body of exactly
//! `Content-Length` bytes and DISCARDS every byte — no SATA/NVMe/HDD, no file,
//! no fsync, no hashing, no TLS, no auth, no DB.
//!
//! It deliberately reuses the capture sink's console vocabulary
//! (`I65_SINK_LISTENING` / `{"i65_sink_result":true,...}` / `I65_SINK_DONE`) so
//! the existing lab launcher's supervise + watchdog loop needs no changes.
//!
//! std-only. NOT an HTTP server: it parses only the request line, the headers,
//! `Content-Length`, `Expect: 100-continue`, and an optional `?label=` query.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

const SCRATCH: usize = 1 << 20; // 1 MiB discard buffer, reused for the whole body
const MAX_HEADER: usize = 64 * 1024;

struct Cfg {
    listen: String,
    count: u64,
    content_length: u64,
}

fn parse() -> Cfg {
    let mut c = Cfg {
        listen: "192.168.99.1:9265".into(),
        count: 3,
        content_length: 2_147_483_648,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--listen" => c.listen = it.next().unwrap_or(c.listen),
            "--count" => c.count = it.next().and_then(|v| v.parse().ok()).unwrap_or(c.count),
            "--content-length" => {
                c.content_length = it.next().and_then(|v| v.parse().ok()).unwrap_or(c.content_length)
            }
            "-h" | "--help" => {
                println!("bamep-i65-http-sink --listen IP:PORT [--count N] [--content-length N]");
                std::process::exit(0);
            }
            other => {
                eprintln!("http-sink: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    c
}

fn main() {
    let cfg = parse();
    let listener = TcpListener::bind(&cfg.listen).unwrap_or_else(|e| {
        eprintln!("http-sink: bind {}: {e}", cfg.listen);
        std::process::exit(1);
    });
    println!(
        "I65_SINK_LISTENING kind=http listen={} count={} content_length={} total_connections={}",
        cfg.listen, cfg.count, cfg.content_length, cfg.count
    );
    let _ = std::io::stdout().flush();

    let mut done = 0u64;
    let mut attempts = 0u64;
    while done < cfg.count && attempts < cfg.count + 4 {
        attempts += 1;
        let (stream, peer) = match listener.accept() {
            Ok(x) => x,
            Err(e) => {
                eprintln!("http-sink: accept: {e}");
                continue;
            }
        };
        match handle(stream, done + 1, cfg.content_length) {
            Ok(()) => done += 1,
            Err(e) => eprintln!("http-sink: conn from {peer}: {e}"),
        }
    }

    println!("I65_SINK_DONE received={done}");
    let _ = std::io::stdout().flush();
    if done < cfg.count {
        std::process::exit(20);
    }
}

fn ioerr(m: &str) -> std::io::Error {
    std::io::Error::other(m)
}

fn handle(mut s: TcpStream, conn: u64, expected: u64) -> std::io::Result<()> {
    let _ = s.set_nodelay(true);

    // ---- request line + headers (read until CRLFCRLF) ----
    let mut head: Vec<u8> = Vec::with_capacity(2048);
    let mut buf = [0u8; 8192];
    let body_start = loop {
        let n = s.read(&mut buf)?;
        if n == 0 {
            return Err(ioerr("connection closed before end of headers"));
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(p) = find_crlfcrlf(&head) {
            break p + 4;
        }
        if head.len() > MAX_HEADER {
            return Err(ioerr("request headers exceed 64 KiB"));
        }
    };
    let carried_body = head.len() - body_start; // body bytes already in the header read
    let text = String::from_utf8_lossy(&head[..body_start]).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let label = request_line
        .split_whitespace()
        .nth(1)
        .and_then(|target| target.split('?').nth(1))
        .and_then(|qs| qs.split('&').find_map(|kv| kv.strip_prefix("label=")))
        .unwrap_or("?")
        .to_string();

    let mut content_length: Option<u64> = None;
    let mut expect_continue = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().ok(),
                "expect" if v.trim().eq_ignore_ascii_case("100-continue") => expect_continue = true,
                _ => {}
            }
        }
    }
    let cl = content_length.ok_or_else(|| ioerr("POST without Content-Length"))?;
    if expect_continue {
        s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }

    // ---- drain the body, discarding every byte ----
    let t0 = Instant::now();
    let mut got: u64 = carried_body as u64;
    let mut scratch = vec![0u8; SCRATCH];
    while got < cl {
        let want = ((cl - got) as usize).min(SCRATCH);
        let n = s.read(&mut scratch[..want])?;
        if n == 0 {
            break;
        }
        got += n as u64;
    }
    let wall = t0.elapsed().as_secs_f64().max(1e-9);
    let complete = got == cl && cl == expected;
    let mb_s = got as f64 / 1_000_000.0 / wall;
    let mib_s = got as f64 / 1_048_576.0 / wall;

    let json = format!(
        r#"{{"i65_sink_result":true,"kind":"http","conn":{conn},"label":"{label}","bytes":{got},"expected_bytes":{expected},"content_length":{cl},"complete":{complete},"wall_ms_recv":{:.1},"recv_mb_s":{:.2},"recv_mib_s":{:.2}}}"#,
        wall * 1000.0,
        mb_s,
        mib_s
    );
    let body = json.as_bytes();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(resp.as_bytes())?;
    s.write_all(body)?;
    s.flush()?;

    println!("{json}");
    let _ = std::io::stdout().flush();
    Ok(())
}

fn find_crlfcrlf(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}
