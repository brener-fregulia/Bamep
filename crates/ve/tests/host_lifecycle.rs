//! Host-dependent BVE lifecycle proof (Issue #67 "Required behavior / tests"
//! item 4-7, and the acceptance criterion "owner can manually validate one
//! real VM lifecycle with a documented command/test entrypoint").
//!
//! This is the documented manual entrypoint. It is **opt-in**: it does
//! nothing unless `BAMEP_BVE_HOST_TEST=1` is set, so an ordinary
//! `cargo test` never requires KVM or QEMU on the machine or in CI. Run it on
//! a prepared Linux reference host with:
//!
//! ```text
//! BAMEP_BVE_HOST_TEST=1 cargo test -p bamep-ve --test host_lifecycle -- --nocapture
//! ```
//!
//! It boots no guest OS. It uses a small throwaway raw disk with no bootable
//! contents — the VM sits in firmware, which is enough to prove
//! `create -> start -> observe(Running) -> reset -> observe(Running) ->
//! stop -> observe(Stopped) -> destroy`. It touches no PXE, no TAP/bridge, no
//! provisioning network, no Server, and no Simulator.

#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use bamep_ve::{
    detect_host_prerequisites, BveDefinition, BveId, BveRuntime, Firmware, LifecycleState,
    RuntimeRoot,
};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("bamep-bve-host-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

#[test]
fn one_real_bve_full_lifecycle() {
    if std::env::var_os("BAMEP_BVE_HOST_TEST").is_none() {
        eprintln!(
            "skipping host lifecycle: set BAMEP_BVE_HOST_TEST=1 on a KVM-capable Linux host \
             (qemu-system-x86_64 on PATH, /dev/kvm read/write) to run it"
        );
        return;
    }

    let prerequisites = detect_host_prerequisites()
        .expect("host prerequisites (qemu-system-x86_64 + usable /dev/kvm) must be satisfied");
    eprintln!("qemu: {}", prerequisites.qemu_version_line);

    let temp = TempRoot::new();
    let root = RuntimeRoot::new(&temp.0);

    // A small throwaway raw disk with no bootable contents.
    let disk = temp.0.join("system.raw");
    {
        let mut f = fs::File::create(&disk).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
        f.write_all(&[0u8; 512]).unwrap();
    }

    let definition = BveDefinition::new(
        BveId::new("bve-host-lifecycle").unwrap(),
        1,
        256,
        Firmware::Default,
        &disk,
    )
    .unwrap();

    let mut runtime = BveRuntime::create(&root, definition).expect("create");

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

    let instance_dir = runtime.instance_dir().to_path_buf();
    runtime.destroy().expect("destroy");
    assert!(
        !instance_dir.exists(),
        "destroy must remove the instance control dir"
    );
}
