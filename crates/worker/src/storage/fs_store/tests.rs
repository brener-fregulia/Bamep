//! Phase D1 storage tests, grouped: unit (naming/shape), filesystem
//! (stage/finalize/read, idempotency, cleanup, symlink, permissions,
//! missing/empty/oversize), concurrency (first-writer), and restart
//! (finalized file survives store reconstruction; orphan final carries no
//! acceptance state).

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Barrier};

use super::*;

/// Isolated temporary storage tree, removed on drop. Mirrors the manual
/// temp-dir + `Drop` pattern already used by this crate's TLS-identity and
/// reconnect tests (no `tempfile` dependency introduced).
struct TempTree {
    base: PathBuf,
}

impl TempTree {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("bamep-chunkstore-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).expect("create temp base");
        Self { base }
    }

    /// A not-yet-created storage root inside the tree (deep enough to be a
    /// safe absolute location).
    fn root(&self) -> PathBuf {
        self.base.join("chunk-storage-root")
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn final_path_for(root: &Path, transfer_id: Uuid, chunk_index: u64) -> PathBuf {
    root.join(TRANSFERS_DIR)
        .join(transfer_id.as_hyphenated().to_string())
        .join(CHUNKS_DIR)
        .join(format!("{chunk_index}{FINAL_SUFFIX}"))
}

fn staging_dir_for(root: &Path, transfer_id: Uuid) -> PathBuf {
    root.join(TRANSFERS_DIR)
        .join(transfer_id.as_hyphenated().to_string())
        .join(CHUNKS_DIR)
        .join(STAGING_DIR)
}

fn stage_and_finalize(
    store: &FilesystemChunkStore,
    transfer_id: Uuid,
    chunk_index: u64,
    max_size: u64,
    bytes: &[u8],
) -> Result<FinalizedChunk, StorageError> {
    let mut staging = store.begin_stage(transfer_id, chunk_index, max_size)?;
    staging.write(bytes)?;
    staging.finalize()
}

// ----------------------------------------------------------------------------
// unit: staging-name recognition and root-shape validation
// ----------------------------------------------------------------------------

#[test]
fn recognizes_only_worker_generated_staging_names() {
    let token = "0123456789abcdef0123456789abcdef";
    assert!(is_recognized_staging_name(&format!("0.{token}.part")));
    assert!(is_recognized_staging_name(&format!("4096.{token}.part")));

    assert!(!is_recognized_staging_name(&format!("{token}.part"))); // no index
    assert!(!is_recognized_staging_name(&format!("0.{token}.chunk"))); // wrong ext
    assert!(!is_recognized_staging_name("0.short.part")); // token too short
    assert!(!is_recognized_staging_name(&format!("x.{token}.part"))); // non-numeric index
    assert!(!is_recognized_staging_name(&format!(
        "0.{}.part",
        "0123456789ABCDEF0123456789ABCDEF"
    ))); // uppercase hex
    assert!(!is_recognized_staging_name(".hidden"));
    assert!(!is_recognized_staging_name("0.chunk"));
}

#[test]
fn generated_staging_token_is_32_lowercase_hex() {
    let token = worker_random_token();
    assert_eq!(token.len(), 32);
    assert!(token
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
}

#[test]
fn root_shape_validation_rejects_unsafe_and_relative_roots() {
    assert!(matches!(
        validate_root_shape(Path::new("relative/dir")),
        Err(StorageError::RootNotAbsolute { .. })
    ));
    assert!(matches!(
        validate_root_shape(Path::new("")),
        Err(StorageError::RootNotAbsolute { .. })
    ));
    assert!(matches!(
        validate_root_shape(Path::new("/")),
        Err(StorageError::RootUnsafe { .. })
    ));
    assert!(matches!(
        validate_root_shape(Path::new("/var/lib/../lib/bamep")),
        Err(StorageError::RootUnsafe { .. })
    ));
    assert!(validate_root_shape(Path::new("/var/lib/bamep-worker/chunks")).is_ok());
}

// ----------------------------------------------------------------------------
// filesystem: initialize
// ----------------------------------------------------------------------------

#[test]
fn initialize_creates_the_root_and_transfers_dir_owner_only() {
    let tree = TempTree::new();
    let root = tree.root();
    FilesystemChunkStore::initialize(&root).expect("initialize");

    for dir in [root.clone(), root.join(TRANSFERS_DIR)] {
        let meta = fs::symlink_metadata(&dir).expect("dir exists");
        assert!(meta.file_type().is_dir());
        assert_eq!(meta.permissions().mode() & 0o077, 0, "{dir:?} owner-only");
    }
}

#[test]
fn initialize_rejects_a_regular_file_root() {
    let tree = TempTree::new();
    let file_root = tree.base.join("i-am-a-file");
    fs::write(&file_root, b"x").unwrap();
    assert!(matches!(
        FilesystemChunkStore::initialize(&file_root),
        Err(StorageError::UnsafeDirectory { .. })
    ));
}

#[test]
fn initialize_rejects_a_symlink_root() {
    let tree = TempTree::new();
    let real = tree.base.join("real-dir");
    fs::create_dir(&real).unwrap();
    let link = tree.base.join("link-root");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(matches!(
        FilesystemChunkStore::initialize(&link),
        Err(StorageError::UnsafeDirectory { .. })
    ));
}

#[test]
fn initialize_rejects_a_relative_root() {
    assert!(matches!(
        FilesystemChunkStore::initialize("still/relative"),
        Err(StorageError::RootNotAbsolute { .. })
    ));
}

#[test]
fn initialize_rejects_the_filesystem_root() {
    assert!(matches!(
        FilesystemChunkStore::initialize("/"),
        Err(StorageError::RootUnsafe { .. })
    ));
}

// ----------------------------------------------------------------------------
// filesystem: basic stage / finalize / read
// ----------------------------------------------------------------------------

#[test]
fn stages_across_multiple_writes_finalizes_and_reads_back_exact_bytes() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    let mut staging = store
        .begin_stage(transfer_id, 0, 4096)
        .expect("begin stage");
    for frame in [b"the quick ".as_slice(), b"brown fox ", b"jumps"] {
        staging.write(frame).expect("write frame");
    }
    let finalized = staging.finalize().expect("finalize");

    let expected = b"the quick brown fox jumps";
    assert_eq!(finalized.disposition, FinalizedDisposition::Installed);
    assert_eq!(finalized.size, expected.len() as u64);
    assert_eq!(finalized.transfer_id, transfer_id);
    assert_eq!(finalized.chunk_index, 0);

    // Deterministic final exists; staging directory is empty.
    let final_path = final_path_for(&root, transfer_id, 0);
    assert!(final_path.is_file());
    assert_eq!(
        fs::read_dir(staging_dir_for(&root, transfer_id))
            .unwrap()
            .count(),
        0,
        "staging file removed after finalize"
    );

    // Reopen through the storage API and read the exact original bytes.
    let mut reader = store.open_final(transfer_id, 0).expect("open final");
    let mut round_tripped = Vec::new();
    reader.read_to_end(&mut round_tripped).expect("read final");
    assert_eq!(round_tripped, expected);

    // inspect_final agrees.
    let facts = store.inspect_final(transfer_id, 0).expect("inspect");
    assert_eq!(facts.size, expected.len() as u64);
    assert_eq!(facts.digest, finalized.digest);
}

#[test]
fn finalized_digest_matches_the_known_sha256_abc_vector() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("initialize");
    let transfer_id = Uuid::new_v4();

    let mut staging = store.begin_stage(transfer_id, 0, 16).expect("begin stage");
    staging.write(b"a").unwrap();
    staging.write(b"b").unwrap();
    staging.write(b"c").unwrap();
    let finalized = staging.finalize().expect("finalize");

    assert_eq!(finalized.size, 3);
    assert_eq!(
        finalized.digest.to_base64url_no_pad(),
        "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0"
    );
}

#[test]
fn worker_created_chunk_files_and_dirs_are_owner_only() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();
    stage_and_finalize(&store, transfer_id, 0, 4096, b"perm-check").expect("finalize");

