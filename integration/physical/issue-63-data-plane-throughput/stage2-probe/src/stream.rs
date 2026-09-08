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
            prepared_peak: 0,
            finalized_digest: None,
            producer_read_log: Vec::new(),
        })
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
}
