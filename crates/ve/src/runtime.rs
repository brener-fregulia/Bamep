//! `BveRuntime`: the host-side lifecycle owner for one BVE.
//!
//! It owns, explicitly and by handle, the exact QEMU process it started, that
//! process's QMP control socket, and a per-instance runtime directory. It
//! never locates a VM by scanning for QEMU-looking processes — lifecycle
//! operations always reach the instance this runtime created.
//!
//! Lifecycle (`m0-bamep-virtual-endpoint-contract.md`): `create` prepares and
//! validates; `start` launches QEMU/KVM and confirms the control boundary
//! came up; `observe` reports truthful `Stopped`/`Running` (never inferring
//! guest health); `reset` issues `system_reset` on the same VM; `stop`
//! requests a controlled `quit` and waits for exit; `destroy` guarantees the
//! runtime is not left active and removes only the transitory control
//! artifacts it created.
//!
//! `reset` and [`BveRuntime::reset_system_storage`] are **distinct** (Issue
//! #69): `reset` reboots the running machine and touches no disk;
//! `reset_system_storage` requires the VM stopped and discards + recreates
//! only the disposable system overlay from the same immutable base.
//! `destroy` still removes only lifecycle/control resources — disk images are
//! disposed explicitly via [`crate::storage::destroy_instance_storage`].

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::definition::{BveDefinition, BveId, DefinitionError, NetworkAttachment};
use crate::network::PreparedBveNetwork;
use crate::qemu::{HostPrerequisites, QemuCommand};
use crate::qmp::{QmpConnection, QmpError};
use crate::storage::{self, BveStorageError, PreparedInstanceStorage};

/// How long [`BveRuntime::start`] waits for QEMU's QMP socket to accept a
/// handshake before treating startup as failed.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(15);

/// How long [`BveRuntime::stop`] waits for QEMU to exit after `quit` before
/// falling back to killing the owned process.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// The truthful lifecycle state Issue #67 requires. Nothing here implies guest
/// OS or Agent health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// No QEMU process is owned, or the owned process has exited.
    Stopped,
    /// The owned QEMU process is alive and its QMP control boundary answered.
    Running,
}

/// The root directory under which each BVE gets an isolated
/// `<root>/<bve-id>/` control subtree.
#[derive(Debug, Clone)]
pub struct RuntimeRoot {
    base: PathBuf,
}

impl RuntimeRoot {
    /// Wraps a base directory. Prefer a configurable or temporary path for
    /// tests and local runs; do not hard-code a global system directory.
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// The base directory.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The per-instance directory for `id`. Because [`BveId`] is validated to
    /// be exactly one safe path segment, this is always `<base>/<id>` and
    /// never escapes `base`.
    pub fn instance_dir(&self, id: &BveId) -> PathBuf {
        self.base.join(id.as_str())
    }
}

