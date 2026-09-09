//! `BveDefinition`: the minimal, validated description of one virtual
//! Endpoint machine for Issue #67.
//!
//! This is deliberately not a future-complete configuration surface
//! (`m0-bamep-virtual-endpoint-contract.md` — the responsibility list "is not
//! a schema" and "must not be read as a future-complete configuration
//! surface"). It carries only what launching one QEMU/KVM VM needs now:
//! identity, vCPU count, RAM, a minimal firmware/boot choice, a deterministic
//! NIC MAC, and one system-disk reference. Fields for later Work Packages
//! (storage reset, PXE, WinPE, Buildroot) are intentionally absent.

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
/// segment (the per-instance runtime directory) and as QEMU's `-name` value.
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
/// Packages and is not made a mandatory dependency here
/// (`m0-bamep-virtual-endpoint-contract.md` "Out of scope"). This enum exists
/// so the choice is explicit in the definition, not so it is a firmware
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    /// QEMU's built-in default firmware for the target machine (SeaBIOS on
    /// `x86_64`). No `-bios`/`-pflash` is passed.
    Default,
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
    /// same MAC on every host and every build — "deterministic virtual NIC
    /// MAC" (Issue #67).
    pub fn deterministic_for(id: &BveId) -> Self {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut hash = FNV_OFFSET;
        for byte in id.as_str().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
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

/// The validated definition of one BVE.
///
/// Construct with [`BveDefinition::new`]; every field is validated there
/// except system-disk presence, which is a host-state check deferred to
/// [`BveDefinition::ensure_system_disk_present`] (called by
/// [`crate::runtime::BveRuntime::create`]) so pure configuration logic stays
/// filesystem-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BveDefinition {
    id: BveId,
    vcpus: u32,
    memory_mib: u32,
    firmware: Firmware,
    mac: MacAddress,
    system_disk: PathBuf,
}

impl BveDefinition {
    /// Validates resource values and builds a definition. The NIC MAC is
    /// derived deterministically from the id.
    ///
    /// Rejects: zero vCPUs, more than [`MAX_VCPUS`], zero RAM, RAM outside
    /// `[MIN_MEMORY_MIB, MAX_MEMORY_MIB]`. The id is already validated by its
    /// type.
    pub fn new(
        id: BveId,
        vcpus: u32,
        memory_mib: u32,
        firmware: Firmware,
        system_disk: impl Into<PathBuf>,
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

        let mac = MacAddress::deterministic_for(&id);
        Ok(Self {
            id,
            vcpus,
            memory_mib,
            firmware,
            mac,
            system_disk: system_disk.into(),
        })
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

    /// The system-disk path (not guaranteed to exist; see
    /// [`BveDefinition::ensure_system_disk_present`]).
    pub fn system_disk(&self) -> &Path {
        &self.system_disk
    }

    /// Verifies the system disk exists and is a regular file. This is a
    /// host-state precondition checked at `create` time, kept out of
    /// [`BveDefinition::new`] so configuration validation performs no I/O.
    pub fn ensure_system_disk_present(&self) -> Result<(), DefinitionError> {
        let meta = std::fs::metadata(&self.system_disk).map_err(|_| {
            DefinitionError::SystemDiskMissing {
                path: self.system_disk.clone(),
            }
        })?;
        if !meta.is_file() {
            return Err(DefinitionError::SystemDiskNotAFile {
                path: self.system_disk.clone(),
            });
        }
        Ok(())
    }
}

/// Why a [`BveDefinition`] (or [`BveId`]) was rejected.
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

    /// The system disk path does not exist.
    #[error("system disk not found: {path}")]
    SystemDiskMissing { path: PathBuf },

    /// The system disk path exists but is not a regular file.
    #[error("system disk is not a regular file: {path}")]
    SystemDiskNotAFile { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bamep-bve-def-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn valid_id() -> BveId {
        BveId::new("bve-01").unwrap()
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
    fn accepts_a_valid_definition() {
        let def =
            BveDefinition::new(valid_id(), 2, 512, Firmware::Default, "/tmp/disk.raw").unwrap();
        assert_eq!(def.vcpus(), 2);
        assert_eq!(def.memory_mib(), 512);
        assert_eq!(def.firmware(), Firmware::Default);
        assert_eq!(def.system_disk(), Path::new("/tmp/disk.raw"));
    }

    #[test]
    fn rejects_zero_vcpus() {
        assert_eq!(
            BveDefinition::new(valid_id(), 0, 512, Firmware::Default, "/tmp/d.raw"),
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
                "/tmp/d.raw"
            ),
            Err(DefinitionError::VcpusUnsupported {
                vcpus: MAX_VCPUS + 1
            })
        );
    }

    #[test]
    fn rejects_zero_memory() {
        assert_eq!(
            BveDefinition::new(valid_id(), 1, 0, Firmware::Default, "/tmp/d.raw"),
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
                "/tmp/d.raw"
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
                "/tmp/d.raw"
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
    fn ensure_system_disk_present_accepts_a_regular_file() {
        let dir = temp_dir();
        let disk = dir.join("system.raw");
        let mut f = std::fs::File::create(&disk).unwrap();
        f.write_all(&[0u8; 64]).unwrap();

        let def = BveDefinition::new(valid_id(), 1, 256, Firmware::Default, &disk).unwrap();
        assert_eq!(def.ensure_system_disk_present(), Ok(()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_system_disk_present_rejects_missing_and_non_file() {
        let dir = temp_dir();

        let missing = dir.join("nope.raw");
        let def = BveDefinition::new(valid_id(), 1, 256, Firmware::Default, &missing).unwrap();
        assert_eq!(
            def.ensure_system_disk_present(),
            Err(DefinitionError::SystemDiskMissing { path: missing })
        );

        let def_dir = BveDefinition::new(valid_id(), 1, 256, Firmware::Default, &dir).unwrap();
        assert_eq!(
            def_dir.ensure_system_disk_present(),
            Err(DefinitionError::SystemDiskNotAFile { path: dir.clone() })
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
