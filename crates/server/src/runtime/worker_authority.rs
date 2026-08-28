//! Runtime Service tracking `bamepd`'s current Worker UDS connection
//! generation (Issue #37 "Connection generations"; "One authoritative
//! active connection"): in-process, never PostgreSQL-durable state, exactly
//! like `presence`/`outbound_sessions`
//! (`m0-stack-and-boundaries-baseline.md` "Component boundaries" — Runtime
//! Services).
//!
//! `bamepd` supervises exactly one Worker process for M1, so there is at
//! most one currently authoritative connection generation. A newer
//! successful handshake always supersedes whatever was current — an
//! explicit atomic runtime handoff, never a fan-out to multiple Worker
//! connections (`m1-worker-data-plane-control-contract.md` "One
//! authoritative active connection"). A stale generation's later disconnect
//! can never revert state back from a newer generation, because
//! [`WorkerAuthorityRegistry::end_generation`] only clears state that still
//! matches the generation it names.
//!
//! # Lock order (Issue #39 Phase B concurrency correction)
//!
//! One order only, never the reverse:
//!
//! 1. [`WorkerAuthorityRegistry`] `state`, then
//! 2. [`WorkerAuthorityRegistry`] `observed_operations`, or a
//!    [`TransientWorkerOperationStore`]'s internal `bindings` mutex.
//!
//! [`WorkerAuthorityRegistry::begin_generation`] and
//! [`WorkerAuthorityRegistry::end_generation`] take `state` **exclusively**;
//! every transient-authority operation — mint, consume, advance, and every
//! non-consuming read — takes `state` **shared** for the whole critical
//! section through [`WorkerAuthorityRegistry::with_current_generation`], so
//! the generation-currency check and the store mutation linearize as one
//! step against supersession. Nothing that holds `bindings` or
//! `observed_operations` ever reaches back for `state`, so the single order
//! cannot deadlock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use uuid::Uuid;

use crate::runtime::transient_worker_operations::{
    TransientOperationError, TransientWorkerOperationStore,
    DEFAULT_TRANSIENT_WORKER_OPERATION_STORE_CAPACITY,
};

/// Opaque connection-generation handle
/// (`m1-worker-data-plane-control-contract.md` "Connection generations and
/// correlation"). Comparable only for equality — callers never construct
/// one directly; it comes from [`WorkerAuthorityRegistry::begin_generation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionGeneration(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerControlState {
    NoConnection,
    Active {
        generation: ConnectionGeneration,
        worker_instance_id: Uuid,
    },
}

impl WorkerControlState {
    pub fn is_available(&self) -> bool {
        matches!(self, WorkerControlState::Active { .. })
    }
}

pub struct WorkerAuthorityRegistry {
    state: RwLock<WorkerControlState>,
    /// A **non-owning** observation handle to the current generation's
    /// transient-operation store (Issue #39 Phase B). The store itself is
    /// owned by the Worker control-plane connection task for exactly that
    /// generation's lifetime; the registry only holds a [`Weak`] so a test —
    /// or a future in-process inspector — can reach the current generation's
    /// store through the same registry it already shares, without the
    /// registry keeping a superseded/ended generation's store alive.
    observed_operations: RwLock<Option<Weak<TransientWorkerOperationStore>>>,
    /// The bounded capacity each generation's [`TransientWorkerOperationStore`]
    /// is constructed with — process-local defensive bounding, set once at
    /// composition-root startup.
    operations_capacity: usize,
    next_generation: AtomicU64,
}

impl WorkerAuthorityRegistry {
    pub fn new() -> Self {
        Self::with_operations_capacity(DEFAULT_TRANSIENT_WORKER_OPERATION_STORE_CAPACITY)
    }

    /// A registry whose per-generation transient-operation stores are bounded
    /// to `operations_capacity` live handles/cursors (clamped to at least 1).
    /// Tests use a small value to exercise the fail-closed saturation path
    /// end to end.
    pub fn with_operations_capacity(operations_capacity: usize) -> Self {
        Self {
            state: RwLock::new(WorkerControlState::NoConnection),
            observed_operations: RwLock::new(None),
            operations_capacity: operations_capacity.max(1),
            next_generation: AtomicU64::new(1),
        }
    }

    pub fn current(&self) -> WorkerControlState {
        *self.state.read().expect("worker authority lock poisoned")
    }

    /// The bounded capacity a fresh per-generation
    /// [`TransientWorkerOperationStore`] must be constructed with.
    pub fn operations_capacity(&self) -> usize {
        self.operations_capacity
    }