/// Why a BVE lifecycle operation failed.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// A definition precondition (such as system-disk presence) failed.
    #[error(transparent)]
    Definition(#[from] DefinitionError),

    /// Filesystem I/O on the runtime directory or control socket failed.
    #[error("BVE runtime I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The QEMU process could not be spawned.
    #[error("failed to spawn {binary:?}: {source}")]
    Spawn {
        /// The binary that failed to spawn.
        binary: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// `start` was called on a runtime that already owns a process.
    #[error("BVE is already started")]
    AlreadyStarted,

    /// QEMU did not reach a usable state: it exited immediately, or its QMP
    /// control boundary never became available.
    #[error(
        "BVE did not reach a usable state ({reason}); qemu exit: {exit:?}; qemu stderr: {stderr}"
    )]
    StartupFailed {
        /// What went wrong (QMP error or an early exit).
        reason: String,
        /// The process exit status, if it had exited.
        exit: Option<ExitStatus>,
        /// Captured QEMU stderr (may be empty).
        stderr: String,
    },

    /// An operation that needs a running VM was called while it was stopped.
    #[error("BVE is not running")]
    NotRunning,

    /// `observe` could not truthfully determine state: the process is alive
    /// but its QMP boundary did not answer.
    #[error("BVE observation failed: {0}")]
    Observe(#[source] QmpError),

    /// A control command (`reset`, `stop`) failed over QMP.
    #[error("BVE control failed: {0}")]
    Control(#[source] QmpError),

    /// After cleanup the instance directory still held entries this runtime
    /// did not create; it was not blindly removed.
    #[error("instance directory {dir} still holds unexpected entries after cleanup: {source}")]
    DestroyDirtyInstanceDir {
        /// The instance directory left in place.
        dir: PathBuf,
        /// The `remove_dir` error.
        #[source]
        source: std::io::Error,
    },

    /// The definition's disk attachments do not match the prepared storage
    /// handed to [`BveRuntime::create`] (Issue #69 — storage/definition
    /// consistency).
    #[error("BVE definition does not match its prepared storage: {detail}")]
    StorageDefinitionMismatch {
        /// What disagreed.
        detail: String,
    },

    /// The definition's network attachment does not match the
    /// [`PreparedBveNetwork`] handed to
    /// [`BveRuntime::create_with_isolated_network`] (Issue #70 —
    /// network/definition consistency, same lesson as #69).
    #[error("BVE definition network does not match its prepared isolated network: {detail}")]
    NetworkDefinitionMismatch {
        /// What disagreed.
        detail: String,
    },

    /// Storage reset was requested while the VM was still running. It fails
    /// closed: the disposable overlay is never unlinked or recreated under a
    /// live QEMU process (Issue #69 §15).
    #[error("cannot reset BVE storage while the VM is running; stop it first")]
    StorageResetWhileRunning,

    /// A storage operation failed.
    #[error(transparent)]
    Storage(#[from] BveStorageError),
}

/// One BVE's host-side lifecycle. Not `Clone`: it owns a process handle.
#[derive(Debug)]
pub struct BveRuntime {
    definition: BveDefinition,
    storage: PreparedInstanceStorage,
    instance_dir: PathBuf,
    qmp_socket: PathBuf,
    child: Option<Child>,
}

impl BveRuntime {
    /// Prepares one BVE: checks the definition's disk attachments agree with
    /// `storage`, verifies the images exist, creates the per-instance runtime
    /// directory, and clears any stale control socket. Does **not** start
    /// QEMU.
    ///
    /// `storage` must come from [`crate::storage::prepare_instance`]; the
    /// definition should be built with
    /// [`PreparedInstanceStorage::define_bve`] so the two cannot disagree,
    /// and this constructor rejects them if they do
    /// ([`RuntimeError::StorageDefinitionMismatch`]).
    pub fn create(
        root: &RuntimeRoot,
        definition: BveDefinition,
        storage: PreparedInstanceStorage,
    ) -> Result<Self, RuntimeError> {
        if definition.system_disk() != &storage.system_attachment() {
            return Err(RuntimeError::StorageDefinitionMismatch {
                detail: format!(
                    "system disk {} vs prepared overlay {}",
                    definition.system_disk().path().display(),
                    storage.layout().system_overlay().display()
                ),
            });
        }
        if definition.source_disk() != storage.source_attachment().as_ref() {
            return Err(RuntimeError::StorageDefinitionMismatch {
                detail: "source disk attachment does not match the prepared source fixture"
                    .to_string(),
            });
        }
        definition.ensure_disks_present()?;

        let instance_dir = root.instance_dir(definition.id());
        fs::create_dir_all(&instance_dir)?;

        let qmp_socket = instance_dir.join("qmp.sock");
        Self::clear_stale_socket(&qmp_socket)?;

        Ok(Self {
            definition,
            storage,
            instance_dir,
            qmp_socket,
            child: None,
        })
    }

    /// Like [`BveRuntime::create`], but for a BVE that uses a prepared
    /// isolated TAP network (Issue #70).
    ///
    /// `definition` must already carry the matching isolated-TAP attachment —
    /// build it with [`PreparedBveNetwork::attach`] so the two cannot
    /// disagree. This constructor still cross-checks and rejects a mismatch
    /// ([`RuntimeError::NetworkDefinitionMismatch`]), the same guard #69 added
    /// for storage. It performs **no** privileged network operation: the
    /// network must already be prepared, and QEMU opens the user-owned TAP
    /// unprivileged.
    pub fn create_with_isolated_network(
        root: &RuntimeRoot,
        definition: BveDefinition,
        storage: PreparedInstanceStorage,
        network: &PreparedBveNetwork,
    ) -> Result<Self, RuntimeError> {
        match definition.network() {
            NetworkAttachment::IsolatedTap { ifname } if ifname == network.tap_ifname() => {}
            NetworkAttachment::IsolatedTap { ifname } => {
                return Err(RuntimeError::NetworkDefinitionMismatch {
                    detail: format!(
                        "definition TAP {ifname} vs prepared TAP {}",
                        network.tap_ifname()
                    ),
                });
            }
            NetworkAttachment::UserMode => {
                return Err(RuntimeError::NetworkDefinitionMismatch {
                    detail: "definition uses user-mode networking but a prepared isolated \
                             network was supplied"
                        .to_string(),
                });
            }
        }
        Self::create(root, definition, storage)
    }

    /// The prepared storage this BVE runs on.
    pub fn storage(&self) -> &PreparedInstanceStorage {
        &self.storage
    }

    /// Discards the disposable system overlay and recreates a fresh one from
    /// the same immutable base (Issue #69). The source fixture and the base
    /// are untouched.
    ///
    /// This is **not** [`BveRuntime::reset`] (a QMP `system_reset` reboot). It
    /// fails closed with [`RuntimeError::StorageResetWhileRunning`] unless the
    /// VM is stopped, so the overlay is never recreated under a live QEMU
    /// process.
    pub fn reset_system_storage(&mut self) -> Result<(), RuntimeError> {
        if self.process_is_alive()? {
            return Err(RuntimeError::StorageResetWhileRunning);
        }
        storage::reset_system_storage(&self.storage)?;
        Ok(())
    }

    /// The per-instance runtime directory.
    pub fn instance_dir(&self) -> &Path {
        &self.instance_dir
    }

    /// The QMP control socket path for this BVE.
    pub fn qmp_socket(&self) -> &Path {
        &self.qmp_socket
    }

    /// The definition this runtime represents.
    pub fn definition(&self) -> &BveDefinition {
        &self.definition
    }

    /// Launches QEMU/KVM for this BVE and confirms startup: it returns an
    /// error (never success) if the process dies immediately or the QMP
    /// control boundary does not become available within
    /// [`DEFAULT_START_TIMEOUT`].
    pub fn start(&mut self, _prerequisites: &HostPrerequisites) -> Result<(), RuntimeError> {
        if self.child.is_some() {
            return Err(RuntimeError::AlreadyStarted);
        }
        Self::clear_stale_socket(&self.qmp_socket)?;

        let command = QemuCommand::for_bve(&self.definition, &self.qmp_socket);
        let mut child = Command::new(command.program())
            .args(command.args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| RuntimeError::Spawn {
                binary: command.program().to_string(),
                source,
            })?;

        let deadline = Instant::now() + DEFAULT_START_TIMEOUT;
        loop {
            if let Some(exit) = child.try_wait()? {
                let stderr = Self::drain_stderr(&mut child);
                return Err(RuntimeError::StartupFailed {
                    reason: "QEMU exited during startup".to_string(),
                    exit: Some(exit),
                    stderr,
                });
            }

            match QmpConnection::connect(&self.qmp_socket) {
                Ok(_handshaken) => {
                    self.child = Some(child);
                    return Ok(());
                }
                Err(err) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let exit = child.wait().ok();
                        let stderr = Self::drain_stderr(&mut child);
                        return Err(RuntimeError::StartupFailed {
                            reason: err.to_string(),
                            exit,
                            stderr,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    /// Returns truthful lifecycle state. `Stopped` when no process is owned or
    /// the owned process has exited; `Running` when the owned process is alive
    /// and its QMP boundary answered. A live process whose QMP boundary does
    /// not answer is a real observation failure ([`RuntimeError::Observe`]),
    /// not a fabricated state.
    pub fn observe(&mut self) -> Result<LifecycleState, RuntimeError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(LifecycleState::Stopped);
        };
        if child.try_wait()?.is_some() {
            self.child = None;
            return Ok(LifecycleState::Stopped);
        }

        let mut conn = QmpConnection::connect(&self.qmp_socket).map_err(RuntimeError::Observe)?;
        conn.query_run_state().map_err(RuntimeError::Observe)?;
        Ok(LifecycleState::Running)
    }

    /// Issues `system_reset` on this exact VM. Requires it to be running.
    /// Does not reset storage and does not create a replacement instance.
    pub fn reset(&mut self) -> Result<(), RuntimeError> {
        self.ensure_process_alive()?;
        let mut conn = QmpConnection::connect(&self.qmp_socket).map_err(RuntimeError::Control)?;
        conn.system_reset().map_err(RuntimeError::Control)
    }

    /// Requests a controlled shutdown (`quit`) and waits for the owned process
    /// to exit. If QMP is unavailable but the process is provably the one this
    /// runtime owns, it performs the single minimal fallback needed for a
    /// safe, explicit teardown: kill the owned process. No retry loop. An
    /// already-stopped BVE returns `Ok` without fabricating state.
    pub fn stop(&mut self) -> Result<(), RuntimeError> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if child.try_wait()?.is_some() {
            return Ok(());
        }

        let quit_outcome = QmpConnection::connect(&self.qmp_socket).and_then(|mut c| c.quit());

        if wait_for_exit(&mut child, DEFAULT_STOP_TIMEOUT)?.is_none() {
            // QMP did not bring QEMU down in time (unavailable, or `quit`
            // ignored). Kill the process this runtime owns — one explicit
            // action, not a retry storm.
            let _ = child.kill();
            child.wait()?;
        }
        drop(quit_outcome);
        Ok(())
    }

    /// Guarantees the runtime is not left active, then removes only the
    /// transitory lifecycle/control artifacts this runtime created (the QMP
    /// socket and the per-instance directory). It never recursively deletes an
    /// unvalidated path: #67 only ever places `qmp.sock` in the instance
    /// directory, so a non-recursive `remove_dir` is sufficient and
    /// fail-closed — an unexpected leftover surfaces as an error.
    pub fn destroy(mut self) -> Result<(), RuntimeError> {
        if self.child.is_some() {
            self.stop()?;
        }

        match fs::remove_file(&self.qmp_socket) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(RuntimeError::Io(e)),
        }

        match fs::remove_dir(&self.instance_dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(RuntimeError::DestroyDirtyInstanceDir {
                dir: self.instance_dir.clone(),
                source,
            }),
        }
    }

    /// Whether this runtime owns a QEMU process that is still alive. Reaps a
    /// process that has exited.
    fn process_is_alive(&mut self) -> Result<bool, RuntimeError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        if child.try_wait()?.is_some() {
            self.child = None;
            return Ok(false);
        }
        Ok(true)
    }

    fn ensure_process_alive(&mut self) -> Result<(), RuntimeError> {
        if self.process_is_alive()? {
            Ok(())
        } else {
            Err(RuntimeError::NotRunning)
        }
    }

    /// Removes a socket path if it exists as a leftover. A stale socket file
    /// must never be mistaken for a live control boundary.
    fn clear_stale_socket(path: &Path) -> Result<(), RuntimeError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(RuntimeError::Io(e)),
        }
    }

    fn drain_stderr(child: &mut Child) -> String {
        let Some(mut stderr) = child.stderr.take() else {
            return String::new();
        };
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf.trim().to_string()
    }
}

