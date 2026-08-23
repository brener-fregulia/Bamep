# M0 — Product, Component, and Packaging Baseline

Status: **Approved**

## Context

This Specification persists the M0 scope items "product boundary and domain vocabulary," "component responsibilities and boundaries," "boot-orchestration architectural boundary," and "packaging and versioning baseline" (`docs/specifications/m0-architecture-baseline.md`) as durable Specification content, executing Issue #1 (`[WP] Define product, runtime, and stack architecture baseline`).

Runtime topology and language-strategy decisions are recorded separately as ADR-0001, ADR-0002, and ADR-0003 rather than duplicated here.

Most content below restates already Discovery-accepted facts (`docs/discovery/architecture-redesign.md`) in their durable Specification location. The component-boundary section elevates Discovery's "Proposed component boundaries" to the M0 baseline; the owner has approved this elevation, with the clarification recorded below that these are responsibility/dependency boundaries, not a mandated physical structure.

## Product boundary and domain vocabulary

Bamep is a standalone bare-metal provisioning and recovery platform for controlled local networks. It discovers and identifies endpoints, coordinates boot and maintenance environments, collects inventory, executes provisioning and recovery workflows, transfers and manages artifacts, schedules concurrent resources, and provides secure, observable, auditable operation through an API and web interface.

Bamep V1:

- provisions Windows, with Windows 11 as the primary modern target;
- supports UEFI x86-64 endpoints;
- initially operates as a single-server deployment;
- assumes a dedicated provisioning interface/VLAN/network where Bamep may control DHCP/PXE;
- does not depend on Internet access once required artifacts are available locally;
- does not require MikroTik hardware, a dedicated hot cache, dedicated archive storage, RAID, or WebSocket.

Bamep is not an ERP, CRM, financial system, general-purpose RMM, NAS, general switch manager, or V1 multi-site platform. A future ERP must integrate through a public/versioned API and domain events, never through Bamep's internal database.

Bamep is also commercially agnostic: commercial product/catalog concepts (customer, contract, subscription, SKU, edition) are outside Bamep's Domain vocabulary. Commercial entitlement verification, when a commercial installation configures it, is a Port/Adapter/Application-level concern only — Domain never gains commercial concepts (ADR-0015).

(Source: `docs/discovery/architecture-redesign.md`, "Product boundary" — already accepted.)

## User-facing localization baseline

Bamep Web user-facing text must use localization boundaries rather than scattered hardcoded strings.

The established product direction is:

- `pt-BR` is the initial UI locale;
- `en-US` is the planned additional locale.

This section records product behavior and direction, not a localization implementation design. The choice of localization library, catalog structure, fallback mechanism, and loading strategy remains an implementation concern unless a future Specification constrains it.

`en-US` support is not an M0 or current M1 acceptance requirement merely because it is planned here. It becomes delivery scope only when an approved Specification or work item explicitly requires it.

Canonical repository engineering content remains English according to `docs/development/documentation-policy.md`; repository language and user-facing UI locale are separate concerns.

## Component responsibilities and boundaries

