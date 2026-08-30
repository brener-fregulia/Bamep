//! The filesystem Adapter for the Worker-local chunk storage Port
//! ([`ChunkStore`]). Unix-only: it depends on owner-restrictive permissions,
//! symlink refusal, `O_NOFOLLOW` opens, `linkat` first-writer placement, and
//! directory `fsync` — none of which have a faithful cross-platform form.
//!
//! # On-disk layout (private Worker implementation detail)
//!
//! ```text
//! <root>/
//!   transfers/
//!     <transfer-uuid, lowercase hyphenated 36 chars>/
//!       chunks/
//!         <chunk_index>.chunk               # finalized, restart-stable
//!         .staging/
//!           <chunk_index>.<32 hex>.part     # in-progress, Worker-generated
//! ```
//!
//! The final path is deterministic from `(transfer_id, chunk_index)`. The
//! staging file lives under the same `chunks/` subtree — same filesystem as
//! its destination — so placement is a single `link(2)` (atomic, no-replace)
//! followed by unlinking the staging name. Directory existence carries no
//! business meaning. This layout is not written into any Specification.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    ChunkIndex, FinalizedChunk, FinalizedDisposition, Sha256Digest, StorageError, StoredChunkFacts,
};

/// Owner-only (`rwx------`) for Worker-created directories.
const DIR_MODE: u32 = 0o700;
/// Owner read/write only (`rw-------`) for staging and finalized chunk files.
const FILE_MODE: u32 = 0o600;
const TRANSFERS_DIR: &str = "transfers";
const CHUNKS_DIR: &str = "chunks";
const STAGING_DIR: &str = ".staging";
const FINAL_SUFFIX: &str = ".chunk";
const STAGING_EXT: &str = "part";
/// Bounded read buffer for scanning a finalized file (idempotency compare,
/// [`ChunkStore::inspect_final`]). Never loads the whole chunk at once.
const SCAN_BUF_LEN: usize = 64 * 1024;

/// The Worker-local chunk storage Port: the mechanical byte operations the
/// future HTTPS data-plane handler (Phase E) and full-Artifact
/// reconstruction (Phase D2) will depend on. [`FilesystemChunkStore`] is the
/// only Adapter today; the trait exists so Phase E code can be written and
/// tested against the contract rather than the filesystem.
pub trait ChunkStore {
    /// Begins staging one `(transfer_id, chunk_index)`. `max_size` is the
    /// authoritative per-chunk maximum the caller already obtained (Phase E:
    /// from `AuthorizationDecision.chunk_size`); the staging operation fails
    /// closed once more than `max_size` bytes are written.
    fn begin_stage(
        &self,
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
        max_size: u64,
    ) -> Result<StagingChunk, StorageError>;

    /// Opens an already-finalized chunk for reading. Resolves only the
    /// deterministic typed location, requires a regular file, refuses a
    /// symlink, and returns [`StorageError::FinalizedChunkNotFound`] if
    /// absent. Does not load the whole chunk.
    fn open_final(
        &self,
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    ) -> Result<StoredChunkReader, StorageError>;

    /// Scans an already-finalized chunk and returns its mechanical size and
    /// SHA-256. Useful for idempotent finalization and Phase D2. Not cached.
    fn inspect_final(
        &self,
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    ) -> Result<StoredChunkFacts, StorageError>;
}

/// Filesystem-backed [`ChunkStore`]. Cheap to clone (just the root path).
#[derive(Debug, Clone)]
pub struct FilesystemChunkStore {
    root: PathBuf,
}

impl FilesystemChunkStore {
    /// Validates and prepares the storage root, then removes recognized
    /// leftover staging files. Fails closed: a Worker that cannot obtain a
    /// usable storage root must not continue to a state where it could
    /// accept future data-plane requests.
    ///
    /// - the root path must be absolute and not the filesystem root or
    ///   contain `.`/`..`;
    /// - if the root is absent it is created `0700`;
    /// - if it exists it must be a real directory (not a symlink, not a
    ///   regular file); group/other permission bits are tightened away;
    /// - recognized `.staging/*.part` files left by a previous run are
    ///   removed. Finalized chunk files, directories, and unrecognized files
    ///   are preserved.
    pub fn initialize(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        validate_root_shape(&root)?;
        ensure_worker_dir(&root)?;
        let transfers = root.join(TRANSFERS_DIR);
        ensure_worker_dir(&transfers)?;
        cleanup_recognized_staging(&transfers)?;
        Ok(Self { root })
    }

