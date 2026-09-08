//! Issue #63 Stage 2 — the single-pass streaming chunk loop.
//!
//! PROVENANCE: adapted from
//! `integration/physical/issue-61-endpoint-capture-data-plane/probe7/src/stream.rs`
//! (Issue #61 CP7A, closed). The single-pass INVARIANT and its host tests are
//! preserved verbatim in spirit; the CP7A **Gate-4 deterministic
//! fault-injection checkpoint** is REMOVED — the Issue-63 clean fast path does
//! ZERO deliberate fault injection. The honest transport-wobble handling
//! (transient retry / auth-denied suspension / uncertain-PUT resume
//! reconciliation) is kept: in the Stage-3 clean matrix any such event marks
//! the case CONTAMINATED, it is never hidden.
//!
//! INVARIANT:
//!   * each logical source chunk is read AT MOST ONCE during the pass;
//!   * each logical source chunk enters the rolling full-Artifact SHA-256
//!     EXACTLY ONCE, in ascending index order;
//!   * a transport/auth retry reuses the SAME buffered bytes and digest — no
//!     re-read, no re-hash, no source-offset advance, no durable-progress
//!     increment until `held` is confirmed;
//!   * resume discovery reporting the current chunk already held with the
//!     expected digest completes it WITHOUT a second PUT; a different held
//!     digest fails closed.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Aggregate per-stage nanosecond counters accumulated across the single pass.
/// Issue #63 Stage 3 needs `read_ms` / `chunk_sha_ms` / `rolling_sha_ms`
/// preserved in the `CaseResult` (the Stage-2 schema already carries the
/// fields; only the fill was missing on the probe path). NOT high-frequency
/// telemetry — three `u128` adds per chunk.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassTimings {
    pub read_ns: u128,
    pub chunk_sha_ns: u128,
    pub rolling_sha_ns: u128,
}

const MAX_LOCAL_PUT_RETRIES: u32 = 3;
const LOCAL_BACKOFF: Duration = Duration::from_millis(50);

/// The physical (or fake) source. `read_chunk` returns exactly `len` bytes for
/// `[offset, offset + len)` and MUST be deterministic for a given `(offset, len)`.
pub trait ChunkReader {
    fn read_chunk(&self, index: u64, offset: u64, len: u64) -> Result<Vec<u8>, String>;
}

/// The Worker-owned HTTPS data plane, reduced to the two operations the pass
/// needs.
#[allow(async_fn_in_trait)]
pub trait DataPlane {
    async fn discover_resume(&mut self) -> ResumeStatus;
    async fn put_chunk(&mut self, index: u64, digest_wire: &str, bytes: &[u8]) -> PutStatus;
}

#[allow(dead_code)]
pub enum ResumeStatus {
    /// `(chunk_index, digest_wire)` for every durably held + verified chunk.
    Ok(Vec<(u64, String)>),
    AuthDenied,
    Transient(String),
    Fatal(String),
}

#[allow(dead_code)]
pub enum PutStatus {
    Accepted,
    AlreadyHeld,
    DigestMismatch,
    IdentityConflict,
    NotContinuable,
    AuthDenied,
    Transient(String),
    Fatal(String),
}