**Approved as the M0 baseline** (elevating Discovery's "Proposed component boundaries"):

- **Presentation**: Web Administration and Administrative API.
- **Application**: Endpoint Management, Provisioning/Recovery Orchestration, Boot Orchestration, and Artifact Management.
- **Domain**: Endpoint, Job, JobStep, Attempt, Inventory, Artifact/Snapshot, Transfer, Storage Target, and Domain Events.
- **Runtime Services**: Scheduler/Resource Arbiter, Agent Control Gateway, Transfer Coordinator, and Runtime Presence Registry.
- **Ports**: repositories, Agent transport, boot, discovery, storage, and infrastructure metrics.
- **Adapters**: persistence, PXE/GRUB, switch integration, filesystem/storage, and protocol transports.
- **Workers**: transfer, compression, verification, and artifact movement (isolation boundary accepted in ADR-0001; language open in ADR-0003).

The Domain must not depend on GRUB, MikroTik, `/dev/sda`, `snmpwalk`, WebSocket, PostgreSQL/SQLx, or zstd — those are Adapter responsibilities.

These boundaries apply within the modular-monolith runtime topology accepted in ADR-0001: one deployable Server artifact with the internal boundaries above, plus a separate Worker process/isolation boundary for heavy workloads.

**Nature of these boundaries**: Presentation, Application, Domain, Runtime Services, Ports, Adapters, and Workers are responsibility and dependency boundaries — statements of what may depend on what, and what each responsibility owns — not a mandatory one-to-one mapping to crates, packages, modules, directories, or processes. Workers are the one boundary in this list with an already-accepted physical consequence (a separate process/isolation boundary, per ADR-0001); the others may be implemented as separate crates, as modules within one crate, or in whatever physical arrangement is simplest, as long as the dependency direction and responsibility ownership stated above are preserved (for example, Domain code must not reference Adapter-level concerns such as GRUB, MikroTik, or `/dev/sda`, regardless of whether Domain and Adapters live in separate crates or the same one). Implementation should use the simplest physical structure that preserves these boundaries, and should not introduce crate/package/module fragmentation merely to mirror this list one-to-one.

## Boot-orchestration architectural boundary

**Principle (already accepted, persisted here, unchanged by Issue #8's completion)**: the Domain must not know about GRUB, MikroTik, or device paths such as `/dev/sda`; boot mechanics belong to Adapters, coordinated through the Application-level Boot Orchestration responsibility via the already-accepted Boot Port ("Component responsibilities and boundaries" above — Ports: repositories, Agent transport, boot, discovery, storage, infrastructure metrics). Concrete boot mechanisms — GRUB, iPXE, wimboot, PXE, or any other candidate — remain Adapter concerns; this Specification does not select among them.

**Issue #8 is complete**, not pending: the `[Spike] Validate WinPE boot mechanism` produced empirical evidence, recorded in `docs/reference/winpe-boot-mechanism-spike.md`. That evidence establishes:

- WinPE itself is viable under UEFI x86-64 in the tested virtualized environment — boot from local/removable media reproducibly reached a fully initialized shell with working inbox network and storage drivers;
- the concrete network-delivered WinPE boot mechanism remains unresolved: neither candidate evaluated (iPXE + wimboot over HTTP; GRUB chainload) was demonstrated viable in that environment, each for a distinct, precisely documented, harness-specific reason.

**No production boot mechanism is selected by M0.** The Spike's evidence does not establish that iPXE, wimboot, GRUB, or any other future candidate is unsuitable for Bamep — the observed failures are specific to the local VirtualBox test harness as configured (a hung native-driver NIC path, an SNP path finding no exposed network device, and a GRUB `chainloader` failure occurring after successful file/path resolution but before BCD/artifact processing), not evidenced as a fundamental incompatibility with Bamep's architecture. None of them is treated as rejected on this basis.

The unresolved network-delivered mechanism is explicitly isolated as a **future Integration Environment validation requirement**, to be resolved with real PXE/DHCP/TFTP infrastructure and real UEFI firmware before production boot implementation — it is not hidden inside a future implementation Work Package, and it does not block M0: the first post-M0 simulated vertical slice does not require real WinPE/PXE hardware (`docs/specifications/m0-simulator-contract-and-validation-strategy.md`).

**Trusted bootstrap baseline (ADR-0010, added by owner decision after Issue #10):** production trusted Agent bootstrap requires authenticated boot-chain integrity — the `trusted bootstrap established` security property (`docs/decisions/0010-trusted-bootstrap-and-secure-boot-baseline.md`). Secure Boot is the V1 baseline implementation direction for that property. Secure Boot remains behind the Boot Adapter boundary already established above — it is one more concrete boot-chain mechanism, alongside GRUB/iPXE/wimboot/PXE, that the Domain must not depend on directly. Selecting Secure Boot as the trust baseline does **not** select GRUB, iPXE, wimboot, shim, or another network-boot mechanism, and does not reverse or narrow Issue #8's conclusion above: the concrete network-delivered boot mechanism remains unresolved and still requires Integration Environment validation, entirely independent of the trusted-bootstrap decision. The two questions — "how is WinPE delivered over the network" and "how is the boot chain's executable integrity trusted" — are orthogonal; ADR-0010 answers only the second.

## Packaging and versioning baseline

Already-accepted direction, persisted here:

- Linux Server, with Debian as the initial production target;
- native `.deb` packages and a signed APT repository as the eventual distribution model;
- no silent application-level self-updater for the Server;
- independent SemVer for Server, Web, and Agent;
- Workers ship with the Server release and do not receive independent versioning (ADR-0001);
- contracts versioned separately, for example Administrative API v1 and Agent Protocol v1;
- no lockstep releases between independently deployable components.

(Source: `docs/discovery/architecture-redesign.md`, "Packaging and versioning" — already accepted.)

## Out of scope

- WinPE implementation, a real production packaging pipeline, or a signed APT repository build (implementation, not M0);
- final production backup/snapshot format;
- Endpoint identity, control-plane/Agent-action contracts, Job/JobStep lifecycle and scheduling, persistence/observability, data-plane/storage, and Simulator decisions — see the sibling M0 Work Packages (Issues #2–#7);
- ERP, licensing enforcement, multi-site management, HA, Tauri (per `docs/specifications/m0-architecture-baseline.md` "Out of scope").

## Acceptance criteria

- Product boundary, vocabulary, and non-goals are persisted (M0 acceptance criterion 1) — satisfied by this document.
- Component responsibilities and boundaries are documented (M0 acceptance criterion 5) — satisfied by this document; approved by the owner as responsibility/dependency boundaries, not a mandated physical structure.
- Packaging and versioning baseline is persisted — satisfied by this document.
- The boot-orchestration boundary principle is persisted; Issue #8 is complete and its evidence is incorporated above; the boundary's concrete network-delivered mechanism remains explicitly isolated as a future Integration Environment validation requirement rather than hidden inside a future implementation Work Package (M0 acceptance criterion 7).

## Related ADRs

- ADR-0001 — Runtime topology: modular monolith with worker/process isolation (`Accepted`).
- ADR-0002 — Backend/Server implementation language: Rust (`Accepted`).
- ADR-0003 — Worker and Agent implementation language strategy: Rust for both, with contracts kept explicit and independently versioned (`Accepted`).
- ADR-0010 — Trusted bootstrap and Secure Boot baseline (`Accepted`) — source of the "Trusted bootstrap baseline" consequence recorded above; does not select a network-boot mechanism.
- ADR-0013 — PostgreSQL persistence backend baseline (`Accepted`) — current persistence-backend decision; source of the "does not require ... PostgreSQL" removal above and the PostgreSQL/SQLx Domain-isolation wording in "Component responsibilities and boundaries".
- ADR-0015 — Commercial entitlement boundary: capacity policy, capabilities, offline verification, and plugin gating (`Accepted`) — records that Bamep remains commercially/ERP agnostic, that commercial entitlement verification is a Port/Adapter/Application concern, and that Domain does not gain commercial vocabulary; cross-reference only, does not change the product boundary or component boundaries recorded above.

## Related work

- Issue #1 — `[WP] Define product, runtime, and stack architecture baseline` (this Specification and the three related ADRs are its output).
- Issue #8 — `[Spike] Validate WinPE boot mechanism` (complete; produced the empirical evidence in `docs/reference/winpe-boot-mechanism-spike.md` incorporated above — WinPE UEFI boot viability established, network-delivered mechanism explicitly isolated as future Integration Environment validation work).
- Issue #10 / ADR-0010 — `[Spike] Validate Secure Boot and hardened boot chain` (complete; produced the empirical evidence in `docs/reference/secure-boot-hardened-chain-spike.md`, incorporated above as the "Trusted bootstrap baseline" consequence).

## Open questions

None remaining for this Specification's scope. Both items previously open here — the component-boundary elevation and the Worker/Agent language strategy (ADR-0003) — were resolved by explicit owner approval, with the clarifications recorded in the "Nature of these boundaries" section above and in ADR-0003's contract-independence constraint.

The Boot Orchestrator's concrete network-delivered mechanism remains open (see "Boot-orchestration architectural boundary" above). Issue #8 is complete and its evidence is incorporated, but that evidence did not itself resolve which mechanism to use — this is explicitly isolated as a future Integration Environment validation requirement, not an open question of this Specification's own scope, and it does not block M0.

Status: Approved.
