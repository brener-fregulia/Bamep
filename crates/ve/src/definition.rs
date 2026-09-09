//! `BveDefinition`: the validated description of one virtual Endpoint machine
//! and the disks actually attached to it.
//!
//! This is deliberately not a future-complete configuration surface
//! (`m0-bamep-virtual-endpoint-contract.md` — the responsibility list "is not
//! a schema"). It carries what launching one QEMU/KVM VM needs: identity,
//! vCPU count, RAM, a minimal firmware/boot choice, a deterministic NIC MAC,
//! and the concrete disk attachments (a required `System` disk and an
//! optional `Source` fixture). How disposable storage behind those
//! attachments is prepared and reset belongs to [`crate::storage`], not here
//! (Issue #69).

use std::path::{Path, PathBuf};

/// Smallest RAM this runtime will launch a VM with. Below this even a
/// firmware-only boot is not worth representing; a caller asking for less has
/// almost certainly made a mistake.
pub const MIN_MEMORY_MIB: u32 = 128;

/// Upper guardrail on requested RAM (1 TiB). Not a capacity promise — just a
/// bound that rejects obviously-wrong values (unit confusion, overflow)
/// before they reach QEMU.
pub const MAX_MEMORY_MIB: u32 = 1_048_576;

/// Upper guardrail on requested vCPUs. Same intent as [`MAX_MEMORY_MIB`].
pub const MAX_VCPUS: u32 = 256;

/// Maximum BVE id length. An id is used verbatim as a single filesystem path
/// segment (the per-instance runtime and storage directories) and as QEMU's
/// `-name` value.
pub const MAX_ID_LEN: usize = 64;

/// A stable BVE identity.
///
/// Validated so it is always safe to use as exactly one filesystem path
/// segment and as a QEMU `-name` argument: non-empty, at most
/// [`MAX_ID_LEN`] bytes, ASCII `[A-Za-z0-9_-]` only, and never starting with
/// `-` (argument confusion) or `.` (`.`, `..`, hidden entries).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BveId(String);

impl BveId {
    /// Validates and wraps a BVE id.
    pub fn new(value: impl Into<String>) -> Result<Self, DefinitionError> {
        let value = value.into();
        let invalid = || DefinitionError::InvalidId { id: value.clone() };

        if value.is_empty() || value.len() > MAX_ID_LEN {
            return Err(invalid());
        }
        if value.starts_with('-') || value.starts_with('.') {
            return Err(invalid());
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(invalid());
        }
        Ok(Self(value))
    }

    /// The validated id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BveId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The firmware / boot path for the VM.
///
/// Issue #67 deliberately uses the smallest firmware that proves QEMU/KVM
/// lifecycle. UEFI/OVMF gains importance only in the later PXE/WinPE Work
/// Packages and is not made a mandatory dependency here. This enum exists so
/// the choice is explicit in the definition, not so it is a firmware
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    /// QEMU's built-in default firmware for the target machine (SeaBIOS on
    /// `x86_64`). No `-bios`/`-pflash` is passed.
    Default,
}

/// The image format of a disk QEMU attaches. Always stated explicitly to QEMU
/// — never left to format auto-detection (Issue #69).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskFormat {
    /// A plain sparse image (the immutable system base, and the source
    /// fixture).
    Raw,
    /// A copy-on-write overlay (the per-instance disposable system disk).
    Qcow2,
}

impl DiskFormat {
    /// The token QEMU's `format=` option expects.
    pub fn as_qemu_str(self) -> &'static str {
        match self {
            DiskFormat::Raw => "raw",
            DiskFormat::Qcow2 => "qcow2",
        }
    }
}

/// Which slot a disk occupies in a BVE. Issue #69 supports exactly two roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskRole {
    /// The boot/system target — the writable disposable overlay.
    System,
    /// An independent virtual disk fixture, separate from the system disk,
    /// intended for later capture scenarios.
    Source,
}

impl DiskRole {
    /// The stable QEMU `id=`/`-device drive=` identity for this role. Disk
    /// attachment is deterministic by this identity, never by argument order.
    pub fn as_qemu_id(self) -> &'static str {
        match self {
            DiskRole::System => "system",
            DiskRole::Source => "source",
        }
    }

    /// The stable guest-visible serial for this role, so the guest can later
    /// disambiguate disks by `/dev/disk/by-id` regardless of probe order.
    pub fn as_qemu_serial(self) -> &'static str {
        match self {
            DiskRole::System => "bamep-system",
            DiskRole::Source => "bamep-source",
        }
    }
}

