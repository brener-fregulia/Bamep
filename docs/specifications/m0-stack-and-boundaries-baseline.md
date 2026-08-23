# M0 — Product, Component, and Packaging Baseline

Status: **Approved**

## Purpose

This Specification is the durable normative baseline for:

- the Bamep V1 product boundary and core domain vocabulary;
- component responsibility and dependency boundaries;
- the boot-orchestration boundary;
- user-facing localization direction;
- packaging and versioning constraints.

It defines **what Bamep must preserve** in these areas.

It does not preserve architectural decision rationale or describe current implementation
structure:

- ADRs in `docs/decisions/` own decision rationale and trade-offs;
- `docs/architecture/` owns architecture implemented in the current repository;
- `docs/reference/` owns reusable empirical evidence;
- unresolved investigation remains in `docs/discovery/`.

This Specification originated from M0 Issue #1
(`[WP] Define product, runtime, and stack architecture baseline`). Historical Discovery and
work-item context remain available through Git and GitHub history; they are not normative
dependencies of this document.

## Product boundary and domain vocabulary

Bamep is a standalone bare-metal provisioning and recovery platform for controlled local
networks.

It is responsible for capabilities including:

- discovering and identifying endpoints;
- coordinating boot and maintenance environments;
- collecting inventory;
- executing provisioning and recovery workflows;
- transferring and managing artifacts;
- scheduling concurrent resources;
- providing secure, observable, auditable operation through explicit interfaces.

Bamep V1:

- provisions Windows, with Windows 11 as the primary modern target;
- supports UEFI x86-64 endpoints;
- initially operates as a single-server deployment;
- assumes a dedicated provisioning interface, VLAN, or network where Bamep may control
  DHCP/PXE;
- must remain operational without Internet access once required artifacts are available
  locally;
- does not require MikroTik hardware;
- does not require dedicated cache or archive storage;
- does not require RAID.

Legacy BIOS, multi-site operation, and HA are outside the V1 baseline unless a later
approved Specification changes that scope.

Bamep is not:

- an ERP;
- a CRM;
- a financial system;
- a general-purpose RMM;
- a NAS;
- a general switch-management platform.

A future ERP or external commercial system must integrate through explicit versioned
interfaces and/or domain events rather than through Bamep's internal persistence schema.

Bamep's Domain remains commercially agnostic. Customer, contract, subscription, SKU,
edition, and equivalent commercial catalog concepts are not Domain vocabulary.
Commercial-entitlement handling, when configured, remains outside Domain according to
ADR-0015.

## User-facing localization baseline

Bamep Web user-facing text must use localization boundaries rather than scattered
hardcoded strings.

The established product direction is:

- `pt-BR` is the initial UI locale;
- `en-US` is the planned additional locale.

`en-US` support is not an M0 or current M1 delivery requirement merely because it is
planned here. It becomes delivery scope only when an approved Specification or work item
explicitly requires it.

This Specification does not choose a localization library, catalog structure, loading
strategy, or fallback implementation.

Canonical repository engineering content remains English according to
`docs/development/documentation-policy.md`. Repository language and user-facing locale are
separate concerns.

## Component responsibility boundaries

The following responsibility boundaries are normative:

- **Presentation** — Web Administration and the Administrative API.
- **Application** — Endpoint Management, Provisioning/Recovery Orchestration, Boot
  Orchestration, and Artifact Management.
- **Domain** — Endpoint, Job, JobStep, Attempt, Inventory, Artifact/Snapshot, Transfer,
  Storage Target, and Domain Events.
- **Runtime Services** — Scheduler/Resource Arbiter, Agent Control Gateway, Transfer
  Coordinator, and Runtime Presence Registry.
- **Ports** — repositories, Agent transport, boot, discovery, storage, and infrastructure
  metrics.
- **Adapters** — persistence, boot/PXE integration, switch integration,
  filesystem/storage integration, and protocol transports.
- **Workers** — transfer, compression, verification, and artifact movement.

These are responsibility and dependency boundaries, not a required one-to-one mapping to
crates, packages, modules, directories, or processes.

The physical runtime topology is owned by ADR-0001. The Server language is owned by
ADR-0002. Worker and Agent language strategy is owned by ADR-0003.

Worker isolation is therefore consumed from ADR-0001 rather than re-decided here, and
Worker/Agent Rust selection is consumed from ADR-0003 rather than treated as open.

## Dependency constraints

Domain logic must remain independent from infrastructure-specific mechanisms.

Domain must not directly depend on concerns such as:

- GRUB, iPXE, wimboot, or concrete PXE mechanics;
- MikroTik or another switch vendor;
- Linux device paths such as `/dev/sda`;
- shell tooling such as `snmpwalk`;
- WebSocket/TLS transport libraries;
- PostgreSQL/SQLx or another persistence implementation;
- compression implementations such as zstd.

Those mechanisms belong behind the appropriate Ports and Adapters.

