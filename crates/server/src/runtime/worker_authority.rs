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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use uuid::Uuid;

use crate::runtime::transient_worker_operations::{
    TransientWorkerOperationStore, DEFAULT_TRANSIENT_WORKER_OPERATION_STORE_CAPACITY,
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

    /// Publishes `store` as the current generation's observable
    /// transient-operation store — a no-op unless `generation` is still the
    /// current one, so a stale generation's late publish can never clobber a
    /// newer generation's store, mirroring [`Self::end_generation`].
    pub fn set_current_operations(
        &self,
        generation: ConnectionGeneration,
        store: Weak<TransientWorkerOperationStore>,
    ) {
        if !self.is_current(generation) {
            return;
        }
        *self
            .observed_operations
            .write()
            .expect("worker authority lock poisoned") = Some(store);
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
}