/// One concrete disk attached to a BVE: its role, its on-host path, and its
/// explicit image format.
///
/// The path is validated to be safely representable in a QEMU `-drive
/// file=...` value: QEMU treats an unescaped `,` as an option separator, so a
/// path containing `,` (or a raw newline/carriage return) is rejected with an
/// actionable error rather than silently producing a malformed argument
/// (Issue #69). Storage-derived paths never contain these characters
/// ([`crate::storage::BveStorageRoot`] enforces it); this guard also covers
/// hand-built attachments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskAttachment {
    role: DiskRole,
    path: PathBuf,
    format: DiskFormat,
}

impl DiskAttachment {
    fn new(
        role: DiskRole,
        path: impl Into<PathBuf>,
        format: DiskFormat,
    ) -> Result<Self, DefinitionError> {
        let path = path.into();
        let text = path.to_string_lossy();
        if text.contains(',') || text.contains('\n') || text.contains('\r') {
            return Err(DefinitionError::UnsupportedDiskPath { path: path.clone() });
        }
        Ok(Self { role, path, format })
    }

    /// A `System`-role attachment.
    pub fn system(path: impl Into<PathBuf>, format: DiskFormat) -> Result<Self, DefinitionError> {
        Self::new(DiskRole::System, path, format)
    }

    /// A `Source`-role attachment.
    pub fn source(path: impl Into<PathBuf>, format: DiskFormat) -> Result<Self, DefinitionError> {
        Self::new(DiskRole::Source, path, format)
    }

    /// The role this disk fills.
    pub fn role(&self) -> DiskRole {
        self.role
    }

    /// The on-host image path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The explicit image format.
    pub fn format(&self) -> DiskFormat {
        self.format
    }

    fn ensure_present(&self) -> Result<(), DefinitionError> {
        let meta =
            std::fs::metadata(&self.path).map_err(|_| DefinitionError::DiskImageMissing {
                role: self.role,
                path: self.path.clone(),
            })?;
        if !meta.is_file() {
            return Err(DefinitionError::DiskImageNotAFile {
                role: self.role,
                path: self.path.clone(),
            });
        }
        Ok(())
    }
}

/// A 48-bit Ethernet MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    /// QEMU's registered OUI prefix for locally-assigned guest NICs.
    pub const QEMU_OUI: [u8; 3] = [0x52, 0x54, 0x00];

    /// Wraps six raw octets.
    pub const fn from_octets(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    /// The raw octets.
    pub const fn octets(&self) -> [u8; 6] {
        self.0
    }

    /// A deterministic locally-administered unicast MAC for a BVE id.
    ///
    /// The prefix is [`MacAddress::QEMU_OUI`] (`52:54:00`); the trailing three
    /// octets are the low 24 bits of a 64-bit FNV-1a hash of the id bytes.
    /// FNV-1a is fixed by this crate (unlike `std::hash::DefaultHasher`, whose
    /// output Rust does not promise to keep stable), so the same id yields the
    /// same MAC on every host and every build.
    pub fn deterministic_for(id: &BveId) -> Self {
        let hash = fnv1a_64(id.as_str().as_bytes());
        let [.., b3, b4, b5] = hash.to_be_bytes();
        Self([
            Self::QEMU_OUI[0],
            Self::QEMU_OUI[1],
            Self::QEMU_OUI[2],
            b3,
            b4,
            b5,
        ])
    }

    /// Whether the address has the locally-administered bit set and the
    /// multicast bit clear (a valid per-host guest NIC address).
    pub const fn is_locally_administered_unicast(&self) -> bool {
        (self.0[0] & 0b0000_0010) != 0 && (self.0[0] & 0b0000_0001) == 0
    }
}

impl std::fmt::Display for MacAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// A fixed 64-bit FNV-1a hash. Used for the deterministic MAC and, in the
/// storage host proof, as a trustworthy-and-simple integrity value for the
/// immutable base (no cryptography dependency — Issue #69).
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The validated definition of one BVE, including the disks attached to it.
///
/// Build it from prepared storage with
/// [`crate::storage::PreparedInstanceStorage::define_bve`] (which cannot
/// produce an attachment that disagrees with what was prepared), or directly
/// with [`BveDefinition::new`] / [`BveDefinition::with_source`] for tests and
/// minimal cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BveDefinition {
    id: BveId,
    vcpus: u32,
    memory_mib: u32,
    firmware: Firmware,
    mac: MacAddress,
    system: DiskAttachment,
    source: Option<DiskAttachment>,
}

