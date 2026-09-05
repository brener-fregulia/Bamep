//! Minimal NDJSON evidence logger, shared by the harness subcommands. One
//! JSON object per line to stderr and (append) to an evidence file.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub enum V {
    S(String),
    I(i64),
    B(bool),
}

pub fn s(v: impl Into<String>) -> V {
    V::S(v.into())
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

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub struct Log {
    started: Instant,
    seq: Mutex<u64>,
    file: Mutex<Option<std::fs::File>>,
}

impl Log {
    pub fn new(path: &str) -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| eprintln!("harness: cannot open evidence file {path}: {e}"))
            .ok();
        Self {
            started: Instant::now(),
            seq: Mutex::new(0),
            file: Mutex::new(file),
        }
    }

    pub fn emit(&self, level: &str, event: &str, fields: &[(&str, V)]) {
        let seq = {
            let mut g = self.seq.lock().unwrap();
            *g += 1;
            *g
        };
        let mut line = String::new();
        line.push('{');
        line.push_str(&format!(r#""ts_ms":{}"#, now_millis()));
        line.push_str(&format!(r#","seq":{seq}"#));
        line.push_str(&format!(
            r#","elapsed_ms":{}"#,
            self.started.elapsed().as_millis()
        ));
        line.push_str(&format!(r#","level":"{}""#, json_escape(level)));
        line.push_str(&format!(r#","event":"{}""#, json_escape(event)));
        line.push_str(r#","component":"harness""#);
        for (k, v) in fields {
            match v {
                V::S(x) => line.push_str(&format!(r#","{}":"{}""#, json_escape(k), json_escape(x))),
                V::I(x) => line.push_str(&format!(r#","{}":{}"#, json_escape(k), x)),
                V::B(x) => line.push_str(&format!(r#","{}":{}"#, json_escape(k), x)),
            }
        }
        line.push('}');

        eprintln!("{line}");
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}
