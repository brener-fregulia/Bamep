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
//! successful handshake, dropped when that connection ends. It additionally
//! self-checks generation currency against
//! [`WorkerAuthorityRegistry`](crate::runtime::worker_authority::WorkerAuthorityRegistry)
//! on every operation, so a stale generation whose task still holds an `Arc`
//! to this store — for example after a newer overlapping handshake
//! superseded it — can no longer mint or consume anything
//! ([`TransientOperationError::StaleGeneration`]).
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

use bamep_domain::{ArtifactId, TransferId};
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
    id_source: IdSource,
    bindings: Mutex<HashMap<String, Binding>>,
}

impl fmt::Debug for TransientWorkerOperationStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately prints neither the opaque handle keys nor the bindings
        // (`m1-worker-data-plane-control-contract.md` "Security and logging":
        // handles "carry no diagnostic value and SHOULD be redacted").
        let live = self.bindings.lock().map(|b| b.len()).unwrap_or(usize::MAX);
        f.debug_struct("TransientWorkerOperationStore")
            .field("generation", &self.generation)
            .field("capacity", &self.capacity)
            .field("live_bindings", &live)
            .finish()
    }
}

impl TransientWorkerOperationStore {
    /// A fresh, empty store for connection `generation`, bounded to
    /// `capacity` live bindings (clamped to at least 1). `authority` is the
    /// same registry that minted `generation`; every operation first checks
    /// `authority.is_current(generation)`.
    pub fn new(
        generation: ConnectionGeneration,
        authority: Arc<WorkerAuthorityRegistry>,
        capacity: usize,
    ) -> Self {
        Self {
            generation,
            authority,
            capacity: capacity.max(1),
            id_source: csprng_id_source(),
            bindings: Mutex::new(HashMap::new()),
        }
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
            id_source,
            bindings: Mutex::new(HashMap::new()),
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
        if self.is_generation_current() {
            self.bindings
                .lock()
                .expect("transient store lock poisoned")
                .len()
        } else {
            0
        }
    }

    fn is_generation_current(&self) -> bool {
        self.authority.is_current(self.generation)
    }

    fn require_current(&self) -> Result<(), TransientOperationError> {
        if self.is_generation_current() {
            Ok(())
        } else {
            Err(TransientOperationError::StaleGeneration)
        }
    }

    /// Inserts `binding` under a fresh opaque id, failing closed at capacity
    /// ([`Saturated`](TransientOperationError::Saturated)) and on a repeated
    /// id ([`IdCollision`](TransientOperationError::IdCollision)). Neither
    /// failure ever evicts or overwrites a live binding.
    fn insert_fresh(
        &self,
        kind: HandleKind,
        binding: Binding,
    ) -> Result<String, TransientOperationError> {
        self.require_current()?;
        let mut map = self.bindings.lock().expect("transient store lock poisoned");
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
        self.require_current()?;
        let mut map = self.bindings.lock().expect("transient store lock poisoned");
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
    }

    /// Non-consuming read of an acceptance binding (diagnostics/tests, and a
    /// legitimate Phase C peek). `None` for an unknown handle, a handle of
    /// another kind, or once the owning generation is superseded.
    pub fn acceptance_binding(&self, handle: &str) -> Option<AcceptanceBinding> {
        self.require_current().ok()?;
        match self
            .bindings
            .lock()
            .expect("transient store lock poisoned")
            .get(handle)
        {
            Some(Binding::Acceptance(b)) => Some(b.clone()),
            _ => None,
        }
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
    /// verification handle minted on the current generation whose bound
    /// `transfer_id`/`artifact_id` match the presented values — the same
    /// single-use / preserve-on-mismatch discipline as
    /// [`consume_acceptance`](Self::consume_acceptance). A wrong
    /// Transfer/Artifact can never be substituted for the sealed identity
    /// this handle authorizes.
    pub fn consume_verification(
        &self,
        handle: &str,
        transfer_id: TransferId,
        artifact_id: ArtifactId,
    ) -> Result<VerificationBinding, TransientOperationError> {
        self.require_current()?;
        let mut map = self.bindings.lock().expect("transient store lock poisoned");
        let matches = match map.get(handle) {
            None => return Err(TransientOperationError::UnknownHandle),
            Some(Binding::Verification(b)) => {
                b.transfer_id == transfer_id && b.artifact_id == artifact_id
            }
            Some(_) => return Err(TransientOperationError::WrongKind),
        };
        if !matches {
            return Err(TransientOperationError::BindingMismatch);
        }
        match map.remove(handle) {
            Some(Binding::Verification(b)) => Ok(b),
            _ => unreachable!("verified under the same lock immediately above"),
        }
    }

    /// Non-consuming read of a verification binding.
    pub fn verification_binding(&self, handle: &str) -> Option<VerificationBinding> {
        self.require_current().ok()?;
        match self
            .bindings
            .lock()
            .expect("transient store lock poisoned")
            .get(handle)
        {
            Some(Binding::Verification(b)) => Some(b.clone()),
            _ => None,
        }
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
        self.require_current()?;
        let mut map = self.bindings.lock().expect("transient store lock poisoned");

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
    }

    /// Non-consuming read of a resume-cursor binding.
    pub fn resume_cursor_binding(&self, cursor: &str) -> Option<ResumeCursorBinding> {
        self.require_current().ok()?;
        match self
            .bindings
            .lock()
            .expect("transient store lock poisoned")
            .get(cursor)
        {
            Some(Binding::ResumeCursor(b)) => Some(b.clone()),
            _ => None,
        }
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
            store.consume_verification(&handle, c.transfer_id, ArtifactId::new()),
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
            store.consume_verification(&handle, a.transfer_id, ArtifactId::new()),
            Err(TransientOperationError::WrongKind)
        );
    }

    // -- verification consume -------------------------------------

    #[test]
    fn verification_consume_is_single_use_and_binds_transfer_and_artifact() {
        let (_r, store) = store();
        let v = verification();
        let handle = store.mint_verification(v.clone()).unwrap();

        assert_eq!(
            store.consume_verification(&handle, TransferId::new(), v.artifact_id),
            Err(TransientOperationError::BindingMismatch)
        );
        assert_eq!(
            store.consume_verification(&handle, v.transfer_id, ArtifactId::new()),
            Err(TransientOperationError::BindingMismatch)
        );
        assert!(store.verification_binding(&handle).is_some());

        let consumed = store
            .consume_verification(&handle, v.transfer_id, v.artifact_id)
            .unwrap();
        assert_eq!(consumed, v);
        assert_eq!(
            store.consume_verification(&handle, v.transfer_id, v.artifact_id),
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
            store.consume_verification(&handle, v.transfer_id, v.artifact_id),
            Err(TransientOperationError::StaleGeneration)
        );
        assert_eq!(
            store.advance_resume_cursor("res_x", TransferId::new(), None),
            Err(TransientOperationError::StaleGeneration)
        );
        assert!(store.verification_binding(&handle).is_none());
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
}
