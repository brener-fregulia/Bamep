//! Deterministic BVE disk storage: one sparse RAW immutable **base**, a
//! per-instance disposable QCOW2 **system overlay** backed by that base, and
//! an independent per-instance RAW **source** fixture (Issue #69; ADR-0023).
//!
//! ```text
//! <storage-root>/
//! ├── base/
//! │   └── system-base.raw          sparse RAW, 80 GiB logical profile, read-only, created once
//! └── instances/
//!     └── <bve-id>/
//!         ├── system.qcow2         disposable overlay, backing = ../../base/system-base.raw (-F raw)
//!         └── source.raw           independent source fixture (sparse RAW)
//! ```
//!
//! Ownership and safety (Issue #69 §7, §10, §15, §16):
//!
//! - every path is derived from a validated [`BveStorageRoot`] plus a
//!   validated [`crate::BveId`] plus a fixed role filename — never from a
//!   caller-supplied delete target;
//! - the base lives outside every instance directory and is **never** in the
//!   deletion set of an instance reset or destroy;
//! - `reset_system_storage` recreates only `system.qcow2`; `source.raw` and
//!   the base are untouched;
//! - `destroy_instance_storage` removes only `system.qcow2`, `source.raw`,
//!   and the (then non-recursively removed) instance directory — an
//!   unexpected leftover fails closed;
//! - image work shells out to `qemu-img` via argv (never a shell string),
//!   always with explicit `-f`/`-F`, capturing exit status and stderr.
//!
//! This module does not expose a generic storage framework: one BVE, one
//! base, one disposable system overlay, one optional source fixture.

use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::definition::{
    BveDefinition, BveId, DefinitionError, DiskAttachment, DiskFormat, Firmware,
};

/// The QEMU image tool used to create the base and overlays. Its absence is a
/// **storage** prerequisite failure only — it is deliberately not part of
/// [`crate::detect_host_prerequisites`], so a BVE whose storage is already
/// prepared can still run its lifecycle with only `qemu-system-x86_64` + KVM
/// (Issue #69 §7).
pub const QEMU_IMG_BINARY: &str = "qemu-img";

/// The initial development profile for the logical system-disk capacity:
/// 80 GiB. This is an implementation profile for current BVE work, not a
/// universal normative BVE requirement.
pub const DEFAULT_SYSTEM_BASE_BYTES: u64 = 80 * 1024 * 1024 * 1024;

/// The default source-fixture logical capacity: 1 GiB. Also an implementation
/// default, not a normative requirement.
pub const DEFAULT_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;

const BASE_DIR: &str = "base";
const BASE_FILE: &str = "system-base.raw";
const INSTANCES_DIR: &str = "instances";
const SYSTEM_OVERLAY_FILE: &str = "system.qcow2";
const SOURCE_FILE: &str = "source.raw";

/// A validated storage root that this crate exclusively owns.
///
/// Rejects a path that cannot be safely used in a QEMU `-drive file=...`
/// value (contains `,`, a newline, or a carriage return) and resolves a
/// relative path to an absolute one, so overlay backing references are always
/// absolute and stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BveStorageRoot {
    base: PathBuf,
}

impl BveStorageRoot {
    /// Validates and wraps a storage root.
    pub fn new(base: impl Into<PathBuf>) -> Result<Self, BveStorageError> {
        let base = base.into();
        let text = base.to_string_lossy();
        if text.contains(',') || text.contains('\n') || text.contains('\r') {
            return Err(BveStorageError::StorageRootRejected { path: base });
        }
        let base = std::path::absolute(&base).map_err(BveStorageError::Io)?;
        Ok(Self { base })
    }

    /// The absolute root directory.
    pub fn path(&self) -> &Path {
        &self.base
    }

    /// The shared immutable base image path (`<root>/base/system-base.raw`).
    pub fn system_base(&self) -> PathBuf {
        self.base.join(BASE_DIR).join(BASE_FILE)
    }
}

/// The logical capacity requested for the system base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemBaseSpec {
    logical_bytes: u64,
}

impl SystemBaseSpec {
    /// A base of `logical_bytes` logical capacity. Rejects zero.
    pub fn new(logical_bytes: u64) -> Result<Self, BveStorageError> {
        if logical_bytes == 0 {
            return Err(BveStorageError::ZeroCapacity {
                what: "system base",
            });
        }
        Ok(Self { logical_bytes })
    }