    fn transfer_dir(&self, transfer_id: Uuid) -> PathBuf {
        self.root
            .join(TRANSFERS_DIR)
            .join(transfer_id.as_hyphenated().to_string())
    }

    fn chunks_dir(&self, transfer_id: Uuid) -> PathBuf {
        self.transfer_dir(transfer_id).join(CHUNKS_DIR)
    }

    fn final_path(&self, transfer_id: Uuid, chunk_index: ChunkIndex) -> PathBuf {
        self.chunks_dir(transfer_id)
            .join(format!("{chunk_index}{FINAL_SUFFIX}"))
    }

    fn open_final_regular(
        &self,
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    ) -> Result<File, StorageError> {
        open_regular_no_follow(&self.final_path(transfer_id, chunk_index)).map_err(
            |kind| match kind {
                OpenReject::NotFound => StorageError::FinalizedChunkNotFound {
                    transfer_id,
                    chunk_index,
                },
                OpenReject::NotRegular => StorageError::FinalizedChunkNotRegular {
                    transfer_id,
                    chunk_index,
                },
                OpenReject::Io(source) => StorageError::Io {
                    context: format!(
                        "open finalized chunk for transfer {transfer_id} index {chunk_index}"
                    ),
                    source,
                },
            },
        )
    }
}

impl ChunkStore for FilesystemChunkStore {
    fn begin_stage(
        &self,
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
        max_size: u64,
    ) -> Result<StagingChunk, StorageError> {
        let chunks_dir = self.chunks_dir(transfer_id);
        let staging_dir = chunks_dir.join(STAGING_DIR);
        ensure_worker_dir(&self.transfer_dir(transfer_id))?;
        ensure_worker_dir(&chunks_dir)?;
        ensure_worker_dir(&staging_dir)?;

        let staging_path = staging_dir.join(format!(
            "{chunk_index}.{}.{STAGING_EXT}",
            worker_random_token()
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&staging_path)
            .map_err(|source| StorageError::Io {
                context: format!("create staging file {}", staging_path.display()),
                source,
            })?;

        Ok(StagingChunk {
            transfer_id,
            chunk_index,
            chunks_dir,
            final_path: self.final_path(transfer_id, chunk_index),
            staging_path,
            file: Some(file),
            written: 0,
            max_size,
            hasher: Sha256::new(),
            poisoned: false,
        })
    }

    fn open_final(
        &self,
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    ) -> Result<StoredChunkReader, StorageError> {
        Ok(StoredChunkReader {
            file: self.open_final_regular(transfer_id, chunk_index)?,
        })
    }

    fn inspect_final(
        &self,
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    ) -> Result<StoredChunkFacts, StorageError> {
        let file = self.open_final_regular(transfer_id, chunk_index)?;
        let (size, digest) = scan_size_and_digest(file).map_err(|source| StorageError::Io {
            context: format!("scan finalized chunk for transfer {transfer_id} index {chunk_index}"),
            source,
        })?;
        Ok(StoredChunkFacts { size, digest })
    }
}

/// A read handle over a finalized chunk file. Streams; never forces the whole
/// chunk into memory.
#[derive(Debug)]
pub struct StoredChunkReader {
    file: File,
}

impl Read for StoredChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for StoredChunkReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}

/// An in-progress staged chunk. Owns its temporary file, the destination
/// identity, the running byte count, the caller-supplied maximum, and the
/// incremental SHA-256 state — and nothing else. No capability, `proof_id`,
/// acceptance handle, or Artifact/Transfer state lives here; those belong to
/// Phase E control orchestration.
///
/// `Send` (its `File`, `Sha256`, and `PathBuf`s all are), so Phase E can move
/// it in and out of `tokio::task::spawn_blocking` between body frames.
pub struct StagingChunk {
    transfer_id: Uuid,
    chunk_index: ChunkIndex,
    chunks_dir: PathBuf,
    final_path: PathBuf,
    staging_path: PathBuf,
    /// `None` once the staging file has been closed (finalized, discarded,
    /// or poisoned by a write failure / oversize).
    file: Option<File>,
    written: u64,
    max_size: u64,
    hasher: Sha256,
    /// Once set, no valid final file can ever be produced from this handle.
    poisoned: bool,
}

impl StagingChunk {
    pub fn transfer_id(&self) -> Uuid {
        self.transfer_id
    }

    pub fn chunk_index(&self) -> ChunkIndex {
        self.chunk_index
    }

    /// Bytes accepted so far.
    pub fn staged_len(&self) -> u64 {
        self.written
    }

