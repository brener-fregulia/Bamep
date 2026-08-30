//! Worker-local full-Artifact reconstruction and independent SHA-256
//! computation (Issue #39 Phase D2).
//!
//! Given the mechanically authoritative sealed facts `(transfer_id,
//! chunk_count, chunk_size)` — Phase E sources these from a successful
//! `ManifestSealDecision`, never from local directory inspection — this
//! layer reopens the Phase D1 finalized chunk files `0..chunk_count`, in
//! that exact ascending order, streams their **raw** bytes through **one
//! fresh** SHA-256 state, and returns the computed full-Artifact digest and
//! total size.
//!
//! ```text
//! full_artifact_bytes := raw(chunk 0) || raw(chunk 1) || ... || raw(chunk chunk_count-1)
//! ```
//!
//! No framing, index encoding, length prefix, separator, newline, padding,
//! JSON, digest text, filename, UUID, or metadata is inserted between or
//! around chunks (`m0-data-plane-and-storage-contracts.md` "Full-Artifact
//! byte reconstruction").
//!
//! # This is an independent verification reread
//!
//! D2 never reuses the incremental hash D1 computed during upload, nor
//! [`FinalizedChunk::digest`](super::FinalizedChunk::digest), nor
//! [`StoredChunkFacts::digest`](super::StoredChunkFacts::digest), nor any
//! cached full digest, nor the Agent-declared / `ManifestSealDecision`
//! expected Artifact digest. Every call rereads the actual restart-stable
//! bytes currently on disk with a fresh hasher.
//!
//! # D2 owns no verdict
//!
//! D2 does **not** compare its computed digest against an expected value and
//! does **not** decide `Verified` / `Failed`. It returns only the mechanical
//! observation. Phase E sends `computed_artifact_digest` to `bamepd` via
//! `ArtifactVerificationReport`, and `bamepd` (Phase C2) independently
//! compares it against the durable expected digest and commits the
//! transition. A finalized chunk whose bytes were altered after D1
//! finalization but whose length is still valid simply contributes its
//! *current* bytes to the digest — that is mechanical reality, not a D2
//! error.
//!
//! # Digest algorithm
//!
//! M1 supports SHA-256 only; this module implements SHA-256 explicitly and
//! carries no algorithm field. Phase E MUST exhaustively match the
//! authoritative `bamep_worker_protocol::WireDigestAlgorithm` (currently
//! only `Sha256`) before calling this module — a future algorithm must not
//! silently fall through to SHA-256.

#[cfg(test)]
mod tests;

use std::io::Read;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{ChunkIndex, ChunkStore, FilesystemChunkStore, Sha256Digest, StorageError};

/// Bounded, reusable read buffer. Peak memory stays independent of Artifact
/// size — the full Artifact (potentially hundreds of GB) is never
/// materialized in memory or as a second file. Matches D1's scan buffer.
const RECONSTRUCT_BUF_LEN: usize = 64 * 1024;

/// The mechanically authoritative sealed facts D2 reconstructs against.
/// Phase E fills these from a successful `ManifestSealDecision`
/// (`chunk_count: u64`, `chunk_size: u32`), never from local file counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullArtifactRequest {
    pub transfer_id: Uuid,
    /// Number of chunks in the sealed manifest. `0` is permitted by the
    /// current Domain seal model (see module tests) and yields
    /// `SHA-256("")` with `total_size == 0` and no filesystem reads.
    pub chunk_count: u64,
    /// The sealed fixed chunk size. Every chunk except the last must be
    /// exactly this many bytes; the last must be `1..=chunk_size`.
    pub chunk_size: u32,
}

/// The mechanical result of one reconstruction. Carries no `ArtifactState`,
/// `held`/`accepted` flag, or expected digest — only what was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullArtifactDigest {
    pub transfer_id: Uuid,
    pub chunk_count: u64,
    /// Exact number of bytes read across all chunks (checked arithmetic).
    pub total_size: u64,
    /// SHA-256 over the raw ordered concatenation. Use
    /// [`Sha256Digest::to_base64url_no_pad`] at the Phase E wire boundary.
    pub digest: Sha256Digest,
}