    /// The requested logical capacity in bytes.
    pub fn logical_bytes(self) -> u64 {
        self.logical_bytes
    }
}

impl Default for SystemBaseSpec {
    fn default() -> Self {
        Self {
            logical_bytes: DEFAULT_SYSTEM_BASE_BYTES,
        }
    }
}

/// The logical capacity requested for the source fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceDiskSpec {
    logical_bytes: u64,
}

impl SourceDiskSpec {
    /// A source fixture of `logical_bytes` logical capacity. Rejects zero.
    pub fn new(logical_bytes: u64) -> Result<Self, BveStorageError> {
        if logical_bytes == 0 {
            return Err(BveStorageError::ZeroCapacity {
                what: "source fixture",
            });
        }
        Ok(Self { logical_bytes })
    }

    /// The requested logical capacity in bytes.
    pub fn logical_bytes(self) -> u64 {
        self.logical_bytes
    }
}

impl Default for SourceDiskSpec {
    fn default() -> Self {
        Self {
            logical_bytes: DEFAULT_SOURCE_BYTES,
        }
    }
}

/// The deterministic set of paths for one BVE's storage. Pure: constructing a
/// layout touches no filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BveStorageLayout {
    system_base: PathBuf,
    instance_dir: PathBuf,
    system_overlay: PathBuf,
    source_disk: PathBuf,
}

impl BveStorageLayout {
    /// The layout for `id` under `root`. `id` is already validated as exactly
    /// one safe path segment, so `instance_dir` is always
    /// `<root>/instances/<id>` and never escapes the root.
    pub fn for_bve(root: &BveStorageRoot, id: &BveId) -> Self {
        let instance_dir = root.path().join(INSTANCES_DIR).join(id.as_str());
        Self {
            system_base: root.system_base(),
            system_overlay: instance_dir.join(SYSTEM_OVERLAY_FILE),
            source_disk: instance_dir.join(SOURCE_FILE),
            instance_dir,
        }
    }

    /// The shared immutable base image (outside every instance directory).
    pub fn system_base(&self) -> &Path {
        &self.system_base
    }

    /// This BVE's instance directory.
    pub fn instance_dir(&self) -> &Path {
        &self.instance_dir
    }

    /// This BVE's disposable system overlay.
    pub fn system_overlay(&self) -> &Path {
        &self.system_overlay
    }

    /// This BVE's independent source fixture.
    pub fn source_disk(&self) -> &Path {
        &self.source_disk
    }
}

/// Prepared storage for one BVE: the layout plus whether a source fixture was
/// actually created. Cheap to clone (paths only). Consumed by
/// [`destroy_instance_storage`]; a clone is what
/// [`crate::runtime::BveRuntime`] holds for [`reset_system_storage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInstanceStorage {
    layout: BveStorageLayout,
    has_source: bool,
}

impl PreparedInstanceStorage {
    /// Wraps a layout whose images a caller has already prepared — by an
    /// earlier [`prepare_instance`] in a previous run, or equivalent tooling.
    /// This performs no I/O; [`crate::runtime::BveRuntime::create`] still
    /// verifies every image exists before launch.
    pub fn from_prepared_layout(layout: BveStorageLayout, has_source: bool) -> Self {
        Self { layout, has_source }
    }

    /// The path layout.
    pub fn layout(&self) -> &BveStorageLayout {
        &self.layout
    }

    /// Whether a source fixture was prepared.
    pub fn has_source(&self) -> bool {
        self.has_source
    }

    /// The `System` disk attachment: the disposable QCOW2 overlay. The
    /// immutable base is never attached as the writable system disk.
    pub fn system_attachment(&self) -> DiskAttachment {
        DiskAttachment::system(self.layout.system_overlay(), DiskFormat::Qcow2)
            .expect("storage-derived overlay path is comma/newline-free by construction")
    }

    /// The `Source` disk attachment, if a source fixture was prepared.
    pub fn source_attachment(&self) -> Option<DiskAttachment> {
        if !self.has_source {
            return None;
        }
        Some(
            DiskAttachment::source(self.layout.source_disk(), DiskFormat::Raw)
                .expect("storage-derived source path is comma/newline-free by construction"),
        )
    }

