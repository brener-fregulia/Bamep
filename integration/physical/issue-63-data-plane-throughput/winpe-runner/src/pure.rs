//! Pure, host-testable Stage-1 helpers. NO `windows-sys` here, so `cargo test`
//! builds and runs on the Linux dev host.

/// Epoch millis between 1601-01-01 (Win32 FILETIME epoch) and 1970-01-01.
/// (`#[allow(dead_code)]`: the FILETIME helpers are only reached from the
/// `#[cfg(windows)]` clock backend and the tests, not from a plain host build.)
#[allow(dead_code)]
const FILETIME_UNIX_EPOCH_TICKS: i128 = 116_444_736_000_000_000;

/// Parse `server_utc_ms` out of a coordinator ACK line:
/// `{"stage1_coord_ack":true,"server_utc_ms":<i64>}`.
pub fn parse_server_utc_ms(line: &str) -> Result<i64, String> {
    let v: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("coord ACK not JSON: {e}"))?;
    if v.get("stage1_coord_ack").and_then(|x| x.as_bool()) != Some(true) {
        return Err(format!("coord ACK missing stage1_coord_ack: {line}"));
    }
    v.get("server_utc_ms")
        .and_then(|x| x.as_i64())
        .filter(|ms| *ms > 0)
        .ok_or_else(|| format!("coord ACK missing/!positive server_utc_ms: {line}"))
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SkewVerdict {
    InBound,
    /// Agent clock is behind the Server by more than the allowed floor.
    TooEarly,
    /// Agent clock is ahead of the Server by more than the allowed ceiling.
    TooLate,
}

/// `skew_ms = agent_now_ms - server_utc_ms`. The window is `[floor_ms, ceil_ms]`
/// (floor is normally negative, ceil normally small-positive).
pub fn classify_skew(skew_ms: i64, floor_ms: i64, ceil_ms: i64) -> SkewVerdict {
    if skew_ms < floor_ms {
        SkewVerdict::TooEarly
    } else if skew_ms > ceil_ms {
        SkewVerdict::TooLate
    } else {
        SkewVerdict::InBound
    }
}

/// Unix epoch millis -> Win32 FILETIME 100 ns ticks since 1601-01-01 UTC.
#[allow(dead_code)]
pub fn unix_ms_to_filetime_ticks(unix_ms: i64) -> u64 {
    ((unix_ms as i128) * 10_000 + FILETIME_UNIX_EPOCH_TICKS) as u64
}

/// Win32 FILETIME 100 ns ticks -> Unix epoch millis.
#[allow(dead_code)]
pub fn filetime_ticks_to_unix_ms(ticks: u64) -> i64 {
    ((ticks as i128 - FILETIME_UNIX_EPOCH_TICKS) / 10_000) as i64
}

/// ISO-8601 `YYYY-MM-DDTHH:MM:SS.mmmZ` from a `SYSTEMTIME`-style tuple
/// `(year, month, day, hour, minute, second, millis)`. Evidence formatting only.
pub fn iso8601_utc(y: u16, mo: u16, d: u16, h: u16, mi: u16, s: u16, ms: u16) -> String {
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

// ---------------------------------------------------------------------------
// bootstrap evidence classification (owner correction 2)
//
// The two boot milestones (`winpe.booted`, `winpe.wpeinit_complete`) MUST be
// OBSERVED: written by the injected bootstrap `.cmd` to `X:\bamep-i63-events.ndjson`
// before/after `wpeinit` and forwarded VERBATIM by the runner. The runner must
// NEVER synthesise them. This classifier is the fail-closed gate.
// ---------------------------------------------------------------------------

/// One milestone line recovered from the bootstrap's on-disk evidence file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapMilestone {
    pub event: String,
    /// The local wall-clock string the bootstrap recorded (`%DATE% %TIME%`),
    /// pre clock-alignment — kept only for order/provenance evidence.
    pub local_ts: Option<String>,
    /// The exact JSON line as written by the bootstrap.
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapEvidence {
    /// Both required milestones present, `winpe.booted` strictly before
    /// `winpe.wpeinit_complete`. The vec is in file order.
    Ok(Vec<BootstrapMilestone>),
    /// Fail closed: file absent/empty, a required milestone missing, wrong order,
    /// or a required milestone line not parseable as the expected JSON object.
    Missing { reason: String },
}

fn milestone_from_line(line: &str) -> Option<BootstrapMilestone> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let event = v.get("event")?.as_str()?.to_string();
    if event != "winpe.booted" && event != "winpe.wpeinit_complete" {
        return None;
    }
    Some(BootstrapMilestone {
        event,
        local_ts: v
            .get("bootstrap_local_ts")
            .and_then(|x| x.as_str())
            .map(String::from),
        raw: line.trim().to_string(),
    })
}