impl BveDefinition {
    /// Validates resource values and builds a definition with only a system
    /// disk. The NIC MAC is derived deterministically from the id.
    ///
    /// Rejects: zero vCPUs, more than [`MAX_VCPUS`], zero RAM, RAM outside
    /// `[MIN_MEMORY_MIB, MAX_MEMORY_MIB]`, and a `system` attachment whose
    /// role is not [`DiskRole::System`].
    pub fn new(
        id: BveId,
        vcpus: u32,
        memory_mib: u32,
        firmware: Firmware,
        system: DiskAttachment,
    ) -> Result<Self, DefinitionError> {
        if vcpus == 0 {
            return Err(DefinitionError::ZeroVcpus);
        }
        if vcpus > MAX_VCPUS {
            return Err(DefinitionError::VcpusUnsupported { vcpus });
        }
        if memory_mib == 0 {
            return Err(DefinitionError::ZeroMemory);
        }
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&memory_mib) {
            return Err(DefinitionError::MemoryUnsupported { memory_mib });
        }
        if system.role() != DiskRole::System {
            return Err(DefinitionError::WrongDiskRole {
                expected: DiskRole::System,
                found: system.role(),
            });
        }

        let mac = MacAddress::deterministic_for(&id);
        Ok(Self {
            id,
            vcpus,
            memory_mib,
            firmware,
            mac,
            system,
            source: None,
        })
    }

    /// Attaches an independent source-disk fixture. Rejects an attachment
    /// whose role is not [`DiskRole::Source`].
    pub fn with_source(mut self, source: DiskAttachment) -> Result<Self, DefinitionError> {
        if source.role() != DiskRole::Source {
            return Err(DefinitionError::WrongDiskRole {
                expected: DiskRole::Source,
                found: source.role(),
            });
        }
        self.source = Some(source);
        Ok(self)
    }

    /// The BVE id.
    pub fn id(&self) -> &BveId {
        &self.id
    }

    /// Requested vCPU count (guaranteed `1..=MAX_VCPUS`).
    pub fn vcpus(&self) -> u32 {
        self.vcpus
    }

    /// Requested RAM in MiB (guaranteed `MIN_MEMORY_MIB..=MAX_MEMORY_MIB`).
    pub fn memory_mib(&self) -> u32 {
        self.memory_mib
    }

    /// The firmware/boot choice.
    pub fn firmware(&self) -> Firmware {
        self.firmware
    }

    /// The deterministic NIC MAC.
    pub fn mac(&self) -> MacAddress {
        self.mac
    }

    /// The system disk attachment (always present).
    pub fn system_disk(&self) -> &DiskAttachment {
        &self.system
    }

    /// The source disk attachment, if one is configured.
    pub fn source_disk(&self) -> Option<&DiskAttachment> {
        self.source.as_ref()
    }

    /// Verifies every attached disk image exists and is a regular file. This
    /// is a host-state precondition checked at `create` time, kept out of the
    /// constructors so configuration validation performs no I/O.
    pub fn ensure_disks_present(&self) -> Result<(), DefinitionError> {
        self.system.ensure_present()?;
        if let Some(source) = &self.source {
            source.ensure_present()?;
        }
        Ok(())
    }
}

