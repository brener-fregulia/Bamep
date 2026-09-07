//! Issue #63 Stage 2 — LAB-ONLY matrix-runner mode. **NOT ARMED.**
#![allow(dead_code)] // Stage-3 helpers; exercised by unit tests in Stage 2
//!
//! Reached ONLY via the explicit `--matrix` subcommand (intercepted in `main`
//! before `parse_args`). The committed Stage-1 invocation
//! (`bamep-i63-runner --mode <physical|host-smoke> ...`) is COMPLETELY
//! unaffected and its regression suite stays green.
//!
//! In Stage 3 this mode will drive the physical matrix loop:
//!   1. ask the Fedora coordinator for the next typed case (`next_case`);
//!   2. align + re-check the WinPE UTC clock (the PROVEN Stage-1
//!      `SetSystemTime` path — NOT redesigned);
//!   3. launch the Issue-63 transfer probe with explicit typed arguments
//!      (one probe process == one transfer case);
//!   4. observe the probe exit / result;
//!   5. report `case_completed` / `case_failed` and request the next case.
//!
//! It is NOT wired into any physical boot payload. There is deliberately NO
//! code path in Stage 2 that starts the 36 physical transfers.

/// Pure: build the exact typed argv for the Issue-63 transfer probe for one
/// planned case. No I/O. Stage 3 uses this; Stage 2 only tests it.
pub struct MatrixRunnerConfig {
    pub probe_path: String,
    pub coord: String,
    pub sink: String,
    pub wss: String,
    pub pin_hex: String,
    pub credential_file: String,
    pub select_model_substr: String,
}

/// The minimal case fields the runner forwards to the probe. Parsed from the
/// coordinator's `next_case` response (a `bamep_i63_stage2_engine::matrix::Case`
/// serialised as JSON).
pub struct PlannedCase {
    pub run_id: String,
    pub case_id: String,
    pub chunk_size_bytes: u64,
    pub extent_bytes: u64,
    pub expected_chunk_count: u64,
}

pub fn parse_case(json: &str) -> Result<PlannedCase, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("case JSON: {e}"))?;
    let get_str = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(String::from)
            .ok_or_else(|| format!("case missing string field {k}"))
    };
    let get_u64 = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_u64())
            .ok_or_else(|| format!("case missing u64 field {k}"))
    };
    Ok(PlannedCase {
        run_id: get_str("run_id")?,
        case_id: get_str("case_id")?,
        chunk_size_bytes: get_u64("chunk_size_bytes")?,
        extent_bytes: get_u64("extent_bytes")?,
        expected_chunk_count: get_u64("expected_chunk_count")?,
    })
}

/// The exact probe argv for `case`. The chunk size / extent are propagated
/// verbatim so the probe's own agreement gate can cross-check them.
pub fn probe_argv(cfg: &MatrixRunnerConfig, case: &PlannedCase) -> Vec<String> {
    vec![
        cfg.probe_path.clone(),
        "--coord".into(),
        cfg.coord.clone(),
        "--sink".into(),
        cfg.sink.clone(),
        "--wss".into(),
        cfg.wss.clone(),
        "--pin".into(),
        cfg.pin_hex.clone(),
        "--auth-credential-file".into(),
        cfg.credential_file.clone(),
        "--select-model-substr".into(),
        cfg.select_model_substr.clone(),
        "--chunk-size".into(),
        case.chunk_size_bytes.to_string(),
        "--extent-bytes".into(),
        case.extent_bytes.to_string(),
        "--run-id".into(),
        case.run_id.clone(),
        "--case-id".into(),
        case.case_id.clone(),
    ]
}

/// The `--matrix` subcommand body for Stage 2: explain the not-armed status and
/// exit. It performs NO network, NO clock change, NO device access, and NO
/// transfer.
pub fn run_not_armed() -> i32 {
    println!("STAGE2_MATRIX_RUNNER_NOT_ARMED");
    println!(
        "bamep-i63-runner --matrix: the Stage-2 matrix-runner mode is present as a module \
         boundary only. The Stage-3 physical loop (next_case -> clock align -> launch probe -> \
         observe -> next) is NOT implemented and NOT armed. Stage-1 behaviour is unchanged. \
         PHYSICAL MATRIX NOT ARMED."
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASE: &str = r#"{
        "run_id":"i63s3-x","case_id":"i63s3-x/c3/s2/32mib","phase":"measured",
        "cycle":3,"slot":2,"chunk_size_bytes":33554432,"extent_bytes":2147483648,
        "expected_chunk_count":64
    }"#;

    fn cfg() -> MatrixRunnerConfig {
        MatrixRunnerConfig {
            probe_path: "X:\\bamep-i63-stage2-probe.exe".into(),
            coord: "192.168.99.1:9206".into(),
            sink: "192.168.99.1:9299".into(),
            wss: "192.168.99.1:8443".into(),
            pin_hex: "aa".repeat(32),
            credential_file: "X:\\cred.txt".into(),
            select_model_substr: "256GB".into(),
        }
    }

    #[test]
    fn parse_case_reads_the_engine_case_shape() {
        let c = parse_case(CASE).unwrap();
        assert_eq!(c.case_id, "i63s3-x/c3/s2/32mib");
        assert_eq!(c.chunk_size_bytes, 33_554_432);
        assert_eq!(c.extent_bytes, 2_147_483_648);
        assert_eq!(c.expected_chunk_count, 64);
    }

    #[test]
    fn parse_case_fails_closed_on_missing_fields() {
        assert!(parse_case(r#"{"run_id":"x"}"#).is_err());
        assert!(parse_case("not json").is_err());
    }

    #[test]
    fn probe_argv_propagates_chunk_size_and_extent_verbatim() {
        let c = parse_case(CASE).unwrap();
        let argv = probe_argv(&cfg(), &c);
        let pos = |k: &str| argv.iter().position(|a| a == k).map(|i| argv[i + 1].clone());
        assert_eq!(pos("--chunk-size").as_deref(), Some("33554432"));
        assert_eq!(pos("--extent-bytes").as_deref(), Some("2147483648"));
        assert_eq!(pos("--case-id").as_deref(), Some("i63s3-x/c3/s2/32mib"));
        assert_eq!(pos("--run-id").as_deref(), Some("i63s3-x"));
    }

    #[test]
    fn not_armed_body_returns_zero_and_does_nothing() {
        assert_eq!(run_not_armed(), 0);
    }
}