    let transfer_dir = root
        .join(TRANSFERS_DIR)
        .join(transfer_id.as_hyphenated().to_string());
    for dir in [
        transfer_dir.clone(),
        transfer_dir.join(CHUNKS_DIR),
        transfer_dir.join(CHUNKS_DIR).join(STAGING_DIR),
    ] {
        let meta = fs::symlink_metadata(&dir).expect("dir exists");
        assert_eq!(meta.permissions().mode() & 0o077, 0, "{dir:?} owner-only");
    }
    let meta = fs::symlink_metadata(final_path_for(&root, transfer_id, 0)).unwrap();
    assert_eq!(
        meta.permissions().mode() & 0o077,
        0,
        "final chunk file owner-only"
    );
}

// ----------------------------------------------------------------------------
// filesystem: oversize
// ----------------------------------------------------------------------------

#[test]
fn a_single_oversize_write_is_rejected_and_produces_no_final() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    let mut staging = store.begin_stage(transfer_id, 0, 8).expect("begin stage");
    assert!(matches!(
        staging.write(b"123456789"),
        Err(StorageError::Oversize { max: 8 })
    ));
    assert!(matches!(
        staging.finalize(),
        Err(StorageError::Oversize { .. })
    ));

    assert!(matches!(
        store.open_final(transfer_id, 0),
        Err(StorageError::FinalizedChunkNotFound { .. })
    ));
    assert_eq!(
        fs::read_dir(staging_dir_for(&root, transfer_id))
            .unwrap()
            .count(),
        0,
        "staging discarded on oversize"
    );
}

