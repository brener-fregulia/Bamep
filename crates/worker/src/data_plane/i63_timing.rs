//! ISSUE #63 STAGE 4 — throwaway lab-only PUT-boundary timing hook.
//!
//! DISABLED unless the environment variable `BAMEP_I63_WORKER_PUT_TIMING` names
//! a file path. When it is unset:
//!
//! * [`PutTimer::start`] returns an inert timer;
//! * every `PutTimer` method is a branch-predicted no-op — no `Instant` is
//!   read, nothing is allocated, nothing is written;
//! * the staging composition is asked for no sub-timings (`i63_active == false`).
//!
//! There is therefore **no observable Stage-4 behaviour** when the variable is
//! absent, and in particular no change to authorization, durability, integrity,
//! HTTP status, or the PUT outcome on any path — with or without the variable.
//!
//! When the variable IS set, one best-effort NDJSON record is appended per
//! chunk PUT, keyed by `transfer_id` + `chunk_index`. The sink file is opened
//! `append` and written with a single `writeln!`; it is **never** `fsync`'d or
//! explicitly flushed, and any open/write error is swallowed. No credential,
//! proof, capability, digest, or other request-secret value is recorded.
//!
//! `body_pump_ns` and `staging_worker_ns` **overlap by design** (the async body
//! pump feeds the blocking staging worker concurrently); they are emitted
//! inside an `"overlapping"` object and MUST NOT be summed as exclusive phases.
//! The `begin_stage_ns` / `write_sum_ns` / `digest_ns` / `finalize_ns`
//! sub-intervals are sequential *within* the staging worker (and therefore also
//! nested inside `staging_worker_ns`). `write_sum_ns` folds the incremental
//! Worker SHA-256 into the write cost — D1 hashes as it writes and separating
//! them is not a cheap instrumentation point.
//!
//! To retire the Spike: delete this file, the `mod i63_timing;` line, and the
//! `i63` / `i63_active` call sites in `http.rs` and `upload.rs`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use uuid::Uuid;

const ENV_VAR: &str = "BAMEP_I63_WORKER_PUT_TIMING";

/// The configured sink path, or `None` if timing is inactive. Read once.
fn sink_path() -> Option<&'static PathBuf> {
    static SINK: OnceLock<Option<PathBuf>> = OnceLock::new();
    SINK.get_or_init(|| {
        std::env::var_os(ENV_VAR)
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    })
    .as_ref()
}

/// Sub-timings the staging composition fills in (nanoseconds). All zero when
/// timing is inactive. `body_pump_ns` and `staging_worker_ns` overlap.
#[derive(Default, Clone, Copy)]
pub(super) struct StageTiming {
    /// OVERLAPS `staging_worker_ns` — the async request-body pump loop wall.
    pub body_pump_ns: u128,
    /// OVERLAPS `body_pump_ns` — the `spawn_blocking` staging worker wall.
    pub staging_worker_ns: u128,
    /// `ChunkStore::begin_stage` (create + open the staging file).
    pub begin_stage_ns: u128,
    /// Sum of `StagingChunk::write` calls (write + incremental SHA-256).
    pub write_sum_ns: u128,
    /// `StagingChunk::digest()` finalisation for the pre-finalize identity test.
    pub digest_ns: u128,
    /// `StagingChunk::finalize()` — flush + fsync(file) + no-replace placement
    /// + fsync(dir). The durability boundary.
    pub finalize_ns: u128,
}

/// A stopwatch for one chunk PUT handler invocation. Inert unless the sink env
/// variable is configured.
pub(super) struct PutTimer(Option<Active>);

struct Active {
    start: Instant,
    transfer_id: Uuid,
    chunk_index: u64,
    authorize_ns: u128,
    stage_call_ns: u128,
    commit_chunk_ns: u128,
    stage: StageTiming,
}

impl PutTimer {
    pub(super) fn start(transfer_id: Uuid, chunk_index: u64) -> Self {
        if sink_path().is_none() {
            return PutTimer(None);
        }
        PutTimer(Some(Active {
            start: Instant::now(),
            transfer_id,
            chunk_index,
            authorize_ns: 0,
            stage_call_ns: 0,
            commit_chunk_ns: 0,
            stage: StageTiming::default(),
        }))
    }

    /// Whether timing is active — the staging composition skips its own
    /// `Instant`s when this is `false`.
    pub(super) fn active(&self) -> bool {
        self.0.is_some()
    }

    pub(super) fn set_authorize_ns(&mut self, ns: u128) {
        if let Some(a) = &mut self.0 {
            a.authorize_ns = ns;
        }
    }

    pub(super) fn set_stage(&mut self, stage_call_ns: u128, stage: StageTiming) {
        if let Some(a) = &mut self.0 {
            a.stage_call_ns = stage_call_ns;
            a.stage = stage;
        }
    }

    pub(super) fn set_commit_ns(&mut self, ns: u128) {
        if let Some(a) = &mut self.0 {
            a.commit_chunk_ns = ns;
        }
    }

    /// Append the record (best effort; consumes the timer). `outcome` is one of
    /// a fixed set of ASCII slugs, never free text.
    pub(super) fn finish(self, outcome: &'static str, verified_size: Option<u32>) {
        let Some(a) = self.0 else {
            return;
        };
        let Some(path) = sink_path() else {
            return;
        };
        let handler_total_ns = a.start.elapsed().as_nanos();
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = format!(
            concat!(
                r#"{{"ts_ms":{ts},"transfer_id":"{tid}","chunk_index":{ci},"#,
                r#""outcome":"{oc}","verified_size":{vs},"#,
                r#""handler_total_ns":{ht},"authorize_ns":{az},"stage_call_ns":{sc},"#,
                r#""commit_chunk_ns":{cc},"#,
                r#""overlapping":{{"body_pump_ns":{bp},"staging_worker_ns":{sw}}},"#,
                r#""begin_stage_ns":{bs},"write_sum_ns":{ws},"digest_ns":{dg},"#,
                r#""finalize_ns":{fz}}}"#,
            ),
            ts = ts_ms,
            tid = a.transfer_id.hyphenated(),
            ci = a.chunk_index,
            oc = outcome,
            vs = verified_size.map(i64::from).unwrap_or(-1),
            ht = handler_total_ns,
            az = a.authorize_ns,
            sc = a.stage_call_ns,
            cc = a.commit_chunk_ns,
            bp = a.stage.body_pump_ns,
            sw = a.stage.staging_worker_ns,
            bs = a.stage.begin_stage_ns,
            ws = a.stage.write_sum_ns,
            dg = a.stage.digest_ns,
            fz = a.stage.finalize_ns,
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}