    /// Appends `buf` to the staging file and folds it into the running
    /// SHA-256. Accepts arbitrary frame sizes; makes no assumption that one
    /// call equals one chunk. Fails closed and discards the staging file on
    /// oversize or write error — after which every later call fails too and
    /// [`finalize`](Self::finalize) cannot produce a file.
    pub fn write(&mut self, buf: &[u8]) -> Result<(), StorageError> {
        if self.poisoned {
            return Err(StorageError::Oversize { max: self.max_size });
        }
        if buf.is_empty() {
            return Ok(());
        }
        let projected = self.written.saturating_add(buf.len() as u64);
        if projected > self.max_size {
            self.poison();
            return Err(StorageError::Oversize { max: self.max_size });
        }
        let file = self
            .file
            .as_mut()
            .expect("staging file is present while not poisoned");
        if let Err(source) = file.write_all(buf) {
            self.poison();
            return Err(StorageError::Io {
                context: format!("write staging file {}", self.staging_path.display()),
                source,
            });
        }
        self.hasher.update(buf);
        self.written = projected;
        Ok(())
    }

    /// Finalizes the staged bytes into a restart-stable final chunk file.
    ///
    /// Order: finalize the incremental SHA-256, flush buffered writes,
    /// `fsync` the staging file, atomically place it at the deterministic
    /// final path with no-replace semantics, then `fsync` the containing
    /// directory. Only after this returns `Ok` may a caller (Phase E) ask
    /// `bamepd` to durably accept the chunk.
    ///
    /// Restart guarantee: after this returns `Ok`, the finalized file
    /// survives dropping every store/handle object and constructing a fresh
    /// [`FilesystemChunkStore`] over the same root within the same or a
    /// later Worker process. Durability against OS crash / power loss is
    /// only whatever the underlying filesystem's `fsync` provides — no
    /// commodity-hardware power-loss atomicity is claimed.
    pub fn finalize(mut self) -> Result<FinalizedChunk, StorageError> {
        let outcome = self.finalize_inner();
        if outcome.is_err() {
            self.discard_staging();
        }
        // On success the staging file is already unlinked and `self.file`
        // is `None`, so `Drop` is a no-op.
        outcome
    }

    /// Explicitly discards the staging file without producing a final chunk.
    pub fn discard(mut self) {
        self.discard_staging();
    }

    fn finalize_inner(&mut self) -> Result<FinalizedChunk, StorageError> {
        if self.poisoned {
            return Err(StorageError::Oversize { max: self.max_size });
        }
        if self.written == 0 {
            return Err(StorageError::EmptyChunk);
        }

        let digest = Sha256Digest::from_raw(self.hasher.clone().finalize().into());
        let size = self.written;

        let mut file = self
            .file
            .take()
            .expect("staging file is present at finalize");
        file.flush()
            .map_err(|source| StorageError::FinalizationSync {
                context: format!("flush staging file {}", self.staging_path.display()),
                source,
            })?;
        file.sync_all()
            .map_err(|source| StorageError::FinalizationSync {
                context: format!("fsync staging file {}", self.staging_path.display()),
                source,
            })?;
        drop(file);

        match place_no_replace(&self.staging_path, &self.final_path)? {
            Placement::Installed => {
                fsync_dir(&self.chunks_dir)?;
                let _ = fs::remove_file(&self.staging_path);
                Ok(FinalizedChunk {
                    transfer_id: self.transfer_id,
                    chunk_index: self.chunk_index,
                    size,
                    digest,
                    disposition: FinalizedDisposition::Installed,
                })
            }
            Placement::DestinationExists => {
                let existing = self.existing_final_facts()?;
                let _ = fs::remove_file(&self.staging_path);
                if existing.size == size && existing.digest == digest {
                    Ok(FinalizedChunk {
                        transfer_id: self.transfer_id,
                        chunk_index: self.chunk_index,
                        size,
                        digest,
                        disposition: FinalizedDisposition::AlreadyPresent,
                    })
                } else {
                    Err(StorageError::FinalizedChunkConflict {
                        transfer_id: self.transfer_id,
                        chunk_index: self.chunk_index,
                    })
                }
            }
        }
    }