#[test]
fn oversize_crossed_incrementally_across_writes_is_rejected() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    let mut staging = store.begin_stage(transfer_id, 1, 10).expect("begin stage");
    staging.write(b"12345").expect("first write within bound");
    assert!(matches!(
        staging.write(b"678901"),
        Err(StorageError::Oversize { max: 10 })
    ));
    // Further writes keep failing closed.
    assert!(matches!(
        staging.write(b"x"),
        Err(StorageError::Oversize { .. })
    ));
    assert!(matches!(
        staging.finalize(),
        Err(StorageError::Oversize { .. })
    ));
    assert!(!final_path_for(&root, transfer_id, 1).exists());
}

#[test]
fn exactly_the_maximum_size_is_accepted() {
    let tree = TempTree::new();
    let store = FilesystemChunkStore::initialize(tree.root()).expect("initialize");
    let transfer_id = Uuid::new_v4();

    let finalized = stage_and_finalize(&store, transfer_id, 0, 8, b"12345678").expect("finalize");
    assert_eq!(finalized.size, 8);
}

// ----------------------------------------------------------------------------
// filesystem: empty chunk
// ----------------------------------------------------------------------------

#[test]
fn finalizing_without_bytes_is_an_empty_chunk_error() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    let staging = store.begin_stage(transfer_id, 0, 16).expect("begin stage");
    assert!(matches!(staging.finalize(), Err(StorageError::EmptyChunk)));

    // Writing only empty slices is still empty.
    let mut staging = store.begin_stage(transfer_id, 1, 16).expect("begin stage");
    staging.write(b"").unwrap();
    assert!(matches!(staging.finalize(), Err(StorageError::EmptyChunk)));

    assert!(!final_path_for(&root, transfer_id, 0).exists());
    assert!(!final_path_for(&root, transfer_id, 1).exists());
}

// ----------------------------------------------------------------------------
// filesystem: idempotent / conflicting re-finalization
// ----------------------------------------------------------------------------

#[test]
fn re_finalizing_identical_bytes_reports_already_present_and_preserves_the_file() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();
    let payload = b"payload-A-identical";

    let first = stage_and_finalize(&store, transfer_id, 0, 64, payload).expect("first finalize");
    assert_eq!(first.disposition, FinalizedDisposition::Installed);

    let final_path = final_path_for(&root, transfer_id, 0);
    let inode_before = fs::symlink_metadata(&final_path).unwrap().ino();

    let second = stage_and_finalize(&store, transfer_id, 0, 64, payload).expect("second finalize");
    assert_eq!(second.disposition, FinalizedDisposition::AlreadyPresent);
    assert_eq!(second.size, first.size);
    assert_eq!(second.digest, first.digest);

    assert_eq!(
        fs::symlink_metadata(&final_path).unwrap().ino(),
        inode_before,
        "original final file preserved (same inode)"
    );
    assert_eq!(
        fs::read_dir(staging_dir_for(&root, transfer_id))
            .unwrap()
            .count(),
        0,
        "second staging discarded"
    );
}

