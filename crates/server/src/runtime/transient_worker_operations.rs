//! Transient, generation-scoped Worker-operation state (Runtime Service)
//! (`docs/specifications/m1-worker-data-plane-control-contract.md`
//! "Connection generations and correlation", "Transient operation handles";
//! Issue #39 Phase B).
//!
//! `acceptance_handle`, `verification_handle`, and `resume_cursor` are **not**
//! credentials and **not** durable business identities. Each exists only to
//! bind one Worker follow-up IPC message to one operation `bamepd` already
//! authorized *on the same current UDS connection generation*. They are
//! opaque to the Worker, minted by `bamepd`, process-local, generation-scoped,
//! never PostgreSQL-persisted, invalid after disconnect/reconnect, bounded,
//! and fail-closed.
//!
//! This store is owned by one Worker control-plane connection generation
//! (`bamep_server::adapters::worker_control_plane`): a fresh empty store per
//! successful handshake, dropped when that connection ends. Every operation
//! runs inside
//! [`WorkerAuthorityRegistry::with_current_generation`](crate::runtime::worker_authority::WorkerAuthorityRegistry::with_current_generation),
//! which verifies generation currency and holds the registry `state` lock
//! **shared for the whole critical section**. Because a new handshake takes
//! that lock **exclusively** in
//! [`begin_generation`](crate::runtime::worker_authority::WorkerAuthorityRegistry::begin_generation),
//! the currency check and the store mutation linearize as one step against
//! supersession — a stale generation whose task still holds an `Arc` to this
//! store can no longer mint, consume, advance, or read anything
//! ([`TransientOperationError::StaleGeneration`]), with no check-then-act
//! race.
//!
//! Phase B prepares only this transient machinery. The durable Phase C
//! business operations that will *use* the consume APIs
//! (`ChunkAcceptanceRequest` durable commit, resume durable-state
//! pagination, `ManifestSealRequest` seal transaction,
//! `ArtifactVerificationReport` verification commit) are **not** implemented
//! here.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use bamep_domain::{ArtifactId, DigestAlgorithm, TransferId};
use uuid::Uuid;

use crate::runtime::worker_authority::{ConnectionGeneration, WorkerAuthorityRegistry};

/// The default bounded capacity of one generation's transient-operation
/// store — the maximum number of live (not-yet-consumed) handles/cursors held
/// at once (`m1-worker-data-plane-control-contract.md` "Out of scope": the
/// concrete bounding is implementation-time; this is process-local defensive
/// bounding, not a product tuning surface).
///
/// `1024`: every binding lives only from an authorizing decision until its
/// immediate follow-up IPC message (milliseconds), so the live count at any
/// instant is bounded by the number of data-plane operations concurrently
/// in-flight on *one* Worker connection between authorization and follow-up.
/// M1's deterministic single-Endpoint vertical (#19) and even the separate
/// 20–24 concurrent Simulated Endpoint scale exercise (#21) stay far below
/// `1024`. A binding whose follow-up never arrives (for example a
/// `chunk_upload` the Worker rejects locally with `409 DIGEST_MISMATCH`
/// before sending `ChunkAcceptanceRequest`) is reclaimed when the generation
/// ends; a connection that somehow accumulated `1024` such bindings simply
/// fails new authorizations closed until it recycles, which is acceptable
/// fail-closed behavior on this host-local boundary. Overridable via
/// [`WorkerAuthorityRegistry::with_operations_capacity`] /
/// [`TransientWorkerOperationStore::new`].
pub const DEFAULT_TRANSIENT_WORKER_OPERATION_STORE_CAPACITY: usize = 1024;

/// How many fresh opaque ids [`TransientWorkerOperationStore`] tries before it
/// gives up and fails closed with [`TransientOperationError::IdCollision`].
/// With the production CSPRNG id source a single attempt always succeeds; a
/// deterministic test id source that repeats a value exhausts this and
/// surfaces the collision path.
const MINT_ATTEMPTS: usize = 4;

const REDACTED: &str = "REDACTED";

/// Why a [`TransientWorkerOperationStore`] operation failed. Every variant is
/// fail-closed. The mint failures are kept as two internal variants —
/// mirroring [`crate::runtime::capability_store::CapabilityIssueError`] —
/// so capacity saturation (an expected, load-dependent condition) is never
/// confused internally with an id collision (a CSPRNG value colliding with a
/// live binding — cryptographically negligible, never silently overwritten).
/// None of these ever reaches the Worker as a distinct reason: the Adapter
/// maps a mint failure to the same generic non-enumerable denial as any other
/// authorization denial (`m1-worker-data-plane-control-contract.md`
/// "Security and logging").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransientOperationError {
    #[error("transient Worker-operation store is at capacity; fails closed")]
    Saturated,
    #[error("minted handle collides with a live binding; fails closed")]
    IdCollision,
    #[error("the connection generation that owns this store is no longer current")]
    StaleGeneration,
    #[error("no live transient binding for the presented handle")]
    UnknownHandle,
    #[error("the presented handle is not of the expected kind")]
    WrongKind,
    #[error("the presented request fields do not match the handle's authorized operation")]
    BindingMismatch,
}

/// The three kinds of transient binding this store holds. Internal — the
/// opaque diagnostic prefix (`acc`/`ver`/`res`) is *not* authority and the
/// Worker never parses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandleKind {
    Acceptance,
    Verification,
    ResumeCursor,
}

impl HandleKind {
    fn prefix(self) -> &'static str {
        match self {
            HandleKind::Acceptance => "acc",
            HandleKind::Verification => "ver",
            HandleKind::ResumeCursor => "res",
        }
    }
}

// ---------------------------------------------------------------------
// Typed bindings
// ---------------------------------------------------------------------

/// Binds one `acceptance_handle` to the exact `chunk_upload` operation
/// `bamepd` authorized on this generation
/// (`m1-worker-data-plane-control-contract.md` "Verified-chunk durable
/// acceptance"): `transfer_id` and `chunk_index` (both from the HTTP route
/// the Worker forwarded) plus `proof_id` (the authorized per-request proof
/// instance — internal metadata only, so the binding satisfies the contract's
/// "one authorized `proof_id`" requirement and stays auditable; it is never
/// echoed to the Worker and never logged, and Phase C does not reconstruct a
/// proof from it).
#[derive(Clone, PartialEq, Eq)]
pub struct AcceptanceBinding {
    pub transfer_id: TransferId,
    pub chunk_index: u64,
    pub proof_id: String,
}

impl fmt::Debug for AcceptanceBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptanceBinding")
            .field("transfer_id", &self.transfer_id)
            .field("chunk_index", &self.chunk_index)
            .field("proof_id", &REDACTED)
            .finish()
    }
}