/// Classify the contents of `X:\bamep-i63-events.ndjson`.
pub fn classify_bootstrap_evidence(file_contents: &str) -> BootstrapEvidence {
    let mut found: Vec<BootstrapMilestone> = Vec::new();
    for line in file_contents.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // A line that *looks* like one of our milestone events but does not parse
        // is corruption, not something to skip past.
        if (line.contains("\"winpe.booted\"") || line.contains("\"winpe.wpeinit_complete\""))
            && milestone_from_line(line).is_none()
        {
            return BootstrapEvidence::Missing {
                reason: format!("unparseable bootstrap milestone line: {line}"),
            };
        }
        if let Some(m) = milestone_from_line(line) {
            found.push(m);
        }
    }
    let booted_at = found.iter().position(|m| m.event == "winpe.booted");
    let wpeinit_at = found
        .iter()
        .position(|m| m.event == "winpe.wpeinit_complete");
    match (booted_at, wpeinit_at) {
        (None, _) => BootstrapEvidence::Missing {
            reason: "winpe.booted not present in bootstrap evidence".into(),
        },
        (_, None) => BootstrapEvidence::Missing {
            reason: "winpe.wpeinit_complete not present in bootstrap evidence".into(),
        },
        (Some(b), Some(w)) if b >= w => BootstrapEvidence::Missing {
            reason: "winpe.wpeinit_complete recorded before winpe.booted".into(),
        },
        _ => BootstrapEvidence::Ok(found),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_utc_ok() {
        let l = r#"{"stage1_coord_ack":true,"server_utc_ms":1757260800123}"#;
        assert_eq!(parse_server_utc_ms(l).unwrap(), 1_757_260_800_123);
    }

    #[test]
    fn parse_server_utc_rejects_missing_ack_flag() {
        assert!(parse_server_utc_ms(r#"{"server_utc_ms":1}"#).is_err());
    }

    #[test]
    fn parse_server_utc_rejects_non_json_and_nonpositive() {
        assert!(parse_server_utc_ms("not json").is_err());
        assert!(parse_server_utc_ms(r#"{"stage1_coord_ack":true,"server_utc_ms":0}"#).is_err());
        assert!(parse_server_utc_ms(r#"{"stage1_coord_ack":true,"server_utc_ms":-5}"#).is_err());
    }

    #[test]
    fn classify_skew_window() {
        assert_eq!(classify_skew(0, -2000, 2000), SkewVerdict::InBound);
        assert_eq!(classify_skew(-2000, -2000, 2000), SkewVerdict::InBound);
        assert_eq!(classify_skew(2000, -2000, 2000), SkewVerdict::InBound);
        assert_eq!(classify_skew(-2001, -2000, 2000), SkewVerdict::TooEarly);
        assert_eq!(classify_skew(2001, -2000, 2000), SkewVerdict::TooLate);
    }

    #[test]
    fn filetime_round_trips() {
        // 2021-01-01T00:00:00Z == 1609459200000 ms.
        // ticks = 1609459200000*10000 + 116444736000000000 = 132539328000000000
        let ms = 1_609_459_200_000i64;
        let ticks = unix_ms_to_filetime_ticks(ms);
        assert_eq!(ticks, 132_539_328_000_000_000);
        assert_eq!(filetime_ticks_to_unix_ms(ticks), ms);
    }

    #[test]
    fn filetime_round_trips_across_a_range() {
        for ms in [1i64, 1_000, 1_757_000_000_000, 4_102_444_800_000] {
            assert_eq!(filetime_ticks_to_unix_ms(unix_ms_to_filetime_ticks(ms)), ms);
        }
    }

    #[test]
    fn iso8601_pads() {
        assert_eq!(iso8601_utc(2026, 9, 7, 3, 4, 5, 9), "2026-09-07T03:04:05.009Z");
    }

    // ---- bootstrap evidence: observed vs missing (owner correction 2) ------

    const BOOTED: &str =
        r#"{"event":"winpe.booted","origin":"bootstrap","run_id":"r","bootstrap_local_ts":"09/07/2026 15:04:05.00"}"#;
    const WPEINIT: &str =
        r#"{"event":"winpe.wpeinit_complete","origin":"bootstrap","run_id":"r","bootstrap_local_ts":"09/07/2026 15:04:41.12"}"#;

    #[test]
    fn bootstrap_evidence_ok_when_both_present_in_order() {
        let c = format!("{BOOTED}\n{WPEINIT}\n");
        match classify_bootstrap_evidence(&c) {
            BootstrapEvidence::Ok(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0].event, "winpe.booted");
                assert_eq!(v[1].event, "winpe.wpeinit_complete");
                assert_eq!(v[0].local_ts.as_deref(), Some("09/07/2026 15:04:05.00"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn bootstrap_evidence_missing_when_file_empty() {
        assert!(matches!(
            classify_bootstrap_evidence(""),
            BootstrapEvidence::Missing { .. }
        ));
        assert!(matches!(
            classify_bootstrap_evidence("   \n\n"),
            BootstrapEvidence::Missing { .. }
        ));
    }

    #[test]
    fn bootstrap_evidence_missing_when_booted_absent() {
        assert!(matches!(
            classify_bootstrap_evidence(WPEINIT),
            BootstrapEvidence::Missing { .. }
        ));
    }

    #[test]
    fn bootstrap_evidence_missing_when_wpeinit_absent() {
        assert!(matches!(
            classify_bootstrap_evidence(BOOTED),
            BootstrapEvidence::Missing { .. }
        ));
    }

    #[test]
    fn bootstrap_evidence_missing_when_out_of_order() {
        let c = format!("{WPEINIT}\n{BOOTED}\n");
        assert!(matches!(
            classify_bootstrap_evidence(&c),
            BootstrapEvidence::Missing { .. }
        ));
    }

    #[test]
    fn bootstrap_evidence_missing_when_milestone_line_is_corrupt() {
        let c = format!("{BOOTED}\n{{\"event\":\"winpe.wpeinit_complete\" TRUNCATED\n");
        assert!(matches!(
            classify_bootstrap_evidence(&c),
            BootstrapEvidence::Missing { .. }
        ));
    }

    #[test]
    fn bootstrap_evidence_tolerates_unrelated_lines() {
        let c = format!("{{\"event\":\"winpe.some.debug\"}}\n{BOOTED}\nnot json at all\n{WPEINIT}\n");
        assert!(matches!(
            classify_bootstrap_evidence(&c),
            BootstrapEvidence::Ok(_)
        ));
    }

    #[test]
    fn bootstrap_evidence_accepts_the_exact_generated_cmd_echo_format() {
        // Exactly what `bamep-i63-bootstrap.cmd` emits once WinPE cmd.exe expands
        // %DATE% / %TIME%, including CRLF line endings and a comma-decimal %TIME%
        // (pt-BR locale). No renaming: the events are the canonical
        // `winpe.booted` / `winpe.wpeinit_complete`.
        let generated = concat!(
            "{\"event\":\"winpe.booted\",\"origin\":\"bootstrap\",\"run_id\":\"stage1-20260907T150102\",\"bootstrap_local_ts\":\"07/09/2026 15:01:02,49\"}\r\n",
            "{\"event\":\"winpe.wpeinit_complete\",\"origin\":\"bootstrap\",\"run_id\":\"stage1-20260907T150102\",\"bootstrap_local_ts\":\"07/09/2026 15:01:41,08\"}\r\n",
        );
        match classify_bootstrap_evidence(generated) {
            BootstrapEvidence::Ok(v) => {
                assert_eq!(
                    v.iter().map(|m| m.event.as_str()).collect::<Vec<_>>(),
                    vec!["winpe.booted", "winpe.wpeinit_complete"]
                );
                assert_eq!(v[0].local_ts.as_deref(), Some("07/09/2026 15:01:02,49"));
                // `raw` preserves the source line (minus the CRLF), so the runner
                // forwards it unchanged as the `raw` field.
                assert!(v[1].raw.starts_with(r#"{"event":"winpe.wpeinit_complete""#));
                assert!(!v[1].raw.ends_with('\r'));
            }
            other => panic!("generated bootstrap format rejected: {other:?}"),
        }
    }

    #[test]
    fn bootstrap_evidence_rejects_a_near_miss_event_name() {
        // e.g. `winpe.boot` / `winpe.wpeinit` — not the canonical names, so they
        // are ignored as unrelated lines and the required milestones are absent.
        let c = "{\"event\":\"winpe.boot\",\"origin\":\"bootstrap\"}\n{\"event\":\"winpe.wpeinit\",\"origin\":\"bootstrap\"}\n";
        assert!(matches!(
            classify_bootstrap_evidence(c),
            BootstrapEvidence::Missing { .. }
        ));
    }
}
