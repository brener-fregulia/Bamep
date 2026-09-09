//! Bamep Virtual Endpoint (BVE) host-side runtime.
//!
//! This crate implements the smallest host-side runtime that can represent
//! and control **one disposable virtual Endpoint** as a real QEMU/KVM virtual
//! machine (Issue #67). It owns the machine's lifecycle and control plumbing
//! and nothing above it.
//!
//! Authorities:
//!
//! - `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — the
//!   normative BVE responsibility and the backend-independent
//!   `create/start/observe/reset/stop/destroy` lifecycle this crate
//!   implements.
//! - `docs/decisions/0022-bve-linux-qemu-kvm-reference-backend.md` — Linux
//!   reference host, QEMU driven directly (no libvirt), KVM acceleration, and
//!   **no** silent fallback to full software CPU emulation. No
//!   multi-hypervisor abstraction is introduced: this crate targets QEMU/KVM
//!   directly.
//!
//! Storage (Issue #69; ADR-0023): a sparse RAW immutable **base**, a
//! per-instance disposable QCOW2 **system overlay** backed by it, and an
//! independent RAW **source** fixture. `system_reset` (VM reboot, QMP) and
//! `reset_system_storage` (discard + recreate the disposable overlay from the
//! same base) are distinct operations. Storage-image work needs `qemu-img`;
//! VM lifecycle execution still needs only `qemu-system-x86_64` + KVM.
//!
//! This crate does **not** depend on `bamep-agent-protocol` or any Agent
//! Protocol semantics. The lifecycle is synchronous: one VM, one owned
//! `std::process::Child`, one QMP Unix socket. No async runtime.

pub mod definition;
pub mod qemu;
pub mod qmp;
pub mod runtime;
pub mod storage;

pub use definition::{
    fnv1a_64, BveDefinition, BveId, DefinitionError, DiskAttachment, DiskFormat, DiskRole,
    Firmware, MacAddress,
};
pub use qemu::{
    check_kvm_device, check_qemu_binary, detect_host_prerequisites, HostPrerequisites,
    PrerequisiteError, QemuCommand, DEFAULT_KVM_DEVICE, QEMU_BINARY,
};
pub use qmp::{QmpConnection, QmpError, QmpResponse, RunState};
pub use runtime::{BveRuntime, LifecycleState, RuntimeError, RuntimeRoot};
pub use storage::{
    check_qemu_img_binary, destroy_instance_storage, ensure_system_base, prepare_instance,
    reset_system_storage, BveStorageError, BveStorageLayout, BveStorageRoot,
    PreparedInstanceStorage, SourceDiskSpec, SystemBaseSpec, DEFAULT_SOURCE_BYTES,
    DEFAULT_SYSTEM_BASE_BYTES, QEMU_IMG_BINARY,
};
