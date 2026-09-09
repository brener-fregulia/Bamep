# ADR-0022: BVE Linux/QEMU/KVM Reference Backend

Status: Accepted

## Context

`docs/specifications/m0-bamep-virtual-endpoint-contract.md` defines the Bamep Virtual
Endpoint (BVE) responsibility, Simulator relationship, minimal lifecycle, and fidelity
boundary, but deliberately leaves the concrete initial virtualization backend, reference
host, and acceleration path as an explicit architectural decision rather than an
implementation detail improvised inside Issue #67.

`docs/development/testing.md` already establishes Linux as the reference environment for
Bamep Server, Agent, Worker, and Simulator, and allows WSL2 for Linux-targeted development
without treating it as a faithful substitute for physical hardware behavior. The Bamep lab
(Fedora Server today, Debian planned) is Linux. No BVE backend code exists yet in the
repository: this is a greenfield decision, not a migration.

Issue #66 requires this decision to leave Issue #67 (a single disposable BVE VM) able to
begin implementation without reopening architecture.

## Decision

- **Linux** is the initial reference host for BVE, consistent with the existing Server/
  Worker/Simulator reference environment.
- **QEMU** is the initial virtualization backend, driven directly (no libvirt) by whichever
  process owns BVE lifecycle in the future implementation.
- **KVM hardware acceleration** is the initial acceleration path. The initial implementation
  must fail clearly rather than silently fall back to full software CPU emulation when
  usable KVM acceleration is unavailable.
- No `VirtualizationBackend`-style multi-hypervisor abstraction is introduced now. Nothing
  in `bamep-simulator` or the future BVE responsibility may be shaped to anticipate a second
  backend before a second backend is an actual requirement.
- No `libvirt` dependency is introduced now.
- Native Windows/Hyper-V BVE host support is deferred.
- VirtualBox, VMware, and VFIO/PCI passthrough are not part of the initial implementation.

This decision binds the initial implementation only. It does not redefine BVE's normative
responsibility, lifecycle, or fidelity boundary, which remain backend-independent per
`m0-bamep-virtual-endpoint-contract.md`. A future backend requirement is a new or updated
ADR, not a silent code-level abstraction.

## Alternatives considered

- **libvirt from day one.** Rejected. Adds a dependency, daemon, and abstraction layer
  before any second backend or orchestration requirement exists to justify it.
- **Native Windows/Hyper-V as the initial host.** Rejected as the initial reference. Linux
  is already the accepted Server/Worker/Simulator/lab reference environment; a Windows-
  hosted BVE would diverge from that parity for no current requirement. Development
  machines may still use WSL2 the same way `testing.md` already allows for other Linux-
  targeted work.
- **VirtualBox or VMware.** Rejected. Neither is already part of the accepted Linux dev/lab
  toolchain; adopting either would add a new dependency with no current requirement it
  satisfies better than QEMU/KVM.
- **A multi-hypervisor abstraction layer now.** Rejected. No second backend requirement
  exists yet; Issue #66 explicitly flags this as a premature-abstraction risk. Introducing
  the abstraction before a second implementation exists to validate it would be designed
  against guesses, not evidence.
- **VFIO/PCI passthrough for BVE virtual NICs/disks.** Rejected for the initial backend.
  It targets physical-device fidelity questions that remain physical Integration
  Environment authority per `m0-bamep-virtual-endpoint-contract.md`, not the software-
  visible VM behavior BVE is scoped to.

## Consequences

- Issue #67 may implement BVE lifecycle directly against QEMU process control with KVM
  acceleration, without designing or reviewing a backend abstraction first.
- A future backend requirement (a second hypervisor, native Windows hosting, libvirt
  adoption) requires revisiting this ADR rather than being decided implicitly in code.
- BVE's own responsibility/lifecycle Specification stays reusable if the backend changes
  later, because it was written independently of this decision.
- No `docs/architecture/README.md` change: no BVE code exists yet.

## Related architecture

None yet. BVE is unimplemented; `docs/architecture/README.md` is updated only once
corresponding code exists.

## Related work

- `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — BVE responsibility and
  lifecycle this decision supplies a backend for.
- Issue #66 — Work Package that required this decision.
- Issue #67 — first implementation Work Package this decision unblocks.