    /// Builds a [`BveDefinition`] whose disk attachments are exactly this
    /// prepared storage — the recommended constructor, since it cannot
    /// produce a definition that disagrees with what was prepared (Issue #69
    /// §8).
    pub fn define_bve(
        &self,
        id: BveId,
        vcpus: u32,
        memory_mib: u32,
        firmware: Firmware,
    ) -> Result<BveDefinition, DefinitionError> {
        let definition =
            BveDefinition::new(id, vcpus, memory_mib, firmware, self.system_attachment())?;
        match self.source_attachment() {
            Some(source) => definition.with_source(source),
            None => Ok(definition),
        }
    }
}

/// Why a storage operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BveStorageError {
    /// The storage root path is not safely representable in a QEMU `-drive`
    /// value.
    #[error("storage root {path} is rejected: it must not contain ',' or a newline")]
    StorageRootRejected {
        /// The offending path.
        path: PathBuf,
    },

    /// A requested capacity was zero.
    #[error("{what} capacity must be greater than zero")]
    ZeroCapacity {
        /// Which image the zero capacity was requested for.
        what: &'static str,
    },

    /// `qemu-img` could not be executed at all.
    #[error("{QEMU_IMG_BINARY:?} could not be executed: {source}")]
    QemuImgUnavailable {
        /// The underlying spawn error.
        #[source]
        source: std::io::Error,
    },

    /// `qemu-img` ran but its `--version` probe reported failure.
    #[error("{QEMU_IMG_BINARY:?} failed its --version probe")]
    QemuImgProbeFailed,

    /// A `qemu-img` operation exited non-zero.
    #[error("{QEMU_IMG_BINARY} {operation} failed: {stderr}")]
    QemuImgFailed {
        /// A short label for the operation attempted.
        operation: String,
        /// Captured `qemu-img` stderr.
        stderr: String,
    },

    /// `qemu-img info` output could not be parsed.
    #[error("could not read image metadata for {path}: {detail}")]
    ImageInfoUnreadable {
        /// The image inspected.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },

    /// An instance overlay was requested but the shared base does not exist.
    #[error("system base image is missing: {path}")]
    BaseMissing {
        /// The expected base path.
        path: PathBuf,
    },

    /// An existing base image is not usable as the known base (wrong type, or
    /// its logical capacity does not match the requested profile).
    #[error("existing system base {path} is invalid: {detail}")]
    BaseInvalid {
        /// The base path.
        path: PathBuf,
        /// Why it is invalid.
        detail: String,
    },

    /// After removing the known disposable files, the instance directory
    /// still held entries this crate did not create; it was not blindly
    /// removed.
    #[error("instance storage directory {dir} still holds unexpected entries: {source}")]
    InstanceStorageDirty {
        /// The directory left in place.
        dir: PathBuf,
        /// The `remove_dir` error.
        #[source]
        source: std::io::Error,
    },

    /// A filesystem operation failed.
    #[error("storage I/O error: {0}")]
    Io(#[source] std::io::Error),
}

/// Probes `qemu-img` with `--version`, returning its first version line. This
/// is the **storage** prerequisite check (Issue #69 §7).
pub fn check_qemu_img_binary() -> Result<String, BveStorageError> {
    let output = Command::new(QEMU_IMG_BINARY)
        .arg("--version")
        .output()
        .map_err(|source| BveStorageError::QemuImgUnavailable { source })?;
    if !output.status.success() {
        return Err(BveStorageError::QemuImgProbeFailed);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string())
}

/// Ensures the shared immutable base exists with the requested logical
/// capacity.
///
/// If absent: creates a sparse RAW image (`qemu-img create -f raw`) and marks
/// it read-only (an extra guard, not the only protection — the data model
/// keeps the base out of every writable attachment and every deletion set).
/// If present: verifies it is a regular file whose logical capacity matches
/// the spec, and **never** recreates or overwrites it.
pub fn ensure_system_base(
    root: &BveStorageRoot,
    spec: &SystemBaseSpec,
) -> Result<PathBuf, BveStorageError> {
    check_qemu_img_binary()?;
    let base = root.system_base();

    if base.exists() {
        let meta = fs::symlink_metadata(&base).map_err(BveStorageError::Io)?;
        if !meta.file_type().is_file() {
            return Err(BveStorageError::BaseInvalid {
                path: base.clone(),
                detail: "not a regular file".to_string(),
            });
        }
        let actual = qemu_img_virtual_size(&base)?;
        if actual != spec.logical_bytes() {
            return Err(BveStorageError::BaseInvalid {
                path: base.clone(),
                detail: format!(
                    "logical capacity is {actual} bytes, expected {}",
                    spec.logical_bytes()
                ),
            });
        }
        return Ok(base);
    }

    let base_dir = base.parent().expect("base path always has a parent");
    fs::create_dir_all(base_dir).map_err(BveStorageError::Io)?;

    let size = spec.logical_bytes().to_string();
    run_qemu_img(
        "create-system-base",
        &[
            OsStr::new("create"),
            OsStr::new("-f"),
            OsStr::new("raw"),
            base.as_os_str(),
            OsStr::new(&size),
        ],
    )?;

    // Best-effort read-only guard. Not the only protection.
    if let Ok(meta) = fs::metadata(&base) {
        let mut perms = meta.permissions();
        perms.set_mode(0o444);
        let _ = fs::set_permissions(&base, perms);
    }

    Ok(base)
}

/// Prepares one BVE's disposable storage: a fresh QCOW2 system overlay backed
/// by the shared base, and (when `source` is `Some`) an independent RAW
/// source fixture.
///
/// The shared base must already exist ([`ensure_system_base`]). A stale
/// overlay from a previous instance is removed first, fail-closed.
pub fn prepare_instance(
    root: &BveStorageRoot,
    id: &BveId,
    source: Option<&SourceDiskSpec>,
) -> Result<PreparedInstanceStorage, BveStorageError> {
    check_qemu_img_binary()?;
    let layout = BveStorageLayout::for_bve(root, id);

    if !layout.system_base().exists() {
        return Err(BveStorageError::BaseMissing {
            path: layout.system_base().to_path_buf(),
        });
    }

    fs::create_dir_all(layout.instance_dir()).map_err(BveStorageError::Io)?;
    remove_disposable_overlay(&layout)?;
    create_system_overlay(&layout, "prepare-system-overlay")?;

    let has_source = match source {
        Some(spec) => {
            if !layout.source_disk().exists() {
                let size = spec.logical_bytes().to_string();
                run_qemu_img(
                    "prepare-source",
                    &[
                        OsStr::new("create"),
                        OsStr::new("-f"),
                        OsStr::new("raw"),
                        layout.source_disk().as_os_str(),
                        OsStr::new(&size),
                    ],
                )?;
            }
            true
        }
        None => false,
    };

    Ok(PreparedInstanceStorage { layout, has_source })
}

/// Discards the disposable system overlay and recreates a fresh one from the
/// **same** base.
///
/// Only `system.qcow2` is removed and recreated. The source fixture and the
/// base are untouched. The base must still exist and be valid; a fresh reset
/// therefore always derives from the same known base state (Issue #69 §16).
pub fn reset_system_storage(prepared: &PreparedInstanceStorage) -> Result<(), BveStorageError> {
    check_qemu_img_binary()?;
    let layout = &prepared.layout;

    if !layout.system_base().exists() {
        return Err(BveStorageError::BaseMissing {
            path: layout.system_base().to_path_buf(),
        });
    }

    remove_disposable_overlay(layout)?;
    create_system_overlay(layout, "reset-system-overlay")?;
    Ok(())
}

/// Removes only this BVE's disposable storage: `system.qcow2`, `source.raw`,
/// and then the (non-recursively removed) instance directory. The shared
/// base, other BVEs' instance directories, and anything else under the
/// storage root are untouched. An unexpected leftover in the instance
/// directory fails closed rather than triggering a recursive delete.
pub fn destroy_instance_storage(prepared: PreparedInstanceStorage) -> Result<(), BveStorageError> {
    let layout = prepared.layout;

    for known in [layout.system_overlay(), layout.source_disk()] {
        match fs::remove_file(known) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(BveStorageError::Io(e)),
        }
    }

    match fs::remove_dir(layout.instance_dir()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BveStorageError::InstanceStorageDirty {
            dir: layout.instance_dir().to_path_buf(),
            source,
        }),
    }
}