/// The closed mechanical error vocabulary for reconstruction. None of these
/// mean "Artifact Failed" — they mean the Worker could not form the
/// contract-defined byte stream and therefore cannot produce a verification
/// report. Phase E owns how the request fails closed and any HTTP mapping.
#[derive(Debug, thiserror::Error)]
pub enum FullArtifactError {
    /// A chunk index in `0..chunk_count` has no finalized file.
    #[error("required chunk {chunk_index} of transfer {transfer_id} has no finalized file")]
    RequiredChunkMissing {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    },
    /// A required chunk's finalized file could not be opened through the
    /// safe storage API — a symlink / non-regular file, or an open I/O
    /// error. The D1 refusal is preserved as the source.
    #[error("required chunk {chunk_index} of transfer {transfer_id} could not be opened")]
    RequiredChunkUnreadable {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
        #[source]
        source: StorageError,
    },
    /// Reading a required chunk's bytes failed partway.
    #[error("reading chunk {chunk_index} of transfer {transfer_id} failed")]
    ChunkReadFailed {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
        #[source]
        source: std::io::Error,
    },
    /// A chunk's byte count exceeded the sealed `chunk_size`. Detected while
    /// streaming: reading stops as soon as the bound is crossed, so a
    /// hugely oversized file is not scanned to its end.
    #[error(
        "chunk {chunk_index} of transfer {transfer_id} exceeds the sealed chunk_size of {chunk_size} bytes"
    )]
    ChunkExceedsChunkSize {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
        chunk_size: u64,
    },
    /// A non-final chunk was shorter than the sealed `chunk_size`.
    #[error(
        "non-final chunk {chunk_index} of transfer {transfer_id} is {observed} bytes; sealed chunk_size is {chunk_size}"
    )]
    NonFinalChunkTooShort {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
        chunk_size: u64,
        observed: u64,
    },
    /// The final chunk of a sealed non-empty Artifact contained zero bytes;
    /// it must be `1..=chunk_size`.
    #[error("final chunk {chunk_index} of transfer {transfer_id} is empty")]
    FinalChunkEmpty {
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
    },
    /// `total_size` accumulation overflowed `u64`.
    #[error("reconstructed total size overflowed for transfer {transfer_id}")]
    TotalSizeOverflow { transfer_id: Uuid },
    /// The `spawn_blocking` reconstruction task did not run to completion
    /// (it panicked, or the runtime is shutting down). Only the async
    /// wrapper produces this.
    #[error("full-Artifact reconstruction task failed to complete")]
    BlockingTaskFailed,
}

/// Reconstructs one sealed Artifact's full SHA-256 from D1 finalized chunk
/// files. Holds a [`ChunkStore`]; read-only, allocates no per-Artifact
/// buffer beyond one reusable [`RECONSTRUCT_BUF_LEN`]-byte scratch buffer.
///
/// Concurrent reconstructions are independent — the hasher state is created
/// fresh per call, D1 finalization is no-replace, and D2 never mutates
/// storage — so two reconstructions over the same stable files always
/// produce the same digest without any global lock.
pub struct FullArtifactHasher<S> {
    store: S,
}