#[derive(Debug)]
pub enum StreamError {
    /// A recorded chunk identity could not be reproduced, or the Worker's
    /// independent hash rejected the bytes — `CHUNK_VERIFICATION_FAILED`.
    ChunkVerificationFailed { index: u64 },
    Fatal(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PassOutcome {
    Complete,
    SuspendedNeedsAuthorization,
    SuspendedDataPlaneUnreachable,
}

#[derive(Debug, Clone, Copy)]
pub struct ProgressTick {
    pub held_bytes: u64,
    pub held_chunks: u64,
}

/// Spike-only lifecycle observability. In the Stage-3 clean matrix ANY of the
/// non-`Resume*Begin/Result` variants is a CONTAMINATION signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    ResumeBegin,
    ResumeResult { outcome: &'static str, held_chunks: u64, sealed: bool },
    ResumeReconciled {
        held_count: u64,
        pending_chunk_index: Option<u64>,
        pending_already_held: bool,
    },
    /// CONTAMINATION: an unexpected 401 on a chunk PUT.
    PutAuthDenied { chunk_index: u64 },
    /// CONTAMINATION: a transport-level PUT failure.
    PutTransient { chunk_index: u64, local_attempt: u32, detail: String },
    /// CONTAMINATION: a transport-level resume failure.
    ResumeTransient { local_attempt: u32, detail: String },
}

#[derive(Clone, Debug)]
struct ChunkFacts {
    digest_wire: String,
    size: u64,
}

/// The in-memory pass state — the durable-across-suspension record.
pub struct StreamState {
    rolling: Sha256,
    hashed_through: u64,
    processed: BTreeMap<u64, ChunkFacts>,
    held: BTreeSet<u64>,
    pending: Option<(u64, Vec<u8>)>,
    total_len: u64,
    chunk_size: u64,
    chunk_count: u64,
    timings: PassTimings,
    // ---- Issue #63 window_8 candidate bookkeeping. All zero/empty unless
    // `run_stream_pass_window8` ran. ----
    /// The bounded PUT window (8) — `0` for serial / prep-ahead passes.
    put_window: u64,
    put_started_count: u64,
    put_completed_count: u64,
    /// Max simultaneously-unacknowledged PUT count (upper bound on true
    /// network concurrency; structurally `<= PUT_WINDOW`).
    peak_puts_in_flight: u64,
    /// The exact chunk-index order PUTs were STARTED in (must be ascending).
    put_start_order: Vec<u64>,
    /// The exact chunk-index order PUT outcomes were observed in (MAY differ
    /// from the start order — completions are unordered by design).
    put_completion_order: Vec<u64>,
    // ---- Issue #63 Stage 4 prep-ahead (depth-2) bookkeeping. Untouched by the
    // serial `run_stream_pass`. ----
    /// The largest LIVE bulk-buffer depth actually observed during the
    /// prep-ahead pass: `1` for the `current` PUT buffer, `+1` while a
    /// `Prepare` request is outstanding (the producer is reading, or the
    /// prepared chunk is sitting in the `sync_channel(1)`). Incremented when a
    /// `Prepare` is sent, decremented when its `PreparedChunk` is consumed —
    /// NOT a constant. MUST NEVER reach 3.
    prepared_peak: u64,
    /// The rolling full-Artifact digest as finalised by the prep-ahead producer
    /// thread (which owns the hasher for the whole pass). `None` in serial mode.
    finalized_digest: Option<String>,
    /// The exact ascending list of chunk indices the prep-ahead producer read,
    /// each expected exactly once. `[]` in serial mode.
    producer_read_log: Vec<u64>,
}

impl StreamState {
    pub fn new(total_len: u64, chunk_size: u64) -> Result<Self, String> {
        if total_len == 0 {
            return Err("total_len must be >= 1".into());
        }
        if chunk_size == 0 {
            return Err("chunk_size must be >= 1".into());
        }
        let chunk_count = total_len.div_ceil(chunk_size);
        if u32::try_from(chunk_count).is_err() {
            return Err(format!(
                "chunk_count {chunk_count} exceeds the manifest 32-bit index space"
            ));
        }
        Ok(Self {
            rolling: Sha256::new(),
            hashed_through: 0,
            processed: BTreeMap::new(),
            held: BTreeSet::new(),
            pending: None,
            total_len,
            chunk_size,
            chunk_count,
            timings: PassTimings::default(),
            put_window: 0,
            put_started_count: 0,
            put_completed_count: 0,
            peak_puts_in_flight: 0,
            put_start_order: Vec::new(),
            put_completion_order: Vec::new(),
            prepared_peak: 0,
            finalized_digest: None,
            producer_read_log: Vec::new(),
        })
    }

    // ---- window_8 observability (0/empty unless the window pass ran) ----
    pub fn put_window(&self) -> u64 {
        self.put_window
    }
    pub fn put_started_count(&self) -> u64 {
        self.put_started_count
    }
    pub fn put_completed_count(&self) -> u64 {
        self.put_completed_count
    }
    pub fn peak_puts_in_flight(&self) -> u64 {
        self.peak_puts_in_flight
    }
    pub fn put_start_order(&self) -> &[u64] {
        &self.put_start_order
    }
    pub fn put_completion_order(&self) -> &[u64] {
        &self.put_completion_order
    }
    /// `true` iff every PUT was started in strictly ascending chunk-index
    /// order (trivially `false` before the window pass ran).
    pub fn put_starts_ascending(&self) -> bool {
        !self.put_start_order.is_empty()
            && self.put_start_order.windows(2).all(|w| w[0] < w[1])
    }

    pub fn chunk_count(&self) -> u64 {
        self.chunk_count
    }

    /// The largest live bulk-buffer depth observed during the prep-ahead pass
    /// (0 in serial mode; `2` for any multi-chunk prep-ahead pass; MUST NEVER
    /// be `> 2`).
    pub fn prepared_peak(&self) -> u64 {
        self.prepared_peak
    }

    /// The prep-ahead producer's read log (ascending, each chunk once). Empty in
    /// serial mode.
    pub fn producer_read_log(&self) -> &[u64] {
        &self.producer_read_log
    }

    /// Aggregate per-stage timings accumulated over the pass.
    pub fn timings(&self) -> PassTimings {
        self.timings
    }
    pub fn held_count(&self) -> u64 {
        self.held.len() as u64
    }
    pub fn pending_index(&self) -> Option<u64> {
        self.pending.as_ref().map(|(i, _)| *i)
    }
    pub fn is_held(&self, index: u64) -> bool {
        self.held.contains(&index)
    }
    pub fn all_uploaded(&self) -> bool {
        self.held.len() as u64 == self.chunk_count
    }

    /// The exact byte length of chunk `index`: `chunk_size` for all but the last,
    /// `1..=chunk_size` for the last.
    pub fn expected_len(&self, index: u64) -> u64 {
        if index + 1 < self.chunk_count {
            self.chunk_size
        } else {
            self.total_len - (self.chunk_count - 1) * self.chunk_size
        }
    }

    fn durably_held_bytes(&self) -> u64 {
        self.held.iter().map(|i| self.expected_len(*i)).sum()
    }

    /// `SHA-256(chunk0 || chunk1 || ...)`, canonical base64url-no-pad — `Some`
    /// only once every chunk has been hashed exactly once.
    ///
    /// In prep-ahead mode the producer thread owns the hasher for the whole
    /// pass and reports the finalised value; it is returned verbatim here.
    pub fn finish_digest(&self) -> Option<String> {
        if let Some(d) = &self.finalized_digest {
            return Some(d.clone());
        }
        if self.hashed_through != self.chunk_count {
            return None;
        }
        Some(crate::base64_ct::b64url_nopad(&self.rolling.clone().finalize()))
    }

    fn mark_held(&mut self, index: u64) {
        self.held.insert(index);
        if self.pending.as_ref().map(|(i, _)| *i) == Some(index) {
            self.pending = None;
        }
    }

    fn reconcile(&mut self, held: &[(u64, String)]) -> Result<(), StreamError> {
        for (idx, digest_wire) in held {
            let idx = *idx;
            if idx >= self.chunk_count {
                return Err(StreamError::Fatal(format!(
                    "resume reports held chunk {idx} >= chunk_count {}",
                    self.chunk_count
                )));
            }
            match self.processed.get(&idx) {
                Some(facts) if &facts.digest_wire == digest_wire => {}
                Some(_) => return Err(StreamError::ChunkVerificationFailed { index: idx }),
                None => return Err(StreamError::ChunkVerificationFailed { index: idx }),
            }
            self.mark_held(idx);
        }
        Ok(())
    }

    fn bytes_for<R: ChunkReader>(
        &mut self,
        index: u64,
        reader: &R,
    ) -> Result<Vec<u8>, StreamError> {
        if let Some((pidx, pbytes)) = &self.pending {
            if *pidx == index {
                return Ok(pbytes.clone());
            }
        }
        if index == self.hashed_through {
            let len = self.expected_len(index);
            let offset = index * self.chunk_size;
            let t = Instant::now();
            let bytes = reader
                .read_chunk(index, offset, len)
                .map_err(|e| StreamError::Fatal(format!("read chunk {index}: {e}")))?;
            self.timings.read_ns += t.elapsed().as_nanos();
            if bytes.len() as u64 != len {
                return Err(StreamError::Fatal(format!(
                    "chunk {index}: source returned {} bytes, expected {len}",
                    bytes.len()
                )));
            }
            let t = Instant::now();
            self.rolling.update(&bytes); // <-- the ONLY rolling.update for this index
            self.timings.rolling_sha_ns += t.elapsed().as_nanos();
            let t = Instant::now();
            let digest_wire = crate::sha256_wire(&bytes);
            self.timings.chunk_sha_ns += t.elapsed().as_nanos();
            self.processed
                .insert(index, ChunkFacts { digest_wire, size: len });
            self.hashed_through = index + 1;
            self.pending = Some((index, bytes.clone()));
            Ok(bytes)
        } else if index < self.hashed_through {
            let facts = self
                .processed
                .get(&index)
                .cloned()
                .ok_or_else(|| StreamError::Fatal(format!("chunk {index}: no recorded facts")))?;
            let offset = index * self.chunk_size;
            let bytes = reader
                .read_chunk(index, offset, facts.size)
                .map_err(|e| StreamError::Fatal(format!("re-read chunk {index}: {e}")))?;
            if crate::sha256_wire(&bytes) != facts.digest_wire {
                return Err(StreamError::ChunkVerificationFailed { index });
            }
            Ok(bytes)
        } else {
            Err(StreamError::Fatal(format!(
                "chunk {index}: requested out of forward order (hashed_through={})",
                self.hashed_through
            )))
        }
    }
}

/// One re-entrant pass over the bounded source. On `Complete` every chunk is
/// durably held. On a `Suspended*` outcome the caller obtains a fresh grant /
/// waits for Worker recovery and calls again with the SAME `state`.
pub async fn run_stream_pass<R, D>(
    state: &mut StreamState,
    reader: &R,
    dp: &mut D,
    progress: &mut impl FnMut(ProgressTick),
    lifecycle: &mut impl FnMut(StreamEvent),
) -> Result<PassOutcome, StreamError>
where
    R: ChunkReader,
    D: DataPlane,
{
    lifecycle(StreamEvent::ResumeBegin);
    match dp.discover_resume().await {
        ResumeStatus::Ok(held) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "approved",
                held_chunks: held.len() as u64,
                sealed: false,
            });
            state.reconcile(&held)?;
            let pending_chunk_index = state.pending_index();
            let pending_already_held =
                pending_chunk_index.map(|i| state.is_held(i)).unwrap_or(false);
            lifecycle(StreamEvent::ResumeReconciled {
                held_count: state.held_count(),
                pending_chunk_index,
                pending_already_held,
            });
        }
        ResumeStatus::AuthDenied => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "auth_denied",
                held_chunks: 0,
                sealed: false,
            });
            return Ok(PassOutcome::SuspendedNeedsAuthorization);
        }
        ResumeStatus::Transient(detail) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "transient",
                held_chunks: 0,
                sealed: false,
            });
            lifecycle(StreamEvent::ResumeTransient { local_attempt: 0, detail });
            return Ok(PassOutcome::SuspendedDataPlaneUnreachable);
        }
        ResumeStatus::Fatal(m) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "fatal",
                held_chunks: 0,
                sealed: false,
            });
            return Err(StreamError::Fatal(m));
        }
    }

    let chunk_count = state.chunk_count;
    for index in 0..chunk_count {
        if state.held.contains(&index) {
            continue;
        }
        let bytes = state.bytes_for(index, reader)?;
        let digest_wire = state
            .processed
            .get(&index)
            .ok_or_else(|| StreamError::Fatal(format!("chunk {index}: missing processed facts")))?
            .digest_wire
            .clone();

        let mut local: u32 = 0;
        'retry: loop {
            match dp.put_chunk(index, &digest_wire, &bytes).await {
                PutStatus::Accepted | PutStatus::AlreadyHeld => {
                    state.mark_held(index);
                    progress(ProgressTick {
                        held_bytes: state.durably_held_bytes(),
                        held_chunks: state.held_count(),
                    });
                    break 'retry;
                }
                PutStatus::DigestMismatch | PutStatus::IdentityConflict => {
                    return Err(StreamError::ChunkVerificationFailed { index });
                }
                PutStatus::NotContinuable => {
                    return Err(StreamError::Fatal(format!(
                        "chunk {index}: 409 TRANSFER_NOT_CONTINUABLE"
                    )));
                }
                PutStatus::Fatal(m) => return Err(StreamError::Fatal(m)),
                PutStatus::AuthDenied => {
                    lifecycle(StreamEvent::PutAuthDenied { chunk_index: index });
                    return Ok(PassOutcome::SuspendedNeedsAuthorization);
                }
                PutStatus::Transient(detail) => {
                    local += 1;
                    lifecycle(StreamEvent::PutTransient {
                        chunk_index: index,
                        local_attempt: local,
                        detail,
                    });
                    if local > MAX_LOCAL_PUT_RETRIES {
                        return Ok(PassOutcome::SuspendedDataPlaneUnreachable);
                    }
                    tokio::time::sleep(LOCAL_BACKOFF).await;
                    lifecycle(StreamEvent::ResumeBegin);
                    match dp.discover_resume().await {
                        ResumeStatus::Ok(held) => {
                            lifecycle(StreamEvent::ResumeResult {
                                outcome: "approved",
                                held_chunks: held.len() as u64,
                                sealed: false,
                            });
                            state.reconcile(&held)?;
                            if state.held.contains(&index) {
                                progress(ProgressTick {
                                    held_bytes: state.durably_held_bytes(),
                                    held_chunks: state.held_count(),
                                });
                                break 'retry;
                            }
                        }
                        ResumeStatus::AuthDenied => {
                            lifecycle(StreamEvent::ResumeResult {
                                outcome: "auth_denied",
                                held_chunks: 0,
                                sealed: false,
                            });
                            return Ok(PassOutcome::SuspendedNeedsAuthorization);
                        }
                        ResumeStatus::Transient(detail) => {
                            lifecycle(StreamEvent::ResumeResult {
                                outcome: "transient",
                                held_chunks: 0,
                                sealed: false,
                            });
                            lifecycle(StreamEvent::ResumeTransient {
                                local_attempt: local,
                                detail,
                            });
                            if local > MAX_LOCAL_PUT_RETRIES {
                                return Ok(PassOutcome::SuspendedDataPlaneUnreachable);
                            }
                        }
                        ResumeStatus::Fatal(m) => {
                            lifecycle(StreamEvent::ResumeResult {
                                outcome: "fatal",
                                held_chunks: 0,
                                sealed: false,
                            });
                            return Err(StreamError::Fatal(m));
                        }
                    }
                }
            }
        }
    }

    if !state.all_uploaded() {
        return Err(StreamError::Fatal(format!(
            "pass ended with {}/{} chunks held",
            state.held.len(),
            chunk_count
        )));
    }
    Ok(PassOutcome::Complete)
}

// =====================================================================
// Issue #63 Stage 4 — depth-2 PREP-AHEAD pipeline (throwaway Spike).
//
// PREP-AHEAD ONLY. At most: chunk N is being PUT / awaiting ACK on the
// foreground task while chunk N+1 is being read + hashed on ONE dedicated
// producer thread. There is still exactly ONE `put_chunk` future in flight,
// PUTs are issued in strictly ascending order, no out-of-order durable
// acceptance is possible, and there are never two concurrent Worker PUT
// requests. This is NOT multi-PUT concurrency.
//
// Ownership (owner-reviewed): the producer thread owns its own
// `GENERIC_READ`-only source handle (a SECOND open of the already-resolved
// locator, AFTER the source-safety predicate has PASSED — zero bulk reads
// happen before PASS because this pipeline is only entered post-Accept) AND
// owns the rolling full-Artifact `Sha256` for the whole pass. Neither the
// handle nor the hasher ever crosses a thread boundary. A bounded
// `sync_channel(1)` carries one `PreparedChunk` at a time; the foreground
// holds at most one `current` buffer and awaits at most one `put_chunk`.
// Depth 2 is therefore structural: `current` (foreground) + one
// prepared/being-prepared buffer (producer / channel).
//
// The serial `run_stream_pass` above is deliberately left byte-for-byte
// unchanged; Stage-3's default behaviour is the already-proven algorithm.
// =====================================================================

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

/// One fully prepared chunk handed from the producer thread to the foreground.
pub struct PreparedChunk {
    pub index: u64,
    pub digest_wire: String,
    pub bytes: Vec<u8>,
}

/// Producer -> foreground.
enum PrepMsg {
    Chunk(PreparedChunk),
    /// Sent once, in response to `Ctrl::Finish`: the rolling digest the
    /// producer finalised, the accumulated per-stage timings, and the exact
    /// ascending read log.
    Final {
        rolling_digest: String,
        timings: PassTimings,
        read_log: Vec<u64>,
    },
    Err(String),
}

