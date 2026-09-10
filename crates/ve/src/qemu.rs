//! Building the exact QEMU invocation for a BVE, and checking the host
//! prerequisites that invocation needs.
//!
//! ADR-0022: QEMU is driven **directly** (no libvirt), with **KVM**
//! acceleration, and the runtime "must fail clearly rather than silently fall
//! back to full software CPU emulation when usable KVM acceleration is
//! unavailable". [`QemuCommand::for_bve`] therefore emits `-accel kvm` with no
//! `tcg` fallback, and [`check_kvm_device`] fails closed.
//!
//! No multi-hypervisor abstraction: this module names `qemu-system-x86_64`
//! directly.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::definition::{BootMode, BveDefinition, Firmware, NetworkAttachment};

/// The QEMU system-emulator binary this runtime drives.
pub const QEMU_BINARY: &str = "qemu-system-x86_64";

/// The KVM character device checked for usable hardware acceleration.
pub const DEFAULT_KVM_DEVICE: &str = "/dev/kvm";

/// Default path of the shared, read-only OVMF firmware **code** pflash image
/// for [`Firmware::Uefi`] (ADR-0025), as installed by the Debian/Ubuntu `ovmf`
/// package. Non-Secure-Boot (`OVMF_CODE_4M.fd`, **not**
/// `OVMF_CODE_4M.secboot.fd`). Never written; never in any deletion set.
/// Override with `BAMEP_VE_OVMF_CODE` (e.g. the Fedora lab installs OVMF under
/// `/usr/share/edk2/ovmf/`).
pub const OVMF_CODE_4M: &str = "/usr/share/OVMF/OVMF_CODE_4M.fd";

/// Default path of the immutable OVMF firmware **variables** template. Each
/// [`Firmware::Uefi`] BVE gets its own writable copy of this, made once at
/// `create` (ADR-0025). This file itself is never written by this crate.
/// Override with `BAMEP_VE_OVMF_VARS_TEMPLATE`.
pub const OVMF_VARS_4M_TEMPLATE: &str = "/usr/share/OVMF/OVMF_VARS_4M.fd";

/// Environment override for [`OVMF_CODE_4M`].
pub const OVMF_CODE_ENV: &str = "BAMEP_VE_OVMF_CODE";
/// Environment override for [`OVMF_VARS_4M_TEMPLATE`].
pub const OVMF_VARS_TEMPLATE_ENV: &str = "BAMEP_VE_OVMF_VARS_TEMPLATE";

/// The OVMF CODE pflash path in effect: `$BAMEP_VE_OVMF_CODE` if set, else
/// [`OVMF_CODE_4M`].
pub fn ovmf_code_path() -> PathBuf {
    std::env::var_os(OVMF_CODE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(OVMF_CODE_4M))
}

/// The OVMF VARS template path in effect: `$BAMEP_VE_OVMF_VARS_TEMPLATE` if
/// set, else [`OVMF_VARS_4M_TEMPLATE`].
pub fn ovmf_vars_template_path() -> PathBuf {
    std::env::var_os(OVMF_VARS_TEMPLATE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(OVMF_VARS_4M_TEMPLATE))
}

/// The two OVMF pflash images for one [`Firmware::Uefi`] launch. The runtime
/// resolves these ([`ovmf_code_path`] + the per-BVE VARS copy) and hands them
/// in, so [`QemuCommand::for_bve`] stays a pure function of its arguments.
#[derive(Debug, Clone, Copy)]
pub struct UefiPflash<'a> {
    /// The shared read-only CODE image.
    pub code: &'a Path,
    /// This BVE's own writable VARS copy.
    pub vars: &'a Path,
}

/// The concrete process + argument vector the runtime will spawn for one BVE.
///
/// Building this is pure and deterministic, so the full invocation can be
/// asserted in unit tests without a QEMU binary or KVM on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QemuCommand {
    program: String,
    args: Vec<String>,
}

