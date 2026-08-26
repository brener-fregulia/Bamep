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
use std::sync::RwLock;

use uuid::Uuid;

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
    next_generation: AtomicU64,
}

impl WorkerAuthorityRegistry {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(WorkerControlState::NoConnection),
            next_generation: AtomicU64::new(1),
        }
    }

    pub fn current(&self) -> WorkerControlState {
        *self.state.read().expect("worker authority lock poisoned")
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
}
