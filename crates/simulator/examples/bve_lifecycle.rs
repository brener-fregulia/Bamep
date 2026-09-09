//! Developer/manual entrypoint for Issue #68: drive one real BVE lifecycle
//! **through the Simulator-side boundary** (`SimulatorBve` -> `bamep-ve` ->
//! QEMU/KVM).
//!
//! Run on a prepared Linux reference host (native or WSL2) with
//! `qemu-system-x86_64` on `PATH` and a usable `/dev/kvm`:
//!
//! ```text
//! cargo run -p bamep-simulator --example bve_lifecycle
//! ```
//!
//! It proves:
//!
//! ```text
//! create -> start -> observe Running -> reset -> observe Running
//!        -> stop -> observe Stopped -> destroy
//! ```
//!
//! for exactly one BVE booting no guest OS (a small blank raw disk; the VM
//! sits in firmware). It uses no Server, no Agent Protocol, no PXE, no
//! TAP/bridge, and installs nothing. Any failed step aborts with a non-zero
//! exit and the preserved error cause.

use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use bamep_simulator::{
    BveDefinition, BveId, BveLifecycleState, BveRuntimeRoot, Firmware, SimulatorBve,
};
use bamep_ve::detect_host_prerequisites;

/// Removes the throwaway runtime root on the way out, whatever happened.
struct ScratchRoot(PathBuf);

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Prerequisites — surfaced early with a clear message. `SimulatorBve::start`
    //    checks them again; this is just a friendlier failure up front.
    let prerequisites = detect_host_prerequisites()
        .map_err(|e| format!("host prerequisites for a BVE are not satisfied: {e}"))?;
    println!("prerequisites: {}", prerequisites.qemu_version_line);
    println!(
        "prerequisites: KVM device {}",
        prerequisites.kvm_device.display()
    );

    // 2. A disposable runtime root.
    let scratch = ScratchRoot(
        std::env::temp_dir().join(format!("bamep-sim-bve-example-{}", std::process::id())),
    );
    fs::create_dir_all(&scratch.0)?;
    let root = BveRuntimeRoot::new(&scratch.0);

    // 3. A small blank raw system disk (no bootable contents).
    let disk = scratch.0.join("system.raw");
    {
        let mut f = fs::File::create(&disk)?;
        f.set_len(64 * 1024 * 1024)?;
        f.write_all(&[0u8; 512])?;
    }

    // 4. Simulator-owned BVE definition, built from `bamep-ve` types directly.
    let definition = BveDefinition::new(
        BveId::new("sim-bve-lifecycle")?,
        1,
        256,
        Firmware::Default,
        &disk,
    )?;
    println!(
        "definition: id={} vcpus={} mem={}MiB mac={}",
        definition.id(),
        definition.vcpus(),
        definition.memory_mib(),
        definition.mac(),
    );

    // 5. Create.
    let mut bve = SimulatorBve::create(&root, definition)?;
    println!("create: instance dir {}", bve.instance_dir().display());

    // 6. Lifecycle.
    bve.start()?;
    println!("start: ok");
    expect_state(&mut bve, BveLifecycleState::Running, "after start")?;

    bve.reset()?;
    println!("reset: ok");
    expect_state(&mut bve, BveLifecycleState::Running, "after reset")?;

    bve.stop()?;
    println!("stop: ok");
    expect_state(&mut bve, BveLifecycleState::Stopped, "after stop")?;

    let instance_dir = bve.instance_dir().to_path_buf();
    bve.destroy()?;
    println!("destroy: ok");
    if instance_dir.exists() {
        return Err(format!(
            "destroy left the instance dir behind: {}",
            instance_dir.display()
        )
        .into());
    }

    println!("lifecycle complete: Simulator -> bamep-ve -> QEMU/KVM");
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