impl QemuCommand {
    /// The QEMU invocation for `definition`, with its QMP control socket at
    /// `qmp_socket`.
    ///
    /// Emitted invariants (asserted by tests):
    ///
    /// - program is [`QEMU_BINARY`];
    /// - `-accel kvm` and `-cpu host` — explicit hardware acceleration, and
    ///   **no** `tcg` anywhere (ADR-0022 fail-closed);
    /// - `-smp <vcpus>` and `-m <memory_mib>M` from the definition;
    /// - headless: `-display none`, no stdio console. The serial line is
    ///   `-serial none` when `serial` is `None` (every path before Issue #72),
    ///   or, when `serial` is `Some(path)`, a file chardev the runtime resolved
    ///   — `-chardev file,id=char0,path=<path>,append=on` + `-serial
    ///   chardev:char0` — a headless, machine-readable capture (Issue #72). No
    ///   VNC/SPICE/display (that is Issue #74);
    /// - networking from `definition.network()`: `-netdev user,id=net0`
    ///   (unprivileged SLIRP — the default) **or**, for an isolated TAP
    ///   (Issue #70), `-netdev tap,id=net0,ifname=<prepared>,script=no,\
    ///   downscript=no` (no `qemu-bridge-helper`, no `/etc/qemu/bridge.conf`,
    ///   no auto-run scripts; the ifname comes from validated prepared network
    ///   state). Either way a `virtio-net-pci` device carries the deterministic
    ///   MAC;
    /// - for [`BootMode::NetworkFirst`], `-boot order=n` so the firmware runs
    ///   its NIC PXE option ROM; [`BootMode::Default`] emits no `-boot`;
    /// - for a [`crate::definition::DirectKernelBoot`] (Issue #72), `-kernel
    ///   <kernel> -initrd <initrd> -append <command_line>` — QEMU/KVM loads the
    ///   Linux kernel + initramfs directly, with no firmware boot device, no
    ///   bootloader, and no ISO. Orthogonal to `Firmware`/`NicModel`; the BARE
    ///   profile pairs it with `Firmware::Default`. `BootMode::NetworkFirst` +
    ///   direct kernel is a caller bug rejected at
    ///   [`crate::BveRuntime::create`], not here;
    /// - the NIC `-device` model is `definition.nic_model()` — `virtio-net-pci`
    ///   (default) or `e1000` (Issue #71); the deterministic MAC is identical
    ///   for both;
    /// - for [`Firmware::Uefi`] (Issue #71 / ADR-0025), a pflash pair:
    ///   `-drive if=pflash,unit=0,format=raw,readonly=on,file=<code>` plus
    ///   `-drive if=pflash,unit=1,format=raw,file=<vars>` from the supplied
    ///   [`UefiPflash`] — a shared read-only CODE and the per-BVE writable VARS
    ///   copy the runtime prepared. [`Firmware::Default`] emits no pflash and
    ///   ignores `uefi`. Passing `Firmware::Uefi` with `uefi == None` is a
    ///   caller bug and panics;
    /// - the `System` disk, and the `Source` disk when present, each as a
    ///   `-drive if=none,id=<role>,file=<path>,format=<fmt>` plus a matching
    ///   `-device virtio-blk-pci,drive=<role>,serial=<role-serial>`. Disk
    ///   identity is the explicit `id=`/`serial=`, never argument order; the
    ///   format is always explicit, never auto-detected; the immutable
    ///   backing base is never attached (Issue #69);
    /// - `-qmp unix:<socket>,server=on,wait=off` — a listening control socket
    ///   that does not block startup on a client;
    /// - `-name <id>`.
    pub fn for_bve(
        definition: &BveDefinition,
        qmp_socket: &Path,
        uefi: Option<UefiPflash<'_>>,
        serial: Option<&Path>,
    ) -> Self {
        let mut args: Vec<String> = Vec::new();
        let mut push = |a: &str| args.push(a.to_string());

        push("-name");
        push(definition.id().as_str());

        // Explicit KVM acceleration. `-accel kvm` with no `,tcg` fallback
        // makes QEMU exit with an error instead of silently using the TCG
        // software emulator when KVM is unusable (ADR-0022).
        push("-accel");
        push("kvm");
        push("-cpu");
        push("host");

        push("-smp");
        push(&definition.vcpus().to_string());
        push("-m");
        push(&format!("{}M", definition.memory_mib()));

        // Headless: no graphical backend. The serial line is either detached
        // (`none`, every path before Issue #72) or captured to a runtime-owned
        // file chardev in append mode — a machine-readable proof log, never a
        // VNC/SPICE/display path (Issue #74).
        push("-display");
        push("none");
        match serial {
            None => {
                push("-serial");
                push("none");
            }
            Some(path) => {
                push("-chardev");
                push(&format!("file,id=char0,path={},append=on", path.display()));
                push("-serial");
                push("chardev:char0");
            }
        }

        // Networking. User-mode SLIRP is the unprivileged default; an isolated
        // TAP (Issue #70) is opened by name — QEMU never runs a bridge helper
        // or a setup/teardown script, and the ifname is validated prepared
        // state, not a caller string. The deterministic MAC is unchanged in
        // both cases.
        push("-netdev");
        match definition.network() {
            NetworkAttachment::UserMode => push("user,id=net0"),
            NetworkAttachment::IsolatedTap { ifname } => push(&format!(
                "tap,id=net0,ifname={ifname},script=no,downscript=no"
            )),
        }
        push("-device");
        push(&format!(
            "{},netdev=net0,mac={}",
            definition.nic_model().as_qemu_device(),
            definition.mac()
        ));

        // System disk (always), then the source fixture (when present). Each
        // is a headless `if=none` blockdev bound to an explicit virtio-blk
        // device by a stable `id=`/`serial=` — deterministic identity, not
        // argument order. Formats are stated explicitly; the immutable
        // backing base is never attached here (Issue #69).
        for attachment in std::iter::once(definition.system_disk()).chain(definition.source_disk())
        {
            let id = attachment.role().as_qemu_id();
            push("-drive");
            push(&format!(
                "if=none,id={id},file={},format={}",
                attachment.path().display(),
                attachment.format().as_qemu_str()
            ));
            push("-device");
            push(&format!(
                "virtio-blk-pci,drive={id},serial={}",
                attachment.role().as_qemu_serial()
            ));
        }

        // QMP control boundary: a listening Unix socket, non-blocking so QEMU
        // does not wait for a client during startup.
        push("-qmp");
        push(&format!("unix:{},server=on,wait=off", qmp_socket.display()));

        match definition.firmware() {
            // SeaBIOS default: no -bios / -pflash. Smallest boot path that
            // proves lifecycle for #67.
            Firmware::Default => {}
            // OVMF non-Secure-Boot pflash pair (ADR-0025): shared read-only
            // CODE + this BVE's own writable VARS copy. The runtime always
            // supplies the VARS path for a UEFI definition.
            Firmware::Uefi => {
                let pflash = uefi.expect(
                    "Firmware::Uefi requires an instance OVMF_VARS path; the runtime must \
                     prepare it at create time (ADR-0025)",
                );
                push("-drive");
                push(&format!(
                    "if=pflash,unit=0,format=raw,readonly=on,file={}",
                    pflash.code.display()
                ));
                push("-drive");
                push(&format!(
                    "if=pflash,unit=1,format=raw,file={}",
                    pflash.vars.display()
                ));
            }
        }

        match definition.boot_mode() {
            // Firmware's own order — current behaviour, no -boot.
            BootMode::Default => {}
            // Attempt the NIC. `order=n` is the minimum that makes the
            // firmware run its virtio PXE option ROM and send DHCPDISCOVER
            // (Issue #70). Not a boot-order DSL; UEFI/OVMF is Issue #71.
            BootMode::NetworkFirst => {
                push("-boot");
                push("order=n");
            }
        }

        // Direct Linux kernel boot (Issue #72): QEMU/KVM loads the kernel +
        // initramfs directly. No firmware boot device, no bootloader, no ISO —
        // the only boot path is this payload. `BootMode::NetworkFirst` + a
        // direct kernel is rejected at `BveRuntime::create`.
        if let Some(direct_kernel) = definition.direct_kernel() {
            push("-kernel");
            push(&direct_kernel.kernel().display().to_string());
            push("-initrd");
            push(&direct_kernel.initrd().display().to_string());
            push("-append");
            push(direct_kernel.command_line());
        }

        Self {
            program: QEMU_BINARY.to_string(),
            args,
        }
    }