/// Foreground -> producer.
enum Ctrl {
    Prepare(u64),
    Finish,
}

/// The exact byte length of `index` for a `(chunk_size, chunk_count, total_len)`
/// geometry — `chunk_size` for every chunk but the last, `1..=chunk_size` for
/// the last. Free-standing twin of [`StreamState::expected_len`] for the
/// producer thread (which does not hold a `StreamState`).
fn chunk_len(index: u64, chunk_size: u64, chunk_count: u64, total_len: u64) -> u64 {
    if index + 1 < chunk_count {
        chunk_size
    } else {
        total_len - (chunk_count - 1) * chunk_size
    }
}

/// The dedicated prep-ahead producer. Owns `reader` (built in-thread via
/// `factory`) and the rolling `Sha256` for the whole pass. Serves `Prepare`
/// requests in strictly ascending order, each index exactly once, updating the
/// rolling hash exactly once per chunk in that order.
fn run_producer<R, F>(
    factory: F,
    chunk_size: u64,
    total_len: u64,
    chunk_count: u64,
    ctrl_rx: Receiver<Ctrl>,
    out_tx: SyncSender<PrepMsg>,
) where
    R: ChunkReader,
    F: FnOnce() -> Result<R, String>,
{
    let reader = match factory() {
        Ok(r) => r,
        Err(e) => {
            let _ = out_tx.send(PrepMsg::Err(format!("producer: open source: {e}")));
            return;
        }
    };
    let mut rolling = Sha256::new();
    let mut expect_next: u64 = 0;
    let mut timings = PassTimings::default();
    let mut read_log: Vec<u64> = Vec::new();

    while let Ok(msg) = ctrl_rx.recv() {
        match msg {
            Ctrl::Prepare(index) => {
                if index != expect_next {
                    let _ = out_tx.send(PrepMsg::Err(format!(
                        "producer: non-ascending prepare {index} (expected {expect_next})"
                    )));
                    return;
                }
                if index >= chunk_count {
                    let _ = out_tx.send(PrepMsg::Err(format!(
                        "producer: prepare {index} >= chunk_count {chunk_count}"
                    )));
                    return;
                }
                let len = chunk_len(index, chunk_size, chunk_count, total_len);
                let offset = index * chunk_size;

                let t = Instant::now();
                let bytes = match reader.read_chunk(index, offset, len) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = out_tx.send(PrepMsg::Err(format!("producer: read chunk {index}: {e}")));
                        return;
                    }
                };
                timings.read_ns += t.elapsed().as_nanos();
                read_log.push(index);
                if bytes.len() as u64 != len {
                    let _ = out_tx.send(PrepMsg::Err(format!(
                        "producer: chunk {index}: source returned {} bytes, expected {len}",
                        bytes.len()
                    )));
                    return;
                }

                let t = Instant::now();
                rolling.update(&bytes); // the ONLY rolling.update for this index
                timings.rolling_sha_ns += t.elapsed().as_nanos();

                let t = Instant::now();
                let digest_wire = crate::sha256_wire(&bytes);
                timings.chunk_sha_ns += t.elapsed().as_nanos();

                expect_next = index + 1;
                if out_tx
                    .send(PrepMsg::Chunk(PreparedChunk {
                        index,
                        digest_wire,
                        bytes,
                    }))
                    .is_err()
                {
                    return; // foreground gone
                }
            }
            Ctrl::Finish => {
                let rolling_digest =
                    crate::base64_ct::b64url_nopad(&rolling.finalize());
                let _ = out_tx.send(PrepMsg::Final {
                    rolling_digest,
                    timings,
                    read_log,
                });
                return;
            }
        }
    }
}

/// Local classification of a single foreground PUT boundary.
enum PutLocal {
    Held,
    VerifyFail,
    Fatal(String),
    SuspendAuth,
    SuspendUnreachable,
}

/// One re-entrant PREP-AHEAD (depth-2) pass over the bounded source. Semantics
/// mirror [`run_stream_pass`] for the caller: `Complete` => every chunk durably
/// held; a `Suspended*` outcome => the caller obtains a fresh grant / waits and
/// (in the Stage-4 clean matrix) treats it as contamination.
///
/// `reader_factory` is invoked ONCE, inside the producer thread, to open the
/// producer's own source handle. It must not perform any I/O itself.
pub async fn run_stream_pass_prep_ahead<R, D, F>(
    state: &mut StreamState,
    reader_factory: F,
    dp: &mut D,
    progress: &mut impl FnMut(ProgressTick),
    lifecycle: &mut impl FnMut(StreamEvent),
) -> Result<PassOutcome, StreamError>
where
    R: ChunkReader + 'static,
    D: DataPlane,
    F: FnOnce() -> Result<R, String> + Send + 'static,
{
    lifecycle(StreamEvent::ResumeBegin);
    match dp.discover_resume().await {
        ResumeStatus::Ok(held) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "approved",
                held_chunks: held.len() as u64,
                sealed: false,
            });
            // Prep-ahead runs the clean fast path only. A non-empty resume is a
            // resume EVENT (contamination in Stage 4) — fail closed rather than
            // trying to prep-ahead across a partial manifest.
            if !held.is_empty() {
                return Err(StreamError::Fatal(format!(
                    "prep-ahead: unexpected non-empty resume ({} held)",
                    held.len()
                )));
            }
            lifecycle(StreamEvent::ResumeReconciled {
                held_count: 0,
                pending_chunk_index: None,
                pending_already_held: false,
            });
        }
        ResumeStatus::AuthDenied => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "auth_denied",
                held_chunks: 0,
                sealed: false,
            });
            return Ok(PassOutcome::SuspendedNeedsAuthorization);
        }
        ResumeStatus::Transient(detail) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "transient",
                held_chunks: 0,
                sealed: false,
            });
            lifecycle(StreamEvent::ResumeTransient { local_attempt: 0, detail });
            return Ok(PassOutcome::SuspendedDataPlaneUnreachable);
        }
        ResumeStatus::Fatal(m) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "fatal",
                held_chunks: 0,
                sealed: false,
            });
            return Err(StreamError::Fatal(m));
        }
    }

    let chunk_count = state.chunk_count;
    let chunk_size = state.chunk_size;
    let total_len = state.total_len;

    let (ctrl_tx, ctrl_rx) = sync_channel::<Ctrl>(1);
    let (out_tx, out_rx) = sync_channel::<PrepMsg>(1);
    let producer = std::thread::Builder::new()
        .name("i63-prep-ahead".into())
        .spawn(move || {
            run_producer(reader_factory, chunk_size, total_len, chunk_count, ctrl_rx, out_tx)
        })
        .map_err(|e| StreamError::Fatal(format!("spawn prep-ahead thread: {e}")))?;

    // Ask for chunk 0 and take it. (These `recv`s block the current worker
    // thread; on the probe's multi-thread runtime that is acceptable for this
    // throwaway Spike, and each `recv` after the first returns near-immediately
    // because the producer's read overlapped the preceding PUT.)
    if ctrl_tx.send(Ctrl::Prepare(0)).is_err() {
        // The producer already exited (its source open failed, so it dropped
        // `ctrl_rx` before we sent). It sent its real reason on `out_tx` first
        // — surface THAT, deterministically, rather than a generic string.
        let msg = match out_rx.recv() {
            Ok(PrepMsg::Err(e)) => e,
            _ => "prep-ahead: producer exited before accepting the first request".to_string(),
        };
        let _ = producer.join();
        return Err(StreamError::Fatal(msg));
    }
    let mut current = match out_rx.recv() {
        Ok(PrepMsg::Chunk(c)) => c,
        Ok(PrepMsg::Err(e)) => {
            let _ = producer.join();
            return Err(StreamError::Fatal(e));
        }
        _ => {
            let _ = producer.join();
            return Err(StreamError::Fatal("prep-ahead: producer sent no chunk 0".into()));
        }
    };
    // `state.processed` is a serial-path structure (read by `bytes_for` /
    // `reconcile`); prep-ahead never reads it, so nothing is recorded here.
    let mut live_depth: u64 = 1; // holding `current`
    state.prepared_peak = state.prepared_peak.max(live_depth);

    /// How the drive loop below terminated. The loop ONLY ever `break`s with one
    /// of these, so `producer` is joined on exactly one path.
    enum Term {
        Complete,
        SuspendAuth,
        SuspendUnreachable,
        VerifyFail(u64),
        Fatal(String),
    }

    let term: Term = loop {
        let next_idx = current.index + 1;
        let has_next = next_idx < chunk_count;
        if has_next {
            // Exactly ONE outstanding Prepare request at a time — the producer
            // does one chunk per request, so the live buffer count stays <= 2
            // (`current` + one prepared/being-prepared).
            if ctrl_tx.send(Ctrl::Prepare(next_idx)).is_err() {
                break Term::Fatal("prep-ahead: producer hung up before Prepare".into());
            }
            live_depth += 1; // producer is now reading / will buffer `next_idx`
            state.prepared_peak = state.prepared_peak.max(live_depth);
        }

        // ---- the single in-flight PUT for `current` (overlaps the producer
        // reading + hashing `next_idx`) ----
        let mut local: u32 = 0;
        let put = loop {
            match dp
                .put_chunk(current.index, &current.digest_wire, &current.bytes)
                .await
            {
                PutStatus::Accepted | PutStatus::AlreadyHeld => break PutLocal::Held,
                PutStatus::DigestMismatch | PutStatus::IdentityConflict => {
                    break PutLocal::VerifyFail
                }
                PutStatus::NotContinuable => {
                    break PutLocal::Fatal(format!(
                        "chunk {}: 409 TRANSFER_NOT_CONTINUABLE",
                        current.index
                    ))
                }
                PutStatus::Fatal(m) => break PutLocal::Fatal(m),
                PutStatus::AuthDenied => {
                    lifecycle(StreamEvent::PutAuthDenied { chunk_index: current.index });
                    break PutLocal::SuspendAuth;
                }
                PutStatus::Transient(detail) => {
                    // ---- Issue #63 Stage 4 clean-fast-path SIMPLIFICATION (throwaway) ----
                    // Serial `run_stream_pass` (above) runs a `discover_resume` +
                    // `reconcile` dance between transient PUT retries. This
                    // prep-ahead path deliberately does NOT: it does a bounded
                    // same-buffer retry only.
                    //   * serial `run_stream_pass` remains the authoritative,
                    //     unmodified transfer algorithm;
                    //   * ANY `PutTransient` marks the physical Stage-4 case
                    //     CONTAMINATED, so the case is unusable for the S-vs-P
                    //     throughput comparison EVEN IF a later bounded retry
                    //     succeeds — the matrix stops on it;
                    //   * this is NOT a proposed production resume semantic and
                    //     MUST NOT be generalised outside this Spike.
                    local += 1;
                    lifecycle(StreamEvent::PutTransient {
                        chunk_index: current.index,
                        local_attempt: local,
                        detail,
                    });
                    if local > MAX_LOCAL_PUT_RETRIES {
                        break PutLocal::SuspendUnreachable;
                    }
                    // Reuse the SAME buffered bytes — no re-read, no re-hash, no
                    // durable-progress increment until `held` is confirmed.
                    tokio::time::sleep(LOCAL_BACKOFF).await;
                }
            }
        };

        match put {
            PutLocal::Held => {
                state.mark_held(current.index);
                progress(ProgressTick {
                    held_bytes: state.durably_held_bytes(),
                    held_chunks: state.held_count(),
                });
            }
            PutLocal::VerifyFail => break Term::VerifyFail(current.index),
            PutLocal::Fatal(m) => break Term::Fatal(m),
            PutLocal::SuspendAuth => break Term::SuspendAuth,
            PutLocal::SuspendUnreachable => break Term::SuspendUnreachable,
        }

        if !has_next {
            break Term::Complete;
        }

        // Collect the chunk the producer prepared while the PUT above ran.
        // Assigning `current` drops the previous buffer, so `live_depth` falls
        // back to 1 (the new `current`).
        match out_rx.recv() {
            Ok(PrepMsg::Chunk(c)) => {
                current = c;
                live_depth -= 1;
            }
            Ok(PrepMsg::Err(e)) => break Term::Fatal(e),
            Ok(_) => break Term::Fatal("prep-ahead: expected next PreparedChunk".into()),
            Err(_) => break Term::Fatal("prep-ahead: producer hung up mid-pass".into()),
        }
    };

    // Fold the last observed depth into the peak (also makes the final
    // `live_depth` adjustment above a live read, never a dead store).
    state.prepared_peak = state.prepared_peak.max(live_depth);

    // ---- single cleanup path ----
    let mut final_data: Option<(String, PassTimings, Vec<u64>)> = None;
    let mut late_err: Option<String> = None;
    if matches!(term, Term::Complete) {
        let _ = ctrl_tx.send(Ctrl::Finish);
        loop {
            match out_rx.recv() {
                Ok(PrepMsg::Final { rolling_digest, timings, read_log }) => {
                    final_data = Some((rolling_digest, timings, read_log));
                    break;
                }
                Ok(PrepMsg::Chunk(_)) => continue,
                Ok(PrepMsg::Err(e)) => {
                    late_err = Some(e);
                    break;
                }
                Err(_) => {
                    late_err = Some("prep-ahead: producer hung up before final".into());
                    break;
                }
            }
        }
    }
    drop(ctrl_tx); // hang up so a producer still in `recv` exits
    let _ = producer.join();

    match term {
        Term::SuspendAuth => return Ok(PassOutcome::SuspendedNeedsAuthorization),
        Term::SuspendUnreachable => return Ok(PassOutcome::SuspendedDataPlaneUnreachable),
        Term::VerifyFail(idx) => return Err(StreamError::ChunkVerificationFailed { index: idx }),
        Term::Fatal(m) => return Err(StreamError::Fatal(m)),
        Term::Complete => {}
    }
    if let Some(e) = late_err {
        return Err(StreamError::Fatal(e));
    }
    let (rolling_digest, timings, read_log) = final_data
        .ok_or_else(|| StreamError::Fatal("prep-ahead: no producer final message".into()))?;

    // INTEGRITY: the producer read every chunk exactly once, ascending 0..N.
    let expected: Vec<u64> = (0..chunk_count).collect();
    if read_log != expected {
        return Err(StreamError::Fatal(format!(
            "prep-ahead: producer read log is not 0..{chunk_count} exactly once ascending"
        )));
    }
    state.timings = timings;
    state.finalized_digest = Some(rolling_digest);
    state.producer_read_log = read_log;
    state.hashed_through = chunk_count;

    if state.prepared_peak > 2 {
        return Err(StreamError::Fatal(format!(
            "prep-ahead: prepared buffer depth {} exceeded 2",
            state.prepared_peak
        )));
    }
    if !state.all_uploaded() {
        return Err(StreamError::Fatal(format!(
            "prep-ahead pass ended with {}/{} chunks held",
            state.held.len(),
            chunk_count
        )));
    }
    Ok(PassOutcome::Complete)
}