    fn existing_final_facts(&self) -> Result<StoredChunkFacts, StorageError> {
        let file = open_regular_no_follow(&self.final_path).map_err(|kind| match kind {
            OpenReject::NotFound => StorageError::FinalizedChunkNotFound {
                transfer_id: self.transfer_id,
                chunk_index: self.chunk_index,
            },
            OpenReject::NotRegular => StorageError::FinalizedChunkNotRegular {
                transfer_id: self.transfer_id,
                chunk_index: self.chunk_index,
            },
            OpenReject::Io(source) => StorageError::Io {
                context: format!(
                    "inspect existing finalized chunk {}",
                    self.final_path.display()
                ),
                source,
            },
        })?;
        let (size, digest) = scan_size_and_digest(file).map_err(|source| StorageError::Io {
            context: format!(
                "scan existing finalized chunk {}",
                self.final_path.display()
            ),
            source,
        })?;
        Ok(StoredChunkFacts { size, digest })
    }

    fn poison(&mut self) {
        self.poisoned = true;
        self.discard_staging();
    }

    fn discard_staging(&mut self) {
        self.file = None;
        let _ = fs::remove_file(&self.staging_path);
    }
}

/// Defense in depth only — explicit [`discard`](StagingChunk::discard) or a
/// failed/poisoned [`finalize`](StagingChunk::finalize) already removes the
/// staging file, and startup cleanup removes anything they miss. Correctness
/// never depends on this running.
impl Drop for StagingChunk {
    fn drop(&mut self) {
        if self.file.is_some() {
            self.discard_staging();
        }
    }
}

enum Placement {
    Installed,
    DestinationExists,
}

/// `link(2)` the staging file to the final path, then unlink the staging
/// name. `link` never replaces an existing destination (or a symlink at the
/// destination), so this is atomic first-writer placement across concurrent
/// finalizations without relying on a process-local lock. A leftover staging
/// name after a crash between `link` and `unlink` is cleaned at next startup.
fn place_no_replace(staging: &Path, final_path: &Path) -> Result<Placement, StorageError> {
    match rustix::fs::linkat(
        rustix::fs::CWD,
        staging,
        rustix::fs::CWD,
        final_path,
        rustix::fs::AtFlags::empty(),
    ) {
        Ok(()) => Ok(Placement::Installed),
        Err(Errno::EXIST) => Ok(Placement::DestinationExists),
        Err(errno) => Err(StorageError::FinalizationSync {
            context: format!(
                "atomically place finalized chunk at {}",
                final_path.display()
            ),
            source: errno.into(),
        }),
    }
}

/// `fsync` a directory so a just-added entry is durable to the extent the
/// filesystem supports it. `O_NOFOLLOW | O_DIRECTORY` refuses a symlink or a
/// non-directory swapped in at the final component.
fn fsync_dir(path: &Path) -> Result<(), StorageError> {
    let dir = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| StorageError::FinalizationSync {
        context: format!("open directory for fsync {}", path.display()),
        source: errno.into(),
    })?;
    rustix::fs::fsync(&dir).map_err(|errno| StorageError::FinalizationSync {
        context: format!("fsync directory {}", path.display()),
        source: errno.into(),
    })
}

enum OpenReject {
    NotFound,
    NotRegular,
    Io(std::io::Error),
}

/// Opens `path` read-only, refusing a symlink at the final component
/// (`O_NOFOLLOW`) and any non-regular file.
fn open_regular_no_follow(path: &Path) -> Result<File, OpenReject> {
    let fd = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Err(OpenReject::NotFound),
        // O_NOFOLLOW hitting a symlink at the final component.
        Err(Errno::LOOP) => return Err(OpenReject::NotRegular),
        Err(errno) => return Err(OpenReject::Io(errno.into())),
    };
    let stat = rustix::fs::fstat(&fd).map_err(|errno| OpenReject::Io(errno.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(OpenReject::NotRegular);
    }
    Ok(File::from(fd))
}

/// Streams a file through a bounded buffer, returning its exact size and
/// SHA-256 without ever holding the whole file in memory.
fn scan_size_and_digest(mut file: File) -> Result<(u64, Sha256Digest), std::io::Error> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; SCAN_BUF_LEN];
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, Sha256Digest::from_raw(hasher.finalize().into())))
}

/// A Worker-generated 32-character lowercase-hex token, so a staging file
/// name is unambiguously Worker-owned and two concurrent stages of the same
/// `(transfer_id, chunk_index)` never collide.
fn worker_random_token() -> String {
    Uuid::new_v4().simple().to_string()
}

