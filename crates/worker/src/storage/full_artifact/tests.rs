//! Phase D2 reconstruction tests, grouped: reconstruction (raw
//! concatenation / read boundaries / partial final / zero count), order
//! (ascending order matters / extra final ignored), size (short / oversized
//! non-final, empty / oversized final), missing & unreadable required
//! chunks, authority separation (same-size byte mutation is not a D2 error),
//! restart-stable input, concurrency, and the async `spawn_blocking`
//! wrapper.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::{FullArtifactError, FullArtifactHasher, FullArtifactRequest};
use crate::storage::{ChunkStore, FilesystemChunkStore, Sha256Digest, StorageError};

/// Isolated temporary storage tree, removed on drop. Same manual pattern as
/// the D1 tests (no `tempfile` dependency).
struct TempTree {
    base: PathBuf,
}

impl TempTree {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("bamep-d2-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).expect("create temp base");
        Self { base }
    }

    fn root(&self) -> PathBuf {
        self.base.join("chunk-storage-root")
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// The D1 on-disk layout, duplicated here exactly as `fs_store::tests`
/// duplicates it — used only to arrange adversarial fixtures D1's safe API
/// refuses to create (empty final, oversized file, symlink). Drift would
/// break both test modules.
fn final_path(root: &Path, transfer_id: Uuid, chunk_index: u64) -> PathBuf {
    root.join("transfers")
        .join(transfer_id.as_hyphenated().to_string())
        .join("chunks")
        .join(format!("{chunk_index}.chunk"))
}

/// Stages and finalizes one chunk through the real D1 API. `stage_max` is
/// the D1 per-stage cap, deliberately allowed to differ from the D2 seal
/// `chunk_size` so oversized-file fixtures can be built (checkpoint §40).
fn put_chunk(
    store: &FilesystemChunkStore,
    transfer_id: Uuid,
    chunk_index: u64,
    bytes: &[u8],
    stage_max: u64,
) {
    let mut staging = store
        .begin_stage(transfer_id, chunk_index, stage_max)
        .expect("begin stage");
    staging.write(bytes).expect("write");
    staging.finalize().expect("finalize");
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from_raw(Sha256::digest(bytes).into())
}

fn hasher(store: &FilesystemChunkStore) -> FullArtifactHasher<FilesystemChunkStore> {
    FullArtifactHasher::for_store(store)
}

fn request(transfer_id: Uuid, chunk_count: u64, chunk_size: u32) -> FullArtifactRequest {
    FullArtifactRequest {
        transfer_id,
        chunk_count,
        chunk_size,
    }
}

// ----------------------------------------------------------------------------
// reconstruction
// ----------------------------------------------------------------------------

/// §33 — the reconstructed stream is exactly `raw(0) || raw(1)`, with no
/// separator, newline, index text, or length prefix.
#[test]
fn exact_raw_concatenation_adds_no_framing() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();
    put_chunk(&store, transfer_id, 0, b"ab", 2);
    put_chunk(&store, transfer_id, 1, b"c", 2);

    let result = hasher(&store)
        .compute(&request(transfer_id, 2, 2))
        .expect("compute");

    assert_eq!(result.total_size, 3);
    assert_eq!(
        result.digest.to_base64url_no_pad(),
        "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0",
        "must be SHA-256(b\"abc\")"
    );
    assert_eq!(result.digest, sha256(b"abc"));
    // Not any framed representation.
    for framed in [
        b"ab|c".as_slice(),
        b"ab\nc",
        b"\x00ab\x01c",
        b"0ab1c",
        b"ab c",
    ] {
        assert_ne!(result.digest, sha256(framed));
    }
}

/// §34 — a chunk larger than the internal read buffer is hashed identically
/// to its logical bytes, independent of how many `read` calls it took.
#[test]
fn hashing_is_independent_of_read_call_boundaries() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let chunk_size = 200_003u32; // > 64 KiB buffer, not a multiple of it
    let c0: Vec<u8> = (0..chunk_size).map(|i| (i % 251) as u8).collect();
    let c1: Vec<u8> = (0..50_000u32).map(|i| (i % 97) as u8).collect();
    put_chunk(&store, transfer_id, 0, &c0, u64::from(chunk_size));
    put_chunk(&store, transfer_id, 1, &c1, u64::from(chunk_size));

    let mut logical = c0.clone();
    logical.extend_from_slice(&c1);

    let result = hasher(&store)
        .compute(&request(transfer_id, 2, chunk_size))
        .expect("compute");
    assert_eq!(result.total_size, logical.len() as u64);
    assert_eq!(result.digest, sha256(&logical));
}

