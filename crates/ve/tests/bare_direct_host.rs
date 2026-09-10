//! Host-dependent BARE direct-boot proof (Issue #72).
//!
//! **Opt-in**: does nothing unless `BAMEP_BVE_BARE_HOST_TEST=1`, and it needs
//! `BAMEP_BVE_BARE_KERNEL` / `BAMEP_BVE_BARE_INITRD` pointing at artifacts built
//! by `scripts/build-bare.sh`. An ordinary `cargo test` never requires KVM,
//! QEMU, or a BARE image.
//!
//! ```text
//! BAMEP_BVE_BARE_HOST_TEST=1 \
//!   BAMEP_BVE_BARE_KERNEL=<cache>/output/bamep_bare_x86_64/images/bzImage \
//!   BAMEP_BVE_BARE_INITRD=<cache>/output/bamep_bare_x86_64/images/rootfs.cpio.gz \
//!   cargo test -p bamep-ve --test bare_direct_host -- --nocapture
//! ```
//!
//! It proves: QEMU/KVM loads the BARE kernel + initramfs directly (no firmware
//! boot device, no bootloader, no ISO), BARE reaches init and emits
//! `BARE READY` on the serial console, then the SAME BVE (same definition,
//! storage, kernel, initrd) boots a second time and emits it again. The
//! primary, full proof - including the `BARE_NET_READY` DHCP evidence and the
//! per-boot line-range parsing - is `scripts/bve-bare-direct-proof.sh`; this
//! test is the smallest in-crate regression anchor.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use bamep_ve::{
    destroy_instance_storage, detect_host_prerequisites, ensure_system_base, prepare_instance,
    BveId, BveRuntime, BveStorageRoot, DirectKernelBoot, Firmware, LifecycleState, RuntimeRoot,
    SystemBaseSpec,
};

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn artifact(var: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os(var)?);
    p.is_file().then_some(p)
}

#[test]
fn bare_boots_directly_twice_in_one_bve() {
    if std::env::var_os("BAMEP_BVE_BARE_HOST_TEST").is_none() {
        eprintln!(
            "skipping BARE direct-boot host test: set BAMEP_BVE_BARE_HOST_TEST=1 plus \
             BAMEP_BVE_BARE_KERNEL / BAMEP_BVE_BARE_INITRD (see scripts/build-bare.sh) on a \
             KVM-capable Linux host"
        );
        return;
    }
    let (Some(kernel), Some(initrd)) = (
        artifact("BAMEP_BVE_BARE_KERNEL"),
        artifact("BAMEP_BVE_BARE_INITRD"),
    ) else {
        panic!("BAMEP_BVE_BARE_KERNEL / BAMEP_BVE_BARE_INITRD must point at built BARE artifacts");
    };

    let prerequisites = detect_host_prerequisites().expect("qemu-system-x86_64 + usable /dev/kvm");
    bamep_ve::check_qemu_img_binary().expect("qemu-img");

    let temp = TempDir(std::env::temp_dir().join(format!("bamep-ve-bare-{}", std::process::id())));
    fs::create_dir_all(&temp.0).unwrap();
    let runtime_root = RuntimeRoot::new(temp.0.join("control"));
    let storage_root = BveStorageRoot::new(temp.0.join("storage")).unwrap();
    let id = BveId::new("bve-bare-host").unwrap();

    ensure_system_base(
        &storage_root,
        &SystemBaseSpec::new(64 * 1024 * 1024).unwrap(),
    )
    .unwrap();
    let storage = prepare_instance(&storage_root, &id, None).unwrap();
    let definition = storage
        .define_bve(id.clone(), 2, 512, Firmware::Default)
        .unwrap()
        .with_direct_kernel(
            DirectKernelBoot::new(&kernel, &initrd, "console=ttyS0,115200 panic=-1").unwrap(),
        )
        .unwrap();

    let mut runtime = BveRuntime::create(&runtime_root, definition, storage.clone())
        .unwrap()
        .with_serial_capture();
    let serial_log = runtime.serial_log().unwrap().to_path_buf();

    let ready_count = |from: usize| -> usize {
        fs::read_to_string(&serial_log)
            .unwrap_or_default()
            .lines()
            .skip(from)
            .filter(|l| l.contains("BARE READY nic="))
            .count()
    };

    for boot in 1..=2u8 {
        let before = fs::read_to_string(&serial_log)
            .unwrap_or_default()
            .lines()
            .count();
        runtime.start(&prerequisites).expect("start");
        assert_eq!(runtime.observe().unwrap(), LifecycleState::Running);
        std::thread::sleep(Duration::from_secs(25));
        runtime.stop().expect("stop");
        assert_eq!(runtime.observe().unwrap(), LifecycleState::Stopped);
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            ready_count(before) >= 1,
            "boot #{boot}: expected 'BARE READY' on the serial console after this boot"
        );
    }

    runtime.destroy().unwrap();
    assert!(!serial_log.exists(), "destroy removes the serial log");
    destroy_instance_storage(storage).unwrap();
}
