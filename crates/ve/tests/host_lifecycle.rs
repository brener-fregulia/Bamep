//! Host-dependent BVE lifecycle proof (Issue #67, kept green through #69).
//!
//! The documented manual entrypoint. **Opt-in**: it does nothing unless
//! `BAMEP_BVE_HOST_TEST=1` is set, so an ordinary `cargo test` never requires
//! KVM, `qemu-system-x86_64`, or `qemu-img`. Run it on a prepared Linux
//! reference host with:
//!
//! ```text
//! BAMEP_BVE_HOST_TEST=1 cargo test -p bamep-ve --test host_lifecycle -- --nocapture
//! ```
//!
//! It boots no guest OS: a fresh disposable QCOW2 system overlay over a small
//! throwaway RAW base (the VM sits in firmware). It proves
//! `create -> start -> observe(Running) -> reset -> observe(Running) ->
//! stop -> observe(Stopped) -> destroy`, then disposes the disposable
//! storage. It touches no PXE, no TAP/bridge, no provisioning network, no
//! Server, and no Simulator.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use bamep_ve::{
    check_qemu_img_binary, destroy_instance_storage, detect_host_prerequisites, ensure_system_base,
    prepare_instance, BveId, BveRuntime, BveStorageRoot, Firmware, LifecycleState, RuntimeRoot,
    SystemBaseSpec,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("bamep-ve-host-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

#[test]
fn one_real_bve_full_lifecycle() {
    if std::env::var_os("BAMEP_BVE_HOST_TEST").is_none() {
        eprintln!(
            "skipping host lifecycle: set BAMEP_BVE_HOST_TEST=1 on a KVM-capable Linux host \
             (qemu-system-x86_64 + qemu-img on PATH, /dev/kvm read/write) to run it"
        );
        return;
    }

    let prerequisites = detect_host_prerequisites()
        .expect("host prerequisites (qemu-system-x86_64 + usable /dev/kvm) must be satisfied");
    check_qemu_img_binary().expect("qemu-img must be available to prepare storage");
    eprintln!("qemu: {}", prerequisites.qemu_version_line);

    let temp = TempDir::new();
    let runtime_root = RuntimeRoot::new(temp.0.join("control"));
    let storage_root = BveStorageRoot::new(temp.0.join("storage")).unwrap();
    let id = BveId::new("bve-host-lifecycle").unwrap();

    // A small throwaway base is enough to prove the lifecycle.
    ensure_system_base(
        &storage_root,
        &SystemBaseSpec::new(512 * 1024 * 1024).unwrap(),
    )
    .unwrap();
    let prepared = prepare_instance(&storage_root, &id, None).unwrap();
    let definition = prepared.define_bve(id, 1, 256, Firmware::Default).unwrap();

    let mut runtime =
        BveRuntime::create(&runtime_root, definition, prepared.clone()).expect("create");

    runtime.start(&prerequisites).expect("start");
    assert_eq!(
        runtime.observe().expect("observe after start"),
        LifecycleState::Running
    );

    runtime.reset().expect("reset");
    assert_eq!(
        runtime.observe().expect("observe after reset"),
        LifecycleState::Running
    );

    runtime.stop().expect("stop");
    assert_eq!(
        runtime.observe().expect("observe after stop"),
        LifecycleState::Stopped
    );

    let control_dir = runtime.instance_dir().to_path_buf();
    runtime.destroy().expect("destroy");
    assert!(!control_dir.exists(), "destroy must remove the control dir");

    destroy_instance_storage(prepared).expect("dispose storage");
}
