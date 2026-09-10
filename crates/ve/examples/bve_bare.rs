//! Issue #72 host-proof building blocks: boot the minimal **BARE** image (the
//! Bamep Agent Runtime Environment, built with Buildroot) **directly** in one
//! BVE — QEMU/KVM loads the BARE kernel + initramfs with `-kernel`/`-initrd`
//! (no firmware boot device, no bootloader, no ISO, no PXE) — capture the
//! serial console, and prove `BARE_READY` and `BARE_NET_READY` in each of two
//! boots of the same BVE.
//!
//! **The primary way to run the proof is `scripts/bve-bare-direct-proof.sh`.**
//! These subcommands stay for manual debugging (all non-privileged — BARE
//! direct boot needs no `sudo`, no TAP/bridge, no #70/#71 network):
//!
//! - `plan  <id> --kernel <p> --initrd <p> [--append "<s>"]` — print the profile;
//! - `check <id> --kernel <p> --initrd <p>` — host prerequisites + artifacts;
//! - `run-bve <id> --kernel <p> --initrd <p> [--append "<s>"] [--serial-out <p>]
//!   [--boot-hold <secs>]` — two boots of one BVE; emits the serial-log line
//!   range bounding each boot for the harness (it does not interpret markers).
//!
//! BARE artifacts are NOT in the repo; build them with `scripts/build-bare.sh`.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bamep_ve::{
    destroy_instance_storage, detect_host_prerequisites, ensure_system_base, prepare_instance,
    BveId, BveRuntime, BveStorageRoot, DirectKernelBoot, Firmware, LifecycleState, MacAddress,
    RuntimeRoot, SystemBaseSpec,
};

type R = Result<(), Box<dyn Error>>;

/// Default kernel command line: serial console for the capture, immediate
/// reboot-less panic so a wedged boot fails fast rather than hanging the hold.
const DEFAULT_APPEND: &str = "console=ttyS0,115200 panic=-1";

/// How long each boot is held so BARE can reach init, probe virtio, run
/// udhcpc, and emit both markers. BARE is tiny; 25 s is generous.
const DEFAULT_BOOT_HOLD: Duration = Duration::from_secs(25);

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> R {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let id = || -> Result<BveId, Box<dyn Error>> {
        let raw = args
            .iter()
            .skip(2)
            .find(|a| !a.starts_with("--"))
            .ok_or("missing <bve-id> argument")?;
        Ok(BveId::new(raw.clone())?)
    };

    match cmd {
        "plan" => plan(&id()?),
        "check" => check(&id()?),
        "run-bve" => run_bve(&id()?),
        other => {
            eprintln!(
                "usage: bve_bare <plan|check|run-bve> <bve-id> \\\n\
                 \t--kernel <bzImage> --initrd <rootfs.cpio.gz> [--append \"<cmdline>\"] \\\n\
                 \t[--serial-out <file>] [--boot-hold <secs>]\n\
                 scripts/bve-bare-direct-proof.sh drives the whole cycle; these are for debugging.\n\
                 unknown subcommand: {other:?}"
            );
            std::process::exit(2);
        }
    }
}

/// The value after `--<name>` on the command line, if present.
fn flag_value(name: &str) -> Option<String> {
    let flag = format!("--{name}");
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    None
}

fn require_artifact(kind: &str, flag: &str) -> Result<PathBuf, Box<dyn Error>> {
    let raw = flag_value(flag).ok_or_else(|| {
        format!("missing --{flag} <path> ({kind}); build it with scripts/build-bare.sh")
    })?;
    let path = PathBuf::from(raw);
    let meta = fs::metadata(&path)
        .map_err(|_| format!("{kind} not found: {} (build it with scripts/build-bare.sh; this proof never downloads it)", path.display()))?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(format!("{kind} is not a usable file: {}", path.display()).into());
    }
    Ok(path)
}

fn payload() -> Result<DirectKernelBoot, Box<dyn Error>> {
    let kernel = require_artifact("BARE kernel (bzImage)", "kernel")?;
    let initrd = require_artifact("BARE initramfs (rootfs.cpio.gz)", "initrd")?;
    let append = flag_value("append").unwrap_or_else(|| DEFAULT_APPEND.to_string());
    Ok(DirectKernelBoot::new(kernel, initrd, append)?)
}

fn plan(id: &BveId) -> R {
    let dk = payload()?;
    println!("bve id      : {id}");
    println!("mac         : {}", MacAddress::deterministic_for(id));
    println!("firmware    : Default (SeaBIOS - bypassed: QEMU/KVM loads the kernel directly)");
    println!("nic         : virtio-net-pci   net: user-mode (SLIRP)   boot: direct kernel");
    println!("disk        : blank disposable virtio-blk overlay (driver proof only, no write)");
    println!("kernel      : {}", dk.kernel().display());
    println!("initrd      : {}", dk.initrd().display());
    println!("append      : {}", dk.command_line());
    println!("serial      : <instance-dir>/serial.log (append; -display none)");
    Ok(())
}