// =====================================================================
// Issue #63 window_8 — the prep-ahead + bounded-concurrent-PUT candidate
// (throwaway Spike).
//
// SOURCE side is the UNCHANGED Stage-4 producer (`run_producer`): one
// dedicated thread owning its own GENERIC_READ handle + the rolling
// full-Artifact SHA-256; chunks prepared strictly ascending, each read exactly
// once; at most ONE outstanding `Prepare` at a time. NETWORK side is new: up
// to `PUT_WINDOW` (8) chunk PUTs concurrently in flight as independent tokio
// tasks, STARTED in ascending chunk-index order, completions accepted in ANY
// order. Each PUT's owned `Vec<u8>` payload is MOVED into its task (no 64 MiB
// clone); the exact prepared bytes+digest are retained by that task until its
// outcome is known. Completion order cannot affect the Artifact digest — the
// producer thread alone owns the rolling hasher, in index order.
//
// Worker durability semantics are UNCHANGED: every individual PUT still runs
// the full authorize -> stage -> validate -> finalize(fsync+placement+fsync)
// -> commit_chunk -> only-then-Accepted path; window_8 merely lets up to 8
// such independently durable PUTs overlap. Contract basis: the m0 data-plane
// Specification's reconstruction rule ("each chunk contributes its bytes at
// one fixed position regardless of transfer order") and
// `commit_chunk_acceptance` (no ordering precondition; per-transfer locked
// first-writer commit).
//
// CLEAN-FAST-PATH FAILURE POLICY (NOT a production recovery proposal): ANY
// non-Accepted PUT outcome (transient, 401, AlreadyHeld, digest/identity
// conflict, ...) terminates the pass — no retry-to-green. Outstanding PUT
// tasks are aborted and joined before returning, so no background request
// object outlives a failed case. Authoritative serial resume behaviour is
// untouched.
//
// Memory model: <= PUT_WINDOW unacknowledged payloads + <= 1 producer/current
// chunk => live payload depth <= PUT_WINDOW + 1 (~576 MiB at 64 MiB chunks).
// =====================================================================

/// The hard-coded window_8 bounded PUT window. Deliberately NOT configurable.
pub const PUT_WINDOW: u64 = 8;

