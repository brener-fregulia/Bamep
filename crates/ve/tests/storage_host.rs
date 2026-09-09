//! Host-dependent storage proof (Issue #69 §22-§24). **Opt-in**: does nothing
//! unless `BAMEP_VE_STORAGE_HOST_TEST=1` is set, and skips (does not fail) if
//! `qemu-img` / `qemu-io` are unavailable, so an ordinary `cargo test` and CI
//! never need QEMU tooling. Run on a prepared Linux reference host:
//!
//! ```text
//! BAMEP_VE_STORAGE_HOST_TEST=1 cargo test -p bamep-ve --test storage_host -- --nocapture
//! ```
//!
//! It proves, with real images:
//!
//! 1. an 80 GiB logical base allocates only a tiny fraction on the host;
//! 2. the disposable overlay is backed by the base with an explicit raw
//!    backing format;
//! 3. a write through the overlay does not change the base's integrity value;
//! 4. `reset_system_storage` discards the write and re-derives from the same
//!    base — twice — and never touches the source fixture;
//! 5. the system overlay and source fixture attach independently to a real
//!    VM (QMP `query-block`);
//! 6. `reset_system_storage` is refused while the VM runs;
//! 7. `destroy_instance_storage` removes only this BVE's disposables — the
//!    base, an unrelated sentinel, and another BVE's directory survive.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use bamep_ve::{
    check_qemu_img_binary, destroy_instance_storage, detect_host_prerequisites, ensure_system_base,
    fnv1a_64, prepare_instance, reset_system_storage, BveId, BveRuntime, BveStorageLayout,
    BveStorageRoot, Firmware, LifecycleState, QmpConnection, RuntimeError, RuntimeRoot,
    SourceDiskSpec, SystemBaseSpec, DEFAULT_SYSTEM_BASE_BYTES,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("bamep-ve-storage-host-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn qemu_io_available() -> bool {
    Command::new("qemu-io")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn qemu_io(args: &[&str]) {
    let out = Command::new("qemu-io").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "qemu-io {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn qemu_img_json(path: &Path) -> serde_json::Value {
    let out = Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

fn hash_file(path: &Path) -> u64 {
    fnv1a_64(&fs::read(path).unwrap())
}

fn allocated_bytes(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

#[test]
fn deterministic_storage_and_reproducible_reset() {
    if std::env::var_os("BAMEP_VE_STORAGE_HOST_TEST").is_none() {
        eprintln!("skipping storage host proof: set BAMEP_VE_STORAGE_HOST_TEST=1 to run it");
        return;
    }
    if check_qemu_img_binary().is_err() || !qemu_io_available() {
        eprintln!("skipping storage host proof: qemu-img and qemu-io are required");
        return;
    }

    // ---- 1. 80 GiB logical base is sparse on the host --------------------
    let big = TempDir::new();
    let big_root = BveStorageRoot::new(big.0.join("storage")).unwrap();
    let base_80g = ensure_system_base(&big_root, &SystemBaseSpec::default()).unwrap();
    let info = qemu_img_json(&base_80g);
    assert_eq!(
        info["virtual-size"].as_u64().unwrap(),
        DEFAULT_SYSTEM_BASE_BYTES,
        "logical capacity is the 80 GiB profile"
    );
    let allocated = allocated_bytes(&base_80g);
    assert!(
        allocated < 16 * 1024 * 1024,
        "80 GiB base allocated {allocated} host bytes; expected a tiny fraction (sparse)"
    );
    eprintln!("80 GiB base: virtual {DEFAULT_SYSTEM_BASE_BYTES}, allocated {allocated}");

    // ---- 2/3. small base: overlay backing + base immutability -----------
    let temp = TempDir::new();
    let storage_root = BveStorageRoot::new(temp.0.join("storage")).unwrap();
    let runtime_root = RuntimeRoot::new(temp.0.join("control"));
    let id = BveId::new("bve-storage-host").unwrap();

    let base = ensure_system_base(
        &storage_root,
        &SystemBaseSpec::new(256 * 1024 * 1024).unwrap(),
    )
    .unwrap();
    let prepared = prepare_instance(
        &storage_root,
        &id,
        Some(&SourceDiskSpec::new(1024 * 1024).unwrap()),
    )
    .unwrap();
    let layout = prepared.layout().clone();

    let overlay_info = qemu_img_json(layout.system_overlay());
    assert!(
        overlay_info["backing-filename"]
            .as_str()
            .unwrap()
            .ends_with("system-base.raw"),
        "overlay must be backed by the base"
    );
    assert_eq!(
        overlay_info["backing-filename-format"].as_str().unwrap(),
        "raw",
        "backing format must be explicit raw"
    );

    let base_before = hash_file(&base);
    let source_before = hash_file(layout.source_disk());
    qemu_io(&[
        "-f",
        "qcow2",
        "-c",
        "write -P 0x5a 1M 64k",
        layout.system_overlay().to_str().unwrap(),
    ]);
    assert_eq!(
        hash_file(&base),
        base_before,
        "a write through the overlay must not mutate the base"
    );

    // ---- 4. reproducible reset (twice), source untouched ---------------
    for round in 1..=2 {
        reset_system_storage(&prepared).unwrap();
        qemu_io(&[
            "-f",
            "qcow2",
            "-c",
            "read -P 0x00 1M 64k",
            layout.system_overlay().to_str().unwrap(),
        ]);
        assert_eq!(
            hash_file(&base),
            base_before,
            "reset round {round}: base still unchanged"
        );
        assert_eq!(
            hash_file(layout.source_disk()),
            source_before,
            "reset round {round}: source fixture untouched"
        );
    }

    // ---- 5/6. real VM: independent attachment + reset refused while up -
    let prerequisites = detect_host_prerequisites().unwrap();
    let definition = prepared.define_bve(id, 1, 256, Firmware::Default).unwrap();
    let mut runtime = BveRuntime::create(&runtime_root, definition, prepared.clone()).unwrap();
    runtime.start(&prerequisites).unwrap();
    assert_eq!(runtime.observe().unwrap(), LifecycleState::Running);

    let mut qmp = QmpConnection::connect(runtime.qmp_socket()).unwrap();
    let blocks = qmp.inserted_block_devices().unwrap();
    drop(qmp);
    assert!(
        blocks.iter().any(|d| d == "system") && blocks.iter().any(|d| d == "source"),
        "both disks attach independently; query-block reported {blocks:?}"
    );

    assert!(
        matches!(
            runtime.reset_system_storage(),
            Err(RuntimeError::StorageResetWhileRunning)
        ),
        "storage reset must fail closed while the VM runs"
    );

    runtime.reset().unwrap();
    assert_eq!(runtime.observe().unwrap(), LifecycleState::Running);
    runtime.stop().unwrap();
    assert_eq!(runtime.observe().unwrap(), LifecycleState::Stopped);

    // storage reset now allowed
    runtime.reset_system_storage().unwrap();

    let control_dir = runtime.instance_dir().to_path_buf();
    runtime.destroy().unwrap();
    assert!(!control_dir.exists());

    // ---- 7. scoped disposal ------------------------------------------
    let sentinel = storage_root.path().join("SENTINEL");
    fs::write(&sentinel, b"keep").unwrap();
    let other = BveStorageLayout::for_bve(&storage_root, &BveId::new("bve-neighbour").unwrap());
    fs::create_dir_all(other.instance_dir()).unwrap();
    fs::write(other.system_overlay(), b"keep").unwrap();

    destroy_instance_storage(prepared).unwrap();

    assert!(
        !layout.instance_dir().exists(),
        "this BVE's disposables removed"
    );
    assert!(base.exists(), "shared base survives disposal");
    assert!(sentinel.exists(), "unrelated sentinel survives");
    assert!(other.system_overlay().exists(), "another BVE survives");

    eprintln!("storage host proof: all checks passed");
}