    /// The program to spawn.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The argument vector (without the program).
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Host facts a BVE launch needs, gathered by [`detect_host_prerequisites`].
#[derive(Debug, Clone)]
pub struct HostPrerequisites {
    /// The QEMU binary name that was found and probed.
    pub qemu_binary: String,
    /// The first line of `qemu-system-x86_64 --version`.
    pub qemu_version_line: String,
    /// The KVM device confirmed present and usable.
    pub kvm_device: PathBuf,
}

/// Why a host cannot launch a BVE.
#[derive(Debug, thiserror::Error)]
pub enum PrerequisiteError {
    /// The QEMU binary could not be executed at all (missing from `PATH`, not
    /// executable, ...).
    #[error("QEMU binary {binary:?} could not be executed: {source}")]
    QemuBinaryUnavailable {
        /// The binary name that was attempted.
        binary: String,
        /// The underlying spawn error.
        #[source]
        source: std::io::Error,
    },

    /// The QEMU binary ran but `--version` reported failure.
    #[error("QEMU binary {binary:?} failed its --version probe")]
    QemuBinaryProbeFailed {
        /// The binary name that was probed.
        binary: String,
    },

    /// The KVM device node is not present — this host cannot provide KVM.
    #[error(
        "KVM device {device} is not present: this host cannot provide KVM hardware \
         acceleration, and ADR-0022 forbids a silent software-emulation fallback"
    )]
    KvmDeviceMissing {
        /// The device path that was checked.
        device: PathBuf,
    },

    /// The KVM device exists but this process cannot open it read/write.
    #[error(
        "KVM device {device} is present but not usable by this process ({source}); grant \
         access (for example add the user to the 'kvm' group) — a silent software-emulation \
         fallback is not allowed (ADR-0022)"
    )]
    KvmDeviceInaccessible {
        /// The device path that was checked.
        device: PathBuf,
        /// The underlying open error.
        #[source]
        source: std::io::Error,
    },

    /// A UEFI firmware image ([`OVMF_CODE_4M`] or [`OVMF_VARS_4M_TEMPLATE`])
    /// needed for [`Firmware::Uefi`] is missing or unreadable. Install `ovmf`
    /// (Debian/Ubuntu) — this crate never downloads a firmware image
    /// (ADR-0025).
    #[error("UEFI firmware image {path} is not usable ({detail}); install the 'ovmf' package")]
    UefiFirmwareUnavailable {
        /// The firmware path that was checked.
        path: PathBuf,
        /// What was wrong (missing, not a file, unreadable).
        detail: String,
    },
}