/// Spike-only owned-payload PUT launcher for the window_8 path. `start_put` is
/// invoked on the foreground task in strictly ascending `index` order; the
/// implementation must synchronously prepare all immutable request material
/// (e.g. mint the per-request proof — see [`AgentTransferAuthorization::create_proof_now`],
/// which is `&self` and safe to call repeatedly from the foreground) and
/// return a future that performs the PUT and resolves to its outcome. The
/// payload `Vec<u8>` is MOVED in — the launcher must not clone it.
///
/// The returned future is spawned as EXACTLY ONE task by the driver
/// (`run_stream_pass_window8`), which is what makes `JoinSet::abort_all`
/// actually cancel the real PUT: the launcher must NOT itself spawn a second,
/// independent task (e.g. via `tokio::spawn`) and hand back a `JoinHandle` to
/// it — a `JoinHandle`'s underlying task keeps running even if its handle is
/// aborted from the wrong place, which would leave a PUT running in the
/// background after a case is declared failed.
pub trait WindowedPutLauncher {
    fn start_put(
        &mut self,
        index: u64,
        digest_wire: String,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = PutStatus> + Send>>;
}

/// One re-entrant window_8 pass over the bounded source. `resume` is the
/// pre-fetched resume-discovery outcome (fetched by the caller so the launcher
/// may immutably borrow the same authorization material). Clean fast path
/// only: a non-empty resume fails closed, any non-`Accepted` PUT outcome
/// terminates the pass, and all outstanding PUT tasks are aborted + joined on
/// every exit path.
pub async fn run_stream_pass_window8<R, F, L>(
    state: &mut StreamState,
    reader_factory: F,
    resume: ResumeStatus,
    launcher: &mut L,
    progress: &mut impl FnMut(ProgressTick),
    lifecycle: &mut impl FnMut(StreamEvent),
) -> Result<PassOutcome, StreamError>
where
    R: ChunkReader + 'static,
    F: FnOnce() -> Result<R, String> + Send + 'static,
    L: WindowedPutLauncher,
{
    lifecycle(StreamEvent::ResumeBegin);
    match resume {
        ResumeStatus::Ok(held) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "approved",
                held_chunks: held.len() as u64,
                sealed: false,
            });
            if !held.is_empty() {
                return Err(StreamError::Fatal(format!(
                    "window_8: unexpected non-empty resume ({} held)",
                    held.len()
                )));
            }
            lifecycle(StreamEvent::ResumeReconciled {
                held_count: 0,
                pending_chunk_index: None,
                pending_already_held: false,
            });
        }
        ResumeStatus::AuthDenied => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "auth_denied",
                held_chunks: 0,
                sealed: false,
            });
            return Ok(PassOutcome::SuspendedNeedsAuthorization);
        }
        ResumeStatus::Transient(detail) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "transient",
                held_chunks: 0,
                sealed: false,
            });
            lifecycle(StreamEvent::ResumeTransient { local_attempt: 0, detail });
            return Ok(PassOutcome::SuspendedDataPlaneUnreachable);
        }
        ResumeStatus::Fatal(m) => {
            lifecycle(StreamEvent::ResumeResult {
                outcome: "fatal",
                held_chunks: 0,
                sealed: false,
            });
            return Err(StreamError::Fatal(m));
        }
    }

    let chunk_count = state.chunk_count;
    let chunk_size = state.chunk_size;
    let total_len = state.total_len;
    state.put_window = PUT_WINDOW;

    let (ctrl_tx, ctrl_rx) = sync_channel::<Ctrl>(1);
    let (out_tx, out_rx) = sync_channel::<PrepMsg>(1);
    let producer = std::thread::Builder::new()
        .name("i63-w8-producer".into())
        .spawn(move || {
            run_producer(reader_factory, chunk_size, total_len, chunk_count, ctrl_rx, out_tx)
        })
        .map_err(|e| StreamError::Fatal(format!("spawn window_8 producer thread: {e}")))?;

    /// How the drive loop terminated (single cleanup path, like prep-ahead).
    enum Term {
        Complete,
        SuspendAuth,
        SuspendUnreachable,
        VerifyFail(u64),
        Fatal(String),
    }

    // In-flight PUT tasks. `window.len()` counts unreaped tasks, so the true
    // network in-flight count is `<= window.len() <= PUT_WINDOW` at all times.
    let mut window: tokio::task::JoinSet<(u64, PutStatus)> = tokio::task::JoinSet::new();
    let mut next_index: u64 = 0;
    // Live 64 MiB payload buffers: in-flight tasks + (producer reading /
    // prepared / foreground-held current). Peak MUST stay <= PUT_WINDOW + 1.
    fn track_payload_peak(state: &mut StreamState, window_len: u64, extra: u64) {
        state.prepared_peak = state.prepared_peak.max(window_len + extra);
    }

    // Classify one completed PUT. Returns Some(Term) to stop the pass.
    fn classify(
        state: &mut StreamState,
        index: u64,
        status: PutStatus,
        progress: &mut impl FnMut(ProgressTick),
        lifecycle: &mut impl FnMut(StreamEvent),
    ) -> Option<Term> {
        state.put_completed_count += 1;
        state.put_completion_order.push(index);
        match status {
            PutStatus::Accepted => {
                state.mark_held(index);
                progress(ProgressTick {
                    held_bytes: state.durably_held_bytes(),
                    held_chunks: state.held_count(),
                });
                None
            }
            // Clean fast path: an AlreadyHeld on a first-and-only PUT is a
            // duplicate/resume anomaly — contamination, never silently green.
            PutStatus::AlreadyHeld => Some(Term::Fatal(format!(
                "window_8: unexpected AlreadyHeld for chunk {index} on the clean path"
            ))),
            PutStatus::DigestMismatch | PutStatus::IdentityConflict => {
                Some(Term::VerifyFail(index))
            }
            PutStatus::NotContinuable => Some(Term::Fatal(format!(
                "chunk {index}: 409 TRANSFER_NOT_CONTINUABLE"
            ))),
            PutStatus::Fatal(m) => Some(Term::Fatal(m)),
            PutStatus::AuthDenied => {
                lifecycle(StreamEvent::PutAuthDenied { chunk_index: index });
                Some(Term::SuspendAuth)
            }
            PutStatus::Transient(detail) => {
                // NO local retry in window_8 — any transient contaminates the
                // case and terminates the pass.
                lifecycle(StreamEvent::PutTransient {
                    chunk_index: index,
                    local_attempt: 1,
                    detail,
                });
                Some(Term::SuspendUnreachable)
            }
        }
    }

    let term: Term = 'drive: loop {
        if next_index < chunk_count && (window.len() as u64) < PUT_WINDOW {
            // Capacity + chunks remain: ask the producer for exactly one next
            // chunk. Its read+hash overlaps the in-flight PUT tasks.
            if ctrl_tx.send(Ctrl::Prepare(next_index)).is_err() {
                let msg = match out_rx.recv() {
                    Ok(PrepMsg::Err(e)) => e,
                    _ => "window_8: producer exited before accepting a request".to_string(),
                };
                break 'drive Term::Fatal(msg);
            }
            track_payload_peak(state, window.len() as u64, 1);
            // Blocking recv on the driving thread is the same throwaway idiom
            // prep-ahead uses: the spawned PUT tasks progress on the runtime's
            // other worker threads, and the wait is bounded by one chunk read.
            let prepared = match out_rx.recv() {
                Ok(PrepMsg::Chunk(c)) => c,
                Ok(PrepMsg::Err(e)) => break 'drive Term::Fatal(e),
                Ok(_) => break 'drive Term::Fatal("window_8: expected PreparedChunk".into()),
                Err(_) => break 'drive Term::Fatal("window_8: producer hung up mid-pass".into()),
            };
            if prepared.index != next_index {
                break 'drive Term::Fatal(format!(
                    "window_8: producer returned chunk {} (expected {next_index})",
                    prepared.index
                ));
            }
            // Launch the PUT: the owned payload Vec MOVES into the future the
            // launcher builds, which `window.spawn` turns into EXACTLY ONE
            // real task (so `abort_all` below can actually cancel it — see
            // `WindowedPutLauncher`'s contract).
            let idx = prepared.index;
            state.put_start_order.push(idx);
            state.put_started_count += 1;
            let fut = launcher.start_put(idx, prepared.digest_wire, prepared.bytes);
            window.spawn(async move { (idx, fut.await) });
            next_index += 1;
            // `window.len()` counts unreaped tasks — an upper bound on true
            // network in-flight, and the value the <= PUT_WINDOW gate enforces.
            state.peak_puts_in_flight = state.peak_puts_in_flight.max(window.len() as u64);
            track_payload_peak(state, window.len() as u64, 0);
            // Opportunistically reap already-finished PUTs (non-blocking) so
            // failures stop the pass promptly and the window stays honest.
            while let Some(done) = window.try_join_next() {
                let (idx, status) = match done {
                    Ok(pair) => pair,
                    Err(e) => break 'drive Term::Fatal(format!("window_8: task join: {e}")),
                };
                if let Some(t) = classify(state, idx, status, progress, lifecycle) {
                    break 'drive t;
                }
            }
            continue;
        }
        // Window full, or no chunks left to start: wait for one completion.
        match window.join_next().await {
            Some(Ok((idx, status))) => {
                if let Some(t) = classify(state, idx, status, progress, lifecycle) {
                    break 'drive t;
                }
            }
            Some(Err(e)) => break 'drive Term::Fatal(format!("window_8: task join: {e}")),
            None => break 'drive Term::Complete, // window empty and all started
        }
    };

    // ---- single cleanup path: no PUT task may outlive the pass ----
    window.abort_all();
    while window.join_next().await.is_some() {}

    let mut final_data: Option<(String, PassTimings, Vec<u64>)> = None;
    let mut late_err: Option<String> = None;
    if matches!(term, Term::Complete) {
        let _ = ctrl_tx.send(Ctrl::Finish);
        loop {
            match out_rx.recv() {
                Ok(PrepMsg::Final { rolling_digest, timings, read_log }) => {
                    final_data = Some((rolling_digest, timings, read_log));
                    break;
                }
                Ok(PrepMsg::Chunk(_)) => continue,
                Ok(PrepMsg::Err(e)) => {
                    late_err = Some(e);
                    break;
                }
                Err(_) => {
                    late_err = Some("window_8: producer hung up before final".into());
                    break;
                }
            }
        }
    }
    drop(ctrl_tx);
    let _ = producer.join();

    match term {
        Term::SuspendAuth => return Ok(PassOutcome::SuspendedNeedsAuthorization),
        Term::SuspendUnreachable => return Ok(PassOutcome::SuspendedDataPlaneUnreachable),
        Term::VerifyFail(idx) => return Err(StreamError::ChunkVerificationFailed { index: idx }),
        Term::Fatal(m) => return Err(StreamError::Fatal(m)),
        Term::Complete => {}
    }
    if let Some(e) = late_err {
        return Err(StreamError::Fatal(e));
    }
    let (rolling_digest, timings, read_log) = final_data
        .ok_or_else(|| StreamError::Fatal("window_8: no producer final message".into()))?;

    // INTEGRITY: source read exactly once, ascending; PUTs started ascending,
    // each logical chunk submitted exactly once; window + payload bounds held.
    let expected: Vec<u64> = (0..chunk_count).collect();
    if read_log != expected {
        return Err(StreamError::Fatal(format!(
            "window_8: producer read log is not 0..{chunk_count} exactly once ascending"
        )));
    }
    if state.put_start_order != expected {
        return Err(StreamError::Fatal(format!(
            "window_8: PUT start order is not 0..{chunk_count} exactly once ascending"
        )));
    }
    if state.peak_puts_in_flight > PUT_WINDOW {
        return Err(StreamError::Fatal(format!(
            "window_8: peak in-flight {} exceeded the {PUT_WINDOW} window",
            state.peak_puts_in_flight
        )));
    }
    if state.prepared_peak > PUT_WINDOW + 1 {
        return Err(StreamError::Fatal(format!(
            "window_8: live payload depth {} exceeded {} (window + producer/current)",
            state.prepared_peak,
            PUT_WINDOW + 1
        )));
    }
    state.timings = timings;
    state.finalized_digest = Some(rolling_digest);
    state.producer_read_log = read_log;
    state.hashed_through = chunk_count;

    if !state.all_uploaded() {
        return Err(StreamError::Fatal(format!(
            "window_8 pass ended with {}/{} chunks held",
            state.held.len(),
            chunk_count
        )));
    }
    Ok(PassOutcome::Complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const CS: u64 = 8 * 1024 * 1024;

    fn pattern(offset: u64, len: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            let p = offset.wrapping_add(i);
            let mut z = p.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            out.push(((z ^ (z >> 31)) & 0xFF) as u8);
        }
        out
    }

    fn reference_digest(total_len: u64) -> String {
        let mut h = Sha256::new();
        let mut off = 0u64;
        while off < total_len {
            let n = (total_len - off).min(1 << 20);
            h.update(pattern(off, n));
            off += n;
        }
        crate::base64_ct::b64url_nopad(&h.finalize())
    }

    struct FakeReader {
        reads: RefCell<BTreeMap<u64, u32>>,
    }
    impl FakeReader {
        fn new() -> Self {
            Self { reads: RefCell::new(BTreeMap::new()) }
        }
        fn read_count(&self, index: u64) -> u32 {
            *self.reads.borrow().get(&index).unwrap_or(&0)
        }
        fn total_reads(&self) -> u32 {
            self.reads.borrow().values().sum()
        }
    }
    impl ChunkReader for FakeReader {
        fn read_chunk(&self, index: u64, offset: u64, len: u64) -> Result<Vec<u8>, String> {
            *self.reads.borrow_mut().entry(index).or_default() += 1;
            Ok(pattern(offset, len))
        }
    }

    struct FakeDataPlane {
        held: BTreeMap<u64, String>,
        put_calls: BTreeMap<u64, u32>,
        transient_put_budget: BTreeMap<u64, u32>,
        authdenied_put_once: BTreeSet<u64>,
        fired_authdenied: BTreeSet<u64>,
        uncertain_put: Option<u64>,
        uncertain_fired: bool,
        corrupt_held_digest: Option<String>,
    }
    impl FakeDataPlane {
        fn new() -> Self {
            Self {
                held: BTreeMap::new(),
                put_calls: BTreeMap::new(),
                transient_put_budget: BTreeMap::new(),
                authdenied_put_once: BTreeSet::new(),
                fired_authdenied: BTreeSet::new(),
                uncertain_put: None,
                uncertain_fired: false,
                corrupt_held_digest: None,
            }
        }
        fn put_calls_for(&self, index: u64) -> u32 {
            *self.put_calls.get(&index).unwrap_or(&0)
        }
    }
    impl DataPlane for FakeDataPlane {
        async fn discover_resume(&mut self) -> ResumeStatus {
            let mut v: Vec<(u64, String)> =
                self.held.iter().map(|(k, d)| (*k, d.clone())).collect();
            v.sort_by_key(|(k, _)| *k);
            ResumeStatus::Ok(v)
        }
        async fn put_chunk(&mut self, index: u64, digest_wire: &str, bytes: &[u8]) -> PutStatus {
            *self.put_calls.entry(index).or_default() += 1;
            if self.authdenied_put_once.contains(&index)
                && !self.fired_authdenied.contains(&index)
            {
                self.fired_authdenied.insert(index);
                return PutStatus::AuthDenied;
            }
            if let Some(b) = self.transient_put_budget.get_mut(&index) {
                if *b > 0 {
                    *b -= 1;
                    return PutStatus::Transient("injected".into());
                }
            }
            if self.uncertain_put == Some(index) && !self.uncertain_fired {
                self.uncertain_fired = true;
                let stored = self
                    .corrupt_held_digest
                    .clone()
                    .unwrap_or_else(|| digest_wire.to_string());
                self.held.insert(index, stored);
                return PutStatus::Transient("uncertain (actually landed)".into());
            }
            if let Some(existing) = self.held.get(&index) {
                return if existing == digest_wire {
                    PutStatus::AlreadyHeld
                } else {
                    PutStatus::IdentityConflict
                };
            }
            if crate::sha256_wire(bytes) != digest_wire {
                return PutStatus::DigestMismatch;
            }
            self.held.insert(index, digest_wire.to_string());
            PutStatus::Accepted
        }
    }

    async fn drive(
        state: &mut StreamState,
        reader: &FakeReader,
        dp: &mut FakeDataPlane,
    ) -> Result<u32, StreamError> {
        let mut suspensions = 0u32;
        for _ in 0..40 {
            match run_stream_pass(state, reader, dp, &mut |_t| {}, &mut |_e| {}).await? {
                PassOutcome::Complete => return Ok(suspensions),
                PassOutcome::SuspendedNeedsAuthorization
                | PassOutcome::SuspendedDataPlaneUnreachable => suspensions += 1,
            }
        }
        Err(StreamError::Fatal("too many suspensions".into()))
    }

    #[tokio::test]
    async fn exactly_once_physical_read_across_transient_and_authdenied() {
        let total = 5 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(2, 1);
        dp.authdenied_put_once.insert(3);
        let mut state = StreamState::new(total, CS).unwrap();
        drive(&mut state, &reader, &mut dp).await.unwrap();
        assert!(state.all_uploaded());
        for i in 0..5 {
            assert_eq!(reader.read_count(i), 1, "chunk {i} read exactly once");
        }
        assert_eq!(reader.total_reads(), 5);
    }

    #[tokio::test]
    async fn rolling_hash_is_exactly_once_per_chunk_regardless_of_retries() {
        let total = 7 * CS + 111; // short final chunk
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(1, 2);
        dp.authdenied_put_once.insert(4);
        dp.uncertain_put = Some(5);
        let mut state = StreamState::new(total, CS).unwrap();
        drive(&mut state, &reader, &mut dp).await.unwrap();
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    #[tokio::test]
    async fn transient_retry_reuses_the_same_buffer() {
        let total = 4 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(2, 2);
        let mut state = StreamState::new(total, CS).unwrap();
        drive(&mut state, &reader, &mut dp).await.unwrap();
        assert_eq!(reader.read_count(2), 1, "no re-read on transient retry");
        assert_eq!(dp.put_calls_for(2), 3, "2 transient + 1 success");
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    #[tokio::test]
    async fn uncertain_put_held_on_resume_is_not_reput() {
        let total = 4 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.uncertain_put = Some(2);
        let mut state = StreamState::new(total, CS).unwrap();
        drive(&mut state, &reader, &mut dp).await.unwrap();
        assert!(state.all_uploaded());
        assert_eq!(dp.put_calls_for(2), 1, "confirmed via resume, never re-sent");
        assert_eq!(reader.read_count(2), 1);
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    #[tokio::test]
    async fn uncertain_put_held_with_wrong_digest_fails_closed() {
        let total = 4 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.uncertain_put = Some(2);
        dp.corrupt_held_digest = Some("this_is_not_the_reproduced_digest".into());
        let mut state = StreamState::new(total, CS).unwrap();
        match drive(&mut state, &reader, &mut dp).await {
            Err(StreamError::ChunkVerificationFailed { index }) => assert_eq!(index, 2),
            other => panic!("expected ChunkVerificationFailed, got {other:?}"),
        }
    }

    #[test]
    fn bounded_extent_arithmetic_2048_mib_at_each_size() {
        // The exact Stage-3 physical arithmetic.
        for (mib, want) in [(8u64, 256u64), (16, 128), (32, 64), (64, 32)] {
            let state = StreamState::new(2048 * 1024 * 1024, mib * 1024 * 1024).unwrap();
            assert_eq!(state.chunk_count(), want, "{mib} MiB");
            for i in 0..want {
                assert_eq!(state.expected_len(i), mib * 1024 * 1024, "no partial chunk");
            }
        }
    }

    #[test]
    fn new_rejects_degenerate_plans() {
        assert!(StreamState::new(0, CS).is_err());
        assert!(StreamState::new(CS, 0).is_err());
        assert!(StreamState::new(1, 1).is_ok());
    }

    #[tokio::test]
    async fn finish_digest_is_none_before_full_pass() {
        let mut state = StreamState::new(3 * CS, CS).unwrap();
        assert!(state.finish_digest().is_none());
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        drive(&mut state, &reader, &mut dp).await.unwrap();
        assert!(state.finish_digest().is_some());
    }

    // =================================================================
    // Issue #63 Stage 4 — depth-2 PREP-AHEAD pipeline.
    // =================================================================

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// Small chunk so `pattern()` for a whole chunk is far faster than the fake
    /// PUT delay — the overlap assertions then key off structure, not luck.
    const SMALL: u64 = 64 * 1024;

    /// A reader whose cumulative read count + ascending read log are observable
    /// from other threads (the producer thread reads through a clone).
    #[derive(Clone)]
    struct ObservedReader {
        reads: Arc<AtomicU64>,
        log: Arc<Mutex<Vec<u64>>>,
        opens: Arc<AtomicU64>,
    }
    impl ObservedReader {
        fn new() -> Self {
            Self {
                reads: Arc::new(AtomicU64::new(0)),
                log: Arc::new(Mutex::new(Vec::new())),
                opens: Arc::new(AtomicU64::new(0)),
            }
        }
        fn total(&self) -> u64 {
            self.reads.load(Ordering::SeqCst)
        }
        fn read_log(&self) -> Vec<u64> {
            self.log.lock().unwrap().clone()
        }
    }
    impl ChunkReader for ObservedReader {
        fn read_chunk(&self, index: u64, offset: u64, len: u64) -> Result<Vec<u8>, String> {
            self.log.lock().unwrap().push(index);
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(pattern(offset, len))
        }
    }

    /// A data plane that (a) makes each PUT take `put_delay` so a concurrent
    /// producer read has time to land, (b) records PUT order + how many source
    /// reads the reader had performed at each PUT's entry and exit, (c) tracks
    /// the max number of overlapping `put_chunk` bodies (structurally 1).
    struct TimedDataPlane {
        reader: ObservedReader,
        held: BTreeMap<u64, String>,
        put_order: Vec<u64>,
        in_flight: u32,
        max_in_flight: u32,
        reads_at_entry: Vec<(u64, u64)>,
        reads_at_exit: Vec<(u64, u64)>,
        put_delay: Duration,
        authdenied_once: BTreeSet<u64>,
        fired: BTreeSet<u64>,
    }
    impl TimedDataPlane {
        fn new(reader: ObservedReader, put_delay: Duration) -> Self {
            Self {
                reader,
                held: BTreeMap::new(),
                put_order: Vec::new(),
                in_flight: 0,
                max_in_flight: 0,
                reads_at_entry: Vec::new(),
                reads_at_exit: Vec::new(),
                put_delay,
                authdenied_once: BTreeSet::new(),
                fired: BTreeSet::new(),
            }
        }
        fn exit_reads_for(&self, index: u64) -> u64 {
            self.reads_at_exit.iter().find(|(i, _)| *i == index).map(|(_, r)| *r).unwrap()
        }
    }
    impl DataPlane for TimedDataPlane {
        async fn discover_resume(&mut self) -> ResumeStatus {
            ResumeStatus::Ok(vec![])
        }
        async fn put_chunk(&mut self, index: u64, digest_wire: &str, bytes: &[u8]) -> PutStatus {
            self.in_flight += 1;
            self.max_in_flight = self.max_in_flight.max(self.in_flight);
            self.put_order.push(index);
            self.reads_at_entry.push((index, self.reader.total()));
            tokio::time::sleep(self.put_delay).await;
            self.reads_at_exit.push((index, self.reader.total()));
            self.in_flight -= 1;

            if self.authdenied_once.contains(&index) && self.fired.insert(index) {
                return PutStatus::AuthDenied;
            }
            if crate::sha256_wire(bytes) != digest_wire {
                return PutStatus::DigestMismatch;
            }
            self.held.insert(index, digest_wire.to_string());
            PutStatus::Accepted
        }
    }

    fn factory_for(reader: &ObservedReader) -> impl FnOnce() -> Result<ObservedReader, String> + Send + 'static {
        let r = reader.clone();
        move || {
            r.opens.fetch_add(1, Ordering::SeqCst);
            Ok(r)
        }
    }

    /// RED: the committed serial pass CANNOT overlap prep(N+1) with PUT(N). For
    /// every chunk, the reader performs no additional read while that chunk's
    /// PUT is in flight — reads at PUT exit equal reads at PUT entry, and equal
    /// exactly `index + 1`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn red_serial_pass_never_prepares_next_chunk_during_a_put() {
        let total = 8 * SMALL;
        let reader = ObservedReader::new();
        let mut dp = TimedDataPlane::new(reader.clone(), Duration::from_millis(40));
        let mut state = StreamState::new(total, SMALL).unwrap();
        let out = run_stream_pass(&mut state, &reader, &mut dp, &mut |_| {}, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(out, PassOutcome::Complete);
        assert_eq!(dp.max_in_flight, 1);
        for (i, entry) in dp.reads_at_entry.clone() {
            let exit = dp.exit_reads_for(i);
            assert_eq!(entry, exit, "serial: no read landed during PUT({i})");
            assert_eq!(exit, i + 1, "serial: exactly chunks 0..={i} read by PUT({i}) exit");
        }
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
        assert_eq!(state.prepared_peak(), 0, "serial pass never uses the prep-ahead buffers");
    }

    /// GREEN: the depth-2 prep-ahead pass DOES read + hash chunk N+1 while
    /// PUT(N) is in flight — for every non-final chunk, reads at PUT exit are at
    /// least `index + 2`. Still one PUT body at a time, ascending PUT order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn green_prep_ahead_reads_and_hashes_next_chunk_during_a_put() {
        let total = 8 * SMALL;
        let n = total / SMALL;
        let reader = ObservedReader::new();
        let mut dp = TimedDataPlane::new(reader.clone(), Duration::from_millis(40));
        let mut state = StreamState::new(total, SMALL).unwrap();
        let out = run_stream_pass_prep_ahead(
            &mut state,
            factory_for(&reader),
            &mut dp,
            &mut |_| {},
            &mut |_| {},
        )
        .await
        .unwrap();
        assert_eq!(out, PassOutcome::Complete);

        assert_eq!(dp.max_in_flight, 1, "exactly one PUT body at a time");
        assert_eq!(dp.put_order, (0..n).collect::<Vec<_>>(), "PUTs ascending, each once");
        for i in 0..n - 1 {
            assert!(
                dp.exit_reads_for(i) >= i + 2,
                "prep-ahead: chunk {} read while PUT({i}) was in flight (exit reads = {})",
                i + 1,
                dp.exit_reads_for(i),
            );
        }
        // integrity
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
        assert_eq!(reader.read_log(), (0..n).collect::<Vec<_>>(), "each source chunk read once, ascending");
        assert_eq!(state.producer_read_log(), &(0..n).collect::<Vec<_>>()[..]);
        assert_eq!(state.prepared_peak(), 2, "depth-2 double-buffering actually engaged, never exceeded");
        assert_eq!(reader.opens.load(Ordering::SeqCst), 1, "producer opened its own source exactly once");
    }

    /// The prep-ahead digest is bit-identical to the serial digest over the same
    /// bounded source (short final chunk included).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prep_ahead_digest_matches_serial_digest_incl_short_final_chunk() {
        let total = 6 * SMALL + 777;
        let r1 = ObservedReader::new();
        let mut dp1 = TimedDataPlane::new(r1.clone(), Duration::from_millis(1));
        let mut s1 = StreamState::new(total, SMALL).unwrap();
        run_stream_pass(&mut s1, &r1, &mut dp1, &mut |_| {}, &mut |_| {}).await.unwrap();

        let r2 = ObservedReader::new();
        let mut dp2 = TimedDataPlane::new(r2.clone(), Duration::from_millis(1));
        let mut s2 = StreamState::new(total, SMALL).unwrap();
        run_stream_pass_prep_ahead(&mut s2, factory_for(&r2), &mut dp2, &mut |_| {}, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(s1.finish_digest().unwrap(), s2.finish_digest().unwrap());
        assert_eq!(s2.finish_digest().unwrap(), reference_digest(total));
    }

    /// Contamination stops the prep-ahead pass: an unexpected 401 on a PUT
    /// suspends the pass (and emits the contamination lifecycle event) rather
    /// than retrying to green.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prep_ahead_put_auth_denied_suspends_and_signals_contamination() {
        let total = 6 * SMALL;
        let reader = ObservedReader::new();
        let mut dp = TimedDataPlane::new(reader.clone(), Duration::from_millis(1));
        dp.authdenied_once.insert(3);
        let mut state = StreamState::new(total, SMALL).unwrap();

        let mut events = Vec::new();
        let out = run_stream_pass_prep_ahead(
            &mut state,
            factory_for(&reader),
            &mut dp,
            &mut |_| {},
            &mut |e| events.push(e),
        )
        .await
        .unwrap();

        assert_eq!(out, PassOutcome::SuspendedNeedsAuthorization);
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::PutAuthDenied { chunk_index: 3 })),
            "the 401 was surfaced as a contamination signal, not hidden"
        );
        assert!(!state.all_uploaded(), "the pass did not complete");
        assert!(state.prepared_peak() <= 2);
    }

    /// A producer read error fails the pass closed (no partial success).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prep_ahead_producer_open_failure_is_fatal_closed() {
        let total = 4 * SMALL;
        let reader = ObservedReader::new();
        let mut dp = TimedDataPlane::new(reader.clone(), Duration::from_millis(1));
        let mut state = StreamState::new(total, SMALL).unwrap();
        let factory = || -> Result<ObservedReader, String> {
            Err("simulated source open failure".to_string())
        };
        let res = run_stream_pass_prep_ahead(
            &mut state,
            factory,
            &mut dp,
            &mut |_| {},
            &mut |_| {},
        )
        .await;
        match res {
            Err(StreamError::Fatal(m)) => assert!(m.contains("open source")),
            other => panic!("expected Fatal, got {other:?}"),
        }
        assert!(!state.all_uploaded());
    }

    // =================================================================
    // Issue #63 window_8 candidate — prep-ahead source + bounded (<= 8)
    // concurrent chunk PUTs, per-PUT Worker durability semantics unchanged.
    // =================================================================

    /// Records the pointer of each source-produced chunk buffer, keyed by
    /// index — used to prove the owned payload is MOVED (never cloned) all
    /// the way from `ObservedReader::read_chunk` into the launcher's task.
    #[derive(Clone)]
    struct PtrTrackingReader {
        inner: ObservedReader,
        ptr_by_index: Arc<Mutex<BTreeMap<u64, usize>>>,
    }
    impl ChunkReader for PtrTrackingReader {
        fn read_chunk(&self, index: u64, offset: u64, len: u64) -> Result<Vec<u8>, String> {
            let bytes = self.inner.read_chunk(index, offset, len)?;
            self.ptr_by_index.lock().unwrap().insert(index, bytes.as_ptr() as usize);
            Ok(bytes)
        }
    }

    /// In-flight tracking shared between a launcher and its spawned tasks.
    /// `enter()` increments on creation and the returned guard decrements on
    /// `Drop` — including when tokio cancels (aborts) the task mid-`.await`,
    /// which drops the future's pinned locals. So `in_flight.load() == 0`
    /// after a pass returns proves every spawned PUT task actually terminated
    /// (naturally or via `abort_all`), never left running in the background.
    #[derive(Clone)]
    struct FlightTracker {
        in_flight: Arc<AtomicU64>,
        max_in_flight: Arc<AtomicU64>,
    }
    impl FlightTracker {
        fn new() -> Self {
            Self { in_flight: Arc::new(AtomicU64::new(0)), max_in_flight: Arc::new(AtomicU64::new(0)) }
        }
        fn enter(&self) -> FlightGuard {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            FlightGuard { in_flight: self.in_flight.clone() }
        }
    }
    struct FlightGuard {
        in_flight: Arc<AtomicU64>,
    }
    impl Drop for FlightGuard {
        fn drop(&mut self) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// A [`WindowedPutLauncher`] whose PUTs complete after a per-index delay
    /// (so the test controls completion order independently of start order),
    /// records the pointer of the bytes it actually receives (proving no
    /// clone happened on the way in), and can be told to fire exactly one
    /// `AuthDenied` / `Transient` outcome for a chosen index.
    #[derive(Clone)]
    struct WindowFakeLauncher {
        held: Arc<Mutex<BTreeMap<u64, String>>>,
        tracker: FlightTracker,
        launcher_ptr_by_index: Arc<Mutex<BTreeMap<u64, usize>>>,
        completion_order: Arc<Mutex<Vec<u64>>>,
        delay_ms: Arc<dyn Fn(u64) -> u64 + Send + Sync>,
        authdenied_once: Arc<Mutex<BTreeSet<u64>>>,
        fired_authdenied: Arc<Mutex<BTreeSet<u64>>>,
        transient_once: Arc<Mutex<BTreeSet<u64>>>,
        fired_transient: Arc<Mutex<BTreeSet<u64>>>,
    }
    impl WindowFakeLauncher {
        fn new(delay_ms: impl Fn(u64) -> u64 + Send + Sync + 'static) -> Self {
            Self {
                held: Arc::new(Mutex::new(BTreeMap::new())),
                tracker: FlightTracker::new(),
                launcher_ptr_by_index: Arc::new(Mutex::new(BTreeMap::new())),
                completion_order: Arc::new(Mutex::new(Vec::new())),
                delay_ms: Arc::new(delay_ms),
                authdenied_once: Arc::new(Mutex::new(BTreeSet::new())),
                fired_authdenied: Arc::new(Mutex::new(BTreeSet::new())),
                transient_once: Arc::new(Mutex::new(BTreeSet::new())),
                fired_transient: Arc::new(Mutex::new(BTreeSet::new())),
            }
        }
        fn max_in_flight(&self) -> u64 {
            self.tracker.max_in_flight.load(Ordering::SeqCst)
        }
        fn in_flight_now(&self) -> u64 {
            self.tracker.in_flight.load(Ordering::SeqCst)
        }
        fn completion_order(&self) -> Vec<u64> {
            self.completion_order.lock().unwrap().clone()
        }
    }
    impl WindowedPutLauncher for WindowFakeLauncher {
        fn start_put(
            &mut self,
            index: u64,
            digest_wire: String,
            bytes: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = PutStatus> + Send>> {
            self.launcher_ptr_by_index
                .lock()
                .unwrap()
                .insert(index, bytes.as_ptr() as usize);
            let held = self.held.clone();
            let tracker = self.tracker.clone();
            let completion_order = self.completion_order.clone();
            let delay_ms = (self.delay_ms)(index);
            let fire_authdenied = self.authdenied_once.lock().unwrap().contains(&index)
                && self.fired_authdenied.lock().unwrap().insert(index);
            let fire_transient = self.transient_once.lock().unwrap().contains(&index)
                && self.fired_transient.lock().unwrap().insert(index);
            // NOT `tokio::spawn` here — a plain future. `window.spawn` in the
            // driver turns this into the ONE real task, so aborting it there
            // actually cancels this body (including dropping `_guard` — the
            // requirement this test module exists to prove).
            Box::pin(async move {
                let _guard = tracker.enter();
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                completion_order.lock().unwrap().push(index);
                if fire_authdenied {
                    return PutStatus::AuthDenied;
                }
                if fire_transient {
                    return PutStatus::Transient("injected".into());
                }
                if crate::sha256_wire(&bytes) != digest_wire {
                    return PutStatus::DigestMismatch;
                }
                held.lock().unwrap().insert(index, digest_wire);
                PutStatus::Accepted
            })
        }
    }

    fn w8_factory(reader: &PtrTrackingReader) -> impl FnOnce() -> Result<PtrTrackingReader, String> + Send + 'static {
        let r = reader.clone();
        move || Ok(r)
    }

    fn w8_reader(ptr_by_index: &Arc<Mutex<BTreeMap<u64, usize>>>) -> (ObservedReader, PtrTrackingReader) {
        let inner = ObservedReader::new();
        let tracked = PtrTrackingReader { inner: inner.clone(), ptr_by_index: ptr_by_index.clone() };
        (inner, tracked)
    }

    /// GREEN: window_8 reaches strictly more than 1 and at most 8 PUTs
    /// simultaneously in flight (requirement: max network in-flight <= 8, and
    /// this candidate must actually reach > 1 physically).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn window8_reaches_more_than_one_and_at_most_eight_puts_in_flight() {
        let n = 16u64;
        let total = n * SMALL;
        let ptrs = Arc::new(Mutex::new(BTreeMap::new()));
        let (_inner, reader) = w8_reader(&ptrs);
        let mut state = StreamState::new(total, SMALL).unwrap();
        let mut launcher = WindowFakeLauncher::new(|_i| 40);

        let out = run_stream_pass_window8(
            &mut state,
            w8_factory(&reader),
            ResumeStatus::Ok(vec![]),
            &mut launcher,
            &mut |_| {},
            &mut |_| {},
        )
        .await
        .unwrap();

        assert_eq!(out, PassOutcome::Complete);
        assert!(launcher.max_in_flight() > 1, "must actually reach concurrency > 1");
        assert!(launcher.max_in_flight() <= PUT_WINDOW, "must never exceed the window");
        // `state.peak_puts_in_flight()` is an upper bound (unreaped JoinSet
        // length can lag real completions) — it must dominate the real
        // observed concurrency and still respect the window cap.
        assert!(state.peak_puts_in_flight() >= launcher.max_in_flight());
        assert!(state.peak_puts_in_flight() <= PUT_WINDOW);
        assert_eq!(launcher.in_flight_now(), 0, "no PUT task left running after the pass");
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    /// PUT starts are strictly ascending regardless of window depth.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn window8_put_starts_are_strictly_ascending() {
        let n = 20u64;
        let total = n * SMALL;
        let ptrs = Arc::new(Mutex::new(BTreeMap::new()));
        let (_inner, reader) = w8_reader(&ptrs);
        let mut state = StreamState::new(total, SMALL).unwrap();
        let mut launcher = WindowFakeLauncher::new(|_i| 5);

        run_stream_pass_window8(
            &mut state,
            w8_factory(&reader),
            ResumeStatus::Ok(vec![]),
            &mut launcher,
            &mut |_| {},
            &mut |_| {},
        )
        .await
        .unwrap();

        assert_eq!(state.put_start_order(), &(0..n).collect::<Vec<_>>()[..]);
        assert!(state.put_starts_ascending());
        assert_eq!(state.put_started_count(), n);
        assert_eq!(state.put_completed_count(), n);
        assert_eq!(state.put_window(), PUT_WINDOW);
    }

    /// Completions MAY land out of order (a later-started chunk given a
    /// SHORTER delay finishes first); the rolling Artifact digest is still
    /// bit-identical to the serial reference because the producer thread
    /// alone owns the hasher, in ascending index order — completion order
    /// never touches it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn window8_completions_out_of_order_still_yield_the_serial_digest() {
        let n = 10u64;
        let total = n * SMALL;
        let ptrs = Arc::new(Mutex::new(BTreeMap::new()));
        let (_inner, reader) = w8_reader(&ptrs);
        let mut state = StreamState::new(total, SMALL).unwrap();
        // chunk 0 is deliberately the SLOWEST PUT; later chunks finish first.
        let mut launcher = WindowFakeLauncher::new(|i| if i == 0 { 120 } else { 5 });

        let out = run_stream_pass_window8(
            &mut state,
            w8_factory(&reader),
            ResumeStatus::Ok(vec![]),
            &mut launcher,
            &mut |_| {},
            &mut |_| {},
        )
        .await
        .unwrap();

        assert_eq!(out, PassOutcome::Complete);
        let completion = launcher.completion_order();
        assert_ne!(
            completion.first(),
            Some(&0),
            "chunk 0 must NOT be the first completion despite being the first PUT started"
        );
        assert_eq!(state.put_start_order(), &(0..n).collect::<Vec<_>>()[..], "starts stay ascending");
        // completion order is a permutation of every chunk index exactly once.
        let mut sorted = completion.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..n).collect::<Vec<_>>());
        assert_eq!(
            state.finish_digest().unwrap(),
            reference_digest(total),
            "out-of-order completion must not perturb the rolling Artifact digest"
        );
    }

    /// The owned 64 MiB payload is MOVED end-to-end (source read -> prepared
    /// channel -> drive loop -> launcher task): the pointer the reader
    /// produced is byte-identical to the pointer the launcher's task actually
    /// received, for every chunk. No clone occurred on the hot path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn window8_owned_put_path_never_clones_the_chunk_buffer() {
        let n = 12u64;
        let total = n * SMALL;
        let ptrs = Arc::new(Mutex::new(BTreeMap::new()));
        let (_inner, reader) = w8_reader(&ptrs);
        let mut state = StreamState::new(total, SMALL).unwrap();
        let mut launcher = WindowFakeLauncher::new(|_i| 3);

        run_stream_pass_window8(
            &mut state,
            w8_factory(&reader),
            ResumeStatus::Ok(vec![]),
            &mut launcher,
            &mut |_| {},
            &mut |_| {},
        )
        .await
        .unwrap();

        let produced = ptrs.lock().unwrap().clone();
        let received = launcher.launcher_ptr_by_index.lock().unwrap().clone();
        assert_eq!(produced.len(), n as usize);
        assert_eq!(received.len(), n as usize);
        for i in 0..n {
            assert_eq!(
                produced.get(&i),
                received.get(&i),
                "chunk {i}: launcher received a DIFFERENT allocation than the source produced (a clone happened)"
            );
        }
    }

    /// CLEAN-FAST-PATH FAILURE POLICY: an unexpected 401 on one PUT suspends
    /// the whole pass (contamination), no retry-to-green, and every other
    /// outstanding PUT task is drained (aborted + joined) before returning —
    /// none is left running in the background, and the pass never reaches
    /// `Complete` (so the caller never seals).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn window8_auth_denied_suspends_drains_outstanding_tasks_no_seal() {
        let n = 16u64;
        let total = n * SMALL;
        let ptrs = Arc::new(Mutex::new(BTreeMap::new()));
        let (_inner, reader) = w8_reader(&ptrs);
        let mut state = StreamState::new(total, SMALL).unwrap();
        // Long delay on every PUT so several are genuinely in flight when
        // chunk 3's 401 lands; chunk 3 itself resolves quickly.
        let mut launcher = WindowFakeLauncher::new(|i| if i == 3 { 5 } else { 200 });
        launcher.authdenied_once.lock().unwrap().insert(3);

        let mut events = Vec::new();
        let out = run_stream_pass_window8(
            &mut state,
            w8_factory(&reader),
            ResumeStatus::Ok(vec![]),
            &mut launcher,
            &mut |_| {},
            &mut |e| events.push(e),
        )
        .await
        .unwrap();

        assert_eq!(out, PassOutcome::SuspendedNeedsAuthorization);
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::PutAuthDenied { chunk_index: 3 })),
            "the 401 was surfaced as a contamination signal, not hidden"
        );
        assert!(!state.all_uploaded(), "the pass did not complete — no seal is possible");
        assert!(state.finish_digest().is_none(), "no Artifact digest is available on a suspended pass");
        assert_eq!(
            launcher.in_flight_now(),
            0,
            "every outstanding PUT task must be drained before the pass returns"
        );
    }

    /// A non-empty resume is rejected fail-closed (window_8 is a clean fast
    /// path only, mirroring the prep-ahead depth-2 contract).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn window8_unexpected_non_empty_resume_fails_closed() {
        let total = 4 * SMALL;
        let ptrs = Arc::new(Mutex::new(BTreeMap::new()));
        let (_inner, reader) = w8_reader(&ptrs);
        let mut state = StreamState::new(total, SMALL).unwrap();
        let mut launcher = WindowFakeLauncher::new(|_i| 1);
        let resume = ResumeStatus::Ok(vec![(0, "x".to_string())]);
        let res = run_stream_pass_window8(
            &mut state,
            w8_factory(&reader),
            resume,
            &mut launcher,
            &mut |_| {},
            &mut |_| {},
        )
        .await;
        match res {
            Err(StreamError::Fatal(m)) => assert!(m.contains("non-empty resume")),
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    /// The live payload-buffer bound never exceeds `PUT_WINDOW + 1` (<= 8
    /// unacknowledged PUT payloads + <= 1 producer/current chunk).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn window8_payload_buffer_peak_stays_within_window_plus_one() {
        let n = 24u64;
        let total = n * SMALL;
        let ptrs = Arc::new(Mutex::new(BTreeMap::new()));
        let (_inner, reader) = w8_reader(&ptrs);
        let mut state = StreamState::new(total, SMALL).unwrap();
        let mut launcher = WindowFakeLauncher::new(|_i| 15);

        run_stream_pass_window8(
            &mut state,
            w8_factory(&reader),
            ResumeStatus::Ok(vec![]),
            &mut launcher,
            &mut |_| {},
            &mut |_| {},
        )
        .await
        .unwrap();

        assert!(
            state.prepared_peak() <= PUT_WINDOW + 1,
            "live payload depth {} exceeded window+1",
            state.prepared_peak()
        );
    }
}
