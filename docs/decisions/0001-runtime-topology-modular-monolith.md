# ADR-0001: Runtime topology — modular monolith with worker/process isolation

Status: Accepted

## Context

During M0, Bamep required a durable Server runtime-topology decision before implementation
could begin.

Issue #1 (`[WP] Define product, runtime, and stack architecture baseline`) recorded the
owner-approved direction that this ADR was required to formalize:

- start with a modular monolith rather than microservices;
- preserve explicit internal responsibility boundaries;
- isolate heavy workloads through a Worker/process boundary;
- do not introduce clustering, HA, Redis, leader election, or a distributed scheduler
  without a V1 requirement that justifies them.

The surrounding product baseline also established a single-server V1 deployment and no V1
HA requirement.

The decision was made for a primarily solo-maintained product where operational
simplicity, failure isolation, and clear internal boundaries mattered more than independent
service deployment.

The original ADR referenced M0 Discovery documents that have since been reduced or retired
after their durable conclusions were promoted. Git history and Issue #1 preserve that
historical process. Current normative product and component boundaries belong to
`docs/specifications/m0-stack-and-boundaries-baseline.md`.

## Decision

Bamep Server uses a **modular-monolith runtime topology**.

The Server is one deployable artifact with explicit internal responsibility and dependency
boundaries. The normative responsibility model is defined by
`docs/specifications/m0-stack-and-boundaries-baseline.md`.

Heavy or risky workloads such as:

- transfer;
- compression;
- verification;
- artifact movement;

must run behind a separate Worker process/isolation boundary rather than executing directly
on the control-plane path.

Workers belong to the Server release and do not receive independent product versioning.

This ADR chooses runtime topology and workload isolation. It does not make the physical
module/crate layout itself normative, and it does not redefine the detailed responsibility
boundaries owned by the Specification.

## Alternatives considered

### Microservices from the outset

Rejected.

No V1 requirement for independent scaling, multi-site operation, or HA justified the added
deployment, observability, networking, failure-mode, and operational complexity.

For a primarily solo-maintained single-node product, distributing the control plane across
services would add system complexity before a requirement existed to benefit from it.

### Single process with no Worker isolation

Rejected.

CPU- or I/O-intensive work such as compression, verification, and large transfers can
starve or destabilize control-plane responsibilities when executed without an isolation
boundary.

The previous PoC also provided reusable evidence that blocking or heavy work could starve
the control plane; that evidence is retained in `docs/reference/poc-lessons.md`.

Keeping one Server deployment does not require every workload to execute in the same
process boundary.

### Distributed scheduler or clustered control plane

Rejected as a V1 requirement.

The accepted product scope did not require multi-node coordination or HA. Introducing
distributed scheduling, leader election, or equivalent coordination infrastructure would
therefore solve a problem Bamep V1 did not have.

## Consequences

- Server responsibilities ship as one deployable Server artifact even when their internal
  boundaries are strongly separated.
- Internal dependency boundaries remain mandatory even though the runtime is a modular
  monolith.
- Heavy workloads require Worker/process isolation from the control-plane path.
- The concrete Worker isolation implementation may evolve as long as the isolation
  property selected by this ADR is preserved.
- Workers remain release-coupled to the Server and do not receive independent product
  versioning.
- The build and release pipeline should therefore avoid unnecessary divergence between
  Server and Worker artifacts.
- Microservices, clustering, leader election, Redis, and a distributed scheduler are not
  implied by the architecture and require new approved requirements before introduction.
- A later requirement for multi-site operation, HA, independent scaling, or materially
  different failure isolation may justify reconsidering this ADR through the normal ADR
  process.

ADR-0003 subsequently selected Rust for both Worker and Agent. That later language decision
does not change the runtime-topology decision recorded here.

## Current implementation relationship

This ADR remains the architectural rationale for the selected runtime topology.

`docs/architecture/README.md` describes only the subset currently implemented in code and
must not be used to infer that every planned Server responsibility or Worker capability
already exists.

The Specification remains authoritative for normative component responsibility and
dependency boundaries; Architecture remains authoritative for current implemented
structure.

## Related specifications and decisions

- `docs/specifications/m0-stack-and-boundaries-baseline.md` — normative product,
  responsibility, dependency, packaging, and versioning baseline.
- `docs/specifications/m0-architecture-baseline.md` — M0 baseline that required the runtime
  decision.
- ADR-0002 — Backend/Server implementation language.
- ADR-0003 — Worker and Agent implementation language strategy.

## Related evidence

- `docs/reference/poc-lessons.md` — reusable PoC evidence, including control-plane
  starvation from blocking/heavy work.

## Related work

- Issue #1 — `[WP] Define product, runtime, and stack architecture baseline`.