/// Probes a QEMU binary with `--version`, returning its first version line.
pub fn check_qemu_binary(binary: &str) -> Result<String, PrerequisiteError> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|source| PrerequisiteError::QemuBinaryUnavailable {
            binary: binary.to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(PrerequisiteError::QemuBinaryProbeFailed {
            binary: binary.to_string(),
        });
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(line)
}

/// Confirms the KVM device is present and openable read/write by this
/// process. Fails closed: a missing or inaccessible device is an error, never
/// a downgrade to software emulation (ADR-0022).
pub fn check_kvm_device(device: &Path) -> Result<(), PrerequisiteError> {
    if !device.exists() {
        return Err(PrerequisiteError::KvmDeviceMissing {
            device: device.to_path_buf(),
        });
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map_err(|source| PrerequisiteError::KvmDeviceInaccessible {
            device: device.to_path_buf(),
            source,
        })?;
    Ok(())
}

/// Confirms both OVMF firmware images [`Firmware::Uefi`] needs are present and
/// readable regular files: the resolved [`ovmf_code_path`] and
/// [`ovmf_vars_template_path`]. **Read-only** and **UEFI-only** — kept separate
/// from [`detect_host_prerequisites`] so a `Firmware::Default` BVE never needs
/// OVMF installed (same split as `check_qemu_img_binary` for storage).
pub fn check_uefi_firmware() -> Result<(), PrerequisiteError> {
    for path in [ovmf_code_path(), ovmf_vars_template_path()] {
        let meta =
            std::fs::metadata(&path).map_err(|e| PrerequisiteError::UefiFirmwareUnavailable {
                path: path.clone(),
                detail: e.to_string(),
            })?;
        if !meta.is_file() {
            return Err(PrerequisiteError::UefiFirmwareUnavailable {
                path: path.clone(),
                detail: "not a regular file".to_string(),
            });
        }
        std::fs::File::open(&path).map_err(|e| PrerequisiteError::UefiFirmwareUnavailable {
            path: path.clone(),
            detail: e.to_string(),
        })?;
    }
    Ok(())
}

/// Gathers every host prerequisite a BVE launch needs: a working
/// `qemu-system-x86_64` and a usable [`DEFAULT_KVM_DEVICE`].
pub fn detect_host_prerequisites() -> Result<HostPrerequisites, PrerequisiteError> {
    let qemu_version_line = check_qemu_binary(QEMU_BINARY)?;
    check_kvm_device(Path::new(DEFAULT_KVM_DEVICE))?;
    Ok(HostPrerequisites {
        qemu_binary: QEMU_BINARY.to_string(),
        qemu_version_line,
        kvm_device: PathBuf::from(DEFAULT_KVM_DEVICE),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{BveId, DiskAttachment, DiskFormat, IfName, NicModel};

    const SYSTEM_OVERLAY: &str = "/srv/bamep/storage/instances/bve-argv/system.qcow2";
    const SOURCE_DISK: &str = "/srv/bamep/storage/instances/bve-argv/source.raw";
    const SYSTEM_BASE: &str = "/srv/bamep/storage/base/system-base.raw";

    fn system_only() -> BveDefinition {
        BveDefinition::new(
            BveId::new("bve-argv").unwrap(),
            4,
            512,
            Firmware::Default,
            DiskAttachment::system(SYSTEM_OVERLAY, DiskFormat::Qcow2).unwrap(),
        )
        .unwrap()
    }

    fn with_source() -> BveDefinition {
        system_only()
            .with_source(DiskAttachment::source(SOURCE_DISK, DiskFormat::Raw).unwrap())
            .unwrap()
    }

    /// The value immediately following the first `flag` occurrence.
    fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    /// Every value that follows a `flag` occurrence, in order.
    fn values_after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == flag)
            .filter_map(|(i, _)| args.get(i + 1))
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn builds_the_expected_machine_invocation() {
        let def = with_source();
        let socket = Path::new("/run/bamep-ve/bve-argv/qmp.sock");
        let cmd = QemuCommand::for_bve(&def, socket, None, None);

        assert_eq!(cmd.program(), "qemu-system-x86_64");
        let args = cmd.args();

        assert_eq!(value_after(args, "-name"), Some("bve-argv"));
        assert_eq!(value_after(args, "-accel"), Some("kvm"));
        assert_eq!(value_after(args, "-cpu"), Some("host"));
        assert_eq!(value_after(args, "-smp"), Some("4"));
        assert_eq!(value_after(args, "-m"), Some("512M"));
        assert_eq!(value_after(args, "-display"), Some("none"));

        let net = values_after(args, "-device")
            .into_iter()
            .find(|d| d.contains("virtio-net-pci"))
            .unwrap();
        assert!(
            net.contains(&format!("mac={}", def.mac())),
            "net device {net:?} must carry the deterministic MAC"
        );

        let qmp = value_after(args, "-qmp").unwrap();
        assert!(qmp.contains("unix:/run/bamep-ve/bve-argv/qmp.sock"));
        assert!(qmp.contains("server=on"));
        assert!(qmp.contains("wait=off"));
    }

    #[test]
    fn attaches_the_system_overlay_deterministically_by_id() {
        let cmd = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"), None, None);
        let drives = values_after(cmd.args(), "-drive");
        let blk: Vec<&str> = values_after(cmd.args(), "-device")
            .into_iter()
            .filter(|d| d.contains("virtio-blk-pci"))
            .collect();

        assert_eq!(
            drives.len(),
            1,
            "system-only BVE attaches exactly one drive"
        );
        assert!(drives[0].contains("if=none"));
        assert!(drives[0].contains("id=system"));
        assert!(drives[0].contains(&format!("file={SYSTEM_OVERLAY}")));
        assert!(
            drives[0].contains("format=qcow2"),
            "system overlay format must be explicit"
        );
        assert_eq!(blk.len(), 1);
        assert!(blk[0].contains("drive=system"));
        assert!(blk[0].contains("serial=bamep-system"));
    }

    #[test]
    fn attaches_the_source_disk_independently_only_when_present() {
        let none = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"), None, None);
        assert!(
            !none.args().iter().any(|a| a.contains("id=source")),
            "no source disk when the definition has none"
        );

        let cmd = QemuCommand::for_bve(&with_source(), Path::new("/s.sock"), None, None);
        let drives = values_after(cmd.args(), "-drive");
        assert_eq!(drives.len(), 2);
        let source = drives.iter().find(|d| d.contains("id=source")).unwrap();
        assert!(source.contains(&format!("file={SOURCE_DISK}")));
        assert!(
            source.contains("format=raw"),
            "source format must be explicit"
        );
        assert!(!source.contains("id=system"));

        let blk_source = values_after(cmd.args(), "-device")
            .into_iter()
            .find(|d| d.contains("virtio-blk-pci") && d.contains("drive=source"))
            .unwrap();
        assert!(blk_source.contains("serial=bamep-source"));
    }

    #[test]
    fn every_drive_states_its_format_and_the_backing_base_is_never_attached() {
        let cmd = QemuCommand::for_bve(&with_source(), Path::new("/s.sock"), None, None);
        for drive in values_after(cmd.args(), "-drive") {
            assert!(
                drive.contains("format="),
                "drive {drive:?} must not rely on format auto-detection"
            );
        }
        for arg in cmd.args() {
            assert!(
                !arg.contains(SYSTEM_BASE),
                "the immutable backing base must never appear in argv: {arg:?}"
            );
        }
    }

    #[test]
    fn user_mode_networking_is_unchanged_and_emits_no_boot_flag() {
        let cmd = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"), None, None);
        assert_eq!(value_after(cmd.args(), "-netdev"), Some("user,id=net0"));
        assert!(
            !cmd.args().iter().any(|a| a == "-boot"),
            "BootMode::Default must not emit -boot"
        );
    }

    #[test]
    fn isolated_tap_networking_opens_the_prepared_tap_by_name_only() {
        let def = system_only().with_isolated_tap(IfName::new("bvtapdeadbeef").unwrap());
        let cmd = QemuCommand::for_bve(&def, Path::new("/s.sock"), None, None);

        assert_eq!(
            value_after(cmd.args(), "-netdev"),
            Some("tap,id=net0,ifname=bvtapdeadbeef,script=no,downscript=no")
        );
        // The NIC device still carries the deterministic MAC, unchanged.
        let net = values_after(cmd.args(), "-device")
            .into_iter()
            .find(|d| d.contains("virtio-net-pci"))
            .unwrap();
        assert!(net.contains(&format!("mac={}", def.mac())));

        for arg in cmd.args() {
            assert!(
                !arg.contains("helper=") && !arg.contains("br=") && !arg.contains("bridge"),
                "isolated TAP argv must not use a bridge helper: {arg:?}"
            );
            for physical in ["eth0", "wlan0", "docker0", "bond0"] {
                assert!(
                    !arg.split(['=', ',']).any(|tok| tok == physical),
                    "no physical interface may appear in argv: {arg:?}"
                );
            }
        }
        assert!(
            !cmd.args()
                .iter()
                .any(|a| a.contains("script=") && !a.contains("script=no")),
            "no auto-run script"
        );
    }

    #[test]
    fn network_first_boot_mode_emits_exactly_boot_order_n() {
        let def = system_only().with_boot_mode(BootMode::NetworkFirst);
        let cmd = QemuCommand::for_bve(&def, Path::new("/s.sock"), None, None);
        assert_eq!(value_after(cmd.args(), "-boot"), Some("order=n"));
        assert_eq!(
            cmd.args().iter().filter(|a| a.as_str() == "-boot").count(),
            1
        );
    }

    /// A `Firmware::Uefi` definition (built directly — firmware is a `new` arg).
    fn uefi_only() -> BveDefinition {
        BveDefinition::new(
            BveId::new("bve-argv").unwrap(),
            4,
            512,
            Firmware::Uefi,
            DiskAttachment::system(SYSTEM_OVERLAY, DiskFormat::Qcow2).unwrap(),
        )
        .unwrap()
    }

    /// The Issue #71 WinPE-proof profile: UEFI + E1000 + NetworkFirst.
    fn uefi_e1000_network_first() -> BveDefinition {
        uefi_only()
            .with_boot_mode(BootMode::NetworkFirst)
            .with_nic_model(NicModel::E1000)
    }

    const FAKE_OVMF_CODE: &str = "/opt/fake/OVMF_CODE.fd";

    fn pflash(vars: &Path) -> UefiPflash<'_> {
        UefiPflash {
            code: Path::new(FAKE_OVMF_CODE),
            vars,
        }
    }

    #[test]
    fn default_firmware_emits_no_pflash_and_ignores_the_vars_path() {
        let ignored = Path::new("/should/be/ignored/OVMF_VARS.fd");
        let with_vars = QemuCommand::for_bve(
            &system_only(),
            Path::new("/s.sock"),
            Some(pflash(ignored)),
            None,
        );
        let without = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"), None, None);
        assert_eq!(
            with_vars, without,
            "Firmware::Default must ignore the UEFI pflash entirely"
        );
        assert!(
            !without.args().iter().any(|a| a.contains("pflash")),
            "SeaBIOS default must emit no -drive if=pflash"
        );
        assert!(
            !without.args().iter().any(|a| a.contains("OVMF")),
            "SeaBIOS default must not reference OVMF"
        );
    }

    #[test]
    fn uefi_firmware_emits_the_ovmf_pflash_pair_code_readonly_vars_writable() {
        let vars = Path::new("/run/bamep-ve/bve-argv/OVMF_VARS.fd");
        let cmd =
            QemuCommand::for_bve(&uefi_only(), Path::new("/s.sock"), Some(pflash(vars)), None);
        let drives: Vec<&str> = values_after(cmd.args(), "-drive")
            .into_iter()
            .filter(|d| d.contains("if=pflash"))
            .collect();
        assert_eq!(drives.len(), 2, "exactly a CODE + VARS pflash pair");

        let code = drives.iter().find(|d| d.contains("unit=0")).unwrap();
        assert!(code.contains(&format!("file={FAKE_OVMF_CODE}")));
        assert!(
            code.contains("readonly=on"),
            "CODE pflash must be read-only"
        );
        assert!(code.contains("format=raw"));

        let vars_drive = drives.iter().find(|d| d.contains("unit=1")).unwrap();
        assert!(vars_drive.contains(&format!("file={}", vars.display())));
        assert!(
            !vars_drive.contains("readonly"),
            "the VARS pflash must be writable"
        );
        assert!(
            !vars_drive.contains("template") && !vars_drive.contains(OVMF_VARS_4M_TEMPLATE),
            "the VARS pflash must be the per-BVE copy, never the immutable template"
        );
        // Secure Boot must not be pulled in.
        for arg in cmd.args() {
            assert!(
                !arg.contains("secboot") && !arg.to_lowercase().contains("smm=on"),
                "UEFI variant must not enable Secure Boot / SMM: {arg:?}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "requires an instance OVMF_VARS path")]
    fn uefi_firmware_without_a_vars_path_is_a_caller_bug() {
        let _ = QemuCommand::for_bve(&uefi_only(), Path::new("/s.sock"), None, None);
    }

    #[test]
    fn nic_model_default_is_virtio_and_e1000_is_opt_in_with_the_same_mac() {
        let virtio = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"), None, None);
        let virtio_dev = values_after(virtio.args(), "-device")
            .into_iter()
            .find(|d| d.starts_with("virtio-net-pci,"))
            .expect("default NIC is virtio-net-pci");
        assert!(virtio_dev.contains(&format!("mac={}", system_only().mac())));

        let e1000_def = system_only().with_nic_model(NicModel::E1000);
        let e1000 = QemuCommand::for_bve(&e1000_def, Path::new("/s.sock"), None, None);
        let e1000_dev = values_after(e1000.args(), "-device")
            .into_iter()
            .find(|d| d.starts_with("e1000,"))
            .expect("NicModel::E1000 emits -device e1000");
        assert!(
            !e1000.args().iter().any(|a| a.contains("virtio-net-pci")),
            "no virtio-net-pci device when E1000 is selected"
        );
        assert_eq!(
            e1000_dev,
            &format!("e1000,netdev=net0,mac={}", e1000_def.mac()),
            "e1000 device carries the unchanged deterministic MAC"
        );
        assert_eq!(
            e1000_def.mac(),
            system_only().mac(),
            "the MAC is model-independent"
        );
    }

    #[test]
    fn winpe_pxe_profile_is_uefi_e1000_network_first_with_no_optical_or_alternate_boot() {
        let vars = Path::new("/run/bamep-ve/bve-argv/OVMF_VARS.fd");
        let cmd = QemuCommand::for_bve(
            &uefi_e1000_network_first(),
            Path::new("/s.sock"),
            Some(pflash(vars)),
            None,
        );
        let args = cmd.args();

        // UEFI PXE via the e1000 EFI option ROM: pflash pair + e1000 + order=n.
        assert!(args
            .iter()
            .any(|a| a.contains("if=pflash") && a.contains("unit=1")));
        assert!(values_after(args, "-device")
            .iter()
            .any(|d| d.starts_with("e1000,")));
        assert_eq!(value_after(args, "-boot"), Some("order=n"));

        // Anti-false-positive: the ONLY bootable path is the NIC. No optical
        // media, no El-Torito, no alternate bootable device, ever.
        for arg in args {
            let a = arg.to_lowercase();
            assert!(
                !a.contains("cdrom")
                    && !a.contains("media=cdrom")
                    && !a.contains("-cdrom")
                    && !a.contains("ide-cd")
                    && !a.contains("scsi-cd")
                    && !a.contains(".iso"),
                "the WinPE PXE proof must expose no optical/ISO fallback: {arg:?}"
            );
        }
        // Only the (blank) system overlay is a disk; no source, no extra drive.
        let drives = values_after(args, "-drive");
        let disk_drives: Vec<&&str> = drives.iter().filter(|d| d.contains("if=none")).collect();
        assert_eq!(
            disk_drives.len(),
            1,
            "only the blank system overlay is attached"
        );
        assert!(disk_drives[0].contains("id=system"));
    }

    #[test]
    fn never_enables_software_cpu_emulation() {
        let cmd = QemuCommand::for_bve(&system_only(), Path::new("/run/x/qmp.sock"), None, None);
        for arg in cmd.args() {
            assert!(
                !arg.contains("tcg"),
                "argument {arg:?} must not request the TCG software emulator (ADR-0022)"
            );
        }
        // Acceleration is stated positively, not left to QEMU's default.
        assert_eq!(value_after(cmd.args(), "-accel"), Some("kvm"));
    }

    #[test]
    fn resource_values_track_the_definition() {
        let def = BveDefinition::new(
            BveId::new("sized").unwrap(),
            8,
            2048,
            Firmware::Default,
            DiskAttachment::system("/d.qcow2", DiskFormat::Qcow2).unwrap(),
        )
        .unwrap();
        let cmd = QemuCommand::for_bve(&def, Path::new("/s.sock"), None, None);
        assert_eq!(value_after(cmd.args(), "-smp"), Some("8"));
        assert_eq!(value_after(cmd.args(), "-m"), Some("2048M"));
    }

    // ---- serial capture (Issue #72) ------------------------------------

    #[test]
    fn no_serial_argument_keeps_the_pre_72_serial_none_and_no_chardev() {
        let cmd = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"), None, None);
        assert_eq!(value_after(cmd.args(), "-serial"), Some("none"));
        assert!(
            !cmd.args().iter().any(|a| a.contains("chardev")),
            "no -chardev when serial capture is not requested"
        );
    }

    #[test]
    fn serial_some_emits_an_append_file_chardev_bound_to_the_serial_line() {
        let log = Path::new("/run/bamep-ve/bve-argv/serial.log");
        let cmd = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"), None, Some(log));
        assert_eq!(
            value_after(cmd.args(), "-chardev"),
            Some("file,id=char0,path=/run/bamep-ve/bve-argv/serial.log,append=on")
        );
        assert_eq!(value_after(cmd.args(), "-serial"), Some("chardev:char0"));
        // Headless only — never a display/VNC/SPICE path (Issue #74).
        assert_eq!(value_after(cmd.args(), "-display"), Some("none"));
        for arg in cmd.args() {
            let a = arg.to_lowercase();
            assert!(
                !a.contains("vnc")
                    && !a.contains("spice")
                    && !a.contains("gtk")
                    && !a.contains("sdl"),
                "serial capture must not pull in a display backend: {arg:?}"
            );
        }
    }

    // ---- direct kernel boot (Issue #72) --------------------------------

    fn direct_kernel_def() -> BveDefinition {
        system_only()
            .with_direct_kernel(
                crate::definition::DirectKernelBoot::new(
                    "/out/images/bzImage",
                    "/out/images/rootfs.cpio.gz",
                    "console=ttyS0,115200 panic=-1",
                )
                .unwrap(),
            )
            .unwrap()
    }

    #[test]
    fn direct_kernel_emits_kernel_initrd_append_and_no_boot_or_optical() {
        let cmd = QemuCommand::for_bve(&direct_kernel_def(), Path::new("/s.sock"), None, None);
        let args = cmd.args();

        assert_eq!(value_after(args, "-kernel"), Some("/out/images/bzImage"));
        assert_eq!(
            value_after(args, "-initrd"),
            Some("/out/images/rootfs.cpio.gz")
        );
        let append = value_after(args, "-append").unwrap();
        assert!(
            append.contains("console=ttyS0"),
            "-append must carry the serial console: {append:?}"
        );

        // BootMode::Default -> no -boot; direct kernel is the only boot path.
        assert!(!args.iter().any(|a| a == "-boot"));
        for arg in args {
            let a = arg.to_lowercase();
            assert!(
                !a.contains("cdrom")
                    && !a.contains("ide-cd")
                    && !a.contains("scsi-cd")
                    && !a.contains(".iso"),
                "direct-kernel boot must expose no optical/ISO device: {arg:?}"
            );
        }
        // Still the virtio machine profile.
        assert!(values_after(args, "-device")
            .iter()
            .any(|d| d.starts_with("virtio-net-pci,")));
        assert!(values_after(args, "-device")
            .iter()
            .any(|d| d.contains("virtio-blk-pci")));
    }

    #[test]
    fn no_direct_kernel_emits_no_kernel_initrd_or_append() {
        let cmd = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"), None, None);
        for flag in ["-kernel", "-initrd", "-append"] {
            assert!(
                !cmd.args().iter().any(|a| a == flag),
                "{flag} must not appear without a DirectKernelBoot"
            );
        }
    }

    #[test]
    fn direct_kernel_and_serial_compose_for_the_bare_proof_profile() {
        let log = Path::new("/run/bamep-ve/bve-bare/serial.log");
        let cmd = QemuCommand::for_bve(&direct_kernel_def(), Path::new("/s.sock"), None, Some(log));
        let args = cmd.args();
        assert_eq!(value_after(args, "-kernel"), Some("/out/images/bzImage"));
        assert_eq!(value_after(args, "-serial"), Some("chardev:char0"));
        assert_eq!(value_after(args, "-accel"), Some("kvm"));
        assert!(!args.iter().any(|a| a.contains("tcg")));
        assert!(!args.iter().any(|a| a == "-boot"));
    }

    #[test]
    fn missing_qemu_binary_is_an_actionable_error() {
        let err = check_qemu_binary("bamep-ve-no-such-qemu-binary-xyzzy").unwrap_err();
        assert!(matches!(
            err,
            PrerequisiteError::QemuBinaryUnavailable { .. }
        ));
    }

    #[test]
    fn missing_kvm_device_fails_closed() {
        let err = check_kvm_device(Path::new("/bamep-bve/definitely/not/here/kvm")).unwrap_err();
        assert!(matches!(err, PrerequisiteError::KvmDeviceMissing { .. }));
        // The message must point the operator at the cause, not hint at a
        // fallback.
        let msg = err.to_string();
        assert!(msg.contains("not present"));
        assert!(!msg.to_lowercase().contains("falling back"));
    }
}
