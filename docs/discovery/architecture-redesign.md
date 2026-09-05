# Bamep — Remaining Architecture Discovery

Status: **Active Discovery — unresolved and future topics only**

## Purpose

This document retains only architecture-related questions that are still materially
unresolved or have not yet been promoted into approved actionable work.

The original M0 architecture discovery baseline previously stored here has been resolved.
Its durable conclusions now belong to Specifications, ADRs, Architecture, Reference
material, and GitHub work items according to `docs/development/documentation-policy.md`.

Git history preserves the former discovery baseline. Do not reintroduce resolved M0
architecture decisions here.

## Future: pre/post provisioning diagnostics

This is a future product use case recorded during M0 review. It is **not** an M0 or M1
requirement and no current Work Package implements it.

Bamep may eventually support an automated diagnostic and benchmark workflow that:

1. runs diagnostics or performance measurements on the client's original Windows
   installation;
2. persists a pre-service baseline;
3. enters the maintenance/provisioning workflow;
4. performs backup and provisioning as required;
5. boots the newly installed or configured Windows environment;
6. runs equivalent post-service diagnostics;
7. compares pre-service and post-service results;
8. produces an operator- or customer-facing report.

The same data may later support aggregate comparison across factors such as:

- Windows versions or builds;
- driver versions;
- Bamep provisioning-process changes;
- hardware migrations such as HDD to SATA SSD or NVMe.

This concept is intentionally not designed here.

Still unresolved:

- the Windows-side execution component;
- the diagnostic and benchmark suite;
- the result and reporting schema;
- telemetry retention and aggregation;
- comparison semantics;
- operator/customer presentation;
- the point at which this future capability should become approved actionable work.

The current Job/JobStep model is expected to be conceptually compatible with a future flow
such as:

```text
PreflightDiagnostics
→ Backup
→ Provision
→ Configure
→ PostflightDiagnostics
→ Report
```

This is a forward-compatibility expectation only. It does not add JobStep types, change the
Job lifecycle contract, or create a current implementation requirement.

## Resolved material no longer owned here

The former discovery baseline also discussed product boundaries, component boundaries,
runtime topology, language choices, control plane, data plane, persistence, storage,
scheduling, security, Endpoint identity, artifact lifecycle, observability, packaging,
versioning, Simulator strategy, and completed Technical Spikes.

The V1 network-delivered WinPE production mechanism question previously tracked here is
now resolved: ADR-0021 selects an iPXE + wimboot Boot Adapter baseline, physically
validated in `docs/reference/physical-secure-boot-winpe-network-delivery.md`, and
`docs/specifications/m0-stack-and-boundaries-baseline.md` owns the resulting boot-orchestration
boundary statement.

Those subjects must now be read from their authoritative Specifications, ADRs,
Architecture, Reference material, or GitHub history rather than reconstructed from this
Discovery document.