/// Binds one `verification_handle` to the exact sealed Artifact identity
/// `bamepd` committed (`sealed` or `already_pending_verification`) on this
/// generation (`m1-worker-data-plane-control-contract.md` "Seal-manifest
/// first durable commit"). Phase C's `ManifestSealRequest` handler mints this
/// with the authoritative durable values it already holds after the seal
/// transaction; Phase B never derives them and defines no durable seal
/// lookup. `chunk_count`/`expected_artifact_digest` are integrity identities,
/// not secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationBinding {
    pub transfer_id: TransferId,
    pub artifact_id: ArtifactId,
    pub chunk_count: u64,
    pub expected_artifact_digest: String,
}

/// A deliberately small typed continuation boundary for one paginated
/// resume-discovery query (`m1-worker-data-plane-control-contract.md`
/// "Resume-manifest pagination"). Phase C populates [`ResumeCursorState`]
/// from its own consistent durable snapshot; Phase B never queries
/// PostgreSQL and defines no generic database-cursor semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCursorBinding {
    pub transfer_id: TransferId,
    pub state: ResumeCursorState,
}

/// The bounded continuation state one `resume_cursor` carries. Kept minimal
/// on purpose — the exact durable snapshot representation is Phase C's to
/// choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCursorState {
    /// The internal identity of the one consistent snapshot this paginated
    /// query is served from — Phase C mints and keys it; opaque here.
    pub snapshot_id: Uuid,
    /// The next held `chunk_index` the following page must begin at — strictly
    /// ascending across pages, no gap and no repeat ("the highest
    /// `chunk_index` already returned" advanced by one, in spirit).
    pub next_chunk_index: u64,
}

#[derive(Debug)]
enum Binding {
    Acceptance(AcceptanceBinding),
    Verification(VerificationBinding),
    ResumeCursor(ResumeCursorBinding),
}

/// The default bound on how many live resume snapshots one connection
/// generation may hold at once (Issue #39 Phase C1). Each snapshot is the
/// immutable authorization-time materialization of one authorized
/// `ResumeDiscoveryQuery`'s durable held-chunk set — released as soon as its
/// pagination completes or its generation ends. `64` is generous defensive
/// headroom for the number of `GET .../chunks` operations one Worker
/// connection could have paginating concurrently; it is process-local
/// fail-closed bounding, not a product tuning surface (compare
/// [`DEFAULT_TRANSIENT_WORKER_OPERATION_STORE_CAPACITY`]). A snapshot whose
/// held-chunk set fits one page is never registered at all.
pub const DEFAULT_RESUME_SNAPSHOT_CAPACITY: usize = 64;

/// One held-and-individually-verified chunk in a [`ResumeSnapshot`]: exactly
/// what `m1-worker-data-plane-control-contract.md` "Resume-discovery
/// authorization and first page" `held_chunks` carries — never Worker-local
/// staged bytes. `digest` is the canonical base64url-no-pad wire value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldChunkEntry {
    pub chunk_index: u64,
    pub digest: String,
}

/// The immutable, process-local, generation-scoped materialization of one
/// authorized `ResumeDiscoveryQuery`'s consistent durable snapshot
/// (`m1-worker-data-plane-control-contract.md` "Resume-discovery pagination":
/// "`bamepd` serves every page of one resume query from a consistent durable
/// snapshot taken at authorization time"; Issue #39 Phase C1).
///
/// It is **not** business authority and **never** PostgreSQL-persisted: it
/// exists only so continuation pages for one HTTP `GET .../chunks` operation
/// come from the exact set of chunks durably held at authorization time — a
/// chunk accepted afterwards is simply absent (safe, because re-submitting an
/// already-held chunk is idempotent). `held` is ordered strictly ascending by
/// `chunk_index`, with no duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeSnapshot {
    pub transfer_id: TransferId,
    pub sealed: bool,
    pub digest_algorithm: DigestAlgorithm,
    pub chunk_size: u32,
    /// Present iff `sealed` (`m1-worker-data-plane-control-contract.md`).
    pub expected_chunk_count: Option<u64>,
    /// Every durably held and individually verified chunk identity belonging
    /// to this snapshot, ascending `chunk_index`.
    pub held: Vec<HeldChunkEntry>,
}

// ---------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------

type IdSource = Box<dyn Fn(HandleKind) -> String + Send + Sync>;

fn csprng_id_source() -> IdSource {
    // v4 UUIDs draw 122 bits from the OS CSPRNG (`getrandom`), so a fresh
    // value is collision-safe as an opaque token. The `acc_`/`ver_`/`res_`
    // prefix is a diagnostic aid only — never authority, never parsed by the
    // Worker.
    Box::new(|kind| format!("{}_{}", kind.prefix(), Uuid::new_v4().simple()))
}

/// One Worker UDS connection generation's transient operation state.
pub struct TransientWorkerOperationStore {
    generation: ConnectionGeneration,
    authority: Arc<WorkerAuthorityRegistry>,
    capacity: usize,
    resume_snapshot_capacity: usize,
    id_source: IdSource,
    bindings: Mutex<HashMap<String, Binding>>,
    /// Immutable resume snapshots keyed by `snapshot_id` (referenced by a
    /// [`ResumeCursorState`]). A separate `Mutex` from `bindings`; every
    /// access is generation-linearized the same way (see
    /// [`Self::with_snapshots`]). Lock order is `state` → this — never held
    /// together with `bindings`.
    resume_snapshots: Mutex<HashMap<Uuid, Arc<ResumeSnapshot>>>,
}

impl fmt::Debug for TransientWorkerOperationStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately prints neither the opaque handle keys nor the bindings
        // (`m1-worker-data-plane-control-contract.md` "Security and logging":
        // handles "carry no diagnostic value and SHOULD be redacted").
        let live = self.bindings.lock().map(|b| b.len()).unwrap_or(usize::MAX);
        let snapshots = self
            .resume_snapshots
            .lock()
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        f.debug_struct("TransientWorkerOperationStore")
            .field("generation", &self.generation)
            .field("capacity", &self.capacity)
            .field("live_bindings", &live)
            .field("live_resume_snapshots", &snapshots)
            .finish()
    }
}

impl TransientWorkerOperationStore {
    /// A fresh, empty store for connection `generation`, bounded to
    /// `capacity` live bindings (clamped to at least 1). `authority` is the
    /// same registry that minted `generation`; every operation runs inside
    /// [`WorkerAuthorityRegistry::with_current_generation`], which verifies
    /// `generation` is still current and holds the registry `state` lock
    /// shared for the whole critical section.
    pub fn new(
        generation: ConnectionGeneration,
        authority: Arc<WorkerAuthorityRegistry>,
        capacity: usize,
    ) -> Self {
        Self {
            generation,
            authority,
            capacity: capacity.max(1),
            resume_snapshot_capacity: DEFAULT_RESUME_SNAPSHOT_CAPACITY,
            id_source: csprng_id_source(),
            bindings: Mutex::new(HashMap::new()),
            resume_snapshots: Mutex::new(HashMap::new()),
        }
    }

