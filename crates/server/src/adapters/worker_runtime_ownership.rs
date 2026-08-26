//! Host-local lifetime ownership of the Worker control boundary (correction
//! audit on Issue #37: the three original HIGH findings — safe UDS path
//! ownership, trusted UDS parent directory, and owned socket cleanup —
//! shared one root cause, `bamepd` inferring *exclusive ownership* of the
//! Worker UDS pathname by inspecting/probing the pathname itself
//! (`stat`/`connect`/`unlink`). Probing a pathname can never be atomic
//! against a second `bamepd` racing the same sequence, no matter how many
//! extra checks are layered on. This module replaces that inference with an
//! explicit, host-local, lifetime-held ownership primitive:
//!
//! ```text
//! bamepd startup
//!     -> validate/create the trusted runtime directory (this module)
//!     -> acquire an exclusive, non-blocking flock on a dedicated lock file
//!        inside it, held for the whole daemon lifetime (this module)
//!     -> only the lock holder may inspect/remove/bind worker.sock
//!        (`worker_control_plane::WorkerControlPlane::bind`)
//!     -> run the control plane
//!     -> clean up the owned socket while the lock is still held
//!     -> release the lock LAST
//! ```
//!
//! A second `bamepd` targeting the same runtime directory fails at
//! [`RuntimeOwnershipLock::acquire`] — before it ever inspects, probes, or
//! attempts to bind the Worker UDS socket pathname.
//!
//! Filesystem metadata checks (directory mode/owner, ancestor-replacement
//! trust, and the pre-existing stale-socket probe in
//! `worker_control_plane`) remain defense in depth. This lock is the
//! primary exclusivity guarantee, per the correction audit's explicit
//! direction: "Filesystem metadata checks are defense in depth. The
//! lifetime ownership primitive is the primary guarantee."
//!
//! Host-local, single-process trust model only: no distributed lease, no
//! PostgreSQL-backed lock, and the Worker UDS socket itself is never used as
//! the sole ownership proof.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{self, Mode, OFlags};

/// Dedicated lock file name inside the trusted runtime directory — distinct
/// from the Worker UDS socket pathname itself. The ownership lock is a local
/// supervision mechanism, never part of the Worker IPC wire contract
/// (`m1-worker-data-plane-control-contract.md` "Out of scope": "process
/// supervision/respawn mechanics ... not this wire contract").
const LOCK_FILE_NAME: &str = "bamepd.lock";