/// §35 — three chunks: two full, one partial; strict ascending order, exact
/// total, exact digest.
#[test]
fn three_chunks_full_full_partial() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 4096u32;
    let c0 = vec![0xA1u8; n as usize];
    let c1 = vec![0xB2u8; n as usize];
    let c2 = vec![0xC3u8; 1000];
    put_chunk(&store, transfer_id, 0, &c0, u64::from(n));
    put_chunk(&store, transfer_id, 1, &c1, u64::from(n));
    put_chunk(&store, transfer_id, 2, &c2, u64::from(n));

    let mut logical = c0.clone();
    logical.extend_from_slice(&c1);
    logical.extend_from_slice(&c2);

    let result = hasher(&store)
        .compute(&request(transfer_id, 3, n))
        .expect("compute");
    assert_eq!(result.chunk_count, 3);
    assert_eq!(result.total_size, u64::from(n) * 2 + 1000);
    assert_eq!(result.digest, sha256(&logical));
}

/// §42 — a one-byte final chunk is valid.
#[test]
fn partial_final_chunk_of_one_byte_is_valid() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 4096u32;
    let c0 = vec![7u8; n as usize];
    put_chunk(&store, transfer_id, 0, &c0, u64::from(n));
    put_chunk(&store, transfer_id, 1, b"Z", u64::from(n));

    let mut logical = c0.clone();
    logical.push(b'Z');

    let result = hasher(&store)
        .compute(&request(transfer_id, 2, n))
        .expect("compute");
    assert_eq!(result.total_size, u64::from(n) + 1);
    assert_eq!(result.digest, sha256(&logical));
}

/// §47 — the current Domain seal model (`ChunkManifest::seal` in
/// `crates/domain/src/chunk_manifest.rs`) has **no** `chunk_count > 0`
/// invariant: a manifest with zero recorded chunks seals successfully with
/// `chunk_count == 0`. The M1 reconstruction of that manifest is the empty
/// byte stream, so D2 returns `SHA-256("")`, `total_size == 0`, and reads no
/// files.
#[test]
fn zero_chunk_count_is_sha256_of_the_empty_stream() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let result = hasher(&store)
        .compute(&request(transfer_id, 0, 4096))
        .expect("compute");
    assert_eq!(result.chunk_count, 0);
    assert_eq!(result.total_size, 0);
    assert_eq!(result.digest, sha256(b""));

    // No transfer directory was created as a side effect.
    assert!(!tree
        .root()
        .join("transfers")
        .join(transfer_id.as_hyphenated().to_string())
        .exists());
}

// ----------------------------------------------------------------------------
// order
// ----------------------------------------------------------------------------

/// §36 — order matters: the result is `SHA-256(A || B || C)` and differs
/// from `SHA-256(B || A || C)`. Guards against directory-enumeration or
/// lexical-sort mistakes.
#[test]
fn wrong_order_would_produce_a_different_digest() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 3u32;
    let a = b"AAA";
    let b = b"BBB";
    let c = b"CC";
    put_chunk(&store, transfer_id, 0, a, u64::from(n));
    put_chunk(&store, transfer_id, 1, b, u64::from(n));
    put_chunk(&store, transfer_id, 2, c, u64::from(n));

    let result = hasher(&store)
        .compute(&request(transfer_id, 3, n))
        .expect("compute");

    let mut abc = a.to_vec();
    abc.extend_from_slice(b);
    abc.extend_from_slice(c);
    let mut bac = b.to_vec();
    bac.extend_from_slice(a);
    bac.extend_from_slice(c);

    assert_eq!(result.digest, sha256(&abc));
    assert_ne!(result.digest, sha256(&bac));
}

/// §37 — an orphan final with `chunk_index >= chunk_count` is not part of
/// the reconstruction and is left untouched.
#[test]
fn an_extra_local_final_is_ignored_and_untouched() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    put_chunk(&store, transfer_id, 0, b"first-", 16);
    put_chunk(&store, transfer_id, 1, b"second", 16);
    put_chunk(&store, transfer_id, 2, b"ORPHAN", 16);

    let result = hasher(&store)
        .compute(&request(transfer_id, 2, 6))
        .expect("compute");
    assert_eq!(result.total_size, 12);
    assert_eq!(result.digest, sha256(b"first-second"));

    let orphan = final_path(&tree.root(), transfer_id, 2);
    assert!(orphan.is_file(), "orphan final still present");
    assert_eq!(std::fs::read(&orphan).unwrap(), b"ORPHAN");
}

// ----------------------------------------------------------------------------
// size validation
// ----------------------------------------------------------------------------