    /// Runs `critical` exactly once **iff** `generation` is the current one,
    /// holding the registry `state` lock in **shared** mode for the entire
    /// duration of `critical`; returns `None` — running nothing — once
    /// `generation` has been superseded or ended.
    ///
    /// This is the generation-linearization primitive (Issue #39 Phase B
    /// concurrency correction). Because [`Self::begin_generation`] and
    /// [`Self::end_generation`] take `state` **exclusively**, no supersession
    /// can linearize between the currency check here and the completion of
    /// `critical`: either `critical` runs to completion while `generation` is
    /// still authoritative and only *then* may a newer generation supersede
    /// it, or supersession already happened and `critical` never runs. There
    /// is no `checked-then-mutated-anyway` window.
    ///
    /// Lock order (see module docs): `state` (shared) → the caller's inner
    /// lock. `critical` MUST NOT call back into any [`WorkerAuthorityRegistry`]
    /// method that takes `state`.
    fn linearized_on<T>(
        &self,
        generation: ConnectionGeneration,
        critical: impl FnOnce() -> T,
    ) -> Option<T> {
        let guard = self.state.read().expect("worker authority lock poisoned");
        match *guard {
            WorkerControlState::Active {
                generation: current,
                ..
            } if current == generation => {
                let out = critical();
                drop(guard);
                Some(out)
            }
            _ => None,
        }
    }

    /// Runs `op` under [`Self::linearized_on`] for `generation`, mapping a
    /// superseded/ended generation to
    /// [`TransientOperationError::StaleGeneration`]. Every
    /// [`TransientWorkerOperationStore`] mint / consume / advance and every
    /// non-consuming read routes its authority-sensitive work through here so
    /// the generation-currency check and the store mutation are one
    /// linearized step (Issue #39 Phase B concurrency correction).
    pub(crate) fn with_current_generation<T>(
        &self,
        generation: ConnectionGeneration,
        op: impl FnOnce() -> T,
    ) -> Result<T, TransientOperationError> {
        self.linearized_on(generation, op)
            .ok_or(TransientOperationError::StaleGeneration)
    }

    /// Publishes `store` as the current generation's observable
    /// transient-operation store. The currency check and the publication are
    /// **one** linearized step (via [`Self::linearized_on`]), so a stale
    /// generation whose task belatedly publishes after a newer generation has
    /// begun cannot clobber the newer generation's slot: [`Self::begin_generation`]
    /// holds `state` exclusively for the whole check-and-publish window.
    pub fn set_current_operations(
        &self,
        generation: ConnectionGeneration,
        store: Weak<TransientWorkerOperationStore>,
    ) {
        self.linearized_on(generation, || {
            *self
                .observed_operations
                .write()
                .expect("worker authority lock poisoned") = Some(store);
        });
    }

    /// The current generation's transient-operation store, if one has been
    /// published and is still alive. `None` once the connection ended (the
    /// owning task dropped the store) or once a newer generation superseded
    /// this one and cleared the slot.
    pub fn current_operations(&self) -> Option<Arc<TransientWorkerOperationStore>> {
        self.observed_operations
            .read()
            .expect("worker authority lock poisoned")
            .as_ref()
            .and_then(Weak::upgrade)
    }

    /// Called once a new UDS connection completes its handshake
    /// successfully. Unconditionally supersedes whatever generation was
    /// previously current, including one that is still technically
    /// connected but has been replaced by a newer overlapping handshake —
    /// the explicit atomic handoff this Port's docs describe. Returns the
    /// new generation.
    pub fn begin_generation(&self, worker_instance_id: Uuid) -> ConnectionGeneration {
        let generation = ConnectionGeneration(self.next_generation.fetch_add(1, Ordering::SeqCst));
        let mut state = self.state.write().expect("worker authority lock poisoned");
        // The new generation starts with no published transient-operation
        // store — its own connection task publishes a fresh empty one
        // immediately after this returns. Any previous generation's store is
        // no longer reachable through the registry.
        *self
            .observed_operations
            .write()
            .expect("worker authority lock poisoned") = None;
        *state = WorkerControlState::Active {
            generation,
            worker_instance_id,
        };
        generation
    }

    /// Called when a connection ends (EOF, I/O error, or protocol
    /// violation). A no-op unless `generation` is still the current one —
    /// so a stale, already-superseded generation's belated disconnect can
    /// never clobber a newer generation's active state
    /// (`m1-worker-data-plane-control-contract.md` "stale response/unknown
    /// correlation").
    pub fn end_generation(&self, generation: ConnectionGeneration) {
        let mut state = self.state.write().expect("worker authority lock poisoned");
        if let WorkerControlState::Active {
            generation: current,
            ..
        } = *state
        {
            if current == generation {
                *state = WorkerControlState::NoConnection;
                *self
                    .observed_operations
                    .write()
                    .expect("worker authority lock poisoned") = None;
            }
        }
    }

