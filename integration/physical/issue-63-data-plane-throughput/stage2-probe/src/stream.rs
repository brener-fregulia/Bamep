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
        })
    }

    pub fn chunk_count(&self) -> u64 {
        self.chunk_count
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
    pub fn finish_digest(&self) -> Option<String> {
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
}