/// §39 — a non-final chunk shorter than `chunk_size` fails; no padding, no
/// digest.
#[test]
fn a_short_non_final_chunk_fails() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 4096u32;
    put_chunk(&store, transfer_id, 0, &vec![1u8; 4095], u64::from(n));
    put_chunk(&store, transfer_id, 1, b"tail", u64::from(n));

    let err = hasher(&store)
        .compute(&request(transfer_id, 2, n))
        .expect_err("short non-final");
    match err {
        FullArtifactError::NonFinalChunkTooShort {
            chunk_index,
            chunk_size,
            observed,
            ..
        } => {
            assert_eq!(chunk_index, 0);
            assert_eq!(chunk_size, 4096);
            assert_eq!(observed, 4095);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

/// §40 — a non-final file larger than the sealed `chunk_size` fails as soon
/// as D2 observes more than `chunk_size` bytes (built with a D1 stage cap
/// larger than the D2 seal size).
#[test]
fn an_oversized_non_final_chunk_fails_while_streaming() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let seal_chunk_size = 4096u32;
    put_chunk(&store, transfer_id, 0, &vec![9u8; 4097], 8192);
    put_chunk(&store, transfer_id, 1, b"tail", u64::from(seal_chunk_size));

    let err = hasher(&store)
        .compute(&request(transfer_id, 2, seal_chunk_size))
        .expect_err("oversized non-final");
    assert!(matches!(
        err,
        FullArtifactError::ChunkExceedsChunkSize {
            chunk_index: 0,
            chunk_size: 4096,
            ..
        }
    ));
}

/// §16/§20 — a final file larger than `chunk_size` also fails.
#[test]
fn an_oversized_final_chunk_fails_while_streaming() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let seal_chunk_size = 4096u32;
    put_chunk(
        &store,
        transfer_id,
        0,
        &vec![3u8; 4096],
        u64::from(seal_chunk_size),
    );
    put_chunk(&store, transfer_id, 1, &vec![4u8; 5000], 8192);

    let err = hasher(&store)
        .compute(&request(transfer_id, 2, seal_chunk_size))
        .expect_err("oversized final");
    assert!(matches!(
        err,
        FullArtifactError::ChunkExceedsChunkSize { chunk_index: 1, .. }
    ));
}

/// §41 — an empty final file (arranged directly, since D1 refuses to
/// finalize a zero-byte chunk) fails; it is not hashed as a valid last
/// chunk.
#[test]
fn an_empty_final_chunk_fails() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 4096u32;
    put_chunk(&store, transfer_id, 0, &vec![5u8; n as usize], u64::from(n));
    // Direct fixture: an empty regular file where chunk 1's final belongs.
    std::fs::File::create(final_path(&tree.root(), transfer_id, 1)).expect("create empty final");

    let err = hasher(&store)
        .compute(&request(transfer_id, 2, n))
        .expect_err("empty final");
    assert!(matches!(
        err,
        FullArtifactError::FinalChunkEmpty { chunk_index: 1, .. }
    ));
}

// ----------------------------------------------------------------------------
// missing / unreadable required chunk
// ----------------------------------------------------------------------------

/// §38 — a missing middle chunk fails, identifying the required index; no
/// digest, no substitution.
#[test]
fn a_missing_middle_chunk_fails_naming_the_index() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 4u32;
    put_chunk(&store, transfer_id, 0, b"AAAA", u64::from(n));
    put_chunk(&store, transfer_id, 2, b"CC", u64::from(n));

    let err = hasher(&store)
        .compute(&request(transfer_id, 3, n))
        .expect_err("missing chunk 1");
    assert!(matches!(
        err,
        FullArtifactError::RequiredChunkMissing { chunk_index: 1, .. }
    ));
}

/// §44 — a symlink where a required final belongs is refused through D1's
/// safe open; the symlink target is never read.
#[test]
fn a_symlink_final_is_refused_through_the_safe_open() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 4096u32;
    put_chunk(&store, transfer_id, 0, &vec![1u8; n as usize], u64::from(n));

    let outside = tree.base.join("outside-secret");
    std::fs::write(&outside, vec![2u8; 10]).unwrap();
    std::os::unix::fs::symlink(&outside, final_path(&tree.root(), transfer_id, 1)).unwrap();

    let err = hasher(&store)
        .compute(&request(transfer_id, 2, n))
        .expect_err("symlink final");
    match err {
        FullArtifactError::RequiredChunkUnreadable {
            chunk_index,
            source,
            ..
        } => {
            assert_eq!(chunk_index, 1);
            assert!(matches!(
                source,
                StorageError::FinalizedChunkNotRegular { .. }
            ));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// ----------------------------------------------------------------------------
// authority separation
// ----------------------------------------------------------------------------

/// §43 / §15 / §17 — a finalized chunk whose bytes are changed *without*
/// changing its length is not a D2 error: D2 recomputes over the current
/// bytes and returns a **different** digest. It does not reuse D1's earlier
/// per-chunk hash and it does not decide `VerificationFailed`; `bamepd`
/// owns the eventual mismatch verdict.
#[test]
fn same_size_byte_mutation_yields_a_new_digest_not_an_error() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 8u32;
    put_chunk(&store, transfer_id, 0, b"ORIGINAL", u64::from(n));
    put_chunk(&store, transfer_id, 1, b"final-01", u64::from(n));

    let clean = hasher(&store)
        .compute(&request(transfer_id, 2, n))
        .expect("clean compute");
    assert_eq!(clean.digest, sha256(b"ORIGINALfinal-01"));

    // Overwrite chunk 0 in place with different bytes of identical length.
    // D1 final files are owner-writable (`0600`), so no permission juggling.
    let path0 = final_path(&tree.root(), transfer_id, 0);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path0)
            .unwrap();
        f.write_all(b"MUTATED!").unwrap(); // exactly 8 bytes, same length
    }

    let mutated = hasher(&store)
        .compute(&request(transfer_id, 2, n))
        .expect("mutation is not a D2 error");
    assert_eq!(mutated.total_size, clean.total_size);
    assert_ne!(mutated.digest, clean.digest);
    assert_eq!(mutated.digest, sha256(b"MUTATED!final-01"));
}