#[test]
fn re_finalizing_different_bytes_conflicts_and_leaves_the_original_untouched() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    stage_and_finalize(&store, transfer_id, 0, 64, b"the-original-bytes").expect("first finalize");
    let final_path = final_path_for(&root, transfer_id, 0);
    let original = fs::read(&final_path).unwrap();

    let err = stage_and_finalize(&store, transfer_id, 0, 64, b"different bytes entirely")
        .expect_err("conflict");
    assert!(matches!(err, StorageError::FinalizedChunkConflict { .. }));

    assert_eq!(fs::read(&final_path).unwrap(), original);
    assert_eq!(fs::read(&final_path).unwrap(), b"the-original-bytes");
    assert_eq!(
        fs::read_dir(staging_dir_for(&root, transfer_id))
            .unwrap()
            .count(),
        0,
        "conflicting staging discarded"
    );
}

// ----------------------------------------------------------------------------
// filesystem: symlink safety
// ----------------------------------------------------------------------------

#[test]
fn a_symlink_where_a_transfer_directory_is_expected_is_rejected() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    let transfer_dir = root
        .join(TRANSFERS_DIR)
        .join(transfer_id.as_hyphenated().to_string());
    let elsewhere = tree.base.join("attacker-controlled");
    fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &transfer_dir).unwrap();

    assert!(matches!(
        store.begin_stage(transfer_id, 0, 4096),
        Err(StorageError::UnsafeDirectory { .. })
    ));
    assert!(
        !elsewhere.join(CHUNKS_DIR).exists(),
        "nothing written through the symlink"
    );
}

#[test]
fn a_symlink_where_a_final_chunk_is_expected_is_refused_for_read_and_finalize() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    // Create the chunks directory legitimately.
    stage_and_finalize(&store, transfer_id, 9, 64, b"real").expect("seed chunk");

    let final0 = final_path_for(&root, transfer_id, 0);
    let outside = tree.base.join("outside-target");
    fs::write(&outside, b"outside-bytes").unwrap();
    std::os::unix::fs::symlink(&outside, &final0).unwrap();

    assert!(matches!(
        store.open_final(transfer_id, 0),
        Err(StorageError::FinalizedChunkNotRegular { .. })
    ));
    assert!(matches!(
        store.inspect_final(transfer_id, 0),
        Err(StorageError::FinalizedChunkNotRegular { .. })
    ));

    // Finalizing into that slot must not follow the symlink or clobber the
    // outside file.
    let err = stage_and_finalize(&store, transfer_id, 0, 64, b"nope").expect_err("refused");
    assert!(matches!(err, StorageError::FinalizedChunkNotRegular { .. }));
    assert_eq!(fs::read(&outside).unwrap(), b"outside-bytes");
}

// ----------------------------------------------------------------------------
// filesystem: missing final
// ----------------------------------------------------------------------------

#[test]
fn opening_or_inspecting_a_missing_final_returns_not_found_without_side_effects() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    assert!(matches!(
        store.open_final(transfer_id, 0),
        Err(StorageError::FinalizedChunkNotFound { .. })
    ));
    assert!(matches!(
        store.inspect_final(transfer_id, 7),
        Err(StorageError::FinalizedChunkNotFound { .. })
    ));
    assert!(
        !root
            .join(TRANSFERS_DIR)
            .join(transfer_id.as_hyphenated().to_string())
            .exists(),
        "no directory created as a side effect of a missing-final lookup"
    );
}

// ----------------------------------------------------------------------------
// filesystem: startup temp cleanup
// ----------------------------------------------------------------------------

