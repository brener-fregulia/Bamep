# M0 — Bamep Virtual Endpoint (BVE) Contract

Status: **Proposed**

This Specification defines the normative responsibility, Simulator relationship, minimal
lifecycle, and validation/fidelity boundary of the Bamep Virtual Endpoint (BVE). It does not
select or justify a concrete virtualization backend; that decision belongs to ADR-0022.
Storage retention/reset/disposal semantics are intentionally left undefined by this
Specification; they are currently tracked as follow-up work in Issue #69.

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
concrete package structure (illustratively a future `bamep-bve` crate) is implementation
work for the Work Package that first implements a BVE.

## Lifecycle

A BVE exposes the following minimal semantic lifecycle:

| Operation | Meaning |
| --- | --- |
| `create` | Establishes the instance/definition needed to represent one BVE. |
| `start` | Begins execution of that instance. |
| `observe` | Returns truthful lifecycle state. It must not infer or imply guest OS/Agent health. |
| `reset` | Resets execution/volatile machine state of the existing BVE instance. It does not create a replacement instance or imply guest OS/Agent health. |
| `stop` | Ends execution of that instance. |
| `destroy` | Removes the transitory lifecycle/control resources owned by that instance. |

`destroy` is scoped to lifecycle/control resources only. This Specification does not define
storage retention/disposal semantics (for example: whether a system disk, an overlay, or a
source fixture is deleted, preserved, or reset). That contract belongs to a later Work
Package (currently anticipated as Issue #69).

This lifecycle is intentionally backend-independent. It is the contract a consumer (such as
the Simulator) programs against; it does not itself select QEMU/KVM or any other mechanism.

## Validation and fidelity boundary

`docs/development/testing.md` owns the general test-layer model and the existing WSL2/
container fidelity boundary; this section does not restate it.

A BVE may provide useful evidence for software-visible, virtualized behavior, illustratively:
VM lifecycle; virtual firmware/UEFI behavior; virtual NIC/block devices; reboot/reset;
virtual PXE flow; WinPE/Buildroot boot inside the VM; guest Agent execution; and protocol/
data-plane behavior crossing the guest's virtual network boundary.

A BVE is never authoritative for physical behavior, illustratively: real motherboard
firmware; real NIC/driver/offload behavior; physical SATA/NVMe controller behavior; real
SSD/HDD characteristics; PHY/switch/cabling behavior; real Secure Boot interoperability;
real hardware PXE compatibility; and physical network throughput. The physical Integration
Environment remains the authority for those claims.

## Backend

The initial supported virtualization backend, reference host, and acceleration path are
owned by ADR-0022. This Specification does not restate or duplicate that decision. A future backend change is a new/updated ADR; it does not by itself
change the responsibility, lifecycle, or fidelity boundary defined above.

## Out of scope

- qcow2 vs. raw, backing images, overlays, and storage reset/disposal policy (Issue #69);
- disk size/vCPU/RAM defaults or profiles;
- disk attachment details, TAP/bridge/macvtap, and DHCP;
- PXE implementation and PXE service topology;
- WinPE assets and Buildroot configuration;
- production Agent implementation;
- multi-BVE orchestration;
- native Windows/Hyper-V BVE host support, libvirt, VirtualBox/VMware, and VFIO/passthrough
  (see ADR-0022);
- CI integration;
- traffic shaping and throughput thresholds;
- port allocation, console mechanism, and backend process/control-supervision
  implementation details;
- concrete package/crate materialization.

## Related

- ADR-0022 — Linux/QEMU/KVM initial BVE backend decision.
- `docs/development/testing.md` — general test-layer model and WSL2/container fidelity
  boundary.
- `docs/specifications/m0-simulator-contract-and-validation-strategy.md` — Simulator
  fidelity boundary and Agent-side protocol contract this Specification does not redefine.
- Issue #66 — Work Package that produced this Specification.
- Issue #67 — first implementation Work Package this Specification unblocks.
