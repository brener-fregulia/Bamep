//! Issue #61 CP7A — the single-pass streaming chunk loop.
//!
//! THROWAWAY Spike logic. This module owns the CP7A INVARIANT and is where the
//! host invariant tests live:
//!
//!   * each logical source chunk is read AT MOST ONCE during the pass;
//!   * each logical source chunk enters the rolling full-Artifact SHA-256
//!     EXACTLY ONCE, in ascending index order;
//!   * a transport/auth retry for one chunk reuses the SAME buffered bytes and
//!     the SAME chunk digest — no re-read, no re-hash, no source-offset advance,
//!     and no durable-progress increment until `held` is confirmed;
//!   * if resume discovery after an uncertain PUT reports the current chunk
//!     already held with the expected digest, that completes the logical chunk
//!     WITHOUT a second PUT; a different held digest fails closed.
//!
//! The loop is generic over a [`ChunkReader`] (the physical source) and a
//! [`DataPlane`] (the real Worker HTTPS surface, or a fault-injecting fake), so
//! the invariant is proven host-side before any WinPE binary is staged.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// Spike-local retry pacing. The real interruption/resume recovery is dominated
/// by the outer fresh-grant round trip; this only bounds a brief local wobble.
const MAX_LOCAL_PUT_RETRIES: u32 = 3;
const LOCAL_BACKOFF: Duration = Duration::from_millis(50);

/// The physical (or fake) source. `read_chunk` returns exactly `len` bytes for
/// `[offset, offset + len)` and MUST be deterministic for a given `(offset,
/// len)`.
pub trait ChunkReader {
    fn read_chunk(&self, index: u64, offset: u64, len: u64) -> Result<Vec<u8>, String>;
}

/// The Worker-owned HTTPS data plane, reduced to the two operations the pass
/// needs. `&mut self` so a real implementation may hold per-connection state.
#[allow(async_fn_in_trait)]
pub trait DataPlane {
    async fn discover_resume(&mut self) -> ResumeStatus;
    async fn put_chunk(&mut self, index: u64, digest_wire: &str, bytes: &[u8]) -> PutStatus;
}

/// Outcome of one `GET /chunks` resume-discovery. The `Transient`/`Fatal`
/// strings are diagnostic detail surfaced by the real implementation.
#[allow(dead_code)]
pub enum ResumeStatus {
    /// `(chunk_index, digest_wire)` for every durably held + verified chunk.
    Ok(Vec<(u64, String)>),
    /// `401` — the single non-enumerable denial.
    AuthDenied,
    /// A transport-level failure — never proof of any durable state.
    Transient(String),
    /// An off-contract response, or a manifest fact that disagrees with this
    /// pass (wrong `chunk_size`, already sealed).
    Fatal(String),
}

/// Outcome of one `PUT /chunks/{index}`. The `Transient`/`Fatal` strings are
/// diagnostic detail surfaced by the real implementation.
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

/// A terminal, fail-closed pass error.
#[derive(Debug)]
pub enum StreamError {
    /// A recorded chunk identity could not be reproduced, or the Worker's
    /// independent hash rejected the bytes — `CHUNK_VERIFICATION_FAILED`.
    ChunkVerificationFailed { index: u64 },
    /// A non-specific terminal inability to continue — `TRANSFER_ABANDONED`.
    Fatal(String),
}

/// A non-terminal stop the caller resolves (fresh grant / Worker recovery) then
/// re-enters `run_stream_pass` with the SAME [`StreamState`].
#[derive(Debug, PartialEq, Eq)]
pub enum PassOutcome {
    Complete,
    SuspendedNeedsAuthorization,
    SuspendedDataPlaneUnreachable,
}

/// One progress observation: cumulative durably-held bytes/chunks. Never
/// increases past the total before every chunk is durably held.
#[derive(Debug, Clone, Copy)]
pub struct ProgressTick {
    pub held_bytes: u64,
    pub held_chunks: u64,
}