/// Removes the disposable system overlay if present. A missing overlay is not
/// an error; any other failure is. Never touches the base or the source.
pub(crate) fn remove_disposable_overlay(layout: &BveStorageLayout) -> Result<(), BveStorageError> {
    match fs::remove_file(layout.system_overlay()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(BveStorageError::Io(e)),
    }
}

fn create_system_overlay(
    layout: &BveStorageLayout,
    operation: &str,
) -> Result<(), BveStorageError> {
    run_qemu_img(
        operation,
        &[
            OsStr::new("create"),
            OsStr::new("-f"),
            OsStr::new("qcow2"),
            OsStr::new("-F"),
            OsStr::new("raw"),
            OsStr::new("-b"),
            layout.system_base().as_os_str(),
            layout.system_overlay().as_os_str(),
        ],
    )
}

fn run_qemu_img(operation: &str, args: &[&OsStr]) -> Result<(), BveStorageError> {
    let output = Command::new(QEMU_IMG_BINARY)
        .args(args)
        .output()
        .map_err(|source| BveStorageError::QemuImgUnavailable { source })?;
    if !output.status.success() {
        return Err(BveStorageError::QemuImgFailed {
            operation: operation.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

fn qemu_img_virtual_size(path: &Path) -> Result<u64, BveStorageError> {
    let output = Command::new(QEMU_IMG_BINARY)
        .args([OsStr::new("info"), OsStr::new("--output=json")])
        .arg(path)
        .output()
        .map_err(|source| BveStorageError::QemuImgUnavailable { source })?;
    if !output.status.success() {
        return Err(BveStorageError::QemuImgFailed {
            operation: "info".to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| {
        BveStorageError::ImageInfoUnreadable {
            path: path.to_path_buf(),
            detail: e.to_string(),
        }
    })?;
    value
        .get("virtual-size")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| BveStorageError::ImageInfoUnreadable {
            path: path.to_path_buf(),
            detail: "no numeric \"virtual-size\" field".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("bamep-ve-storage-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn root(&self) -> BveStorageRoot {
            BveStorageRoot::new(&self.0).unwrap()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(path).unwrap().write_all(b"x").unwrap();
    }

    fn id(s: &str) -> BveId {
        BveId::new(s).unwrap()
    }

    #[test]
    fn storage_root_rejects_qemu_hostile_paths() {
        for bad in ["/a,b", "/line\nbreak", "/carriage\rreturn"] {
            assert!(matches!(
                BveStorageRoot::new(bad),
                Err(BveStorageError::StorageRootRejected { .. })
            ));
        }
    }

    #[test]
    fn storage_root_is_made_absolute() {
        let root = BveStorageRoot::new("relative/storage").unwrap();
        assert!(root.path().is_absolute());
        assert!(root.path().ends_with("relative/storage"));
    }

    #[test]
    fn layout_paths_are_deterministic_and_isolated() {
        let temp = TempRoot::new();
        let root = temp.root();
        let a = BveStorageLayout::for_bve(&root, &id("bve-a"));
        let a2 = BveStorageLayout::for_bve(&root, &id("bve-a"));
        let b = BveStorageLayout::for_bve(&root, &id("bve-b"));

        assert_eq!(a, a2, "same id -> same layout");
        assert_ne!(a.instance_dir(), b.instance_dir());
        assert_ne!(a.system_overlay(), b.system_overlay());
        assert_ne!(a.source_disk(), b.source_disk());
        assert_eq!(
            a.system_base(),
            b.system_base(),
            "the base is shared per storage root"
        );
    }

    #[test]
    fn the_base_lives_outside_every_instance_directory() {
        let temp = TempRoot::new();
        let layout = BveStorageLayout::for_bve(&temp.root(), &id("bve-x"));

        assert!(layout.instance_dir().starts_with(temp.root().path()));
        assert!(layout.system_overlay().starts_with(layout.instance_dir()));
        assert!(layout.source_disk().starts_with(layout.instance_dir()));
        assert!(!layout.system_base().starts_with(layout.instance_dir()));
        assert!(layout.system_base().starts_with(temp.root().path()));
    }

    #[test]
    fn remove_disposable_overlay_touches_only_the_overlay() {
        let temp = TempRoot::new();
        let layout = BveStorageLayout::for_bve(&temp.root(), &id("bve-r"));
        touch(layout.system_overlay());
        touch(layout.source_disk());
        touch(layout.system_base());

        remove_disposable_overlay(&layout).unwrap();

        assert!(!layout.system_overlay().exists(), "overlay removed");
        assert!(layout.source_disk().exists(), "source untouched");
        assert!(layout.system_base().exists(), "base untouched");
    }

    #[test]
    fn destroy_instance_storage_removes_only_this_instances_disposables() {
        let temp = TempRoot::new();
        let root = temp.root();
        let layout = BveStorageLayout::for_bve(&root, &id("bve-d"));
        touch(layout.system_overlay());
        touch(layout.source_disk());
        touch(layout.system_base());
        let sentinel = root.path().join("SENTINEL");
        touch(&sentinel);
        let other = BveStorageLayout::for_bve(&root, &id("bve-other"));
        touch(other.system_overlay());

        destroy_instance_storage(PreparedInstanceStorage::from_prepared_layout(
            layout.clone(),
            true,
        ))
        .unwrap();

        assert!(!layout.system_overlay().exists());
        assert!(!layout.source_disk().exists());
        assert!(!layout.instance_dir().exists());
        assert!(layout.system_base().exists(), "base survives");
        assert!(sentinel.exists(), "unrelated sentinel survives");
        assert!(other.system_overlay().exists(), "another BVE survives");
    }

    #[test]
    fn destroy_instance_storage_fails_closed_on_an_unexpected_leftover() {
        let temp = TempRoot::new();
        let layout = BveStorageLayout::for_bve(&temp.root(), &id("bve-dirty"));
        touch(layout.system_overlay());
        touch(&layout.instance_dir().join("not-ours.bin"));

        let err = destroy_instance_storage(PreparedInstanceStorage::from_prepared_layout(
            layout.clone(),
            false,
        ))
        .unwrap_err();
        assert!(matches!(err, BveStorageError::InstanceStorageDirty { .. }));
        assert!(layout.instance_dir().join("not-ours.bin").exists());
    }

    #[test]
    fn prepared_attachments_carry_the_right_roles_and_formats() {
        let temp = TempRoot::new();
        let layout = BveStorageLayout::for_bve(&temp.root(), &id("bve-att"));

        let with = PreparedInstanceStorage::from_prepared_layout(layout.clone(), true);
        let sys = with.system_attachment();
        assert_eq!(sys.role(), crate::definition::DiskRole::System);
        assert_eq!(sys.format(), DiskFormat::Qcow2);
        assert_eq!(sys.path(), layout.system_overlay());
        let src = with.source_attachment().unwrap();
        assert_eq!(src.role(), crate::definition::DiskRole::Source);
        assert_eq!(src.format(), DiskFormat::Raw);
        assert_eq!(src.path(), layout.source_disk());

        let without = PreparedInstanceStorage::from_prepared_layout(layout, false);
        assert!(without.source_attachment().is_none());
    }

    #[test]
    fn define_bve_matches_the_prepared_storage_exactly() {
        let temp = TempRoot::new();
        let layout = BveStorageLayout::for_bve(&temp.root(), &id("bve-def"));
        let prepared = PreparedInstanceStorage::from_prepared_layout(layout.clone(), true);

        let def = prepared
            .define_bve(id("bve-def"), 2, 512, Firmware::Default)
            .unwrap();
        assert_eq!(def.system_disk().path(), layout.system_overlay());
        assert_eq!(def.system_disk().format(), DiskFormat::Qcow2);
        assert_eq!(def.source_disk().unwrap().path(), layout.source_disk());
        assert_eq!(def.source_disk().unwrap().format(), DiskFormat::Raw);

        let prepared_no_src = PreparedInstanceStorage::from_prepared_layout(layout, false);
        let def = prepared_no_src
            .define_bve(id("bve-def"), 1, 256, Firmware::Default)
            .unwrap();
        assert!(def.source_disk().is_none());
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(matches!(
            SystemBaseSpec::new(0),
            Err(BveStorageError::ZeroCapacity { .. })
        ));
        assert!(matches!(
            SourceDiskSpec::new(0),
            Err(BveStorageError::ZeroCapacity { .. })
        ));
        assert_eq!(
            SystemBaseSpec::default().logical_bytes(),
            DEFAULT_SYSTEM_BASE_BYTES
        );
    }
}