#[test]
fn reinitialization_removes_recognized_staging_but_preserves_finals_and_unknown_files() {
    let tree = TempTree::new();
    let root = tree.root();
    let transfer_id = Uuid::new_v4();

    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    stage_and_finalize(&store, transfer_id, 0, 64, b"final-bytes").expect("finalize");

    let staging_dir = staging_dir_for(&root, transfer_id);
    let recognized = staging_dir.join(format!("7.{}.part", "0".repeat(32)));
    fs::write(&recognized, b"half-written staging").unwrap();
    let unknown = root
        .join(TRANSFERS_DIR)
        .join(transfer_id.as_hyphenated().to_string())
        .join(CHUNKS_DIR)
        .join("operator-note.txt");
    fs::write(&unknown, b"not ours").unwrap();

    drop(store);
    let store = FilesystemChunkStore::initialize(&root).expect("re-initialize");

    assert!(!recognized.exists(), "recognized staging file removed");
    assert!(unknown.exists(), "unrelated file preserved");

    let mut bytes = Vec::new();
    store
        .open_final(transfer_id, 0)
        .expect("final preserved")
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"final-bytes");
}

#[test]
fn explicit_discard_removes_the_staging_file_and_produces_no_final() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    let mut staging = store.begin_stage(transfer_id, 2, 64).expect("begin stage");
    staging.write(b"partial").unwrap();
    staging.discard();

    assert_eq!(
        fs::read_dir(staging_dir_for(&root, transfer_id))
            .unwrap()
            .count(),
        0
    );
    assert!(!final_path_for(&root, transfer_id, 2).exists());
}

#[test]
fn dropping_a_staging_handle_without_finalizing_removes_its_temp_file() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = FilesystemChunkStore::initialize(&root).expect("initialize");
    let transfer_id = Uuid::new_v4();

    {
        let mut staging = store.begin_stage(transfer_id, 3, 64).expect("begin stage");
        staging.write(b"abandoned").unwrap();
    }
    assert_eq!(
        fs::read_dir(staging_dir_for(&root, transfer_id))
            .unwrap()
            .count(),
        0
    );
}

// ----------------------------------------------------------------------------
// concurrency: race-safe first writer
// ----------------------------------------------------------------------------

#[test]
fn concurrent_identical_finalizations_converge_to_one_installed_one_already_present() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = Arc::new(FilesystemChunkStore::initialize(&root).expect("initialize"));
    let transfer_id = Uuid::new_v4();
    let payload = b"concurrent-identical-payload".to_vec();

    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let payload = payload.clone();
            std::thread::spawn(move || {
                let mut staging = store
                    .begin_stage(transfer_id, 0, 4096)
                    .expect("begin stage");
                barrier.wait();
                staging.write(&payload).expect("write");
                staging.finalize()
            })
        })
        .collect();

    let dispositions: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().unwrap().expect("both finalize ok").disposition)
        .collect();
    assert!(dispositions.contains(&FinalizedDisposition::Installed));
    assert!(dispositions.contains(&FinalizedDisposition::AlreadyPresent));

    let mut bytes = Vec::new();
    store
        .open_final(transfer_id, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, payload);
}