/// Spike-only lifecycle observability for the CP7A Gate-4 subtests. NOT a
/// general event framework — it carries only the few facts the physical
/// evidence needs. The probe `main` logs these; host tests collect and assert
/// them. No secrets ever flow through here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// A resume-discovery request is about to be sent.
    ResumeBegin,
    /// The resume-discovery outcome. `held_chunks`/`sealed` are meaningful
    /// only for `outcome == "approved"`.
    ResumeResult { outcome: &'static str, held_chunks: u64, sealed: bool },
    /// Emitted after the ENTRY reconciliation folds the durable held set in.
    ResumeReconciled {
        held_count: u64,
        pending_chunk_index: Option<u64>,
        pending_already_held: bool,
    },
    /// The current chunk's PUT was answered with the generic `401`.
    PutAuthDenied { chunk_index: u64 },
    /// A transport-level PUT failure inside the bounded local-retry envelope
    /// (observability only — no behaviour change).
    PutTransient { chunk_index: u64, local_attempt: u32, detail: String },
    /// A transport-level resume failure inside the bounded local-retry
    /// envelope (observability only).
    ResumeTransient { local_attempt: u32, detail: String },
}

#[derive(Clone, Debug)]
struct ChunkFacts {
    digest_wire: String,
    size: u64,
}

