//! Simulator-side orchestration of one Bamep Virtual Endpoint (Issue #68).
//!
//! [`SimulatorBve`] is a thin boundary: it *owns* a
//! [`bamep_ve::BveRuntime`] and exposes the same
//! `create/start/observe/reset/stop/destroy` lifecycle
//! (`m0-bamep-virtual-endpoint-contract.md`) to the rest of the Simulator,
//! while keeping every QEMU/KVM/QMP concern behind the `bamep-ve` crate
//! (ADR-0022; #66 code boundary). The Simulator never builds a QEMU command
//! line, parses QMP, reads `/dev/kvm`, owns a QEMU process, or knows a control
//! socket path — it asks `bamep-ve` to.
//!
//! Dependency direction is strictly `bamep-simulator -> bamep-ve`. `bamep-ve`
//! does not depend on the Simulator, Agent Protocol, Domain, or Server.
//!
//! This is an **additive** capability. The lightweight in-process Agent
//! participant ([`crate::action`], [`crate::handshake`], [`crate::transport`],
//! [`crate::data_plane`], [`crate::trusted_bootstrap`], [`crate::transfer_action`])
//! is unchanged and never routes through a BVE. A BVE is a real virtual
//! machine with a power lifecycle; a lightweight participant is Agent-side
//! protocol behavior. #68 deliberately does not put both behind one
//! `EndpointBackend`-style trait — there is no second real backend with the
//! same lifecycle semantics to justify it, and ADR-0022 defers any generic
//! hypervisor/backend abstraction.

use bamep_ve::{detect_host_prerequisites, BveRuntime, PrerequisiteError, RuntimeError};

// Re-exported (not wrapped) so a Simulator BVE scenario composes one BVE from
// the authoritative `bamep-ve` types directly — the Simulator defines no
// parallel definition/config/root types (Issue #68 "não duplicar BVE config").
pub use bamep_ve::{BveDefinition, BveId, Firmware, LifecycleState, RuntimeRoot};