/// `<digits>.<32 lowercase hex>.part` — specific enough that startup cleanup
/// can identify a Worker-owned staging file without matching broad patterns
/// like "any dotfile".
fn is_recognized_staging_name(name: &str) -> bool {
    let mut parts = name.split('.');
    let (Some(index), Some(token), Some(ext), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
        && token.len() == 32
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && ext == STAGING_EXT
}

fn validate_root_shape(root: &Path) -> Result<(), StorageError> {
    if root.as_os_str().is_empty() || !root.is_absolute() {
        return Err(StorageError::RootNotAbsolute {
            path: root.to_path_buf(),
        });
    }
    let mut normal_components = 0usize;
    for component in root.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {}
            Component::Normal(_) => normal_components += 1,
            Component::CurDir | Component::ParentDir => {
                return Err(StorageError::RootUnsafe {
                    path: root.to_path_buf(),
                    reason: "must not contain '.' or '..' components",
                });
            }
        }
    }
    if normal_components == 0 {
        return Err(StorageError::RootUnsafe {
            path: root.to_path_buf(),
            reason: "must not be the filesystem root",
        });
    }
    Ok(())
}

/// Ensures `path` is a Worker-owned directory: create it `0700` if absent,
/// otherwise verify it is a real directory (not a symlink, not a regular
/// file) and tighten away any group/other permission bits. Races to create
/// the same directory converge.
fn ensure_worker_dir(path: &Path) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => validate_existing_worker_dir(path, &meta),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let meta = fs::symlink_metadata(path).map_err(|source| StorageError::Io {
                        context: format!("stat storage directory {}", path.display()),
                        source,
                    })?;
                    return validate_existing_worker_dir(path, &meta);
                }
                Err(source) => {
                    return Err(StorageError::Io {
                        context: format!("create storage directory {}", path.display()),
                        source,
                    });
                }
            }
            fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE)).map_err(|source| {
                StorageError::Io {
                    context: format!("set permissions on storage directory {}", path.display()),
                    source,
                }
            })?;
            // Make the new directory entry durable in its parent.
            if let Some(parent) = path.parent() {
                let _ = fsync_dir(parent);
            }
            Ok(())
        }
        Err(source) => Err(StorageError::Io {
            context: format!("stat storage directory {}", path.display()),
            source,
        }),
    }
}

fn validate_existing_worker_dir(path: &Path, meta: &fs::Metadata) -> Result<(), StorageError> {
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return Err(StorageError::UnsafeDirectory {
            path: path.to_path_buf(),
            reason: "is a symlink",
        });
    }
    if !file_type.is_dir() {
        return Err(StorageError::UnsafeDirectory {
            path: path.to_path_buf(),
            reason: "is not a directory",
        });
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        // Narrowing only — never widening the Worker's authority over a
        // pre-existing directory.
        fs::set_permissions(path, fs::Permissions::from_mode(mode & DIR_MODE)).map_err(
            |source| StorageError::Io {
                context: format!(
                    "tighten permissions on storage directory {}",
                    path.display()
                ),
                source,
            },
        )?;
    }
    Ok(())
}

/// Removes recognized `.staging/*.part` files left by a previous run. Only
/// regular files whose name matches [`is_recognized_staging_name`], only
/// inside a real `<transfer>/chunks/.staging` directory, never following a
/// symlink, never recursing. Finalized files, directories, and unrecognized
/// files are left untouched.
fn cleanup_recognized_staging(transfers: &Path) -> Result<(), StorageError> {
    let entries = match fs::read_dir(transfers) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StorageError::Io {
                context: format!("read {}", transfers.display()),
                source,
            })
        }
    };

    for entry in entries {
        let entry = entry.map_err(|source| StorageError::Io {
            context: format!("read entry under {}", transfers.display()),
            source,
        })?;
        let transfer_dir = entry.path();
        if !is_real_dir(&transfer_dir) {
            continue;
        }
        let staging_dir = transfer_dir.join(CHUNKS_DIR).join(STAGING_DIR);
        if !is_real_dir(&staging_dir) {
            continue;
        }
        let staging_entries = match fs::read_dir(&staging_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for staging_entry in staging_entries.flatten() {
            let candidate = staging_entry.path();
            let Ok(meta) = fs::symlink_metadata(&candidate) else {
                continue;
            };
            let recognized = candidate
                .file_name()
                .and_then(|name| name.to_str())
                .map(is_recognized_staging_name)
                .unwrap_or(false);
            if meta.file_type().is_file() && recognized {
                let _ = fs::remove_file(&candidate);
            }
        }
    }
    Ok(())
}

fn is_real_dir(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Ok(meta) if meta.file_type().is_dir())
}

#[cfg(test)]
mod tests;
