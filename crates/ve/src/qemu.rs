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

use crate::definition::{BveDefinition, Firmware};

/// The QEMU system-emulator binary this runtime drives.
pub const QEMU_BINARY: &str = "qemu-system-x86_64";

/// The KVM character device checked for usable hardware acceleration.
pub const DEFAULT_KVM_DEVICE: &str = "/dev/kvm";

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
    /// - headless: `-display none -serial none`, no stdio console;
    /// - `-netdev user` (unprivileged SLIRP — no TAP/bridge, out of scope for
    ///   #67) with a `virtio-net-pci` device carrying the deterministic MAC;
    /// - the `System` disk, and the `Source` disk when present, each as a
    ///   `-drive if=none,id=<role>,file=<path>,format=<fmt>` plus a matching
    ///   `-device virtio-blk-pci,drive=<role>,serial=<role-serial>`. Disk
    ///   identity is the explicit `id=`/`serial=`, never argument order; the
    ///   format is always explicit, never auto-detected; the immutable
    ///   backing base is never attached (Issue #69);
    /// - `-qmp unix:<socket>,server=on,wait=off` — a listening control socket
    ///   that does not block startup on a client;
    /// - `-name <id>`.
    pub fn for_bve(definition: &BveDefinition, qmp_socket: &Path) -> Self {
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

        // Headless: no graphical backend, no serial line attached to stdio.
        push("-display");
        push("none");
        push("-serial");
        push("none");

        // Unprivileged user-mode networking, deterministic MAC. No TAP/bridge
        // (Issue #67 out of scope).
        push("-netdev");
        push("user,id=net0");
        push("-device");
        push(&format!(
            "virtio-net-pci,netdev=net0,mac={}",
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
    use crate::definition::{BveId, DiskAttachment, DiskFormat};

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
        let cmd = QemuCommand::for_bve(&def, socket);

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
        let cmd = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"));
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
        let none = QemuCommand::for_bve(&system_only(), Path::new("/s.sock"));
        assert!(
            !none.args().iter().any(|a| a.contains("id=source")),
            "no source disk when the definition has none"
        );

        let cmd = QemuCommand::for_bve(&with_source(), Path::new("/s.sock"));
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
        let cmd = QemuCommand::for_bve(&with_source(), Path::new("/s.sock"));
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
    fn never_enables_software_cpu_emulation() {
        let cmd = QemuCommand::for_bve(&system_only(), Path::new("/run/x/qmp.sock"));
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
        let cmd = QemuCommand::for_bve(&def, Path::new("/s.sock"));
        assert_eq!(value_after(cmd.args(), "-smp"), Some("8"));
        assert_eq!(value_after(cmd.args(), "-m"), Some("2048M"));
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
