//! Worker-local chunk byte storage (Issue #39 Phase D1): the narrow
//! filesystem mechanism that stages one authorized chunk body, hashes it
//! incrementally with SHA-256, and finalizes it into a restart-stable file
//! that the later HTTPS data-plane handler (Phase E) and full-Artifact
//! reconstruction (Phase D2) will consume.
//!
//! This layer owns **bytes only**. It owns no Transfer/Artifact business
//! state, no durable authority, and performs no `bamepd` coordination
//! (ADR-0018 "PostgreSQL and storage": "Storage I/O and durable
//! business-state persistence are different responsibilities: the former is
//! execution, the latter is authority reserved to `bamepd`"). A finalized
//! file on disk means only "these exact bytes were mechanically staged and
//! hashed here" — never that `bamepd` durably accepted or holds the chunk
//! (`m0-data-plane-and-storage-contracts.md` "Durable chunk acceptance
//! ordering": "A Worker-local verified buffer/file is never itself durable
//! Artifact state"). See [`FinalizedChunk`] for the authority caveats an
//! orphan final file carries.
//!
//! Deliberately **not** in Phase D1: the HTTPS listener, TLS serving, route
//! parsing, HTTP header handling, any UDS business message
//! (`AuthorizationQuery`, `ChunkAcceptanceRequest`, `ResumeDiscoveryQuery`,
//! `ManifestSealRequest`, `ArtifactVerificationReport`), and full-Artifact
//! concatenation/digest across chunks.
//!
//! # Blocking-I/O model
//!
//! The storage API is synchronous and blocking: it uses `std::fs` plus
//! `rustix` for the Unix-safe primitives (`O_NOFOLLOW` opens, `fstat`,
//! `linkat` no-replace placement, directory `fsync`) that `tokio::fs` does
//! not expose. It never buffers a whole chunk in memory and it exposes
//! explicit `flush`/`fsync`/finalize steps. Phase E's async request handler
//! MUST drive it from `tokio::task::spawn_blocking` (or an equivalent
//! dedicated blocking-I/O boundary) so a large chunk write never blocks the
//! Tokio reactor; [`StagingChunk`] is `Send`, so a handler can move it in
//! and out of `spawn_blocking` across `await` points between body frames.

mod digest;

pub use digest::Sha256Digest;

use std::path::PathBuf;

use uuid::Uuid;

/// A mechanical chunk index. The Worker uses the protocol's plain `u64`
/// rather than depending on `bamep_domain::ChunkIndex` (ADR-0018: the Worker
/// crate has no Domain dependency).
pub type ChunkIndex = u64;

/// What a successful [`StagingChunk::finalize`] did with the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizedDisposition {
    /// This finalization installed the final chunk file.
    Installed,
    /// A byte-for-byte identical final chunk file (same size and SHA-256)
    /// was already present. The freshly staged copy was discarded and the
    /// original file left exactly as it was.
    AlreadyPresent,
}

/// The mechanical facts about a chunk this Worker just finalized.
///
/// This value is **not** durable authority. In particular a
/// `FinalizedChunk` (or a finalized file discovered on disk at startup)
/// never means `bamepd` durably accepted or holds the chunk: a Worker that
/// finalizes a file and then crashes before the `ChunkAcceptanceRequest`
/// round trip is a valid state, and the residual file is restart-stable
/// residue, not business state. Phase E still consults `bamepd` authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedChunk {
    pub transfer_id: Uuid,
    pub chunk_index: ChunkIndex,
    /// Exact number of bytes in the finalized chunk (always `>= 1`).
    pub size: u64,
    /// SHA-256 over exactly those bytes.
    pub digest: Sha256Digest,
    pub disposition: FinalizedDisposition,
}

/// Mechanical facts read back from an already-finalized chunk file. Computed
/// by scanning the file; never cached as business state, never persisted to
/// a sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredChunkFacts {
    pub size: u64,
    pub digest: Sha256Digest,
}

/// The closed mechanical error vocabulary for the storage layer. These are
/// Worker-internal; Phase E owns any mapping to HTTP status codes.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The configured storage root is not an absolute path.
    #[error("chunk storage root must be an absolute path: {path}")]
    RootNotAbsolute { path: PathBuf },
    /// The configured storage root is absolute but structurally unsafe
    /// (e.g. the filesystem root itself, or contains `.`/`..`).
    #[error("chunk storage root is not a safe location ({reason}): {path}")]
    RootUnsafe { path: PathBuf, reason: &'static str },
    /// A path where the Worker requires its own directory is a symlink, a
    /// non-directory, or otherwise not usable as a Worker-owned directory.
    #[error("chunk storage path is not a usable Worker-owned directory ({reason}): {path}")]
    UnsafeDirectory { path: PathBuf, reason: &'static str },
    /// Staged bytes exceeded the caller-supplied maximum. No final file is
    /// produced and the staging file is discarded.
    #[error("staged chunk exceeded its maximum of {max} bytes")]
    Oversize { max: u64 },
    /// Finalization was attempted with zero bytes written. A chunk must
    /// carry at least one byte (`m1-worker-data-plane-control-contract.md`).
    #[error("a chunk must contain at least one byte")]
    EmptyChunk,
    /// A different chunk (different size or SHA-256) is already finalized at
    /// this `(transfer_id, chunk_index)`. The existing file is left
    /// unchanged; the new staging is discarded.
    #[error(
        "a different chunk is already finalized for transfer {transfer_id} index {chunk_index}"
    )]
    FinalizedChunkConflict {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    },
    /// No finalized chunk exists at this `(transfer_id, chunk_index)`. The
    /// Worker never fabricates chunk bytes from anything else.
    #[error("no finalized chunk for transfer {transfer_id} index {chunk_index}")]
    FinalizedChunkNotFound {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    },
    /// The deterministic final-chunk path exists but is a symlink or a
    /// non-regular file. Refused rather than followed.
    #[error(
        "finalized chunk path for transfer {transfer_id} index {chunk_index} is not a regular file"
    )]
    FinalizedChunkNotRegular {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    },
    /// A staging or read I/O operation failed.
    #[error("chunk storage I/O failed ({context})")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    /// A durability/finalization step (flush, file `fsync`, atomic
    /// placement, directory `fsync`) failed.
    #[error("chunk finalization synchronization failed ({context})")]
    FinalizationSync {
        context: String,
        #[source]
        source: std::io::Error,
    },
    /// The storage mechanism is not available on this (non-Unix) platform.
    /// Linux is the Worker reference/production environment
    /// (`docs/development/testing.md`).
    #[error("chunk storage is not supported on this platform")]
    Unsupported,
}

#[cfg(unix)]
mod fs_store;

#[cfg(unix)]
pub use fs_store::{ChunkStore, FilesystemChunkStore, StagingChunk, StoredChunkReader};

#[cfg(not(unix))]
mod fs_store {
    use super::{PathBuf, StorageError};

    /// Non-Unix portability stub. The real filesystem storage Adapter is
    /// Unix-only (owner-restrictive permissions, symlink refusal,
    /// `O_NOFOLLOW`, `linkat` no-replace placement, directory `fsync`);
    /// this keeps the crate compilable elsewhere and never becomes usable —
    /// no substitute storage path is introduced.
    #[derive(Debug, Clone)]
    pub struct FilesystemChunkStore;

    impl FilesystemChunkStore {
        pub fn initialize(_root: impl Into<PathBuf>) -> Result<Self, StorageError> {
            Err(StorageError::Unsupported)
        }
    }
}

#[cfg(not(unix))]
pub use fs_store::FilesystemChunkStore;