/// Failure of a Simulator-driven BVE lifecycle step.
///
/// The smallest composition over the two authoritative `bamep-ve` error
/// families. It never collapses a startup failure, unavailable KVM, QMP
/// failure, or runtime error into success or `Stopped` — the cause is
/// preserved and surfaced to the caller.
#[derive(Debug, thiserror::Error)]
pub enum SimulatorBveError {
    /// A host prerequisite for launching a BVE was not satisfied
    /// (missing `qemu-system-x86_64`, unusable `/dev/kvm`, ...).
    #[error(transparent)]
    Prerequisite(#[from] PrerequisiteError),

    /// A BVE runtime lifecycle operation failed (definition precondition,
    /// spawn, startup confirmation, observation, control, or cleanup).
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

/// One BVE, orchestrated by the Simulator. Owns the underlying
/// [`bamep_ve::BveRuntime`]; not `Clone`.
#[derive(Debug)]
pub struct SimulatorBve {
    runtime: BveRuntime,
}

impl SimulatorBve {
    /// Prepares one BVE from Simulator-owned configuration, delegating
    /// validation and runtime-directory preparation to `bamep-ve`. Does not
    /// start the VM.
    pub fn create(
        root: &RuntimeRoot,
        definition: BveDefinition,
    ) -> Result<Self, SimulatorBveError> {
        Ok(Self {
            runtime: BveRuntime::create(root, definition)?,
        })
    }

    /// Detects host prerequisites and starts the BVE.
    ///
    /// The Simulator boundary composes prerequisite detection so a scenario
    /// does not have to thread [`bamep_ve::HostPrerequisites`] itself. Errors
    /// are not hidden: a failed prerequisite is a [`SimulatorBveError::Prerequisite`],
    /// and a QEMU/KVM startup failure is a [`SimulatorBveError::Runtime`].
    pub fn start(&mut self) -> Result<(), SimulatorBveError> {
        let prerequisites = detect_host_prerequisites()?;
        self.runtime.start(&prerequisites)?;
        Ok(())
    }

    /// Returns the truthful lifecycle state of this exact BVE, as reported by
    /// `bamep-ve`. Never inferred; a real observation failure is an error.
    pub fn observe(&mut self) -> Result<LifecycleState, SimulatorBveError> {
        Ok(self.runtime.observe()?)
    }

    /// Resets (reboots) this exact BVE instance.
    pub fn reset(&mut self) -> Result<(), SimulatorBveError> {
        self.runtime.reset()?;
        Ok(())
    }

    /// Requests a controlled stop of this BVE and waits for the process to
    /// exit. Idempotent on an already-stopped BVE, per the `bamep-ve` runtime
    /// contract.
    pub fn stop(&mut self) -> Result<(), SimulatorBveError> {
        self.runtime.stop()?;
        Ok(())
    }

    /// Stops the BVE if needed and removes its disposable runtime/control
    /// state.
    pub fn destroy(self) -> Result<(), SimulatorBveError> {
        self.runtime.destroy()?;
        Ok(())
    }

    /// The definition this BVE was created from.
    pub fn definition(&self) -> &BveDefinition {
        self.runtime.definition()
    }

    /// The per-instance runtime directory owned by `bamep-ve`.
    pub fn instance_dir(&self) -> &std::path::Path {
        self.runtime.instance_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("bamep-sim-bve-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn root(&self) -> RuntimeRoot {
            RuntimeRoot::new(&self.0)
        }

        fn disk(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            let mut f = fs::File::create(&path).unwrap();
            f.write_all(&[0u8; 512]).unwrap();
            path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn definition(temp: &TempRoot, id: &str) -> BveDefinition {
        BveDefinition::new(
            BveId::new(id).unwrap(),
            1,
            256,
            Firmware::Default,
            temp.disk(&format!("{id}.raw")),
        )
        .unwrap()
    }

    #[test]
    fn create_with_a_valid_definition_then_observe_is_stopped() {
        let temp = TempRoot::new();
        let mut bve = SimulatorBve::create(&temp.root(), definition(&temp, "sim-create")).unwrap();
        assert_eq!(bve.observe().unwrap(), LifecycleState::Stopped);
        assert!(bve.instance_dir().is_dir());
    }

    #[test]
    fn reset_before_start_propagates_the_runtime_error() {
        let temp = TempRoot::new();
        let mut bve = SimulatorBve::create(&temp.root(), definition(&temp, "sim-reset")).unwrap();
        assert!(matches!(
            bve.reset(),
            Err(SimulatorBveError::Runtime(RuntimeError::NotRunning))
        ));
    }

    #[test]
    fn stop_before_start_is_idempotent() {
        let temp = TempRoot::new();
        let mut bve = SimulatorBve::create(&temp.root(), definition(&temp, "sim-stop")).unwrap();
        assert!(bve.stop().is_ok());
        assert_eq!(bve.observe().unwrap(), LifecycleState::Stopped);
    }

    #[test]
    fn destroy_cleans_the_runtime_directory() {
        let temp = TempRoot::new();
        let bve = SimulatorBve::create(&temp.root(), definition(&temp, "sim-destroy")).unwrap();
        let dir = bve.instance_dir().to_path_buf();
        bve.destroy().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn create_propagates_a_definition_precondition_failure() {
        let temp = TempRoot::new();
        let def = BveDefinition::new(
            BveId::new("sim-nodisk").unwrap(),
            1,
            256,
            Firmware::Default,
            temp.0.join("absent.raw"),
        )
        .unwrap();
        assert!(matches!(
            SimulatorBve::create(&temp.root(), def),
            Err(SimulatorBveError::Runtime(RuntimeError::Definition(_)))
        ));
    }

    #[test]
    fn error_model_preserves_the_prerequisite_cause() {
        let err: SimulatorBveError = PrerequisiteError::KvmDeviceMissing {
            device: PathBuf::from("/definitely/not/here/kvm"),
        }
        .into();
        assert!(matches!(err, SimulatorBveError::Prerequisite(_)));
    }

    #[test]
    fn error_model_preserves_the_runtime_cause() {
        let err: SimulatorBveError = RuntimeError::AlreadyStarted.into();
        assert!(matches!(err, SimulatorBveError::Runtime(_)));
    }
}
