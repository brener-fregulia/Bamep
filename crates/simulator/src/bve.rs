//! Simulator-side orchestration of one Bamep Virtual Endpoint (Issue #68,
//! extended for storage in #69).
//!
//! [`SimulatorBve`] is a thin boundary: it *owns* a
//! [`bamep_ve::BveRuntime`] and exposes the same
//! `create/start/observe/reset/stop/destroy` lifecycle plus
//! `reset_system_storage` (`m0-bamep-virtual-endpoint-contract.md`) to the
//! rest of the Simulator, while keeping every QEMU/KVM/QMP/`qemu-img` concern
//! behind the `bamep-ve` crate (ADR-0022, ADR-0023; #66 code boundary). The
//! Simulator never builds a QEMU command line, parses QMP, reads `/dev/kvm`,
//! runs `qemu-img`, owns a QEMU process, or derives a storage path — it asks
//! `bamep-ve` to. A Simulator BVE scenario composes its storage and
//! definition from the re-exported `bamep-ve` types directly; the Simulator
//! defines no parallel storage/disk model.
//!
//! Dependency direction is strictly `bamep-simulator -> bamep-ve`. `bamep-ve`
//! does not depend on the Simulator, Agent Protocol, Domain, or Server.
//!
//! This is an **additive** capability. The lightweight in-process Agent
//! participant ([`crate::action`], [`crate::handshake`], [`crate::transport`],
//! [`crate::data_plane`], [`crate::trusted_bootstrap`], [`crate::transfer_action`])
//! is unchanged and never routes through a BVE. #68 deliberately does not put
//! both behind one `EndpointBackend`-style trait — there is no second real
//! backend with the same VM power lifecycle, and ADR-0022 defers any generic
//! hypervisor/backend abstraction.

use bamep_ve::{detect_host_prerequisites, BveRuntime, PrerequisiteError, RuntimeError};

// Re-exported (not wrapped) so a Simulator BVE scenario composes one BVE from
// the authoritative `bamep-ve` types directly — the Simulator defines no
// parallel definition/config/storage types (Issue #68/#69).
pub use bamep_ve::{
    check_qemu_img_binary, destroy_instance_storage, ensure_system_base, prepare_instance,
    BveDefinition, BveId, BveStorageError, BveStorageLayout, BveStorageRoot, DiskAttachment,
    DiskFormat, DiskRole, Firmware, LifecycleState, PreparedInstanceStorage, RuntimeRoot,
    SourceDiskSpec, SystemBaseSpec,
};