    /// A store with a non-default resume-snapshot bound, so a test can
    /// exercise the fail-closed `Saturated` path without registering
    /// [`DEFAULT_RESUME_SNAPSHOT_CAPACITY`] snapshots.
    pub fn with_resume_snapshot_capacity(mut self, resume_snapshot_capacity: usize) -> Self {
        self.resume_snapshot_capacity = resume_snapshot_capacity.max(1);
        self
    }

    /// A store with a deterministic id source, for exercising the
    /// [`TransientOperationError::IdCollision`] path without relying on an
    /// astronomically unlikely real UUID collision.
    #[cfg(test)]
    fn with_id_source(
        generation: ConnectionGeneration,
        authority: Arc<WorkerAuthorityRegistry>,
        capacity: usize,
        id_source: IdSource,
    ) -> Self {
        Self {
            generation,
            authority,
            capacity: capacity.max(1),
            resume_snapshot_capacity: DEFAULT_RESUME_SNAPSHOT_CAPACITY,
            id_source,
            bindings: Mutex::new(HashMap::new()),
            resume_snapshots: Mutex::new(HashMap::new()),
        }
    }

    pub fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of live bindings currently held — for diagnostics/tests
    /// only. `0` once the owning generation is superseded, since every
    /// operation then fails closed regardless of the map contents.
    pub fn live_count(&self) -> usize {
        self.with_bindings_read(|map| map.len()).unwrap_or(0)
    }

    /// Runs `op` against the live bindings map **iff** this store's generation
    /// is still current, with the registry `state` lock held **shared** for
    /// the whole critical section (Issue #39 Phase B concurrency correction).
    /// The generation-currency check and the map mutation therefore linearize
    /// as **one** step against
    /// [`WorkerAuthorityRegistry::begin_generation`] /
    /// [`end_generation`](WorkerAuthorityRegistry::end_generation): there is no
    /// window in which this store observed its generation current and then
    /// mutated after a newer generation had already taken over. A
    /// superseded/ended generation yields
    /// [`TransientOperationError::StaleGeneration`] and `op` never runs.
    ///
    /// Lock order (see [`crate::runtime::worker_authority`] module docs):
    /// registry `state` (shared, via
    /// [`WorkerAuthorityRegistry::with_current_generation`]) →
    /// `self.bindings`. `op` never touches the registry.
    fn with_bindings<T>(
        &self,
        op: impl FnOnce(&mut HashMap<String, Binding>) -> Result<T, TransientOperationError>,
    ) -> Result<T, TransientOperationError> {
        self.authority
            .with_current_generation(self.generation, || {
                let mut map = self.bindings.lock().expect("transient store lock poisoned");
                op(&mut map)
            })
            .and_then(|inner| inner)
    }

    /// Non-consuming counterpart of [`Self::with_bindings`]: `op` cannot fail,
    /// but the read still happens only while the generation is current, so a
    /// superseded generation's bindings are never surfaced. `None` once
    /// superseded/ended.
    fn with_bindings_read<T>(&self, op: impl FnOnce(&HashMap<String, Binding>) -> T) -> Option<T> {
        self.authority
            .with_current_generation(self.generation, || {
                let map = self.bindings.lock().expect("transient store lock poisoned");
                op(&map)
            })
            .ok()
    }

    // -- resume snapshots (Issue #39 Phase C1) -----------------------

    fn with_snapshots<T>(
        &self,
        op: impl FnOnce(&mut HashMap<Uuid, Arc<ResumeSnapshot>>) -> Result<T, TransientOperationError>,
    ) -> Result<T, TransientOperationError> {
        self.authority
            .with_current_generation(self.generation, || {
                let mut map = self
                    .resume_snapshots
                    .lock()
                    .expect("transient store lock poisoned");
                op(&mut map)
            })
            .and_then(|inner| inner)
    }

    fn with_snapshots_read<T>(
        &self,
        op: impl FnOnce(&HashMap<Uuid, Arc<ResumeSnapshot>>) -> T,
    ) -> Option<T> {
        self.authority
            .with_current_generation(self.generation, || {
                let map = self
                    .resume_snapshots
                    .lock()
                    .expect("transient store lock poisoned");
                op(&map)
            })
            .ok()
    }

    /// Materializes one authorized `ResumeDiscoveryQuery`'s consistent durable
    /// snapshot into this generation's process-local state, returning its
    /// fresh opaque `snapshot_id` (`m1-worker-data-plane-control-contract.md`
    /// "Resume-discovery pagination"). Fails closed
    /// ([`Saturated`](TransientOperationError::Saturated) /
    /// [`IdCollision`](TransientOperationError::IdCollision) /
    /// [`StaleGeneration`](TransientOperationError::StaleGeneration)); the
    /// Adapter maps any failure to the same generic denial. Only call this
    /// when the held-chunk set does not fit one page — a single-page snapshot
    /// needs no registration and no cursor.
    pub fn register_resume_snapshot(
        &self,
        snapshot: ResumeSnapshot,
    ) -> Result<Uuid, TransientOperationError> {
        self.with_snapshots(|map| {
            if map.len() >= self.resume_snapshot_capacity {
                return Err(TransientOperationError::Saturated);
            }
            let mut chosen: Option<Uuid> = None;
            for _ in 0..MINT_ATTEMPTS {
                let candidate = Uuid::new_v4();
                if !map.contains_key(&candidate) {
                    chosen = Some(candidate);
                    break;
                }
            }
            let Some(id) = chosen else {
                return Err(TransientOperationError::IdCollision);
            };
            map.insert(id, Arc::new(snapshot));
            Ok(id)
        })
    }

    /// A live resume snapshot by id, or `None` for an unknown id or once the
    /// owning generation is superseded. Non-consuming.
    pub fn resume_snapshot(&self, snapshot_id: Uuid) -> Option<Arc<ResumeSnapshot>> {
        self.with_snapshots_read(|map| map.get(&snapshot_id).map(Arc::clone))
            .flatten()
    }

    /// Releases the snapshot for `snapshot_id` — called when its pagination
    /// completes (final page). A no-op for an unknown id or a superseded
    /// generation. Generation end drops the whole store, reclaiming every
    /// still-live snapshot regardless.
    pub fn drop_resume_snapshot(&self, snapshot_id: Uuid) {
        let _ = self.with_snapshots(|map| {
            map.remove(&snapshot_id);
            Ok(())
        });
    }