#[derive(Debug, thiserror::Error)]
pub enum RuntimeDirError {
    #[error("{path} is a symlink, not a real directory")]
    Symlink { path: String },
    #[error("{path} is not a directory")]
    NotADirectory { path: String },
    #[error("{path} grants group/other permissions; expected owner-only (0700)")]
    InsecureMode { path: String },
    #[error("{path} is owned by uid {actual}, not the effective uid {expected} running bamepd")]
    WrongOwner {
        path: String,
        expected: u32,
        actual: u32,
    },
    #[error("refusing untrusted ancestor directory {path}: {reason}")]
    UnsafeAncestor { path: String, reason: String },
    #[error("failed to create trusted runtime directory {path}")]
    Create {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect {path}")]
    Inspect {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// A runtime directory already validated (or freshly created) as the
/// trusted host-local boundary for the Worker UDS socket and the ownership
/// lock file.
#[derive(Debug)]
pub struct TrustedRuntimeDir {
    path: PathBuf,
}

impl TrustedRuntimeDir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Validates `path` as the trusted runtime directory, or creates it
    /// fresh under an already-trusted ancestor (correction audit "Directory
    /// creation policy"). A missing directory is created with a single
    /// `mkdir` under its parent — never a recursive `create_dir_all` across
    /// potentially untrusted ancestors — and only after
    /// [`ensure_trusted_ancestor`] confirms that parent cannot be replaced
    /// by an unrelated principal.
    ///
    /// An *existing* directory must already satisfy the same shape this
    /// function would have created: a real directory, never a symlink,
    /// owner-only mode (no group/other bits), and owned by the effective
    /// UID running `bamepd` (correction audit "Owner check") — it is never
    /// blindly `chmod`ed or `chown`ed into compliance.
    pub fn validate_or_create(path: &Path) -> Result<Self, RuntimeDirError> {
        ensure_trusted_ancestor(path)?;

        match std::fs::symlink_metadata(path) {
            Ok(metadata) => validate_shape(path, &metadata)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => create_fresh(path)?,
            Err(source) => {
                return Err(RuntimeDirError::Inspect {
                    path: path.display().to_string(),
                    source,
                })
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Pure, unit-testable predicate: an owner-only mode grants no permission
/// bits to group or other.
fn mode_is_owner_only(mode: u32) -> bool {
    mode & 0o077 == 0
}

/// Pure, unit-testable predicate for ancestor-replacement trust (correction
/// audit "Ancestor trust"). Deliberately does **not** demand 0700 all the
/// way up to `/`: a standard root-owned, non-group/other-writable system
/// directory (e.g. `/run`) is trusted as-is. A directory that grants
/// group/other write must carry the sticky bit — the same rule that makes
/// `/tmp` safe for multi-principal use — or it is refused outright, since an
/// unrelated principal could otherwise delete/replace the runtime directory
/// entry itself.
fn ancestor_is_trusted(mode: u32, owner_uid: u32, effective_uid: u32) -> bool {
    let owned_by_root_or_self = owner_uid == 0 || owner_uid == effective_uid;
    let group_or_other_writable = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    owned_by_root_or_self && (!group_or_other_writable || sticky)
}

fn validate_shape(path: &Path, metadata: &std::fs::Metadata) -> Result<(), RuntimeDirError> {
    if metadata.file_type().is_symlink() {
        return Err(RuntimeDirError::Symlink {
            path: path.display().to_string(),
        });
    }
    if !metadata.is_dir() {
        return Err(RuntimeDirError::NotADirectory {
            path: path.display().to_string(),
        });
    }
    if !mode_is_owner_only(metadata.mode()) {
        return Err(RuntimeDirError::InsecureMode {
            path: path.display().to_string(),
        });
    }
    let uid = effective_uid();
    if metadata.uid() != uid {
        return Err(RuntimeDirError::WrongOwner {
            path: path.display().to_string(),
            expected: uid,
            actual: metadata.uid(),
        });
    }
    Ok(())
}

/// Validates the *immediate parent* of the runtime directory carries enough
/// replacement-authority trust that an unrelated principal could not delete
/// or replace the runtime directory entry out from under `bamepd`. Does not
/// recurse further up the filesystem tree.
fn ensure_trusted_ancestor(path: &Path) -> Result<(), RuntimeDirError> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };

    let metadata =
        std::fs::symlink_metadata(parent).map_err(|source| RuntimeDirError::Inspect {
            path: parent.display().to_string(),
            source,
        })?;

    if metadata.file_type().is_symlink() {
        return Err(RuntimeDirError::UnsafeAncestor {
            path: parent.display().to_string(),
            reason: "ancestor path is a symlink, not a real directory".to_string(),
        });
    }
    if !metadata.is_dir() {
        return Err(RuntimeDirError::UnsafeAncestor {
            path: parent.display().to_string(),
            reason: "ancestor path is not a directory".to_string(),
        });
    }
    if !ancestor_is_trusted(metadata.mode(), metadata.uid(), effective_uid()) {
        return Err(RuntimeDirError::UnsafeAncestor {
            path: parent.display().to_string(),
            reason: "ancestor directory can be replaced by an untrusted principal (owned by \
                      neither root nor the effective uid running bamepd, or group/other-writable \
                      without the sticky bit)"
                .to_string(),
        });
    }
    Ok(())
}

fn create_fresh(path: &Path) -> Result<(), RuntimeDirError> {
    std::fs::create_dir(path).map_err(|source| RuntimeDirError::Create {
        path: path.display().to_string(),
        source,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        RuntimeDirError::Create {
            path: path.display().to_string(),
            source,
        }
    })?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(
        "another live bamepd instance already owns the Worker control boundary lock at {path}"
    )]
    AlreadyOwned { path: String },
    #[error("failed to open the ownership lock file {path}")]
    Open {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to acquire the ownership lock at {path}")]
    Acquire {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Held for the entire `bamepd` daemon lifetime. Only the process holding
/// this lock may inspect, remove, or bind the Worker UDS socket pathname
/// (correction audit "Lifetime-held UDS ownership primitive").
///
/// Dropping this value releases the advisory `flock` (the kernel releases
/// `flock` locks automatically when every file descriptor referring to the
/// open file description is closed — including on process death, so a
/// crashed `bamepd` never leaves a stale lock behind). [`RuntimeOwnershipLock::release`]
/// exists so `bamepd`'s shutdown ordering — stop handlers, stop Worker,
/// clean up the Worker socket, release this lock LAST — is explicit in code
/// rather than left to incidental `Drop` ordering across unrelated values.
pub struct RuntimeOwnershipLock {
    // Never read after acquisition; its only purpose is to keep the
    // `flock`-holding open file description alive for the daemon's
    // lifetime. `flock` locks are associated with the open file
    // description, not the file descriptor number or the process, so a
    // second `open()` in the same or a different process always contends
    // for the same lock regardless of how many descriptors either side
    // holds.
    _fd: OwnedFd,
    lock_path: PathBuf,
}

impl RuntimeOwnershipLock {
    /// Opens (creating if necessary) the dedicated lock file inside
    /// `runtime_dir` and acquires an exclusive, non-blocking advisory lock
    /// on it (`flock(LOCK_EX | LOCK_NB)`). Fails immediately —
    /// never blocking — when another live owner already holds it.
    pub fn acquire(runtime_dir: &TrustedRuntimeDir) -> Result<Self, LockError> {
        let lock_path = runtime_dir.path().join(LOCK_FILE_NAME);
        let fd = fs::open(
            &lock_path,
            OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|errno| LockError::Open {
            path: lock_path.display().to_string(),
            source: errno.into(),
        })?;

        match fs::flock(&fd, fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self { _fd: fd, lock_path }),
            Err(errno)
                if errno == rustix::io::Errno::WOULDBLOCK || errno == rustix::io::Errno::AGAIN =>
            {
                Err(LockError::AlreadyOwned {
                    path: lock_path.display().to_string(),
                })
            }
            Err(errno) => Err(LockError::Acquire {
                path: lock_path.display().to_string(),
                source: errno.into(),
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.lock_path
    }

    /// Explicit last step of `bamepd` shutdown: releases the lock only
    /// after every dependent boundary (Worker socket cleanup) has already
    /// completed. Behaviorally identical to dropping this value; exists
    /// purely to make shutdown ordering explicit at the call site.
    pub fn release(self) {
        drop(self);
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn owner_only_mode_is_accepted() {
        assert!(mode_is_owner_only(0o700));
        assert!(mode_is_owner_only(0o600));
    }

    #[test]
    fn group_or_other_bits_are_rejected() {
        assert!(!mode_is_owner_only(0o750));
        assert!(!mode_is_owner_only(0o701));
        assert!(!mode_is_owner_only(0o707));
    }

    #[test]
    fn root_owned_non_writable_ancestor_is_trusted() {
        assert!(ancestor_is_trusted(0o755, 0, 1000));
    }

    #[test]
    fn self_owned_ancestor_is_trusted() {
        assert!(ancestor_is_trusted(0o755, 1000, 1000));
    }

    #[test]
    fn group_or_other_writable_ancestor_without_sticky_bit_is_untrusted() {
        assert!(!ancestor_is_trusted(0o777, 0, 1000));
        assert!(!ancestor_is_trusted(0o777, 0, 1000));
    }

    #[test]
    fn group_or_other_writable_ancestor_with_sticky_bit_is_trusted() {
        // Exactly `/tmp`'s conventional mode (1777): world-writable, but
        // the sticky bit prevents an unrelated principal from removing
        // another user's entries.
        assert!(ancestor_is_trusted(0o1777, 0, 1000));
    }

    #[test]
    fn ancestor_owned_by_an_unrelated_uid_is_untrusted_even_if_not_writable() {
        assert!(!ancestor_is_trusted(0o755, 4242, 1000));
    }
}

#[cfg(all(test, unix))]
mod fs_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fresh_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "bamep-worker-runtime-ownership-unit-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn missing_directory_is_created_owner_only_under_a_trusted_ancestor() {
        let dir = fresh_dir();
        let trusted = TrustedRuntimeDir::validate_or_create(&dir).expect("must create");
        let metadata = std::fs::symlink_metadata(trusted.path()).expect("stat");
        assert!(metadata.is_dir());
        assert_eq!(metadata.mode() & 0o777, 0o700);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_existing_owner_only_directory_owned_by_self_is_accepted() {
        let dir = fresh_dir();
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        TrustedRuntimeDir::validate_or_create(&dir).expect("must accept a secure existing dir");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_insecure_mode_is_rejected() {
        let dir = fresh_dir();
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let err = TrustedRuntimeDir::validate_or_create(&dir).unwrap_err();
        assert!(matches!(err, RuntimeDirError::InsecureMode { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_symlink_at_the_runtime_dir_path_is_rejected() {
        let real_dir = fresh_dir();
        std::fs::create_dir_all(&real_dir).expect("create real dir");
        std::fs::set_permissions(&real_dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let symlink_path = fresh_dir();
        std::os::unix::fs::symlink(&real_dir, &symlink_path).expect("symlink");

        let err = TrustedRuntimeDir::validate_or_create(&symlink_path).unwrap_err();
        assert!(matches!(err, RuntimeDirError::Symlink { .. }));

        std::fs::remove_file(&symlink_path).ok();
        std::fs::remove_dir_all(&real_dir).ok();
    }

    #[test]
    fn a_world_writable_non_sticky_parent_is_rejected() {
        let parent = fresh_dir();
        std::fs::create_dir_all(&parent).expect("create parent");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777))
            .expect("relax parent");
        let child = parent.join("bamep");

        let err = TrustedRuntimeDir::validate_or_create(&child).unwrap_err();
        assert!(matches!(err, RuntimeDirError::UnsafeAncestor { .. }));

        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn a_trusted_service_style_runtime_directory_succeeds() {
        // Mirrors the expected production shape: a dedicated directory
        // freshly created under a trusted, already-existing parent (e.g.
        // `/run/bamep` under `/run`).
        let parent = fresh_dir();
        std::fs::create_dir_all(&parent).expect("create parent");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
            .expect("set trusted parent mode");
        let child = parent.join("bamep");

        TrustedRuntimeDir::validate_or_create(&child)
            .expect("a legitimate service-style runtime directory must succeed");

        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn first_owner_acquires_the_lock_and_a_second_open_object_cannot() {
        let dir = fresh_dir();
        let trusted = TrustedRuntimeDir::validate_or_create(&dir).expect("create dir");

        let first = RuntimeOwnershipLock::acquire(&trusted).expect("first acquire");
        let second = RuntimeOwnershipLock::acquire(&trusted);
        assert!(
            matches!(second, Err(LockError::AlreadyOwned { .. })),
            "a second independently-opened lock object must never acquire the same lock"
        );

        first.release();
        RuntimeOwnershipLock::acquire(&trusted)
            .expect("lock must become acquirable again once the first owner releases it");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dropping_the_lock_releases_it_without_calling_release_explicitly() {
        let dir = fresh_dir();
        let trusted = TrustedRuntimeDir::validate_or_create(&dir).expect("create dir");

        {
            let _first = RuntimeOwnershipLock::acquire(&trusted).expect("first acquire");
            assert!(matches!(
                RuntimeOwnershipLock::acquire(&trusted),
                Err(LockError::AlreadyOwned { .. })
            ));
        }

        RuntimeOwnershipLock::acquire(&trusted)
            .expect("dropping the guard must release the lock, mirroring process death");

        std::fs::remove_dir_all(&dir).ok();
    }
}