impl Drop for BveRuntime {
    /// Best-effort: never leak an owned QEMU process if the runtime is dropped
    /// without `stop`/`destroy` (for example on a test panic).
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Polls `child` until it exits or `timeout` elapses.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>, RuntimeError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{DiskAttachment, DiskFormat, Firmware};
    use crate::network::{BveNetworkPlan, PreparedBveNetwork};
    use crate::storage::{BveStorageLayout, BveStorageRoot, PreparedInstanceStorage};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("bamep-ve-rt-{}", uuid::Uuid::new_v4()));
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

    /// Fakes the on-disk result of `storage::prepare_instance` without
    /// `qemu-img`: real files at the deterministic layout paths (their
    /// contents are irrelevant to lifecycle/path logic).
    fn fake_prepared(
        storage_root: &BveStorageRoot,
        id: &BveId,
        with_source: bool,
    ) -> PreparedInstanceStorage {
        let layout = BveStorageLayout::for_bve(storage_root, id);
        fs::create_dir_all(layout.instance_dir()).unwrap();
        fs::create_dir_all(layout.system_base().parent().unwrap()).unwrap();
        fs::write(layout.system_base(), b"fake-base").unwrap();
        fs::write(layout.system_overlay(), b"fake-overlay").unwrap();
        if with_source {
            fs::write(layout.source_disk(), b"fake-source").unwrap();
        }
        PreparedInstanceStorage::from_prepared_layout(layout, with_source)
    }

    fn bve(temp: &TempRoot, id: &str) -> (BveRuntime, PreparedInstanceStorage) {
        let id = BveId::new(id).unwrap();
        let storage = fake_prepared(&temp.storage_root(), &id, true);
        let def = storage.define_bve(id, 1, 256, Firmware::Default).unwrap();
        let rt = BveRuntime::create(&temp.runtime_root(), def, storage.clone()).unwrap();
        (rt, storage)
    }

    #[test]
    fn instance_dirs_are_isolated_and_inside_the_root() {
        let temp = TempRoot::new();
        let root = temp.runtime_root();
        let a = root.instance_dir(&BveId::new("bve-a").unwrap());
        let b = root.instance_dir(&BveId::new("bve-b").unwrap());

        assert_ne!(a, b);
        assert!(a.starts_with(root.base()));
        assert!(b.starts_with(root.base()));
        assert_eq!(a.parent(), Some(root.base()));
    }

    #[test]
    fn create_prepares_the_instance_without_starting() {
        let temp = TempRoot::new();
        let (rt, _storage) = bve(&temp, "bve-create");

        assert!(rt.instance_dir().is_dir());
        assert_eq!(rt.qmp_socket(), rt.instance_dir().join("qmp.sock"));
        assert!(rt.child.is_none());
        assert!(!rt.qmp_socket().exists());
    }

    #[test]
    fn create_clears_a_stale_control_socket() {
        let temp = TempRoot::new();
        let id = BveId::new("bve-stale").unwrap();
        let dir = temp.runtime_root().instance_dir(&id);
        fs::create_dir_all(&dir).unwrap();
        fs::File::create(dir.join("qmp.sock")).unwrap();

        let storage = fake_prepared(&temp.storage_root(), &id, true);
        let def = storage.define_bve(id, 1, 256, Firmware::Default).unwrap();
        let rt = BveRuntime::create(&temp.runtime_root(), def, storage).unwrap();
        assert!(!rt.qmp_socket().exists(), "stale socket must be cleared");
    }

    #[test]
    fn observe_reports_stopped_before_start_even_with_a_stale_socket() {
        let temp = TempRoot::new();
        let (mut rt, _s) = bve(&temp, "bve-obs");

        // A leftover socket file must not fabricate a Running result: observe
        // keys off the owned process handle, which is absent here.
        fs::File::create(rt.qmp_socket()).unwrap();
        assert_eq!(rt.observe().unwrap(), LifecycleState::Stopped);
    }

    #[test]
    fn vm_reset_on_a_stopped_bve_is_not_running() {
        let temp = TempRoot::new();
        let (mut rt, _s) = bve(&temp, "bve-reset");
        assert!(matches!(rt.reset(), Err(RuntimeError::NotRunning)));
    }

    #[test]
    fn stop_on_a_stopped_bve_succeeds_without_fabricating_state() {
        let temp = TempRoot::new();
        let (mut rt, _s) = bve(&temp, "bve-stop");
        assert!(rt.stop().is_ok());
        assert_eq!(rt.observe().unwrap(), LifecycleState::Stopped);
    }

    #[test]
    fn storage_reset_fails_closed_while_a_process_is_alive() {
        let temp = TempRoot::new();
        let (mut rt, _s) = bve(&temp, "bve-storage-running");

        // Simulate a live VM by owning a real, still-running child.
        rt.child = Some(
            Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn a placeholder child"),
        );
        assert!(matches!(
            rt.reset_system_storage(),
            Err(RuntimeError::StorageResetWhileRunning)
        ));

        let mut child = rt.child.take().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn create_rejects_a_missing_disk_image() {
        let temp = TempRoot::new();
        let id = BveId::new("bve-nodisk").unwrap();
        let storage = fake_prepared(&temp.storage_root(), &id, true);
        fs::remove_file(storage.layout().system_overlay()).unwrap();
        let def = storage.define_bve(id, 1, 256, Firmware::Default).unwrap();

        assert!(matches!(
            BveRuntime::create(&temp.runtime_root(), def, storage),
            Err(RuntimeError::Definition(
                DefinitionError::DiskImageMissing { .. }
            ))
        ));
    }

    #[test]
    fn create_rejects_a_definition_that_disagrees_with_prepared_storage() {
        let temp = TempRoot::new();
        let id = BveId::new("bve-mismatch").unwrap();
        let storage = fake_prepared(&temp.storage_root(), &id, true);

        // A definition pointing at a different system image than was prepared.
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
            BveRuntime::create(&temp.runtime_root(), def, storage),
            Err(RuntimeError::StorageDefinitionMismatch { .. })
        ));
    }

    #[test]
    fn create_with_isolated_network_rejects_mismatch_and_accepts_the_matching_pair() {
        let temp = TempRoot::new();
        let id = BveId::new("bve-net").unwrap();
        let storage = fake_prepared(&temp.storage_root(), &id, false);

        let net_a = PreparedBveNetwork::from_prepared_plan(BveNetworkPlan::for_bve(&id));
        let net_b = PreparedBveNetwork::from_prepared_plan(BveNetworkPlan::for_bve(
            &BveId::new("bve-net-other").unwrap(),
        ));

        // A user-mode definition + a prepared network -> mismatch.
        let usermode = storage
            .define_bve(id.clone(), 1, 256, Firmware::Default)
            .unwrap();
        assert!(matches!(
            BveRuntime::create_with_isolated_network(
                &temp.runtime_root(),
                usermode,
                storage.clone(),
                &net_a
            ),
            Err(RuntimeError::NetworkDefinitionMismatch { .. })
        ));

        // Attached to net_b but created against net_a -> mismatch.
        let def_b = net_b.attach(
            storage
                .define_bve(id.clone(), 1, 256, Firmware::Default)
                .unwrap(),
        );
        assert!(matches!(
            BveRuntime::create_with_isolated_network(
                &temp.runtime_root(),
                def_b,
                storage.clone(),
                &net_a
            ),
            Err(RuntimeError::NetworkDefinitionMismatch { .. })
        ));

        // Attached to net_a and created against net_a -> ok.
        let def_a = net_a.attach(storage.define_bve(id, 1, 256, Firmware::Default).unwrap());
        let rt =
            BveRuntime::create_with_isolated_network(&temp.runtime_root(), def_a, storage, &net_a)
                .unwrap();
        assert!(matches!(
            rt.definition().network(),
            NetworkAttachment::IsolatedTap { .. }
        ));
    }

    #[test]
    fn destroy_removes_only_control_artifacts_and_never_storage() {
        let temp = TempRoot::new();
        let (rt, storage) = bve(&temp, "bve-destroy");
        let control_dir = rt.instance_dir().to_path_buf();
        fs::File::create(control_dir.join("qmp.sock")).unwrap();

        rt.destroy().unwrap();

        assert!(!control_dir.exists(), "control dir removed");
        // Storage is disposed only by the explicit storage operation.
        assert!(
            storage.layout().system_overlay().is_file(),
            "overlay survives destroy"
        );
        assert!(
            storage.layout().source_disk().is_file(),
            "source survives destroy"
        );
        assert!(
            storage.layout().system_base().is_file(),
            "base survives destroy"
        );
    }

    #[test]
    fn destroy_fails_closed_on_unexpected_control_leftovers() {
        let temp = TempRoot::new();
        let (rt, _s) = bve(&temp, "bve-dirty");
        let dir = rt.instance_dir().to_path_buf();
        fs::write(dir.join("not-ours.txt"), b"x").unwrap();

        assert!(matches!(
            rt.destroy(),
            Err(RuntimeError::DestroyDirtyInstanceDir { .. })
        ));
        assert!(dir.join("not-ours.txt").is_file());
    }
}