    /// The number of live resume snapshots currently held — diagnostics/tests
    /// only. `0` once the owning generation is superseded.
    pub fn live_resume_snapshot_count(&self) -> usize {
        self.with_snapshots_read(|map| map.len()).unwrap_or(0)
    }

    /// Inserts `binding` under a fresh opaque id, failing closed at capacity
    /// ([`Saturated`](TransientOperationError::Saturated)) and on a repeated
    /// id ([`IdCollision`](TransientOperationError::IdCollision)). Neither
    /// failure ever evicts or overwrites a live binding. Runs entirely inside
    /// the generation-linearized critical section (see [`Self::with_bindings`]).
    fn insert_fresh(
        &self,
        kind: HandleKind,
        binding: Binding,
    ) -> Result<String, TransientOperationError> {
        self.with_bindings(|map| {
            if map.len() >= self.capacity {
                return Err(TransientOperationError::Saturated);
            }
            let mut chosen: Option<String> = None;
            for _ in 0..MINT_ATTEMPTS {
                let candidate = (self.id_source)(kind);
                if !map.contains_key(&candidate) {
                    chosen = Some(candidate);
                    break;
                }
            }
            let Some(id) = chosen else {
                return Err(TransientOperationError::IdCollision);
            };
            map.insert(id.clone(), binding);
            Ok(id)
        })
    }

    // -- acceptance --------------------------------------------------

    /// Mints an `acceptance_handle` for a just-approved `chunk_upload`
    /// (`m1-worker-data-plane-control-contract.md` "Chunk-upload
    /// authorization" — the `AuthorizationDecision.acceptance_handle`).
    pub fn mint_acceptance(
        &self,
        binding: AcceptanceBinding,
    ) -> Result<String, TransientOperationError> {
        self.insert_fresh(HandleKind::Acceptance, Binding::Acceptance(binding))
    }

    /// Consumes the acceptance binding for `handle` **iff** it is an
    /// acceptance handle minted on the current generation whose bound
    /// `transfer_id`/`chunk_index` match the presented values
    /// (`m1-worker-data-plane-control-contract.md`: "`bamepd` rejects a
    /// `ChunkAcceptanceRequest` whose handle it did not mint on the current
    /// generation, or whose `transfer_id`/`chunk_index` do not match the
    /// handle's authorized operation").
    ///
    /// Single-use: a successful consume removes the binding atomically, so a
    /// second consume fails [`UnknownHandle`](TransientOperationError::UnknownHandle).
    /// A *mismatched* presentation — wrong `transfer_id`, wrong `chunk_index`,
    /// or a handle of another kind — is rejected
    /// ([`BindingMismatch`](TransientOperationError::BindingMismatch) /
    /// [`WrongKind`](TransientOperationError::WrongKind)) **without**
    /// consuming the still-legitimate binding, since the Specification makes
    /// the handle single-use *on its authorized follow-up*, not one-shot on
    /// any presentation.
    pub fn consume_acceptance(
        &self,
        handle: &str,
        transfer_id: TransferId,
        chunk_index: u64,
    ) -> Result<AcceptanceBinding, TransientOperationError> {
        self.with_bindings(|map| {
            let matches = match map.get(handle) {
                None => return Err(TransientOperationError::UnknownHandle),
                Some(Binding::Acceptance(b)) => {
                    b.transfer_id == transfer_id && b.chunk_index == chunk_index
                }
                Some(_) => return Err(TransientOperationError::WrongKind),
            };
            if !matches {
                return Err(TransientOperationError::BindingMismatch);
            }
            match map.remove(handle) {
                Some(Binding::Acceptance(b)) => Ok(b),
                _ => unreachable!("verified under the same lock immediately above"),
            }
        })
    }

    /// Non-consuming read of an acceptance binding (diagnostics/tests, and a
    /// legitimate Phase C peek). `None` for an unknown handle, a handle of
    /// another kind, or once the owning generation is superseded.
    pub fn acceptance_binding(&self, handle: &str) -> Option<AcceptanceBinding> {
        self.with_bindings_read(|map| match map.get(handle) {
            Some(Binding::Acceptance(b)) => Some(b.clone()),
            _ => None,
        })
        .flatten()
    }

    // -- verification ---------------------------------------------

    /// Mints a `verification_handle` for a just-committed
    /// `sealed`/`already_pending_verification` seal
    /// (`m1-worker-data-plane-control-contract.md` "Seal-manifest first
    /// durable commit"). Phase C supplies the authoritative durable
    /// [`VerificationBinding`] fields.
    pub fn mint_verification(
        &self,
        binding: VerificationBinding,
    ) -> Result<String, TransientOperationError> {
        self.insert_fresh(HandleKind::Verification, Binding::Verification(binding))
    }

    /// Consumes the verification binding for `handle` iff it is a
    /// verification handle minted on the current generation — atomically,
    /// single-use, current-generation only, with the same wrong-kind
    /// rejection and secret redaction as
    /// [`consume_acceptance`](Self::consume_acceptance).
    ///
    /// Unlike `consume_acceptance`, this takes **only** the handle: the v1
    /// `ArtifactVerificationReport` wire message carries no `transfer_id`/
    /// `artifact_id` (`m1-worker-data-plane-control-contract.md` "Minimum
    /// messages" #6 — only `verification_handle` + `computed_artifact_digest`),
    /// so there is no presented Transfer/Artifact value to compare against and
    /// no substitution field to reject. The returned [`VerificationBinding`]
    /// *is* the authoritative correlation target; `bamepd` then independently
    /// revalidates that bound sealed identity against current durable
    /// PostgreSQL state before mutating anything (Issue #39 Phase C2).
    pub fn consume_verification(
        &self,
        handle: &str,
    ) -> Result<VerificationBinding, TransientOperationError> {
        self.with_bindings(|map| {
            match map.get(handle) {
                None => return Err(TransientOperationError::UnknownHandle),
                Some(Binding::Verification(_)) => {}
                Some(_) => return Err(TransientOperationError::WrongKind),
            };
            match map.remove(handle) {
                Some(Binding::Verification(b)) => Ok(b),
                _ => unreachable!("verified under the same lock immediately above"),
            }
        })
    }

    /// Non-consuming read of a verification binding.
    pub fn verification_binding(&self, handle: &str) -> Option<VerificationBinding> {
        self.with_bindings_read(|map| match map.get(handle) {
            Some(Binding::Verification(b)) => Some(b.clone()),
            _ => None,
        })
        .flatten()
    }

    // -- resume cursor ------------------------------------------