#[test]
fn concurrent_different_byte_finalizations_yield_one_winner_and_one_conflict() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = Arc::new(FilesystemChunkStore::initialize(&root).expect("initialize"));
    let transfer_id = Uuid::new_v4();

    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [b"aaaaaaaaAAAA".to_vec(), b"bbbbbbbbBBBB".to_vec()]
        .into_iter()
        .map(|payload| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut staging = store
                    .begin_stage(transfer_id, 0, 4096)
                    .expect("begin stage");
                barrier.wait();
                staging.write(&payload).expect("write");
                (payload, staging.finalize())
            })
        })
        .collect();

    let mut winner_bytes = None;
    let mut conflicts = 0;
    for handle in handles {
        let (payload, result) = handle.join().unwrap();
        match result {
            Ok(finalized) => {
                assert_eq!(finalized.disposition, FinalizedDisposition::Installed);
                assert!(
                    winner_bytes.replace(payload).is_none(),
                    "exactly one winner"
                );
            }
            Err(StorageError::FinalizedChunkConflict { .. }) => conflicts += 1,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(conflicts, 1);

    let winner_bytes = winner_bytes.expect("one winner");
    let mut on_disk = Vec::new();
    store
        .open_final(transfer_id, 0)
        .unwrap()
        .read_to_end(&mut on_disk)
        .unwrap();
    assert_eq!(on_disk, winner_bytes, "winner's bytes are unchanged");
}

/// The synchronous, blocking storage API is designed to be driven from
/// `tokio::task::spawn_blocking` — `StagingChunk` is `Send`, so a Phase E
/// async handler can move it in and out across `await` points between
/// network body frames without ever blocking the reactor on a large write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_staging_handle_can_be_moved_through_spawn_blocking_between_frames() {
    let tree = TempTree::new();
    let root = tree.root();
    let store = Arc::new(FilesystemChunkStore::initialize(&root).expect("initialize"));
    let transfer_id = Uuid::new_v4();

    let opening = {
        let store = Arc::clone(&store);
        tokio::task::spawn_blocking(move || store.begin_stage(transfer_id, 0, 4096))
    };
    let mut staging = opening.await.unwrap().expect("begin stage");

    for frame in [
        b"frame-one-".to_vec(),
        b"frame-two-".to_vec(),
        b"frame-3".to_vec(),
    ] {
        staging = tokio::task::spawn_blocking(move || staging.write(&frame).map(|()| staging))
            .await
            .unwrap()
            .expect("write frame");
    }

    let finalized = tokio::task::spawn_blocking(move || staging.finalize())
        .await
        .unwrap()
        .expect("finalize");
    assert_eq!(finalized.size, "frame-one-frame-two-frame-3".len() as u64);

    let mut bytes = Vec::new();
    store
        .open_final(transfer_id, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"frame-one-frame-two-frame-3");
}

// ----------------------------------------------------------------------------
// restart: finalized file survives store reconstruction; orphan final is
// not authority
// ----------------------------------------------------------------------------

#[test]
fn a_finalized_chunk_survives_dropping_and_rebuilding_the_store() {
    let tree = TempTree::new();
    let root = tree.root();
    let transfer_id = Uuid::new_v4();
    let payload = b"survives-process-restart-model".to_vec();

    let digest = {
        let store = FilesystemChunkStore::initialize(&root).expect("initialize");
        stage_and_finalize(&store, transfer_id, 4, 4096, &payload)
            .expect("finalize")
            .digest
        // store dropped here
    };

    let store = FilesystemChunkStore::initialize(&root).expect("re-initialize");
    let facts = store
        .inspect_final(transfer_id, 4)
        .expect("inspect after restart");
    assert_eq!(facts.size, payload.len() as u64);
    assert_eq!(
        facts.digest, digest,
        "mechanical digest unchanged across restart"
    );

    let mut bytes = Vec::new();
    store
        .open_final(transfer_id, 4)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, payload);
}

/// Crash-window model: the Worker finalized a chunk and no `bamepd`
/// acceptance exists (Phase D1 cannot send one). After restart the file is
/// still there and is still just bytes — there is no storage API or returned
/// value that calls it held / accepted / committed. `FinalizedChunk` /
/// `StoredChunkFacts` expose only mechanical `size` + `digest`.
#[test]
fn an_orphan_finalized_file_is_restart_stable_residue_not_acceptance_state() {
    let tree = TempTree::new();
    let root = tree.root();
    let transfer_id = Uuid::new_v4();

    {
        let store = FilesystemChunkStore::initialize(&root).expect("initialize");
        stage_and_finalize(&store, transfer_id, 0, 4096, b"orphan-final").expect("finalize");
    }

    let store = FilesystemChunkStore::initialize(&root).expect("re-initialize");
    // The bytes are still readable...
    let facts = store.inspect_final(transfer_id, 0).expect("still present");
    assert_eq!(facts.size, 12);
    // ...and that is the entire story the storage layer tells: no acceptance
    // bit anywhere in the returned facts.
    let _: StoredChunkFacts = facts;
}
