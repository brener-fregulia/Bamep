//! Developer/manual entrypoint for Issues #68 and #69: drive one real BVE
//! lifecycle **through the Simulator-side boundary**
//! (`SimulatorBve` -> `bamep-ve` -> QEMU/KVM), including one storage-reset
//! cycle.
//!
//! Run on a prepared Linux reference host (native or WSL2) with
//! `qemu-system-x86_64` + `qemu-img` on `PATH` and a usable `/dev/kvm`:
//!
//! ```text
//! cargo run -p bamep-simulator --example bve_lifecycle
//! ```
//!
//! It proves:
//!
//! ```text
//! ensure base -> prepare instance (overlay + source) -> create
//!   -> start -> observe Running -> reset (VM) -> observe Running -> stop
//!   -> reset_system_storage -> start -> observe Running -> stop
//!   -> destroy (control) -> destroy_instance_storage (disks)
//! ```
//!
//! The VM boots no guest OS (a small blank base; it sits in firmware). It
//! uses no Server, no Agent Protocol, no PXE, no TAP/bridge, and installs
//! nothing. Any failed step aborts with a non-zero exit and the preserved
//! error cause.

use std::error::Error;
use std::fs;
use std::path::PathBuf;

use bamep_simulator::{
    check_qemu_img_binary, destroy_instance_storage, ensure_system_base, prepare_instance, BveId,
    BveLifecycleState, BveRuntimeRoot, BveStorageRoot, Firmware, SimulatorBve, SourceDiskSpec,
    SystemBaseSpec,
};
use bamep_ve::detect_host_prerequisites;

/// A small throwaway base — enough to prove sparse/CoW/reset behaviour fast.
const BASE_BYTES: u64 = 512 * 1024 * 1024;

/// Removes the throwaway scratch tree on the way out, whatever happened.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let prerequisites = detect_host_prerequisites()
        .map_err(|e| format!("host prerequisites for a BVE are not satisfied: {e}"))?;
    let qemu_img = check_qemu_img_binary()
        .map_err(|e| format!("qemu-img is required to prepare BVE storage: {e}"))?;
    println!("prerequisites: {}", prerequisites.qemu_version_line);
    println!("prerequisites: {qemu_img}");
    println!(
        "prerequisites: KVM device {}",
        prerequisites.kvm_device.display()
    );

    let scratch =
        Scratch(std::env::temp_dir().join(format!("bamep-sim-bve-example-{}", std::process::id())));
    fs::create_dir_all(&scratch.0)?;
    let runtime_root = BveRuntimeRoot::new(scratch.0.join("control"));
    let storage_root = BveStorageRoot::new(scratch.0.join("storage"))?;
    let id = BveId::new("sim-bve-lifecycle")?;

    let base = ensure_system_base(&storage_root, &SystemBaseSpec::new(BASE_BYTES)?)?;
    println!("base: {}", base.display());
    let prepared = prepare_instance(
        &storage_root,
        &id,
        Some(&SourceDiskSpec::new(64 * 1024 * 1024)?),
    )?;
    println!(
        "prepared: overlay {} + source {}",
        prepared.layout().system_overlay().display(),
        prepared.layout().source_disk().display(),
    );

    let definition = prepared.define_bve(id, 1, 256, Firmware::Default)?;
    println!(
        "definition: id={} vcpus={} mem={}MiB mac={} system={} source={}",
        definition.id(),
        definition.vcpus(),
        definition.memory_mib(),
        definition.mac(),
        definition.system_disk().format().as_qemu_str(),
        definition
            .source_disk()
            .map(|s| s.format().as_qemu_str())
            .unwrap_or("<none>"),
    );

    let mut bve = SimulatorBve::create(&runtime_root, definition, prepared.clone())?;
    println!("create: instance dir {}", bve.instance_dir().display());

    bve.start()?;
    println!("start: ok");
    expect_state(&mut bve, BveLifecycleState::Running, "after start")?;

    bve.reset()?;
    println!("reset (VM system_reset): ok");
    expect_state(&mut bve, BveLifecycleState::Running, "after VM reset")?;

    bve.stop()?;
    println!("stop: ok");
    expect_state(&mut bve, BveLifecycleState::Stopped, "after stop")?;

    bve.reset_system_storage()?;
    println!("reset_system_storage: fresh overlay from the same base");

    bve.start()?;
    println!("start (on the fresh overlay): ok");
    expect_state(
        &mut bve,
        BveLifecycleState::Running,
        "after storage reset + start",
    )?;
    bve.stop()?;
    expect_state(&mut bve, BveLifecycleState::Stopped, "after final stop")?;

    let instance_dir = bve.instance_dir().to_path_buf();
    bve.destroy()?;
    println!("destroy (control only): ok");
    if instance_dir.exists() {
        return Err(format!(
            "destroy left the control dir behind: {}",
            instance_dir.display()
        )
        .into());
    }

    destroy_instance_storage(prepared)?;
    println!("destroy_instance_storage (disks): ok");
    if base.exists() {
        println!("base survives disposal: {}", base.display());
    } else {
        return Err("the shared base must survive instance disposal".into());
    }

    println!("lifecycle complete: Simulator -> bamep-ve -> QEMU/KVM (+ storage reset)");
    Ok(())
}

fn expect_state(
    bve: &mut SimulatorBve,
    want: BveLifecycleState,
    when: &str,
) -> Result<(), Box<dyn Error>> {
    let got = bve.observe()?;
    println!("observe {when}: {got:?}");
    if got != want {
        return Err(format!("expected {want:?} {when}, observed {got:?}").into());
    }
    Ok(())
}