The current physical implementation may use a simpler subset of the responsibility model.
`docs/architecture/README.md` is authoritative for what is actually implemented now.

## Communication-boundary constraint

Different Bamep communication responsibilities may use different protocols.

In particular:

- Agent ↔ Server control-plane transport is owned by ADR-0005 and the Agent Protocol
  Specification;
- bulk Artifact transfer is owned by the data-plane decision and Specification;
- Web ↔ Server Administrative API behavior is owned by its own Specification.

A protocol selected for one responsibility must not be generalized into a requirement for
another boundary without an approved decision.

Externally relevant contracts must remain explicit and independently versioned rather than
being defined solely by shared Rust implementation types.

## Boot-orchestration boundary

The Domain must not depend on a concrete network-boot mechanism.

Boot mechanics belong to Adapters and are coordinated through the Application-level Boot
Orchestration responsibility via the boot Port.

Therefore GRUB, iPXE, wimboot, PXE delivery mechanics, and equivalent future mechanisms
remain implementation choices behind that boundary unless a later Specification or ADR
makes one normative.

WinPE itself has validated UEFI x86-64 viability in the tested environment. The reusable
evidence belongs to `docs/reference/winpe-boot-mechanism-spike.md`.

The **production network-delivered WinPE mechanism remains unresolved** and is retained as
active Discovery in `docs/discovery/architecture-redesign.md`. It requires Integration
Environment evidence before production boot implementation.

M0 does not select a production network-delivery mechanism.

Trusted-bootstrap integrity is a separate concern from network-delivery mechanics.
ADR-0010 owns the Secure Boot trusted-bootstrap decision, and the trusted-bootstrap
Specifications own its normative contracts. Selecting that trust baseline does not select
GRUB, iPXE, wimboot, or another network-delivery mechanism.

## Packaging and versioning baseline

The following packaging and versioning constraints are normative:

- Bamep Server targets Linux, with Debian as the initial production distribution;
- native `.deb` packages and a signed APT repository are the intended production
  distribution model;
- Server must not silently self-update at the application level;
- Server, Web, and Agent are independently versioned with SemVer;
- Workers ship with the Server release and do not receive independent product versioning,
  consistent with ADR-0001;
- externally relevant contracts are versioned independently from component SemVer;
- independently deployable components do not require lockstep releases unless a future
  compatibility contract explicitly requires it.

Examples of independently versioned contracts include Agent Protocol v1 and Administrative
API v1.

## Stack decisions owned by ADRs

This Specification consumes, but does not duplicate the rationale of:

- ADR-0001 — modular-monolith Server topology with Worker/process isolation;
- ADR-0002 — Rust for the Backend/Server implementation;
- ADR-0003 — Rust for Worker and Agent, while preserving explicit language-independent
  external contracts;
- ADR-0010 — trusted-bootstrap / Secure Boot baseline;
- ADR-0013 — PostgreSQL persistence-backend baseline;
- ADR-0015 — commercial-entitlement boundary outside Domain.

Changes to those decisions must occur through the ADR lifecycle rather than by editing this
Specification as a substitute for reconsidering the decision.

## Out of scope

This Specification does not define:

- WinPE implementation;
- the final production network-delivered WinPE mechanism;
- a production packaging pipeline or signed APT repository build procedure;
- final production backup/snapshot format;
- Endpoint identity lifecycle;
- Agent Protocol message semantics;
- Job/JobStep/Attempt lifecycle and scheduling;
- persistence durability semantics;
- data-plane transfer semantics;
- Simulator fidelity;
- ERP implementation;
- licensing enforcement;
- multi-site management;
- HA;
- Tauri.

Those responsibilities belong to their own Specifications, ADRs, future approved work, or
explicitly unresolved Discovery.

## M0 acceptance mapping

This document continues to satisfy the M0 baseline responsibilities assigned to Issue #1:

- product boundary, vocabulary, and non-goals are persisted;
- component responsibilities and dependency boundaries are persisted;
- packaging and versioning constraints are persisted;
- the boot-orchestration abstraction boundary is explicit;
- the concrete production network-delivery mechanism is explicitly isolated rather than
  hidden inside implementation.

The broader M0 acceptance criteria remain owned by
`docs/specifications/m0-architecture-baseline.md`.

## Related work

- Issue #1 — `[WP] Define product, runtime, and stack architecture baseline`.
- Issue #8 — `[Spike] Validate WinPE boot mechanism`; reusable evidence is in
  `docs/reference/winpe-boot-mechanism-spike.md`.
- Issue #10 — `[Spike] Validate Secure Boot and hardened boot chain`; the resulting durable
  decision is ADR-0010 and empirical evidence is in
  `docs/reference/secure-boot-hardened-chain-spike.md`.

## Remaining question

No unresolved question remains **inside this Specification's owned scope**.

The concrete production network-delivered WinPE mechanism remains intentionally outside
this Specification as active Discovery and a future Integration Environment validation
requirement.

Status: Approved.
