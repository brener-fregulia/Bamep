# M0 — Product, Component, and Packaging Baseline

Status: **Approved**

This Specification defines the normative Bamep V1 product boundary, component/dependency
boundaries, boot-orchestration boundary, localization direction, and packaging/versioning
constraints.

## Product boundary

Bamep is a standalone bare-metal provisioning and recovery platform for controlled local
networks.

Its responsibilities include:

- Endpoint discovery/identity and inventory;
- boot and maintenance orchestration;
- provisioning and recovery workflows;
- Artifact transfer/management;
- resource-aware scheduling;
- secure, observable, auditable operation through explicit interfaces.

Bamep V1:

- provisions Windows, with Windows 11 as the primary modern target;
- supports UEFI x86-64 endpoints;
- uses a single-server deployment;
- assumes a dedicated provisioning interface/VLAN/network where Bamep may control DHCP/PXE;
- operates without Internet access once required artifacts are local;
- does not require MikroTik hardware, dedicated cache/archive storage, or RAID.

Legacy BIOS, multi-site operation, and HA are outside the V1 baseline unless later approved
work changes that scope.

Bamep is not an ERP, CRM, financial system, general-purpose RMM, NAS, or general
switch-management platform.

External commercial/ERP systems integrate through explicit versioned interfaces and/or
domain events, never Bamep's internal persistence schema.

Bamep Domain remains commercially agnostic: customer, contract, subscription, SKU, edition,
and similar commercial catalog concepts remain outside Domain. ADR-0015 owns the commercial
entitlement boundary.

## Localization

Bamep Web user-facing text uses localization boundaries rather than scattered hardcoded
strings.

- `pt-BR` is the initial UI locale.
- `en-US` is the planned additional locale.

Planned `en-US` support is not automatically an M0/current-M1 delivery requirement.

Localization library, catalog structure, loading, and fallback strategy are implementation
concerns unless later constrained.

Repository engineering language is separate from user-facing locale.

## Component boundaries

The normative responsibility boundaries are:

- **Presentation** — Web Administration and Administrative API.
- **Application** — Endpoint Management, Provisioning/Recovery Orchestration, Boot
  Orchestration, and Artifact Management.
- **Domain** — Endpoint, Job, JobStep, Attempt, Inventory, Artifact/Snapshot, Transfer,
  Storage Target, and Domain Events.
- **Runtime Services** — Scheduler/Resource Arbiter, Agent Control Gateway, Transfer
  Coordinator, and Runtime Presence Registry.
- **Ports** — repositories, Agent transport, boot, discovery, storage, and infrastructure
  metrics.
- **Adapters** — persistence, boot/PXE, switch, filesystem/storage, and protocol transports.
- **Workers** — transfer, compression, verification, and Artifact movement.

These are responsibility/dependency boundaries, not a required one-to-one mapping to
crates, packages, modules, directories, or processes.

ADR-0001 owns physical runtime topology and Worker isolation. ADR-0002 owns Server language.
ADR-0003 owns Worker/Agent language.

## Dependency constraints

Domain logic must not directly depend on infrastructure mechanisms such as:

- GRUB, iPXE, wimboot, or concrete PXE mechanics;
- MikroTik or another switch vendor;
- Linux device paths such as `/dev/sda`;
- shell tools such as `snmpwalk`;
- WebSocket/TLS libraries;
- PostgreSQL/SQLx;
- compression implementations such as zstd.

Infrastructure mechanisms stay behind the appropriate Ports/Adapters.

`docs/architecture/README.md` describes the currently implemented physical structure; it
may use a simpler subset of these logical boundaries.

## Communication boundaries

Different responsibilities may use different protocols:

- Agent ↔ Server control plane — ADR-0005 and `m0-agent-protocol-contract.md`;
- bulk Artifact transfer — ADR-0008 and `m0-data-plane-and-storage-contracts.md`;
- Web ↔ Server Administrative API — its own Specification.

A protocol selected for one boundary does not become a requirement for another.

Externally relevant contracts remain explicit and independently versioned rather than being
defined solely by shared Rust types.

### Presentation dependency boundary

Presentation clients — browser, desktop, or mobile — consume Bamep business state and
operations only through the applicable versioned Administrative API contract.

They must not bypass that contract through direct persistence access, Server-internal
models, or privileged native commands.

Native-only capabilities may be exposed through narrow platform adapters, but those
adapters must not become an alternative Application or Domain boundary.

ADR-0016 owns the Presentation client stack, static delivery model, and native-shell
platform boundary rationale.

## Boot orchestration

Domain must not depend on a concrete network-boot mechanism.

Boot mechanics are Adapter concerns coordinated through Application-level Boot
Orchestration and the boot Port. GRUB, iPXE, wimboot, PXE delivery, and equivalent future
mechanisms remain behind that boundary unless explicitly made normative later.

WinPE UEFI x86-64 viability has been validated in the tested environment; evidence is in
`docs/reference/winpe-boot-mechanism-spike.md`.

The production network-delivered WinPE mechanism remains unresolved in
`docs/discovery/architecture-redesign.md` and requires Integration Environment evidence
before production boot implementation.

Trusted-bootstrap integrity is orthogonal to network delivery. ADR-0010 owns the Secure
Boot/trusted-bootstrap decision; it does not select GRUB, iPXE, wimboot, or another
network-delivery mechanism.

## Packaging and versioning

- Server targets Linux, initially Debian.
- Production distribution direction is native `.deb` packages through a signed APT
  repository.
- Server must not silently self-update at application level.
- Server, Web, and Agent use independent SemVer.
- Workers ship/version with the Server release.
- Externally relevant contracts version independently from component SemVer.
- Independently deployable components do not require lockstep releases unless a compatibility
  contract explicitly requires it.

Examples of separately versioned contracts include Agent Protocol v1 and Administrative API
v1.

## Out of scope

This Specification does not define:

- the production WinPE network-delivery mechanism;
- concrete packaging-pipeline implementation;
- final backup/snapshot format;
- Endpoint, Job, persistence, data-plane, Agent Protocol, or Simulator contracts owned by
  their respective Specifications;
- ERP implementation or licensing enforcement;
- multi-site or HA behavior.

## Related

- ADR-0001 — runtime topology and Worker isolation.
- ADR-0002 — Server language.
- ADR-0003 — Worker and Agent language.
- ADR-0010 — trusted bootstrap / Secure Boot.
- ADR-0013 — PostgreSQL persistence backend.
- ADR-0015 — commercial entitlement boundary.
- ADR-0016 — static SvelteKit Presentation client and platform boundary.
- `docs/reference/winpe-boot-mechanism-spike.md` — WinPE boot evidence.
- `docs/discovery/architecture-redesign.md` — unresolved production network-delivery
  mechanism.
