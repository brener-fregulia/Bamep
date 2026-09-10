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
//! Networking (Issue #70; ADR-0024): a BVE NIC is user-mode SLIRP by default
//! (unprivileged, unchanged), or attached to an **isolated, PXE-capable**
//! provisioning network — a private host bridge + a user-owned TAP + a veth
//! into a dedicated netns that holds a disposable DHCP/PXE fixture, with no
//! physical uplink, no default route and no NAT. Creating those host resources
//! needs `CAP_NET_ADMIN` and is an explicit owner-run step ([`network`]); this
//! crate never calls `sudo` and never changes host firewall/routing. Once the
//! TAP exists, QEMU opens it as the normal user and the VM lifecycle stays
//! unprivileged. A minimal [`BootMode::NetworkFirst`] makes the firmware
//! attempt PXE.
//!
//! This crate does **not** depend on `bamep-agent-protocol` or any Agent
//! Protocol semantics. The lifecycle is synchronous: one VM, one owned
//! `std::process::Child`, one QMP Unix socket. No async runtime.

pub mod definition;
pub mod network;
pub mod qemu;
pub mod qmp;
pub mod runtime;
pub mod storage;

pub use definition::{
    fnv1a_64, BootMode, BveDefinition, BveId, DefinitionError, DiskAttachment, DiskFormat,
    DiskRole, Firmware, IfName, MacAddress, NetworkAttachment, NicModel, MAX_IFNAME_LEN,
};
pub use network::{
    apply_bridged_forward_accommodation, apply_dhcp_forward_accommodation, assert_l2_isolation,
    bridged_forward_accommodation_rules, bve_run_dir, check_network_prerequisites,
    dhcp_forward_accommodation_rules, fixture_command, fixture_dnsmasq_argv, fixture_lease_file,
    fixture_pid_file, fixture_run_dir, prepare as prepare_network,
    remove_bridged_forward_accommodation, remove_dhcp_forward_accommodation, residual_resources,
    teardown as teardown_network, winpe_boot_ipxe_script, winpe_fixture_dnsmasq_argv,
    winpe_http_base_url, winpe_http_fixture_command, winpe_http_root, winpe_tftp_root,
    BveNetworkError, BveNetworkPlan, NetResource, NetResourceKind, PreparedBveNetwork,
    DNSMASQ_BINARY, FIXTURE_DHCP_FIRST, FIXTURE_DHCP_LAST, FIXTURE_PEER_CIDR, FIXTURE_PEER_IP,
    IPTABLES_BINARY, IP_BINARY, PYTHON3_BINARY, TUN_DEVICE, WINPE_FIXTURE_HTTP_PORT,
    WINPE_TFTP_BOOTFILE,
};
pub use qemu::{
    check_kvm_device, check_qemu_binary, check_uefi_firmware, detect_host_prerequisites,
    ovmf_code_path, ovmf_vars_template_path, HostPrerequisites, PrerequisiteError, QemuCommand,
    UefiPflash, DEFAULT_KVM_DEVICE, OVMF_CODE_4M, OVMF_CODE_ENV, OVMF_VARS_4M_TEMPLATE,
    OVMF_VARS_TEMPLATE_ENV, QEMU_BINARY,
};
pub use qmp::{QmpConnection, QmpError, QmpResponse, RunState};
pub use runtime::{BveRuntime, LifecycleState, RuntimeError, RuntimeRoot, UEFI_VARS_FILENAME};
pub use storage::{
    check_qemu_img_binary, destroy_instance_storage, ensure_system_base, prepare_instance,
    reset_system_storage, BveStorageError, BveStorageLayout, BveStorageRoot,
    PreparedInstanceStorage, SourceDiskSpec, SystemBaseSpec, DEFAULT_SOURCE_BYTES,
    DEFAULT_SYSTEM_BASE_BYTES, QEMU_IMG_BINARY,
};
