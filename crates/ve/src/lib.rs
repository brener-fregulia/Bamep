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
//! Scope boundary for Issue #67: no guest OS, no PXE/DHCP/TAP, no WinPE, no
//! Buildroot, no Simulator orchestration, no deterministic storage/reset
//! semantics (Issue #69 owns disk retention/overlay/reset). A blank throwaway
//! disk is sufficient to prove the lifecycle. This crate does **not** depend
//! on `bamep-agent-protocol` or any Agent Protocol semantics.
//!
//! The lifecycle is synchronous: one VM, one owned `std::process::Child`, one
//! QMP Unix socket. No async runtime is pulled in for it.

pub mod definition;
pub mod qemu;
pub mod qmp;
pub mod runtime;

pub use definition::{BveDefinition, BveId, DefinitionError, Firmware, MacAddress};
pub use qemu::{
    check_kvm_device, check_qemu_binary, detect_host_prerequisites, HostPrerequisites,
    PrerequisiteError, QemuCommand, DEFAULT_KVM_DEVICE, QEMU_BINARY,
};
pub use qmp::{QmpConnection, QmpError, QmpResponse, RunState};
pub use runtime::{BveRuntime, LifecycleState, RuntimeError, RuntimeRoot};