impl<S: ChunkStore> FullArtifactHasher<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// Synchronous, blocking reconstruction core. Directly unit-testable.
    ///
    /// Generates the indices `0..chunk_count` itself; it never enumerates
    /// directory entries, sorts filenames, infers `chunk_count` from local
    /// files, or includes an orphan final with `chunk_index >= chunk_count`.
    /// A finalized file it does not need is left completely untouched.
    pub fn compute(
        &self,
        request: &FullArtifactRequest,
    ) -> Result<FullArtifactDigest, FullArtifactError> {
        let FullArtifactRequest {
            transfer_id,
            chunk_count,
            chunk_size,
        } = *request;
        let chunk_size = u64::from(chunk_size);

        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; RECONSTRUCT_BUF_LEN];
        let mut total_size: u64 = 0;

        // `chunk_count == 0` never enters this loop: the hasher stays fresh,
        // `total_size` stays 0, and the result is SHA-256 of the empty
        // stream with no filesystem reads. Inside the loop `chunk_count >= 1`
        // always holds, so `chunk_count - 1` cannot underflow.
        for chunk_index in 0..chunk_count {
            let is_final_chunk = chunk_index == chunk_count - 1;
            let chunk_bytes = self.hash_one_chunk(
                transfer_id,
                chunk_index,
                chunk_size,
                &mut hasher,
                &mut buffer,
                &mut total_size,
            )?;

            if is_final_chunk {
                if chunk_bytes == 0 {
                    return Err(FullArtifactError::FinalChunkEmpty {
                        transfer_id,
                        chunk_index,
                    });
                }
            } else if chunk_bytes != chunk_size {
                // `chunk_bytes > chunk_size` already failed inside
                // `hash_one_chunk`, so this is strictly a short chunk.
                return Err(FullArtifactError::NonFinalChunkTooShort {
                    transfer_id,
                    chunk_index,
                    chunk_size,
                    observed: chunk_bytes,
                });
            }
        }

        // The per-chunk checks already guarantee the M1 size formula
        // (`(chunk_count - 1) * chunk_size + final_size`); this only guards
        // against an internal accounting bug, never external file state.
        debug_assert!(size_formula_holds(chunk_count, chunk_size, total_size));

        Ok(FullArtifactDigest {
            transfer_id,
            chunk_count,
            total_size,
            digest: Sha256Digest::from_raw(hasher.finalize().into()),
        })
    }

    /// Streams one required chunk's bytes into `hasher`, enforcing the
    /// `chunk_size` upper bound *while reading* (not only at EOF). Returns
    /// the exact number of bytes this chunk contributed.
    fn hash_one_chunk(
        &self,
        transfer_id: Uuid,
        chunk_index: ChunkIndex,
        chunk_size: u64,
        hasher: &mut Sha256,
        buffer: &mut [u8],
        total_size: &mut u64,
    ) -> Result<u64, FullArtifactError> {
        let mut reader = match self.store.open_final(transfer_id, chunk_index) {
            Ok(reader) => reader,
            Err(StorageError::FinalizedChunkNotFound { .. }) => {
                return Err(FullArtifactError::RequiredChunkMissing {
                    transfer_id,
                    chunk_index,
                });
            }
            Err(source) => {
                return Err(FullArtifactError::RequiredChunkUnreadable {
                    transfer_id,
                    chunk_index,
                    source,
                });
            }
        };

        let mut chunk_bytes: u64 = 0;
        loop {
            let read =
                reader
                    .read(buffer)
                    .map_err(|source| FullArtifactError::ChunkReadFailed {
                        transfer_id,
                        chunk_index,
                        source,
                    })?;
            if read == 0 {
                break;
            }
            let read = read as u64;

            chunk_bytes = chunk_bytes
                .checked_add(read)
                .ok_or(FullArtifactError::TotalSizeOverflow { transfer_id })?;
            if chunk_bytes > chunk_size {
                return Err(FullArtifactError::ChunkExceedsChunkSize {
                    transfer_id,
                    chunk_index,
                    chunk_size,
                });
            }
            *total_size = total_size
                .checked_add(read)
                .ok_or(FullArtifactError::TotalSizeOverflow { transfer_id })?;

            hasher.update(&buffer[..read as usize]);
        }

        Ok(chunk_bytes)
    }
}

impl<S> FullArtifactHasher<S>
where
    S: ChunkStore + Send + 'static,
{
    /// Runs the **entire** synchronous [`compute`](Self::compute) inside one
    /// `tokio::task::spawn_blocking` — not one blocking task per read or per
    /// chunk — so a large Artifact scan never blocks the Tokio reactor and a
    /// reconstruction crosses the blocking boundary exactly once.
    ///
    /// Cancellation: dropping the returned future does not stop a
    /// `spawn_blocking` closure that has already started; the filesystem
    /// scan runs to completion on the blocking pool. Phase E simply
    /// discards the result if the originating HTTP/UDS operation is no
    /// longer current — D2 makes no hard-cancellation claim.
    pub async fn compute_blocking(
        self,
        request: FullArtifactRequest,
    ) -> Result<FullArtifactDigest, FullArtifactError> {
        match tokio::task::spawn_blocking(move || self.compute(&request)).await {
            Ok(result) => result,
            Err(_join_error) => Err(FullArtifactError::BlockingTaskFailed),
        }
    }
}

impl FullArtifactHasher<FilesystemChunkStore> {
    /// Convenience constructor for the production Adapter, which is cheaply
    /// `Clone` (it holds only its immutable root path), so Phase E can build
    /// a hasher per verification without sharing mutable state.
    pub fn for_store(store: &FilesystemChunkStore) -> Self {
        Self::new(store.clone())
    }
}

/// `total_size == (chunk_count - 1) * chunk_size + final_size`, with
/// `1 <= final_size <= chunk_size`, using checked arithmetic. Only used in a
/// `debug_assert!`.
fn size_formula_holds(chunk_count: u64, chunk_size: u64, total_size: u64) -> bool {
    let Some(last) = chunk_count.checked_sub(1) else {
        return total_size == 0;
    };
    let Some(full_prefix) = last.checked_mul(chunk_size) else {
        return false;
    };
    let Some(final_size) = total_size.checked_sub(full_prefix) else {
        return false;
    };
    (1..=chunk_size).contains(&final_size)
}