fn check(id: &BveId) -> R {
    let _ = payload()?;
    detect_host_prerequisites()?;
    bamep_ve::check_qemu_img_binary()?;
    println!("ok: qemu-system-x86_64 + /dev/kvm + qemu-img present; BARE artifacts present");
    let _ = id;
    Ok(())
}

fn count_lines(path: &Path) -> usize {
    fs::read(path)
        .map(|b| b.iter().filter(|&&c| c == b'\n').count())
        .unwrap_or(0)
}

/// 1-indexed inclusive `start:end` for a boot, given the serial-log line count
/// before and after it. An empty boot yields `n+1:n`, which the harness reads
/// as "not proven".
fn range(before: usize, after: usize) -> String {
    format!("{}:{}", before + 1, after.max(before))
}

fn run_bve(id: &BveId) -> R {
    let dk = payload()?;
    let boot_hold = flag_value("boot-hold")
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_BOOT_HOLD);
    let serial_out = flag_value("serial-out").map(PathBuf::from);

    let prerequisites = detect_host_prerequisites()?;
    bamep_ve::check_qemu_img_binary()?;

    let scratch =
        std::env::temp_dir().join(format!("bamep-bve-bare-{}-{}", id, std::process::id()));
    let _guard = RemoveOnDrop(scratch.clone());
    fs::create_dir_all(&scratch)?;
    let runtime_root = RuntimeRoot::new(scratch.join("control"));
    let storage_root = BveStorageRoot::new(scratch.join("storage"))?;

    // A blank overlay over a small blank base: nothing bootable on disk, no
    // source disk, no cdrom - the only boot path is the direct kernel payload.
    let base = ensure_system_base(&storage_root, &SystemBaseSpec::new(64 * 1024 * 1024)?)?;
    let storage = prepare_instance(&storage_root, id, None)?;
    let definition = storage
        .define_bve(id.clone(), 2, 512, Firmware::Default)?
        .with_direct_kernel(dk)?;

    println!(
        "BVE {id}: direct kernel boot, virtio-net-pci + user-mode, MAC = {}",
        definition.mac()
    );

    let mut runtime =
        BveRuntime::create(&runtime_root, definition, storage.clone())?.with_serial_capture();
    let serial_log = runtime
        .serial_log()
        .expect("serial capture was enabled")
        .to_path_buf();

    // Both boots reuse the SAME runtime / definition / storage / kernel /
    // initrd - only stop/start between them, no hidden recreate (Issue #72
    // repeatability). QEMU appends to serial.log, so each boot owns a line
    // range.
    let one_boot = |runtime: &mut BveRuntime, boot: u8| -> R {
        runtime.start(&prerequisites)?;
        assert_eq!(runtime.observe()?, LifecycleState::Running);
        println!(
            "boot #{boot}: Running - holding {}s for BARE_READY + BARE_NET_READY",
            boot_hold.as_secs()
        );
        std::thread::sleep(boot_hold);
        runtime.stop()?;
        assert_eq!(runtime.observe()?, LifecycleState::Stopped);
        std::thread::sleep(Duration::from_millis(500));
        println!("boot #{boot}: stopped");
        Ok(())
    };

    let before1 = count_lines(&serial_log);
    one_boot(&mut runtime, 1)?;
    let after1 = count_lines(&serial_log);
    one_boot(&mut runtime, 2)?;
    let after2 = count_lines(&serial_log);

    // Persist the serial capture before destroy removes it.
    if let Some(out) = &serial_out {
        fs::copy(&serial_log, out)?;
        println!("BVE_SERIAL_LOG={}", out.display());
    } else {
        println!("BVE_SERIAL_LOG={}", serial_log.display());
    }
    println!("BVE_BOOT1_LOG_RANGE={}", range(before1, after1));
    println!("BVE_BOOT2_LOG_RANGE={}", range(after1, after2));

    runtime.destroy()?;
    destroy_instance_storage(storage)?;
    let _ = base;
    println!("both boots done; BVE disposed.");
    Ok(())
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_is_1_indexed_inclusive_and_marks_an_empty_boot_unsatisfiable() {
        assert_eq!(range(100, 118), "101:118");
        assert_eq!(range(118, 118), "119:118");
        // defensive: a shrinking log never yields a wild range
        assert_eq!(range(50, 30), "51:50");
    }
}