/// Why a [`BveDefinition`], [`BveId`], or [`DiskAttachment`] was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DefinitionError {
    /// The id is empty, too long, has a leading `-`/`.`, or contains a
    /// character outside `[A-Za-z0-9_-]`.
    #[error(
        "invalid BVE id {id:?}: expected 1..={} chars of [A-Za-z0-9_-] \
         not starting with '-' or '.'",
        MAX_ID_LEN
    )]
    InvalidId { id: String },

    /// A BVE needs at least one vCPU.
    #[error("BVE must have at least one vCPU")]
    ZeroVcpus,

    /// More vCPUs were requested than this runtime accepts.
    #[error("unsupported vCPU count {vcpus} (max {})", MAX_VCPUS)]
    VcpusUnsupported { vcpus: u32 },

    /// RAM was zero.
    #[error("BVE memory must be greater than zero")]
    ZeroMemory,

    /// RAM was non-zero but outside the accepted range.
    #[error(
        "unsupported memory {memory_mib} MiB (accepted {}..={} MiB)",
        MIN_MEMORY_MIB,
        MAX_MEMORY_MIB
    )]
    MemoryUnsupported { memory_mib: u32 },

    /// A disk path cannot be safely represented in a QEMU `-drive file=...`
    /// value (it contains `,`, a newline, or a carriage return).
    #[error(
        "unsupported disk image path {path}: a QEMU -drive path must not contain ',' or a newline"
    )]
    UnsupportedDiskPath { path: PathBuf },

    /// An attachment was supplied for the wrong disk slot.
    #[error("expected a {expected:?} disk attachment, got {found:?}")]
    WrongDiskRole {
        /// The role the slot requires.
        expected: DiskRole,
        /// The role the supplied attachment carries.
        found: DiskRole,
    },

    /// A disk image path does not exist.
    #[error("{role:?} disk image not found: {path}")]
    DiskImageMissing {
        /// The affected role.
        role: DiskRole,
        /// The missing path.
        path: PathBuf,
    },

    /// A disk image path exists but is not a regular file.
    #[error("{role:?} disk image is not a regular file: {path}")]
    DiskImageNotAFile {
        /// The affected role.
        role: DiskRole,
        /// The offending path.
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bamep-ve-def-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn valid_id() -> BveId {
        BveId::new("bve-01").unwrap()
    }

    fn system(path: impl Into<PathBuf>) -> DiskAttachment {
        DiskAttachment::system(path, DiskFormat::Qcow2).unwrap()
    }

    #[test]
    fn accepts_reasonable_ids() {
        for good in ["a", "bve-01", "LAB_03", "x1-y2_z3", &"n".repeat(MAX_ID_LEN)] {
            assert!(BveId::new(good).is_ok(), "expected {good:?} to be valid");
        }
    }

    #[test]
    fn rejects_unsafe_or_malformed_ids() {
        for bad in [
            "",
            "..",
            ".",
            ".hidden",
            "-flag",
            "a/b",
            "a\\b",
            "a b",
            "abc$",
            "café",
            &"n".repeat(MAX_ID_LEN + 1),
        ] {
            assert_eq!(
                BveId::new(bad),
                Err(DefinitionError::InvalidId {
                    id: bad.to_string()
                }),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn disk_attachment_constructors_fix_the_role() {
        assert_eq!(
            DiskAttachment::system("/x.qcow2", DiskFormat::Qcow2)
                .unwrap()
                .role(),
            DiskRole::System
        );
        assert_eq!(
            DiskAttachment::source("/y.raw", DiskFormat::Raw)
                .unwrap()
                .role(),
            DiskRole::Source
        );
    }

    #[test]
    fn disk_attachment_rejects_paths_that_break_qemu_drive_syntax() {
        for bad in ["/a,b.qcow2", "/line\nbreak.raw", "/carriage\rreturn.raw"] {
            assert_eq!(
                DiskAttachment::system(bad, DiskFormat::Raw),
                Err(DefinitionError::UnsupportedDiskPath {
                    path: PathBuf::from(bad)
                }),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_a_valid_definition() {
        let def = BveDefinition::new(
            valid_id(),
            2,
            512,
            Firmware::Default,
            system("/tmp/system.qcow2"),
        )
        .unwrap();
        assert_eq!(def.vcpus(), 2);
        assert_eq!(def.memory_mib(), 512);
        assert_eq!(def.firmware(), Firmware::Default);
        assert_eq!(def.system_disk().path(), Path::new("/tmp/system.qcow2"));
        assert_eq!(def.system_disk().format(), DiskFormat::Qcow2);
        assert!(def.source_disk().is_none());
    }

    #[test]
    fn new_rejects_a_non_system_attachment() {
        let source = DiskAttachment::source("/s.raw", DiskFormat::Raw).unwrap();
        assert_eq!(
            BveDefinition::new(valid_id(), 1, 256, Firmware::Default, source),
            Err(DefinitionError::WrongDiskRole {
                expected: DiskRole::System,
                found: DiskRole::Source,
            })
        );
    }

    #[test]
    fn with_source_rejects_a_non_source_attachment() {
        let def =
            BveDefinition::new(valid_id(), 1, 256, Firmware::Default, system("/s.qcow2")).unwrap();
        let wrong = DiskAttachment::system("/again.qcow2", DiskFormat::Qcow2).unwrap();
        assert_eq!(
            def.with_source(wrong),
            Err(DefinitionError::WrongDiskRole {
                expected: DiskRole::Source,
                found: DiskRole::System,
            })
        );
    }

    #[test]
    fn with_source_attaches_an_independent_source_disk() {
        let def =
            BveDefinition::new(valid_id(), 1, 256, Firmware::Default, system("/s.qcow2")).unwrap();
        let src = DiskAttachment::source("/src.raw", DiskFormat::Raw).unwrap();
        let def = def.with_source(src).unwrap();
        assert_eq!(def.source_disk().unwrap().path(), Path::new("/src.raw"));
        assert_eq!(def.source_disk().unwrap().format(), DiskFormat::Raw);
    }

    #[test]
    fn rejects_zero_vcpus() {
        assert_eq!(
            BveDefinition::new(valid_id(), 0, 512, Firmware::Default, system("/d.qcow2")),
            Err(DefinitionError::ZeroVcpus)
        );
    }

    #[test]
    fn rejects_too_many_vcpus() {
        assert_eq!(
            BveDefinition::new(
                valid_id(),
                MAX_VCPUS + 1,
                512,
                Firmware::Default,
                system("/d.qcow2")
            ),
            Err(DefinitionError::VcpusUnsupported {
                vcpus: MAX_VCPUS + 1
            })
        );
    }

    #[test]
    fn rejects_zero_memory() {
        assert_eq!(
            BveDefinition::new(valid_id(), 1, 0, Firmware::Default, system("/d.qcow2")),
            Err(DefinitionError::ZeroMemory)
        );
    }

    #[test]
    fn rejects_memory_below_minimum_and_absurdly_large() {
        assert_eq!(
            BveDefinition::new(
                valid_id(),
                1,
                MIN_MEMORY_MIB - 1,
                Firmware::Default,
                system("/d.qcow2")
            ),
            Err(DefinitionError::MemoryUnsupported {
                memory_mib: MIN_MEMORY_MIB - 1
            })
        );
        assert_eq!(
            BveDefinition::new(
                valid_id(),
                1,
                MAX_MEMORY_MIB + 1,
                Firmware::Default,
                system("/d.qcow2")
            ),
            Err(DefinitionError::MemoryUnsupported {
                memory_mib: MAX_MEMORY_MIB + 1
            })
        );
    }

    #[test]
    fn deterministic_mac_is_stable_and_locally_administered() {
        let a = MacAddress::deterministic_for(&BveId::new("lab-03").unwrap());
        let b = MacAddress::deterministic_for(&BveId::new("lab-03").unwrap());
        let c = MacAddress::deterministic_for(&BveId::new("lab-04").unwrap());

        assert_eq!(a, b, "same id -> same MAC");
        assert_ne!(a, c, "different id -> different MAC");
        assert_eq!(&a.octets()[..3], &MacAddress::QEMU_OUI);
        assert!(a.is_locally_administered_unicast());
    }

    #[test]
    fn mac_display_is_lowercase_colon_hex() {
        let mac = MacAddress::from_octets([0x52, 0x54, 0x00, 0x0a, 0xbc, 0x09]);
        let text = mac.to_string();
        assert_eq!(text, "52:54:00:0a:bc:09");
        assert_eq!(text.len(), 17);
    }

    #[test]
    fn fnv1a_64_is_a_stable_known_vector() {
        // FNV-1a 64-bit of "" and of "a" — fixed reference values.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn ensure_disks_present_accepts_regular_files_and_rejects_the_rest() {
        let dir = temp_dir();
        let sys = dir.join("system.qcow2");
        let src = dir.join("source.raw");
        std::fs::File::create(&sys)
            .unwrap()
            .write_all(&[0u8; 64])
            .unwrap();
        std::fs::File::create(&src)
            .unwrap()
            .write_all(&[0u8; 64])
            .unwrap();

        let def = BveDefinition::new(
            valid_id(),
            1,
            256,
            Firmware::Default,
            DiskAttachment::system(&sys, DiskFormat::Qcow2).unwrap(),
        )
        .unwrap()
        .with_source(DiskAttachment::source(&src, DiskFormat::Raw).unwrap())
        .unwrap();
        assert_eq!(def.ensure_disks_present(), Ok(()));

        let missing = dir.join("gone.qcow2");
        let bad = BveDefinition::new(
            valid_id(),
            1,
            256,
            Firmware::Default,
            DiskAttachment::system(&missing, DiskFormat::Qcow2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            bad.ensure_disks_present(),
            Err(DefinitionError::DiskImageMissing {
                role: DiskRole::System,
                path: missing,
            })
        );

        let dir_as_disk = BveDefinition::new(
            valid_id(),
            1,
            256,
            Firmware::Default,
            DiskAttachment::system(&dir, DiskFormat::Qcow2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            dir_as_disk.ensure_disks_present(),
            Err(DefinitionError::DiskImageNotAFile {
                role: DiskRole::System,
                path: dir.clone(),
            })
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