// ----------------------------------------------------------------------------
// restart-stable input
// ----------------------------------------------------------------------------

/// §45 — reconstruction depends only on the restart-stable finalized files:
/// drop every store/hasher object, rebuild the store on the same root, and
/// the digest is unchanged.
#[test]
fn reconstruction_survives_dropping_and_rebuilding_the_store() {
    let tree = TempTree::new();
    let transfer_id = Uuid::new_v4();
    let n = 4096u32;

    let clean = {
        let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
        put_chunk(&store, transfer_id, 0, &vec![1u8; n as usize], u64::from(n));
        put_chunk(&store, transfer_id, 1, b"tail-bytes", u64::from(n));
        hasher(&store)
            .compute(&request(transfer_id, 2, n))
            .expect("compute")
        // store + hasher dropped here
    };

    let store = FilesystemChunkStore::initialize(tree.root()).expect("re-init");
    let again = hasher(&store)
        .compute(&request(transfer_id, 2, n))
        .expect("recompute after restart");
    assert_eq!(again.digest, clean.digest);
    assert_eq!(again.total_size, clean.total_size);
}

// ----------------------------------------------------------------------------
// concurrency
// ----------------------------------------------------------------------------

/// §46 — concurrent read-only reconstructions over the same stable files
/// produce identical results, no global serialization, no sleeps.
#[test]
fn concurrent_reconstructions_agree() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 4096u32;
    put_chunk(
        &store,
        transfer_id,
        0,
        &vec![0x5Au8; n as usize],
        u64::from(n),
    );
    put_chunk(
        &store,
        transfer_id,
        1,
        &vec![0xA5u8; n as usize],
        u64::from(n),
    );
    put_chunk(&store, transfer_id, 2, b"partial-final", u64::from(n));

    let expected = hasher(&store)
        .compute(&request(transfer_id, 3, n))
        .expect("baseline");

    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let h = FullArtifactHasher::new(store);
                barrier.wait();
                h.compute(&request(transfer_id, 3, n))
            })
        })
        .collect();

    for handle in handles {
        let got = handle.join().unwrap().expect("concurrent compute");
        assert_eq!(got.digest, expected.digest);
        assert_eq!(got.total_size, expected.total_size);
    }
}

// ----------------------------------------------------------------------------
// async spawn_blocking wrapper
// ----------------------------------------------------------------------------

/// §23 / §48 — the async wrapper runs the whole synchronous core inside one
/// `spawn_blocking` (its body is literally
/// `spawn_blocking(move || self.compute(&request))`) and returns exactly
/// what the sync core returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compute_blocking_matches_the_sync_core() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();

    let n = 4096u32;
    put_chunk(
        &store,
        transfer_id,
        0,
        &vec![0x11u8; n as usize],
        u64::from(n),
    );
    put_chunk(&store, transfer_id, 1, b"final", u64::from(n));

    let req = request(transfer_id, 2, n);
    let sync = hasher(&store).compute(&req).expect("sync");
    let via_blocking = hasher(&store)
        .compute_blocking(req)
        .await
        .expect("blocking");

    assert_eq!(sync, via_blocking);
}

/// The blocking scan is offloaded even on a current-thread runtime — it does
/// not run inline on the reactor.
#[tokio::test(flavor = "current_thread")]
async fn compute_blocking_offloads_on_a_current_thread_runtime() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("init");
    let transfer_id = Uuid::new_v4();
    put_chunk(&store, transfer_id, 0, b"abc", 8);

    let result = hasher(&store)
        .compute_blocking(request(transfer_id, 1, 8))
        .await
        .expect("blocking on current-thread runtime");
    assert_eq!(result.digest, sha256(b"abc"));
    assert_eq!(result.total_size, 3);
}
