# M0 — Bamep Virtual Endpoint (BVE) Contract

Status: **Approved**

This Specification defines the normative responsibility, Simulator relationship, minimal
lifecycle, disposable-storage semantics, and validation/fidelity boundary of the Bamep
Virtual Endpoint (BVE). It does not select or justify a concrete virtualization backend or
image format; the backend decision belongs to ADR-0022 and the initial disk storage
mechanism to ADR-0023.

BVE is transversal validation/development infrastructure related to the Simulator, not a
component of the M2 Endpoint Capture product surface, even though it is being prioritized
during M2 development.

## Responsibility

A BVE represents and controls **one independently controllable virtual Endpoint machine**.
It owns the lifecycle and resources needed to represent that machine, illustratively:

- virtual CPU/resources;
- RAM;
- firmware/boot environment;
- virtual NIC(s);
- virtual disk(s);
- boot ordering;
- lifecycle/power/reset control;
- minimal control/observation plumbing.

This list describes responsibility, not a schema. It is not exhaustive and must not be read
as a future-complete configuration surface.

A BVE ends before:

- Agent Protocol semantics;
- Job/JobStep/Attempt interpretation;
- Server/Domain business logic;
- production Agent behavior;
- interpretation of any Bamep-defined action.

A BVE does not speak to the Server as its own responsibility. Software that later runs
inside the guest — a Simulator participant, a WinPE-hosted test participant, a future
Buildroot-hosted participant, or the production Agent — may participate in Agent Protocol;
that participation belongs to the guest software, not to BVE.

## Relationship to the Simulator

```text
Simulator
    |
    | may orchestrate / consume
    v
BVE
    |
    v
virtualization backend
```

`bamep-simulator` remains the Agent-side participant that crosses the real Agent Protocol
and data-plane boundaries, per
`docs/specifications/m0-simulator-contract-and-validation-strategy.md`. BVE does not replace
or redefine that role.

The Simulator may continue running lightweight in-process participants when VM-level
fidelity adds no useful evidence. BVE is used when isolation of machine/kernel/process/
NIC/disk/boot genuinely matters to the scenario. Where a Simulator scenario chooses to use
one, BVE is consumed as an execution backend: it must never need to understand Agent
Protocol, Job/Attempt semantics, or any other Simulator-owned contract to fulfill its own
responsibility.

BVE is not a new formal test layer. It is an execution substrate/backend of higher fidelity
that an automated validation scenario may select; the layered validation model owned by
`docs/development/testing.md` is unchanged.

## Code boundary

BVE is a responsibility separate from the Simulator's Agent Protocol participant
responsibility. The expected dependency direction is a Simulator-side consumer depending on
a BVE-owning responsibility, never the reverse, and BVE must not depend on
`bamep-agent-protocol` or any Agent Protocol semantics merely to control a virtual machine.

This Specification does not materialize that boundary as a crate, module, or path. The
concrete package structure (the `bamep-ve` crate) is implementation work for the Work
Package that first implements a BVE.

## Lifecycle

A BVE exposes the following minimal semantic lifecycle:

| Operation | Meaning |
| --- | --- |
| `create` | Establishes the instance/definition needed to represent one BVE. |
| `start` | Begins execution of that instance. |
| `observe` | Returns truthful lifecycle state. It must not infer or imply guest OS/Agent health. |
| `reset` | Resets execution/volatile machine state of the existing BVE instance. It does not create a replacement instance, discard disk state, or imply guest OS/Agent health. |
| `stop` | Ends execution of that instance. |
| `destroy` | Removes the transitory lifecycle/control resources owned by that instance. It does not delete disk images. |

`destroy` is scoped to lifecycle/control resources only. Disk images are disposed by a
separate, explicit storage operation (see "Storage").

This lifecycle is intentionally backend-independent. It is the contract a consumer (such as
the Simulator) programs against; it does not itself select QEMU/KVM or any other mechanism.

## Storage

A BVE has a reproducible disk model with a distinct lifecycle from the machine:

- **Known base state.** A BVE derives its system disk from one identified, immutable base
  state. Ordinary BVE operation — running the machine, `reset`, `stop`, `destroy` — must not
  mutate that base.
- **Disposable writable system instance.** The disk the guest boots and writes to is a
  per-instance, disposable instance derived from the base. Guest writes land in the
  disposable instance, not the base.
- **Storage reset is distinct from `reset`.** `reset` reboots the running machine and
  changes no disk state. A separate storage-reset operation discards the disposable system
  instance and derives a fresh one from the same base; it does not reboot a running
  machine, reinstall any guest, or create a replacement BVE instance. Two consecutive
  storage resets begin from the same base state.
- **Independent source fixture.** A BVE may carry one additional virtual disk that is
  independent of the system disk, deterministically identified, and separate from the boot
  path. Storage reset does not alter it; it is removed only by explicit instance-storage
  disposal.
- **Scoped, fail-closed deletion.** Storage reset and instance-storage disposal act only on
  disk state owned by that BVE instance, derived from a validated instance-owned location.
  They never delete the base, another BVE's storage, or an arbitrary host path, and an
  unexpected leftover fails closed rather than escalating to a broad recursive delete.