/// Failure of a Simulator-driven BVE lifecycle step.
///
/// The smallest composition over the two authoritative `bamep-ve` error
/// families. It never collapses a startup failure, unavailable KVM, QMP
/// failure, storage failure, or runtime error into success or `Stopped` — the
/// cause is preserved and surfaced to the caller. Storage-operation failures
/// arrive as `Runtime` (`bamep-ve` folds `BveStorageError` into
/// `RuntimeError`).
#[derive(Debug, thiserror::Error)]
pub enum SimulatorBveError {
    /// A host prerequisite for launching a BVE was not satisfied
    /// (missing `qemu-system-x86_64`, unusable `/dev/kvm`, ...).
    #[error(transparent)]
    Prerequisite(#[from] PrerequisiteError),

    /// A BVE runtime lifecycle or storage operation failed.
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
    /// Prepares one BVE from Simulator-owned configuration and prepared
    /// storage, delegating validation and runtime-directory preparation to
    /// `bamep-ve`. Does not start the VM.
    ///
    /// Build `definition` with [`PreparedInstanceStorage::define_bve`] so it
    /// cannot disagree with `storage`.
    pub fn create(
        runtime_root: &RuntimeRoot,
        definition: BveDefinition,
        storage: PreparedInstanceStorage,
    ) -> Result<Self, SimulatorBveError> {
        Ok(Self {
            runtime: BveRuntime::create(runtime_root, definition, storage)?,
        })
    }

    /// Detects host prerequisites and starts the BVE.
    ///
    /// The Simulator boundary composes prerequisite detection so a scenario
    /// does not have to thread [`bamep_ve::HostPrerequisites`] itself. Errors
    /// are not hidden.
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

    /// Reboots this exact BVE instance (QMP `system_reset`). Does not touch
    /// storage — that is [`SimulatorBve::reset_system_storage`].
    pub fn reset(&mut self) -> Result<(), SimulatorBveError> {
        self.runtime.reset()?;
        Ok(())
    }

    /// Discards this BVE's disposable system overlay and recreates a fresh one
    /// from the same immutable base (Issue #69). Fails closed unless the VM is
    /// stopped. The source fixture and the base are untouched.
    pub fn reset_system_storage(&mut self) -> Result<(), SimulatorBveError> {
        self.runtime.reset_system_storage()?;
        Ok(())
    }

    /// Requests a controlled stop of this BVE and waits for the process to
    /// exit. Idempotent on an already-stopped BVE.
    pub fn stop(&mut self) -> Result<(), SimulatorBveError> {
        self.runtime.stop()?;
        Ok(())
    }

    /// Stops the BVE if needed and removes its disposable runtime/control
    /// state. Does **not** delete disk images — dispose those explicitly with
    /// [`destroy_instance_storage`] on the [`PreparedInstanceStorage`].
    pub fn destroy(self) -> Result<(), SimulatorBveError> {
        self.runtime.destroy()?;
        Ok(())
    }

    /// The definition this BVE was created from.
    pub fn definition(&self) -> &BveDefinition {
        self.runtime.definition()
    }

    /// The prepared storage this BVE runs on.
    pub fn storage(&self) -> &PreparedInstanceStorage {
        self.runtime.storage()
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
    use std::path::PathBuf;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("bamep-sim-bve-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn runtime_root(&self) -> RuntimeRoot {
            RuntimeRoot::new(self.0.join("control"))
        }

        fn storage_root(&self) -> BveStorageRoot {
            BveStorageRoot::new(self.0.join("storage")).unwrap()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Fakes the on-disk result of `prepare_instance` without `qemu-img`.
    fn fake_prepared(temp: &TempRoot, id: &BveId, with_source: bool) -> PreparedInstanceStorage {
        let layout = BveStorageLayout::for_bve(&temp.storage_root(), id);
        fs::create_dir_all(layout.instance_dir()).unwrap();
        fs::create_dir_all(layout.system_base().parent().unwrap()).unwrap();
        fs::write(layout.system_base(), b"fake-base").unwrap();
        fs::write(layout.system_overlay(), b"fake-overlay").unwrap();
        if with_source {
            fs::write(layout.source_disk(), b"fake-source").unwrap();
        }
        PreparedInstanceStorage::from_prepared_layout(layout, with_source)
    }

    fn simulator_bve(temp: &TempRoot, id: &str) -> SimulatorBve {
        let id = BveId::new(id).unwrap();
        let storage = fake_prepared(temp, &id, true);
        let definition = storage.define_bve(id, 1, 256, Firmware::Default).unwrap();
        SimulatorBve::create(&temp.runtime_root(), definition, storage).unwrap()
    }

    #[test]
    fn create_with_a_valid_definition_then_observe_is_stopped() {
        let temp = TempRoot::new();
        let mut bve = simulator_bve(&temp, "sim-create");
        assert_eq!(bve.observe().unwrap(), LifecycleState::Stopped);
        assert!(bve.instance_dir().is_dir());
        assert!(bve.storage().layout().system_overlay().is_file());
    }

    #[test]
    fn vm_reset_before_start_propagates_the_runtime_error() {
        let temp = TempRoot::new();
        let mut bve = simulator_bve(&temp, "sim-reset");
        assert!(matches!(
            bve.reset(),
            Err(SimulatorBveError::Runtime(RuntimeError::NotRunning))
        ));
    }

    #[test]
    fn stop_before_start_is_idempotent() {
        let temp = TempRoot::new();
        let mut bve = simulator_bve(&temp, "sim-stop");
        assert!(bve.stop().is_ok());
        assert_eq!(bve.observe().unwrap(), LifecycleState::Stopped);
    }

    #[test]
    fn destroy_cleans_control_state_and_leaves_storage_for_explicit_disposal() {
        let temp = TempRoot::new();
        let bve = simulator_bve(&temp, "sim-destroy");
        let control_dir = bve.instance_dir().to_path_buf();
        let overlay = bve.storage().layout().system_overlay().to_path_buf();
        bve.destroy().unwrap();
        assert!(!control_dir.exists());
        assert!(overlay.is_file(), "destroy does not delete disk images");
    }

    #[test]
    fn create_rejects_a_definition_that_disagrees_with_prepared_storage() {
        let temp = TempRoot::new();
        let id = BveId::new("sim-mismatch").unwrap();
        let storage = fake_prepared(&temp, &id, true);
        let elsewhere = temp.0.join("elsewhere.qcow2");
        fs::write(&elsewhere, b"x").unwrap();
        let def = BveDefinition::new(
            id,
            1,
            256,
            Firmware::Default,
            DiskAttachment::system(&elsewhere, DiskFormat::Qcow2).unwrap(),
        )
        .unwrap();

        assert!(matches!(
            SimulatorBve::create(&temp.runtime_root(), def, storage),
            Err(SimulatorBveError::Runtime(
                RuntimeError::StorageDefinitionMismatch { .. }
            ))
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
    fn error_model_preserves_the_runtime_and_storage_cause() {
        let err: SimulatorBveError = RuntimeError::StorageResetWhileRunning.into();
        assert!(matches!(err, SimulatorBveError::Runtime(_)));
        let err: SimulatorBveError =
            RuntimeError::Storage(BveStorageError::QemuImgProbeFailed).into();
        assert!(matches!(
            err,
            SimulatorBveError::Runtime(RuntimeError::Storage(_))
        ));
    }
}