    /// Mints the **first** `resume_cursor` for a paginated resume-discovery
    /// query — the one a `ResumeDiscoveryPage` carries when more held-chunk
    /// pages remain (`m1-worker-data-plane-control-contract.md`
    /// "Resume-manifest pagination"). Grows the store, so it can fail
    /// [`Saturated`](TransientOperationError::Saturated).
    pub fn mint_resume_cursor(
        &self,
        binding: ResumeCursorBinding,
    ) -> Result<String, TransientOperationError> {
        self.insert_fresh(HandleKind::ResumeCursor, Binding::ResumeCursor(binding))
    }

    /// Consumes the current `resume_cursor` and, atomically, optionally mints
    /// its successor: `next` is `Some(state)` when Phase C determines at
    /// least one more page remains, `None` when the aggregate is complete
    /// (`m1-worker-data-plane-control-contract.md`: "one cursor authorizes
    /// exactly the NEXT page" and then advances; "each cursor consumed
    /// once").
    ///
    /// Atomicity (`m1-worker-data-plane-control-contract.md` "Failure
    /// semantics"): the successor is minted **before** the current cursor is
    /// removed, under one lock hold. If minting the successor fails
    /// ([`IdCollision`](TransientOperationError::IdCollision)) the current
    /// cursor is left completely intact — there is never a window where the
    /// current cursor is consumed but no successor exists, and never a
    /// duplicate live cursor pointing at the same one-shot continuation.
    /// A 1-for-1 advance does not grow the store, so it cannot be
    /// [`Saturated`](TransientOperationError::Saturated); the first-cursor
    /// growth is the only place that can be, at
    /// [`mint_resume_cursor`](Self::mint_resume_cursor).
    pub fn advance_resume_cursor(
        &self,
        cursor: &str,
        transfer_id: TransferId,
        next: Option<ResumeCursorState>,
    ) -> Result<Option<String>, TransientOperationError> {
        // The whole authoritative advance — currency check, successor mint,
        // and current-cursor removal — runs inside one
        // [`Self::with_bindings`] critical section: generation protection
        // surrounds it end to end (Issue #39 Phase B concurrency correction),
        // and the successor is still chosen *before* the current cursor is
        // removed under the single `bindings` lock, so a collision leaves the
        // current cursor completely intact with no gap and no duplicate.
        self.with_bindings(|map| {
            match map.get(cursor) {
                None => return Err(TransientOperationError::UnknownHandle),
                Some(Binding::ResumeCursor(b)) => {
                    if b.transfer_id != transfer_id {
                        return Err(TransientOperationError::BindingMismatch);
                    }
                }
                Some(_) => return Err(TransientOperationError::WrongKind),
            }

            let successor = match next {
                None => None,
                Some(state) => {
                    let mut chosen: Option<String> = None;
                    for _ in 0..MINT_ATTEMPTS {
                        let candidate = (self.id_source)(HandleKind::ResumeCursor);
                        if candidate != cursor && !map.contains_key(&candidate) {
                            chosen = Some(candidate);
                            break;
                        }
                    }
                    let Some(id) = chosen else {
                        // Current cursor untouched — no gap, no duplicate.
                        return Err(TransientOperationError::IdCollision);
                    };
                    Some((
                        id,
                        Binding::ResumeCursor(ResumeCursorBinding { transfer_id, state }),
                    ))
                }
            };

            map.remove(cursor);
            match successor {
                None => Ok(None),
                Some((id, binding)) => {
                    map.insert(id.clone(), binding);
                    Ok(Some(id))
                }
            }
        })
    }