The concrete image format, backing mechanism, host tooling, sparse-allocation technique,
and the initial Windows-oriented logical capacity are an implementation decision owned by
ADR-0023, not this Specification. A future change to that model is a new/updated ADR.

## Provisioning network

A BVE's virtual NIC may be attached either to a lightweight unprivileged path or to an
**isolated, Layer-2-capable provisioning network** suitable for observing guest
firmware PXE/DHCP discovery:

- **Deterministic identity is preserved.** The BVE keeps its deterministic NIC MAC
  regardless of attachment; a MAC address remains inventory evidence, not identity.
- **Isolation.** The isolated network has no physical uplink, assigns the BVE no route to
  a real network, and performs no NAT. Any DHCP/PXE peer used to exercise the exchange is
  a disposable validation fixture confined to that isolated network — never production
  DHCP/PXE service, and never exposed to the developer's normal network.
- **Explicit, scoped, fail-closed host-resource lifecycle.** Host network resources for an
  isolated BVE network are created and destroyed by explicit operations, act only on
  resources that operation owns (derived, deterministic names — never an arbitrary or
  pattern-matched host object), refuse to adopt a pre-existing resource, roll back a
  partial setup to exactly what it created, and fail closed on an unexpected state rather
  than escalating to a broad delete.
- **Privilege is bounded.** Creating the isolated network may require host
  network-administration privilege; that is an explicit preparation/teardown step. It is
  not a precondition for the lightweight path, and the BVE runtime never escalates
  privilege on its own.

The concrete host virtual-network mechanism (bridge/TAP/veth/namespace layout), the
disposable DHCP/PXE fixture tooling, boot-order handling for firmware network boot, and
any host firewall/routing interaction are an implementation decision owned by ADR-0024,
not this Specification. Production PXE/DHCP delivery architecture is owned elsewhere
(ADR-0021) and is out of scope here.

## Validation and fidelity boundary

`docs/development/testing.md` owns the general test-layer model and the existing WSL2/
container fidelity boundary; this section does not restate it.

A BVE may provide useful evidence for software-visible, virtualized behavior, illustratively:
VM lifecycle; virtual firmware/UEFI behavior; virtual NIC/block devices; reboot/reset;
virtual PXE flow; guest firmware DHCP/PXE discovery crossing the virtual NIC onto an
isolated virtual network; WinPE/Buildroot boot inside the VM; guest Agent execution; and
protocol/data-plane behavior crossing the guest's virtual network boundary.

A BVE is never authoritative for physical behavior, illustratively: real motherboard
firmware; real NIC/driver/option-ROM/offload behavior; physical SATA/NVMe controller
behavior; real SSD/HDD characteristics; PHY/switch/cabling/VLAN behavior; real Secure Boot
interoperability; real hardware PXE compatibility; physical DHCP coexistence; and physical
network throughput. Host-internal virtual-network evidence is not physical-network
evidence. The physical Integration Environment remains the authority for those claims.

## Backend

The initial supported virtualization backend, reference host, and acceleration path are
owned by ADR-0022. This Specification does not restate or duplicate that decision. A future backend change is a new/updated ADR; it does not by itself
change the responsibility, lifecycle, or fidelity boundary defined above.

## Out of scope

- concrete image format/backing mechanism, host image tooling, sparse-allocation
  technique, and disk capacity values (ADR-0023);
- guest OS, filesystem, or partition content in the base; snapshot trees, image catalogs,
  deduplication, compression, encryption, and remote/cross-host image distribution;
- vCPU/RAM defaults or profiles;
- disk attachment details;
- the concrete host virtual-network mechanism, the disposable DHCP/PXE fixture tooling,
  and host firewall/routing interaction (ADR-0024);
- production PXE/DHCP implementation and production PXE/DHCP service topology;
- WinPE assets and Buildroot configuration;
- production Agent implementation;
- multi-BVE orchestration;
- native Windows/Hyper-V BVE host support, libvirt, VirtualBox/VMware, and VFIO/passthrough
  (see ADR-0022);
- CI integration;
- traffic shaping and throughput thresholds;
- port allocation, console mechanism, and backend process/control-supervision
  implementation details.

## Related

- ADR-0022 — Linux/QEMU/KVM initial BVE backend decision.
- ADR-0023 — initial BVE disk storage mechanism (sparse RAW base + per-instance QCOW2
  overlay).
- ADR-0024 — isolated BVE provisioning network (private bridge + TAP + fixture network
  namespace) and its privilege model.
- ADR-0025 — BVE virtual UEFI firmware (OVMF non-Secure-Boot, per-BVE writable VARS) and
  the independent `NicModel` choice.
- `docs/development/testing.md` — general test-layer model and WSL2/container fidelity
  boundary.
- `docs/specifications/m0-simulator-contract-and-validation-strategy.md` — Simulator
  fidelity boundary and Agent-side protocol contract this Specification does not redefine.
- Issue #66 — Work Package that produced this Specification.
- Issues #67–#71 — implementation Work Packages: one-BVE lifecycle, Simulator
  orchestration, deterministic storage and reproducible reset, the isolated
  PXE-capable provisioning network, and the virtual UEFI PXE boot of the
  existing WinPE path.