/// The in-memory pass state. It is the durable-across-suspension record: the
/// rolling hash, the `hashed_through` cursor, the per-index reproduced facts,
/// the confirmed-held set, and the single in-flight buffered chunk. It survives
/// any number of `run_stream_pass` re-entries within one probe process.
pub struct StreamState {
    rolling: Sha256,
    /// The next index whose bytes have NOT yet entered `rolling`. Advances by
    /// exactly one each time the forward cursor first reaches an index.
    hashed_through: u64,
    processed: BTreeMap<u64, ChunkFacts>,
    held: BTreeSet<u64>,
    /// The one chunk that has been read + hashed but not yet confirmed held.
    /// Retained across a suspension so re-entry never re-reads it.
    pending: Option<(u64, Vec<u8>)>,
    total_len: u64,
    chunk_size: u64,
    chunk_count: u64,
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
        })
    }

    pub fn chunk_count(&self) -> u64 {
        self.chunk_count
    }

    pub fn held_count(&self) -> u64 {
        self.held.len() as u64
    }

    /// The index of the single in-flight buffered chunk retained across a
    /// suspension, if any. Spike observability only.
    pub fn pending_index(&self) -> Option<u64> {
        self.pending.as_ref().map(|(i, _)| *i)
    }

    /// Whether `index` is in the durably-confirmed held set. Spike
    /// observability only.
    pub fn is_held(&self, index: u64) -> bool {
        self.held.contains(&index)
    }

    pub fn all_uploaded(&self) -> bool {
        self.held.len() as u64 == self.chunk_count
    }

    /// The exact byte length of chunk `index` under the M1 size formula:
    /// every chunk except the last is `chunk_size`; the last is `1..=chunk_size`.
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

    /// The rolling full-Artifact digest (`SHA-256(chunk0 || chunk1 || ...)`),
    /// canonical base64url-no-pad — `Some` only once every chunk has been
    /// hashed exactly once.
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

    /// Fold durable held-chunk facts from a resume response into the state,
    /// failing closed on any identity we cannot reproduce.
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
                // The Server holds a chunk this pass never read/hashed. For a
                // fresh CP7A transfer this is impossible; fail closed rather
                // than trust an unverifiable identity.
                None => return Err(StreamError::ChunkVerificationFailed { index: idx }),
            }
            self.mark_held(idx);
        }
        Ok(())
    }

    /// Obtain this logical chunk's bytes exactly once for the forward pass, or
    /// re-read (without re-hashing) for a re-upload of an already-hashed index.
    fn bytes_for<R: ChunkReader>(
        &mut self,
        index: u64,
        reader: &R,
    ) -> Result<Vec<u8>, StreamError> {
        // The one in-flight buffered chunk — never re-read.
        if let Some((pidx, pbytes)) = &self.pending {
            if *pidx == index {
                return Ok(pbytes.clone());
            }
        }
        if index == self.hashed_through {
            let len = self.expected_len(index);
            let offset = index * self.chunk_size;
            let bytes = reader
                .read_chunk(index, offset, len)
                .map_err(|e| StreamError::Fatal(format!("read chunk {index}: {e}")))?;
            if bytes.len() as u64 != len {
                return Err(StreamError::Fatal(format!(
                    "chunk {index}: source returned {} bytes, expected {len}",
                    bytes.len()
                )));
            }
            self.rolling.update(&bytes); // <-- the ONLY rolling.update for this index
            let digest_wire = crate::sha256_wire(&bytes);
            self.processed
                .insert(index, ChunkFacts { digest_wire, size: len });
            self.hashed_through = index + 1;
            self.pending = Some((index, bytes.clone()));
            Ok(bytes)
        } else if index < self.hashed_through {
            // Re-upload of an already-hashed index (an uncertain Accepted that
            // did not persist). Re-read, validate against the recorded digest,
            // NEVER re-hash into the rolling digest.
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
    // Entry reconciliation. Harmless on the first call (a fresh transfer holds
    // nothing); on re-entry it folds in whatever crossed durably before the
    // suspension.
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
                    // The uncertain PUT may have landed durably — reconcile.
                    // A denied bodyless resume here is exactly how the Gate-4
                    // auth-denial episode reaches the outer suspend path.
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
                            // still missing -> retry the SAME buffered bytes
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
// Host invariant tests + mutation-based RED/sensitivity verification.
//
// These tests assert the single-pass invariant directly. They were authored
// alongside a working implementation, not strictly test-first; the RED
// evidence is produced separately by deliberately mutating this module
// (physical reread, duplicate rolling-hash update, uncertain-PUT re-send,
// held-digest mismatch) and observing the relevant test fail. That is
// sensitivity verification, not a chronological test-first RED->GREEN cycle.
// =====================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const CS: u64 = 8 * 1024 * 1024;

    /// Deterministic, position-keyed source pattern. Byte at absolute position
    /// `p` is a pure function of `p`, so any partition concatenates to the
    /// whole — matching `crate::sources::pattern_bytes` for the non-Windows
    /// build.
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

    /// The reference full-Artifact digest, computed independently by streaming
    /// the exact bounded source bytes.
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
            Self {
                reads: RefCell::new(BTreeMap::new()),
            }
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
        resume_calls: u32,
        /// First N PUTs of an index -> Transient (>= MAX_LOCAL_PUT_RETRIES + 1
        /// forces one outer suspension).
        transient_put_budget: BTreeMap<u64, u32>,
        /// First PUT of an index -> AuthDenied (fires once).
        authdenied_put_once: BTreeSet<u64>,
        fired_authdenied: BTreeSet<u64>,
        /// PUT of this index records the chunk as held, then reports Transient.
        uncertain_put: Option<u64>,
        uncertain_fired: bool,
        /// Overrides the digest stored for the uncertain PUT (fail-closed case).
        corrupt_held_digest: Option<String>,
        /// First N resume calls -> Transient.
        transient_resume_first_n: u32,
        /// The Nth resume call (1-based) -> AuthDenied, fired at most once.
        /// Models the Gate-4 auth-denial episode's denial #2 landing on the
        /// probe's bodyless post-transient discover_resume.
        authdenied_resume_at_call: Option<u32>,
    }
    impl FakeDataPlane {
        fn new() -> Self {
            Self {
                held: BTreeMap::new(),
                put_calls: BTreeMap::new(),
                resume_calls: 0,
                transient_put_budget: BTreeMap::new(),
                authdenied_put_once: BTreeSet::new(),
                fired_authdenied: BTreeSet::new(),
                uncertain_put: None,
                uncertain_fired: false,
                corrupt_held_digest: None,
                transient_resume_first_n: 0,
                authdenied_resume_at_call: None,
            }
        }
        fn put_calls_for(&self, index: u64) -> u32 {
            *self.put_calls.get(&index).unwrap_or(&0)
        }
    }
    impl DataPlane for FakeDataPlane {
        async fn discover_resume(&mut self) -> ResumeStatus {
            self.resume_calls += 1;
            if self.authdenied_resume_at_call == Some(self.resume_calls) {
                self.authdenied_resume_at_call = None; // fire once
                return ResumeStatus::AuthDenied;
            }
            if self.resume_calls <= self.transient_resume_first_n {
                return ResumeStatus::Transient("injected".into());
            }
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
        drive_collecting(state, reader, dp, &mut Vec::new()).await
    }

    /// Same as [`drive`] but records every [`StreamEvent`] across all
    /// re-entries, for the Gate-4 subtest A lifecycle-evidence tests.
    async fn drive_collecting(
        state: &mut StreamState,
        reader: &FakeReader,
        dp: &mut FakeDataPlane,
        events: &mut Vec<StreamEvent>,
    ) -> Result<u32, StreamError> {
        let mut suspensions = 0u32;
        for _ in 0..40 {
            let outcome =
                run_stream_pass(state, reader, dp, &mut |_t| {}, &mut |e| events.push(e)).await?;
            match outcome {
                PassOutcome::Complete => return Ok(suspensions),
                // "obtain a fresh grant" / "wait for Worker recovery" — the
                // fault injections above self-clear, so re-entry makes progress.
                PassOutcome::SuspendedNeedsAuthorization
                | PassOutcome::SuspendedDataPlaneUnreachable => suspensions += 1,
            }
        }
        Err(StreamError::Fatal("too many suspensions".into()))
    }

    // ---- A — exactly-once physical read ----------------------------------
    #[tokio::test]
    async fn a_exactly_once_physical_read_across_transient_and_authdenied() {
        let total = 5 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(2, 1);
        dp.authdenied_put_once.insert(3);
        let mut state = StreamState::new(total, CS).unwrap();

        drive(&mut state, &reader, &mut dp).await.unwrap();

        assert!(state.all_uploaded());
        for i in 0..5 {
            assert_eq!(reader.read_count(i), 1, "chunk {i} must be read exactly once");
        }
        assert_eq!(reader.total_reads(), 5);
    }

    // ---- B — exactly-once rolling hash ----------------------------------
    #[tokio::test]
    async fn b_rolling_hash_is_exactly_once_per_chunk_regardless_of_retries() {
        let total = 7 * CS + 111; // deliberately a short final chunk
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(1, 2);
        dp.authdenied_put_once.insert(4);
        dp.uncertain_put = Some(5);
        let mut state = StreamState::new(total, CS).unwrap();

        drive(&mut state, &reader, &mut dp).await.unwrap();

        assert_eq!(
            state.finish_digest().unwrap(),
            reference_digest(total),
            "rolling digest must equal SHA-256 of the exact bounded source bytes"
        );
    }

    // ---- C — retry reuses the same buffer, no re-read, no re-hash -------
    #[tokio::test]
    async fn c_transient_retry_reuses_the_same_buffer() {
        let total = 4 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(2, 2); // 2 transient PUTs, then success
        let mut state = StreamState::new(total, CS).unwrap();

        drive(&mut state, &reader, &mut dp).await.unwrap();

        assert_eq!(reader.read_count(2), 1, "no re-read on transient retry");
        assert_eq!(dp.put_calls_for(2), 3, "2 transient PUTs + 1 success");
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    // ---- D — uncertain PUT + held-on-resume => no second PUT -----------
    #[tokio::test]
    async fn d_uncertain_put_held_on_resume_is_not_reput() {
        let total = 4 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.uncertain_put = Some(2);
        let mut state = StreamState::new(total, CS).unwrap();

        drive(&mut state, &reader, &mut dp).await.unwrap();

        assert!(state.all_uploaded());
        assert_eq!(
            dp.put_calls_for(2),
            1,
            "the uncertain PUT is confirmed via resume, never re-sent"
        );
        assert_eq!(reader.read_count(2), 1);
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    // ---- D2 — uncertain PUT held with a DIFFERENT digest => fail closed
    #[tokio::test]
    async fn d2_uncertain_put_held_with_wrong_digest_fails_closed() {
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

    // ---- E — bounded extent: 257 chunks, final 1 MiB (structural) ------
    #[test]
    fn e_bounded_extent_arithmetic_257_chunks_final_1mib() {
        let total: u64 = 2_148_532_224;
        let cs: u64 = 8_388_608;
        let state = StreamState::new(total, cs).unwrap();

        assert_eq!(state.chunk_count(), 257);
        for i in 0..256 {
            assert_eq!(state.expected_len(i), 8_388_608, "chunk {i} is a full chunk");
        }
        assert_eq!(state.expected_len(256), 1_048_576, "final chunk is exactly 1 MiB");
        assert_eq!(
            256 * 8_388_608u64 + 1_048_576,
            2_148_532_224,
            "the CP7A arithmetic itself"
        );
    }

    // ---- E (full) — the exact 2,148,532,224-byte digest + exactly-once
    // read across a real suspension/resume. Heavy (streams ~2.15 GB several
    // times); `#[ignore]` by default, run explicitly in `--release` for the
    // full-extent digest evidence:
    // `cargo test --release -- --ignored e_full_bounded`.
    #[tokio::test]
    #[ignore]
    async fn e_full_bounded_digest_and_exactly_once_across_interruption() {
        let total: u64 = 2_148_532_224;
        let cs: u64 = 8_388_608;
        let mut state = StreamState::new(total, cs).unwrap();
        assert_eq!(state.chunk_count(), 257);

        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        // Simulate the one controlled Worker-listener interruption around chunk 8.
        dp.transient_put_budget.insert(8, MAX_LOCAL_PUT_RETRIES + 1);
        dp.transient_resume_first_n = 1;

        let suspensions = drive(&mut state, &reader, &mut dp).await.unwrap();

        assert!(state.all_uploaded());
        assert_eq!(state.held_count(), 257);
        assert!(
            suspensions >= 1,
            "the interruption must produce a real suspension/resume"
        );
        for i in 0..257 {
            assert_eq!(
                reader.read_count(i),
                1,
                "chunk {i} read exactly once across the interruption"
            );
        }
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    // =================================================================
    // Gate-4 Subtest A — deterministic one-shot AuthDenied on the CURRENT
    // chunk's PUT, then suspend -> resume -> continue with the SAME state.
    // Makes the suspension boundary itself explicit (not relying only on
    // the broad `a_exactly_once_...` test).
    // =================================================================

    /// The REVISED Gate-4 Subtest A shape: a chunk PUT that the auth-denial
    /// episode's denial #1 turns into a transport `Transient` (8 MiB body
    /// in-flight when the Worker rejects), then the probe's bodyless
    /// post-transient resume gets denial #2 as a clean `AuthDenied`, which is
    /// what actually reaches the outer suspend path. Manual pass-by-pass so
    /// the suspension boundary is asserted directly.
    #[tokio::test]
    async fn authdenial_episode_transient_put_then_authdenied_resume_suspends() {
        const K: u64 = 8;
        let total = 12 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(K, 1); // denial #1: PUT(K) fails once at transport
        dp.authdenied_resume_at_call = Some(2); // denial #2: post-transient resume (call #2)
        let mut state = StreamState::new(total, CS).unwrap();

        // ---- pass 1: 0..K accepted; PUT(K) transient; resume auth_denied ----
        let mut ev1 = Vec::new();
        let out1 =
            run_stream_pass(&mut state, &reader, &mut dp, &mut |_t| {}, &mut |e| ev1.push(e))
                .await
                .unwrap();
        assert_eq!(out1, PassOutcome::SuspendedNeedsAuthorization);
        assert_eq!(reader.read_count(K), 1, "chunk K read once before suspension");
        assert_eq!(state.pending_index(), Some(K), "denied chunk is the pending buffer");
        assert!(!state.is_held(K), "denied chunk is NOT durably held");
        assert_eq!(state.held_count(), K, "chunks 0..K held after pass 1");
        assert_eq!(
            ev1.iter()
                .filter(|e| matches!(e, StreamEvent::PutTransient { .. }))
                .count(),
            1,
            "exactly one PutTransient for the denied chunk; got {ev1:?}"
        );
        assert!(
            ev1.contains(&StreamEvent::PutTransient {
                chunk_index: K,
                local_attempt: 1,
                detail: "injected".into(),
            }),
            "PutTransient{{K, attempt 1}}; got {ev1:?}"
        );
        assert!(
            ev1.contains(&StreamEvent::ResumeResult {
                outcome: "auth_denied",
                held_chunks: 0,
                sealed: false,
            }),
            "the post-transient resume must surface auth_denied; got {ev1:?}"
        );
        assert!(
            !ev1.iter().any(|e| matches!(e, StreamEvent::PutAuthDenied { .. })),
            "the PUT itself is a transport transient here, not PutAuthDenied"
        );

        // ---- pass 2: recovery — resume approved, reconcile, continue ----
        let mut ev2 = Vec::new();
        let out2 =
            run_stream_pass(&mut state, &reader, &mut dp, &mut |_t| {}, &mut |e| ev2.push(e))
                .await
                .unwrap();
        assert_eq!(out2, PassOutcome::Complete);
        assert_eq!(ev2.first(), Some(&StreamEvent::ResumeBegin), "recovery begins with resume");
        assert!(
            ev2.contains(&StreamEvent::ResumeResult {
                outcome: "approved",
                held_chunks: K,
                sealed: false,
            }),
            "recovery resume reports the K already-held chunks; got {ev2:?}"
        );
        assert!(
            ev2.contains(&StreamEvent::ResumeReconciled {
                held_count: K,
                pending_chunk_index: Some(K),
                pending_already_held: false,
            }),
            "recovery reconcile: pending chunk K still missing; got {ev2:?}"
        );

        // ---- invariants ----
        assert!(state.all_uploaded());
        for i in 0..12 {
            assert_eq!(reader.read_count(i), 1, "chunk {i} read once across the suspension");
        }
        assert_eq!(dp.put_calls_for(K), 2, "chunk K: one transient PUT + one accepted PUT");
        assert_eq!(
            state.finish_digest().unwrap(),
            reference_digest(total),
            "no chunk rolled into the Artifact digest twice"
        );
    }

    /// The `drive`-loop form, asserting the aggregate lifecycle facts a
    /// physical Gate-4 run must show.
    #[tokio::test]
    async fn authdenial_episode_emits_lifecycle_facts() {
        const K: u64 = 8;
        let total = 10 * CS + 777;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(K, 1);
        dp.authdenied_resume_at_call = Some(2);
        let mut state = StreamState::new(total, CS).unwrap();

        let mut events = Vec::new();
        let suspensions = drive_collecting(&mut state, &reader, &mut dp, &mut events)
            .await
            .unwrap();

        assert_eq!(suspensions, 1, "exactly one suspension");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::PutTransient { chunk_index, .. } if *chunk_index == K))
                .count(),
            1,
            "exactly one PutTransient on the denied chunk"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    StreamEvent::ResumeResult { outcome: "auth_denied", .. }
                ))
                .count(),
            1,
            "exactly one auth_denied resume result"
        );
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::PutAuthDenied { .. })),
            "Subtest A normally contains ZERO PutAuthDenied events"
        );
        let reconciled: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ResumeReconciled {
                    held_count,
                    pending_chunk_index,
                    pending_already_held,
                } => Some((*held_count, *pending_chunk_index, *pending_already_held)),
                _ => None,
            })
            .collect();
        assert!(reconciled.contains(&(0, None, false)), "entry reconcile; got {reconciled:?}");
        assert!(
            reconciled.contains(&(K, Some(K), false)),
            "recovery reconcile with pending K missing; got {reconciled:?}"
        );
        assert!(state.all_uploaded());
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    /// The direct `PutStatus::AuthDenied` path stays instrumented and tested
    /// for future cases (small-body denials), even though Subtest A on the
    /// real stack takes the transient-then-resume path above.
    #[tokio::test]
    async fn put_authdenied_direct_still_suspends_and_recovers() {
        const K: u64 = 8;
        let total = 12 * CS;
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.authdenied_put_once.insert(K);
        let mut state = StreamState::new(total, CS).unwrap();

        let mut events = Vec::new();
        let suspensions = drive_collecting(&mut state, &reader, &mut dp, &mut events)
            .await
            .unwrap();

        assert_eq!(suspensions, 1);
        assert!(events.contains(&StreamEvent::PutAuthDenied { chunk_index: K }));
        assert!(state.all_uploaded());
        assert_eq!(reader.read_count(K), 1);
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    // ---- E (full, AuthDenial episode) — the exact bounded extent with the
    // two-step episode around the threshold. Heavy; `#[ignore]` by default:
    // `cargo test --release -- --ignored e_full_bounded_authdenied`.
    #[tokio::test]
    #[ignore]
    async fn e_full_bounded_authdenied_episode_around_threshold() {
        let total: u64 = 2_148_532_224;
        let cs: u64 = 8_388_608;
        let mut state = StreamState::new(total, cs).unwrap();
        assert_eq!(state.chunk_count(), 257);

        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        dp.transient_put_budget.insert(8, 1); // denial #1 -> transport transient
        dp.authdenied_resume_at_call = Some(2); // denial #2 -> auth_denied resume

        let mut events = Vec::new();
        let suspensions = drive_collecting(&mut state, &reader, &mut dp, &mut events)
            .await
            .unwrap();

        assert_eq!(suspensions, 1, "exactly one suspension");
        assert!(state.all_uploaded());
        assert_eq!(state.held_count(), 257);
        for i in 0..257 {
            assert_eq!(reader.read_count(i), 1, "chunk {i} read exactly once");
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::PutTransient { chunk_index: 8, .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ResumeResult { outcome: "auth_denied", .. }))
                .count(),
            1
        );
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::PutAuthDenied { .. })));
        assert!(events.contains(&StreamEvent::ResumeReconciled {
            held_count: 8,
            pending_chunk_index: Some(8),
            pending_already_held: false,
        }));
        assert_eq!(state.finish_digest().unwrap(), reference_digest(total));
    }

    // ---- guard — finish_digest is None until the whole source is hashed
    #[tokio::test]
    async fn finish_digest_is_none_before_full_pass() {
        let mut state = StreamState::new(3 * CS, CS).unwrap();
        assert!(state.finish_digest().is_none());
        let reader = FakeReader::new();
        let mut dp = FakeDataPlane::new();
        drive(&mut state, &reader, &mut dp).await.unwrap();
        assert!(state.finish_digest().is_some());
    }

    // ---- new-plan validation ------------------------------------------
    #[test]
    fn new_rejects_degenerate_plans() {
        assert!(StreamState::new(0, CS).is_err());
        assert!(StreamState::new(CS, 0).is_err());
        assert!(StreamState::new(1, 1).is_ok());
    }
}