    pub fn is_current(&self, generation: ConnectionGeneration) -> bool {
        matches!(
            self.current(),
            WorkerControlState::Active { generation: current, .. } if current == generation
        )
    }

    /// Test-only, non-blocking probe: `true` when the `state` lock is
    /// currently uncontended (no reader or writer holds it). Concurrency
    /// tests use it to prove that a transient-authority operation really does
    /// hold `state` **shared for its whole critical section** — the property
    /// the former check-then-act race lacked. `try_write` never blocks, even
    /// with a writer pending, so this is safe to call from a thread that must
    /// not deadlock.
    #[cfg(test)]
    pub(crate) fn state_lock_is_uncontended(&self) -> bool {
        self.state.try_write().is_ok()
    }
}

impl Default for WorkerAuthorityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_no_connection() {
        let registry = WorkerAuthorityRegistry::new();
        assert_eq!(registry.current(), WorkerControlState::NoConnection);
        assert!(!registry.current().is_available());
    }

    #[test]
    fn begin_generation_makes_authority_available() {
        let registry = WorkerAuthorityRegistry::new();
        let instance_id = Uuid::new_v4();
        let generation = registry.begin_generation(instance_id);
        assert!(registry.current().is_available());
        assert!(registry.is_current(generation));
    }

    #[test]
    fn end_generation_of_the_current_generation_clears_availability() {
        let registry = WorkerAuthorityRegistry::new();
        let generation = registry.begin_generation(Uuid::new_v4());
        registry.end_generation(generation);
        assert_eq!(registry.current(), WorkerControlState::NoConnection);
    }

    #[test]
    fn a_newer_generation_supersedes_an_older_one() {
        let registry = WorkerAuthorityRegistry::new();
        let first = registry.begin_generation(Uuid::new_v4());
        let second = registry.begin_generation(Uuid::new_v4());
        assert_ne!(first, second);
        assert!(!registry.is_current(first));
        assert!(registry.is_current(second));
    }

    #[test]
    fn a_stale_generations_disconnect_never_clobbers_a_newer_one() {
        let registry = WorkerAuthorityRegistry::new();
        let first = registry.begin_generation(Uuid::new_v4());
        let second = registry.begin_generation(Uuid::new_v4());

        // The first (now-superseded) connection's belated disconnect
        // arrives after the second has already become current.
        registry.end_generation(first);

        assert!(registry.is_current(second));
        assert!(registry.current().is_available());
    }

    #[test]
    fn reconnect_from_the_same_worker_process_keeps_the_same_instance_id_but_a_new_generation() {
        let registry = WorkerAuthorityRegistry::new();
        let instance_id = Uuid::new_v4();
        let first = registry.begin_generation(instance_id);
        registry.end_generation(first);
        let second = registry.begin_generation(instance_id);

        assert_ne!(first, second);
        match registry.current() {
            WorkerControlState::Active {
                worker_instance_id, ..
            } => {
                assert_eq!(worker_instance_id, instance_id)
            }
            WorkerControlState::NoConnection => panic!("expected an active connection"),
        }
    }

    // -- Issue #39 Phase B: per-generation transient-operation store --------

    fn published_store(
        registry: &Arc<WorkerAuthorityRegistry>,
        generation: ConnectionGeneration,
    ) -> Arc<TransientWorkerOperationStore> {
        let store = Arc::new(TransientWorkerOperationStore::new(
            generation,
            Arc::clone(registry),
            registry.operations_capacity(),
        ));
        registry.set_current_operations(generation, Arc::downgrade(&store));
        store
    }

    #[test]
    fn a_fresh_registry_publishes_no_transient_store() {
        let registry = WorkerAuthorityRegistry::new();
        assert!(registry.current_operations().is_none());
    }

    #[test]
    fn the_current_generations_published_store_is_observable_then_cleared_on_end() {
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let generation = registry.begin_generation(Uuid::new_v4());
        let store = published_store(&registry, generation);

        let observed = registry.current_operations().expect("published store");
        assert!(Arc::ptr_eq(&observed, &store));

        registry.end_generation(generation);
        assert!(
            registry.current_operations().is_none(),
            "ending the generation clears the observable store"
        );
    }

    #[test]
    fn beginning_a_new_generation_clears_the_previous_stores_observation() {
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let first = registry.begin_generation(Uuid::new_v4());
        let _first_store = published_store(&registry, first);
        assert!(registry.current_operations().is_some());

        let _second = registry.begin_generation(Uuid::new_v4());
        assert!(
            registry.current_operations().is_none(),
            "a new generation starts with no published store until its own task publishes one"
        );
    }

    #[test]
    fn a_stale_generation_cannot_publish_over_the_current_ones_store() {
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let first = registry.begin_generation(Uuid::new_v4());
        let second = registry.begin_generation(Uuid::new_v4());
        let second_store = published_store(&registry, second);

        // The superseded first generation's task belatedly tries to publish.
        let stale = Arc::new(TransientWorkerOperationStore::new(
            first,
            Arc::clone(&registry),
            registry.operations_capacity(),
        ));
        registry.set_current_operations(first, Arc::downgrade(&stale));

        let observed = registry.current_operations().expect("current store");
        assert!(Arc::ptr_eq(&observed, &second_store));
    }

    #[test]
    fn a_dropped_store_is_no_longer_observable_even_without_end_generation() {
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let generation = registry.begin_generation(Uuid::new_v4());
        {
            let _store = published_store(&registry, generation);
            assert!(registry.current_operations().is_some());
        }
        // The owning connection task dropped its `Arc`; the registry's `Weak`
        // can no longer upgrade.
        assert!(registry.current_operations().is_none());
    }

    /// Deterministic proof (no sleeps) that the currency check and the
    /// publication in [`WorkerAuthorityRegistry::set_current_operations`] are
    /// **one** linearized step relative to
    /// [`WorkerAuthorityRegistry::begin_generation`] (Issue #39 Phase B
    /// concurrency correction — the former `set_current_operations` TOCTOU).
    ///
    /// Thread P parks holding the `state` read lock as generation A, using the
    /// very same [`WorkerAuthorityRegistry::with_current_generation`]
    /// primitive that `set_current_operations` and every store op route
    /// through. While P is parked, `RwLock` exclusion makes it a hard
    /// invariant that a concurrent `begin_generation` for B cannot complete,
    /// and a concurrent stale `set_current_operations(gen_a, …)` cannot land
    /// after B. When the dust settles, the newer generation's store owns the
    /// observed slot and no stale publish clobbered it.
    #[test]
    fn a_stale_generation_cannot_publish_over_a_newer_one_under_concurrency() {
        use std::sync::mpsc;
        use std::thread;

        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let gen_a = registry.begin_generation(Uuid::new_v4());
        let store_a = Arc::new(TransientWorkerOperationStore::new(
            gen_a,
            Arc::clone(&registry),
            registry.operations_capacity(),
        ));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        // Thread P: parks holding the state read lock as generation A.
        let registry_p = Arc::clone(&registry);
        let p = thread::spawn(move || {
            registry_p.with_current_generation(gen_a, || {
                entered_tx.send(()).expect("test receiver alive");
                release_rx.recv().expect("test sender alive");
            })
        });
        entered_rx
            .recv()
            .expect("P holds the state read lock as generation A");

        // Thread B: supersede generation A and publish B's own store.
        let registry_b = Arc::clone(&registry);
        let (b_started_tx, b_started_rx) = mpsc::channel();
        let b = thread::spawn(move || {
            b_started_tx.send(()).expect("test receiver alive");
            let gen_b = registry_b.begin_generation(Uuid::new_v4());
            let store_b = Arc::new(TransientWorkerOperationStore::new(
                gen_b,
                Arc::clone(&registry_b),
                registry_b.operations_capacity(),
            ));
            registry_b.set_current_operations(gen_b, Arc::downgrade(&store_b));
            (gen_b, store_b)
        });
        b_started_rx.recv().expect("B is attempting supersession");

        // Thread S: a stale publisher for generation A, racing B.
        let registry_s = Arc::clone(&registry);
        let store_a_s = Arc::clone(&store_a);
        let s = thread::spawn(move || {
            registry_s.set_current_operations(gen_a, Arc::downgrade(&store_a_s));
        });

        // Deterministic, non-blocking: while P's linearized section is open
        // the state lock is held shared and cannot be write-locked, so B's
        // begin_generation is provably blocked and generation A has not been
        // superseded. (A blocking `is_current` read would deadlock behind the
        // pending writer, so only the non-blocking probe is used here.)
        for _ in 0..10_000 {
            assert!(
                !registry.state_lock_is_uncontended(),
                "P's linearized section must hold the state lock for its whole duration"
            );
        }

        // Release P; B and S may now make progress.
        release_tx.send(()).expect("P still parked");
        p.join()
            .expect("P thread")
            .expect("P ran under generation A");
        s.join().expect("S thread");
        let (gen_b, store_b) = b.join().expect("B thread");

        // A further belated stale publish from generation A must also be a
        // no-op now that B is current.
        registry.set_current_operations(gen_a, Arc::downgrade(&store_a));

        assert!(registry.is_current(gen_b));
        assert!(!registry.is_current(gen_a));
        let observed = registry
            .current_operations()
            .expect("the newer generation published a store");
        assert!(
            Arc::ptr_eq(&observed, &store_b),
            "generation B's store owns the observed slot; no stale publish clobbered it"
        );
    }
}
