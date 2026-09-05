//! Issue #60 Physical Integration Spike — lab evidence sink.
//!
//! THROWAWAY Spike artifact. NOT the Bamep Agent control plane: no TLS, no
//! authentication, no Agent Protocol. It exists only so the WinPE probe can
//! ship machine-readable NDJSON evidence to the Fedora lab Server over the
//! isolated #53 provisioning link, instead of the owner transcribing console
//! output.
//!
//! Each inbound TCP connection is read to EOF, wrapped with a receive
//! envelope, appended to the evidence file, and echoed to stdout. The client
//! gets a short "OK <bytes>\n" ack.
//!
//! Usage: bamep-probe-sink [BIND_ADDR] [EVIDENCE_FILE]
//!   BIND_ADDR      default 0.0.0.0:9099
//!   EVIDENCE_FILE  default ./probe-events.ndjson

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn json_escape(input: &str) -> String {
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

fn handle(mut stream: TcpStream, file: &Arc<Mutex<std::fs::File>>) -> std::io::Result<()> {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;

    let mut payload = Vec::new();
    stream.read_to_end(&mut payload)?;
    let body = String::from_utf8_lossy(&payload);

    let envelope = format!(
        r#"{{"sink_recv_ms":{},"peer":"{}","payload_bytes":{}}}"#,
        now_millis(),
        json_escape(&peer),
        payload.len()
    );

    {
        let mut f = file.lock().unwrap();
        writeln!(f, "{envelope}")?;
        for line in body.lines() {
            if !line.trim().is_empty() {
                writeln!(f, "{line}")?;
            }
        }
        writeln!(f, r#"{{"sink_recv_end":true,"peer":"{}"}}"#, json_escape(&peer))?;
        f.flush()?;
    }

    println!("--- {peer}  ({} bytes)  {} ---", payload.len(), envelope);
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
    let _ = std::io::stdout().flush();

    let _ = stream.write_all(format!("OK {}\n", payload.len()).as_bytes());
    let _ = stream.flush();
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let bind = args.next().unwrap_or_else(|| "0.0.0.0:9099".to_string());
    let evidence_path = args
        .next()
        .unwrap_or_else(|| "probe-events.ndjson".to_string());

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&evidence_path)
        .unwrap_or_else(|e| {
            eprintln!("bamep-probe-sink: cannot open {evidence_path}: {e}");
            std::process::exit(1);
        });
    let file = Arc::new(Mutex::new(file));

    let listener = TcpListener::bind(&bind).unwrap_or_else(|e| {
        eprintln!("bamep-probe-sink: cannot bind {bind}: {e}");
        std::process::exit(1);
    });

    eprintln!(
        "bamep-probe-sink: listening on {bind}, appending to {evidence_path}  (pid {})",
        std::process::id()
    );

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let file = Arc::clone(&file);
                std::thread::spawn(move || {
                    if let Err(e) = handle(stream, &file) {
                        eprintln!("bamep-probe-sink: connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("bamep-probe-sink: accept error: {e}"),
        }
    }
}