    /// Non-consuming read of a resume-cursor binding.
    pub fn resume_cursor_binding(&self, cursor: &str) -> Option<ResumeCursorBinding> {
        self.with_bindings_read(|map| match map.get(cursor) {
            Some(Binding::ResumeCursor(b)) => Some(b.clone()),
            _ => None,
        })
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    fn generation() -> (Arc<WorkerAuthorityRegistry>, ConnectionGeneration) {
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let generation = registry.begin_generation(Uuid::new_v4());
        (registry, generation)
    }

    fn store() -> (Arc<WorkerAuthorityRegistry>, TransientWorkerOperationStore) {
        let (registry, generation) = generation();
        let store = TransientWorkerOperationStore::new(generation, Arc::clone(&registry), 64);
        (registry, store)
    }

    fn acceptance(chunk_index: u64) -> AcceptanceBinding {
        AcceptanceBinding {
            transfer_id: TransferId::new(),
            chunk_index,
            proof_id: "proof-id-secret".to_string(),
        }
    }

    fn verification() -> VerificationBinding {
        VerificationBinding {
            transfer_id: TransferId::new(),
            artifact_id: ArtifactId::new(),
            chunk_count: 3,
            expected_artifact_digest: "ead".to_string(),
        }
    }

    fn cursor(next_chunk_index: u64) -> ResumeCursorBinding {
        ResumeCursorBinding {
            transfer_id: TransferId::new(),
            state: ResumeCursorState {
                snapshot_id: Uuid::new_v4(),
                next_chunk_index,
            },
        }
    }

    /// An id source that hands out a fixed queue of values (then repeats the
    /// last), for deterministic collision tests.
    fn fixed_ids(values: &[&str]) -> IdSource {
        let queue: StdMutex<VecDeque<String>> =
            StdMutex::new(values.iter().map(|s| s.to_string()).collect());
        Box::new(move |_kind| {
            let mut q = queue.lock().unwrap();
            let front = q.front().cloned().unwrap_or_default();
            if q.len() > 1 {
                q.pop_front();
            }
            front
        })
    }

    /// An id source whose **first** invocation signals on `entered` and then
    /// blocks until `release` fires, so a test can hold a store operation open
    /// *inside* its generation-linearized critical section (registry `state`
    /// read lock held, `bindings` lock held) with no sleeps. Later
    /// invocations behave normally.
    fn parking_id_source(
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> IdSource {
        let armed = std::sync::atomic::AtomicBool::new(true);
        let release = StdMutex::new(release);
        Box::new(move |kind| {
            if armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                entered.send(()).expect("test receiver alive");
                release
                    .lock()
                    .expect("release channel mutex")
                    .recv()
                    .expect("test sender alive");
            }
            format!("{}_{}", kind.prefix(), Uuid::new_v4().simple())
        })
    }

    // -- acceptance mint / consume -----------------------------------

    #[test]
    fn mint_then_consume_acceptance_returns_the_exact_binding_including_proof_id() {
        let (_r, store) = store();
        let binding = acceptance(4);
        let handle = store.mint_acceptance(binding.clone()).unwrap();
        assert!(handle.starts_with("acc_"));

        let peek = store.acceptance_binding(&handle).expect("live binding");
        assert_eq!(peek, binding);

        let consumed = store
            .consume_acceptance(&handle, binding.transfer_id, 4)
            .unwrap();
        assert_eq!(consumed.transfer_id, binding.transfer_id);
        assert_eq!(consumed.chunk_index, 4);
        assert_eq!(consumed.proof_id, "proof-id-secret");
    }

    #[test]
    fn a_second_consume_of_the_same_acceptance_handle_fails() {
        let (_r, store) = store();
        let binding = acceptance(0);
        let handle = store.mint_acceptance(binding.clone()).unwrap();
        store
            .consume_acceptance(&handle, binding.transfer_id, 0)
            .unwrap();
        assert_eq!(
            store.consume_acceptance(&handle, binding.transfer_id, 0),
            Err(TransientOperationError::UnknownHandle)
        );
    }

    #[test]
    fn a_wrong_transfer_id_does_not_consume_the_acceptance_binding() {
        let (_r, store) = store();
        let binding = acceptance(2);
        let handle = store.mint_acceptance(binding.clone()).unwrap();

        assert_eq!(
            store.consume_acceptance(&handle, TransferId::new(), 2),
            Err(TransientOperationError::BindingMismatch)
        );
        // still live and still consumable with the correct fields
        assert!(store.acceptance_binding(&handle).is_some());
        assert!(store
            .consume_acceptance(&handle, binding.transfer_id, 2)
            .is_ok());
    }

    #[test]
    fn a_wrong_chunk_index_does_not_consume_the_acceptance_binding() {
        let (_r, store) = store();
        let binding = acceptance(7);
        let handle = store.mint_acceptance(binding.clone()).unwrap();
        assert_eq!(
            store.consume_acceptance(&handle, binding.transfer_id, 8),
            Err(TransientOperationError::BindingMismatch)
        );
        assert!(store.acceptance_binding(&handle).is_some());
    }

    #[test]
    fn an_unknown_acceptance_handle_fails() {
        let (_r, store) = store();
        assert_eq!(
            store.consume_acceptance("acc_nope", TransferId::new(), 0),
            Err(TransientOperationError::UnknownHandle)
        );
    }

    // -- kind confusion --------------------------------------------

    #[test]
    fn a_verification_handle_cannot_be_consumed_as_an_acceptance() {
        let (_r, store) = store();
        let v = verification();
        let handle = store.mint_verification(v.clone()).unwrap();
        assert!(handle.starts_with("ver_"));
        assert_eq!(
            store.consume_acceptance(&handle, v.transfer_id, 0),
            Err(TransientOperationError::WrongKind)
        );
        // the verification binding is preserved
        assert!(store.verification_binding(&handle).is_some());
    }

    #[test]
    fn a_resume_cursor_cannot_be_consumed_as_another_kind() {
        let (_r, store) = store();
        let c = cursor(0);
        let handle = store.mint_resume_cursor(c.clone()).unwrap();
        assert!(handle.starts_with("res_"));
        assert_eq!(
            store.consume_acceptance(&handle, c.transfer_id, 0),
            Err(TransientOperationError::WrongKind)
        );
        assert_eq!(
            store.consume_verification(&handle),
            Err(TransientOperationError::WrongKind)
        );
        assert!(store.resume_cursor_binding(&handle).is_some());
    }

    #[test]
    fn an_acceptance_handle_cannot_be_consumed_as_a_verification() {
        let (_r, store) = store();
        let a = acceptance(0);
        let handle = store.mint_acceptance(a.clone()).unwrap();
        assert_eq!(
            store.consume_verification(&handle),
            Err(TransientOperationError::WrongKind)
        );
        // the acceptance binding is preserved on a wrong-kind consume
        assert!(store.acceptance_binding(&handle).is_some());
    }

    // -- verification consume -------------------------------------

    #[test]
    fn verification_consume_is_single_use_by_handle_alone() {
        // The v1 `ArtifactVerificationReport` wire carries only the handle
        // (Issue #39 Phase C2 item 25): consume is by handle alone, still
        // atomic / single-use / current-generation, and the returned binding
        // is the authoritative correlation target `bamepd` then revalidates
        // against durable state.
        let (_r, store) = store();
        let v = verification();
        let handle = store.mint_verification(v.clone()).unwrap();

        let consumed = store.consume_verification(&handle).unwrap();
        assert_eq!(consumed, v);
        // Single-use: a second consume of the same handle fails.
        assert_eq!(
            store.consume_verification(&handle),
            Err(TransientOperationError::UnknownHandle)
        );
    }

    // -- resume cursor advance -----------------------------------

    #[test]
    fn advancing_a_cursor_consumes_it_once_and_mints_the_next() {
        let (_r, store) = store();
        let c = cursor(0);
        let first = store.mint_resume_cursor(c.clone()).unwrap();

        let next_state = ResumeCursorState {
            snapshot_id: c.state.snapshot_id,
            next_chunk_index: 10,
        };
        let second = store
            .advance_resume_cursor(&first, c.transfer_id, Some(next_state.clone()))
            .unwrap()
            .expect("a successor cursor");
        assert_ne!(second, first);

        // the old cursor is gone
        assert_eq!(
            store.advance_resume_cursor(&first, c.transfer_id, None),
            Err(TransientOperationError::UnknownHandle)
        );
        // the new cursor carries the advanced state
        assert_eq!(
            store.resume_cursor_binding(&second).unwrap().state,
            next_state
        );

        // final page: advance with `None` removes it and mints nothing
        assert_eq!(
            store.advance_resume_cursor(&second, c.transfer_id, None),
            Ok(None)
        );
        assert!(store.resume_cursor_binding(&second).is_none());
    }

    #[test]
    fn advancing_a_cursor_with_a_wrong_transfer_id_preserves_it() {
        let (_r, store) = store();
        let c = cursor(0);
        let handle = store.mint_resume_cursor(c.clone()).unwrap();
        assert_eq!(
            store.advance_resume_cursor(&handle, TransferId::new(), None),
            Err(TransientOperationError::BindingMismatch)
        );
        assert!(store.resume_cursor_binding(&handle).is_some());
    }

    #[test]
    fn a_collision_while_advancing_leaves_the_current_cursor_intact_with_no_duplicate() {
        let (registry, generation) = generation();
        // The store will mint "res_first" for the initial cursor, then keep
        // returning "res_dup" — which, on advance, is a brand-new id the
        // first time but collides forever after, so the *second* advance
        // attempt to create a successor fails.
        let store = TransientWorkerOperationStore::with_id_source(
            generation,
            Arc::clone(&registry),
            64,
            fixed_ids(&["res_first", "res_dup"]),
        );
        let c = cursor(0);
        let first = store.mint_resume_cursor(c.clone()).unwrap();
        assert_eq!(first, "res_first");

        let state = ResumeCursorState {
            snapshot_id: c.state.snapshot_id,
            next_chunk_index: 5,
        };
        let second = store
            .advance_resume_cursor(&first, c.transfer_id, Some(state.clone()))
            .unwrap()
            .expect("successor");
        assert_eq!(second, "res_dup");

        // Now "res_dup" is live; a further advance that needs a fresh
        // successor id can only produce "res_dup" again -> collision, and the
        // current ("res_dup") cursor must remain the single live continuation.
        assert_eq!(
            store.advance_resume_cursor(&second, c.transfer_id, Some(state)),
            Err(TransientOperationError::IdCollision)
        );
        assert!(store.resume_cursor_binding(&second).is_some());
        assert_eq!(
            store.live_count(),
            1,
            "exactly one live cursor, no duplicate"
        );
    }

    // -- capacity / collision ------------------------------------

    #[test]
    fn the_store_fails_closed_at_capacity_without_evicting_a_live_binding() {
        let (registry, generation) = generation();
        let store = TransientWorkerOperationStore::new(generation, Arc::clone(&registry), 2);
        let a = store.mint_acceptance(acceptance(0)).unwrap();
        let b = store.mint_acceptance(acceptance(1)).unwrap();

        assert_eq!(
            store.mint_acceptance(acceptance(2)),
            Err(TransientOperationError::Saturated)
        );
        assert_eq!(
            store.mint_verification(verification()),
            Err(TransientOperationError::Saturated)
        );
        // neither live binding was evicted to make room
        assert!(store.acceptance_binding(&a).is_some());
        assert!(store.acceptance_binding(&b).is_some());
    }

    #[test]
    fn a_minted_id_collision_fails_closed_and_preserves_the_existing_binding() {
        let (registry, generation) = generation();
        let store = TransientWorkerOperationStore::with_id_source(
            generation,
            Arc::clone(&registry),
            64,
            fixed_ids(&["acc_only"]),
        );
        let original = acceptance(0);
        let handle = store.mint_acceptance(original.clone()).unwrap();
        assert_eq!(handle, "acc_only");

        let mut colliding = acceptance(9);
        colliding.proof_id = "different".to_string();
        assert_eq!(
            store.mint_acceptance(colliding),
            Err(TransientOperationError::IdCollision)
        );
        // the original binding is completely unchanged
        assert_eq!(store.acceptance_binding("acc_only").unwrap(), original);
    }

    #[test]
    fn saturation_and_collision_are_distinct_internal_causes() {
        assert_ne!(
            TransientOperationError::Saturated,
            TransientOperationError::IdCollision
        );
    }

    // -- generation lifetime -----------------------------------

    #[test]
    fn a_handle_is_rejected_once_a_newer_generation_supersedes_its_own() {
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let gen_a = registry.begin_generation(Uuid::new_v4());
        let store_a = TransientWorkerOperationStore::new(gen_a, Arc::clone(&registry), 64);
        let binding = acceptance(0);
        let handle = store_a.mint_acceptance(binding.clone()).unwrap();

        // A newer overlapping handshake supersedes generation A while its
        // task still holds `store_a`.
        let _gen_b = registry.begin_generation(Uuid::new_v4());

        assert_eq!(
            store_a.mint_acceptance(acceptance(1)),
            Err(TransientOperationError::StaleGeneration)
        );
        assert_eq!(
            store_a.consume_acceptance(&handle, binding.transfer_id, 0),
            Err(TransientOperationError::StaleGeneration)
        );
        assert!(store_a.acceptance_binding(&handle).is_none());
        assert_eq!(store_a.live_count(), 0);
    }

    #[test]
    fn ending_a_generation_invalidates_its_store() {
        let (registry, generation) = generation();
        let store = TransientWorkerOperationStore::new(generation, Arc::clone(&registry), 64);
        let v = verification();
        let handle = store.mint_verification(v.clone()).unwrap();

        registry.end_generation(generation);

        assert_eq!(
            store.consume_verification(&handle),
            Err(TransientOperationError::StaleGeneration)
        );
        assert_eq!(
            store.advance_resume_cursor("res_x", TransferId::new(), None),
            Err(TransientOperationError::StaleGeneration)
        );
        assert!(store.verification_binding(&handle).is_none());
    }

    #[test]
    fn every_authority_operation_fails_closed_once_the_generation_is_superseded() {
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let gen_a = registry.begin_generation(Uuid::new_v4());
        let store = TransientWorkerOperationStore::new(gen_a, Arc::clone(&registry), 64);

        // One live binding of each kind, minted while generation A is current.
        let acc = store.mint_acceptance(acceptance(0)).unwrap();
        let ver = store.mint_verification(verification()).unwrap();
        let res = store.mint_resume_cursor(cursor(0)).unwrap();

        // A newer overlapping handshake supersedes generation A.
        let _gen_b = registry.begin_generation(Uuid::new_v4());

        // Every mint / consume / advance now fails closed.
        assert_eq!(
            store.mint_acceptance(acceptance(1)),
            Err(TransientOperationError::StaleGeneration)
        );
        assert_eq!(
            store.mint_verification(verification()),
            Err(TransientOperationError::StaleGeneration)
        );
        assert_eq!(
            store.mint_resume_cursor(cursor(1)),
            Err(TransientOperationError::StaleGeneration)
        );
        assert_eq!(
            store.consume_acceptance(&acc, TransferId::new(), 0),
            Err(TransientOperationError::StaleGeneration)
        );
        assert_eq!(
            store.consume_verification(&ver),
            Err(TransientOperationError::StaleGeneration)
        );
        assert_eq!(
            store.advance_resume_cursor(&res, TransferId::new(), None),
            Err(TransientOperationError::StaleGeneration)
        );

        // Non-consuming reads also refuse to surface a superseded binding.
        assert!(store.acceptance_binding(&acc).is_none());
        assert!(store.verification_binding(&ver).is_none());
        assert!(store.resume_cursor_binding(&res).is_none());
        assert_eq!(store.live_count(), 0);
    }

    /// Deterministic proof (no sleeps, no timeouts) that a transient-authority
    /// operation holds the registry `state` lock **shared for its whole
    /// critical section**, so generation supersession cannot linearize in the
    /// middle of it (Issue #39 Phase B concurrency correction; contract
    /// "transient handles are valid only on the current connection
    /// generation").
    ///
    /// Thread A parks *inside* `mint_acceptance`, between the currency check
    /// and the map mutation. The main thread then probes the `state` lock
    /// with a **non-blocking** `try_write`:
    ///
    /// * with the fix, A holds `state` shared across the whole critical
    ///   section, so the probe reports the lock contended — and a concurrent
    ///   `begin_generation` is provably blocked;
    /// * with the former check-then-act race, A would hold only the `bindings`
    ///   lock here and the probe would find `state` free — the assertion
    ///   fails, catching the regression.
    ///
    /// The probe is non-blocking, so the main thread cannot deadlock behind
    /// the pending writer (`std` `RwLock` is write-preferring: a blocking
    /// `read` would stall while B waits for `write`).
    #[test]
    fn a_newer_generation_cannot_supersede_while_an_authority_operation_is_in_flight() {
        use std::sync::mpsc;
        use std::thread;

        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let gen_a = registry.begin_generation(Uuid::new_v4());

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let store = Arc::new(TransientWorkerOperationStore::with_id_source(
            gen_a,
            Arc::clone(&registry),
            64,
            parking_id_source(entered_tx, release_rx),
        ));

        // Thread A: enters mint_acceptance and parks deep inside the
        // generation-linearized critical section.
        let store_a = Arc::clone(&store);
        let a = thread::spawn(move || {
            store_a.mint_acceptance(AcceptanceBinding {
                transfer_id: TransferId::new(),
                chunk_index: 0,
                proof_id: "p".to_string(),
            })
        });
        entered_rx.recv().expect("A reached its critical section");

        // Thread B: tries to supersede generation A.
        let registry_b = Arc::clone(&registry);
        let (b_started_tx, b_started_rx) = mpsc::channel();
        let b = thread::spawn(move || {
            b_started_tx.send(()).expect("test receiver alive");
            registry_b.begin_generation(Uuid::new_v4())
        });
        b_started_rx.recv().expect("B is attempting supersession");

        // Deterministic: while A's mint is in flight, the state lock is held
        // shared and cannot be write-locked. Probed repeatedly to rule out
        // any sneak-through window.
        for _ in 0..10_000 {
            assert!(
                !registry.state_lock_is_uncontended(),
                "the in-flight mint must hold the state lock for its whole critical section"
            );
        }

        // Release A; its mint completes under generation A.
        release_tx.send(()).expect("A still parked");
        let handle = a
            .join()
            .expect("A thread")
            .expect("A minted while generation A was current");

        // Only now can B supersede.
        let gen_b = b.join().expect("B thread");
        assert!(registry.is_current(gen_b));
        assert!(!registry.is_current(gen_a));

        // A's handle is now non-authoritative on every path.
        assert_eq!(
            store.consume_acceptance(&handle, TransferId::new(), 0),
            Err(TransientOperationError::StaleGeneration)
        );
        assert!(store.acceptance_binding(&handle).is_none());
    }

    // -- redaction --------------------------------------------

    #[test]
    fn debug_never_exposes_handle_values_or_the_proof_id() {
        let (_r, store) = store();
        let handle = store
            .mint_acceptance(AcceptanceBinding {
                transfer_id: TransferId::new(),
                chunk_index: 0,
                proof_id: "top-secret-proof".to_string(),
            })
            .unwrap();

        let store_debug = format!("{store:?}");
        assert!(
            !store_debug.contains(&handle),
            "store Debug leaked a handle"
        );
        assert!(!store_debug.contains("top-secret-proof"));
        assert!(store_debug.contains("live_bindings"));

        let binding = store.acceptance_binding(&handle).unwrap();
        let binding_debug = format!("{binding:?}");
        assert!(!binding_debug.contains("top-secret-proof"));
        assert!(binding_debug.contains("REDACTED"));
    }

    #[test]
    fn minted_ids_are_opaque_and_distinct_per_kind() {
        let (_r, store) = store();
        let a = store.mint_acceptance(acceptance(0)).unwrap();
        let v = store.mint_verification(verification()).unwrap();
        let r = store.mint_resume_cursor(cursor(0)).unwrap();
        assert!(a.starts_with("acc_") && v.starts_with("ver_") && r.starts_with("res_"));
        assert_ne!(a, v);
        assert_ne!(v, r);
        // a UUID simple form is 32 hex chars; "<pfx>_" adds 4
        assert_eq!(a.len(), 4 + 32);
    }

    // -- resume snapshots (Issue #39 Phase C1) ----------------------

    fn snapshot(held_indices: &[u64]) -> ResumeSnapshot {
        ResumeSnapshot {
            transfer_id: TransferId::new(),
            sealed: false,
            digest_algorithm: DigestAlgorithm::Sha256,
            chunk_size: 4096,
            expected_chunk_count: None,
            held: held_indices
                .iter()
                .map(|i| HeldChunkEntry {
                    chunk_index: *i,
                    digest: format!("digest-{i}"),
                })
                .collect(),
        }
    }

    #[test]
    fn a_resume_snapshot_is_registered_readable_then_dropped() {
        let (_r, store) = store();
        let snap = snapshot(&[0, 2, 5]);
        let id = store.register_resume_snapshot(snap.clone()).unwrap();

        let read = store.resume_snapshot(id).expect("live snapshot");
        assert_eq!(*read, snap);
        assert_eq!(store.live_resume_snapshot_count(), 1);

        store.drop_resume_snapshot(id);
        assert!(store.resume_snapshot(id).is_none());
        assert_eq!(store.live_resume_snapshot_count(), 0);
        // dropping an unknown id is a harmless no-op
        store.drop_resume_snapshot(Uuid::new_v4());
    }

    #[test]
    fn the_resume_snapshot_registry_fails_closed_at_capacity_without_evicting() {
        let (registry, generation) = generation();
        let store = TransientWorkerOperationStore::new(generation, Arc::clone(&registry), 64)
            .with_resume_snapshot_capacity(2);
        let a = store.register_resume_snapshot(snapshot(&[0])).unwrap();
        let b = store.register_resume_snapshot(snapshot(&[1])).unwrap();
        assert_eq!(
            store.register_resume_snapshot(snapshot(&[2])),
            Err(TransientOperationError::Saturated)
        );
        assert!(store.resume_snapshot(a).is_some());
        assert!(store.resume_snapshot(b).is_some());
    }

    #[test]
    fn resume_snapshots_are_invisible_once_the_generation_is_superseded() {
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let gen_a = registry.begin_generation(Uuid::new_v4());
        let store = TransientWorkerOperationStore::new(gen_a, Arc::clone(&registry), 64);
        let id = store.register_resume_snapshot(snapshot(&[0, 1])).unwrap();
        assert!(store.resume_snapshot(id).is_some());

        let _gen_b = registry.begin_generation(Uuid::new_v4());

        assert!(store.resume_snapshot(id).is_none());
        assert_eq!(store.live_resume_snapshot_count(), 0);
        assert_eq!(
            store.register_resume_snapshot(snapshot(&[2])),
            Err(TransientOperationError::StaleGeneration)
        );
    }

    #[test]
    fn debug_reports_the_live_resume_snapshot_count_without_leaking_contents() {
        let (_r, store) = store();
        store
            .register_resume_snapshot(snapshot(&[0, 1, 2]))
            .unwrap();
        let debug = format!("{store:?}");
        assert!(debug.contains("live_resume_snapshots"));
        assert!(!debug.contains("digest-0"));
    }
}
