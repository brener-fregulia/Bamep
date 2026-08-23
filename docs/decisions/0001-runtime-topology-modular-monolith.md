# ADR-0001: Runtime topology — modular monolith

Status: Accepted

## Context

Bamep V1 is a single-server product without HA, multi-site, or independent-service-scaling
requirements.

The Server needs clear internal boundaries while keeping deployment and operations simple.
Heavy transfer/compression/verification work also needs isolation from the control plane.

Normative component responsibility/dependency boundaries belong to
`docs/specifications/m0-stack-and-boundaries-baseline.md`.

## Decision

Bamep Server uses a **modular-monolith** runtime topology:

- one deployable Server artifact;
- explicit internal responsibility/dependency boundaries;
- heavy or risky workloads execute behind a separate Worker process/isolation boundary;
- Workers ship with the Server release and do not receive independent product versioning.

Worker-isolated work includes transfer, compression, verification, and Artifact movement.

The exact crate/module/process implementation may evolve as long as the modular-monolith
boundary and control-plane workload isolation are preserved.

## Alternatives considered

### Microservices from the outset

Rejected. V1 has no requirement for independent scaling, HA, or multi-site operation that
justifies the added deployment, networking, observability, and failure-mode complexity.

### Single process without Worker isolation

Rejected. CPU/I/O-heavy work can starve or destabilize control-plane responsibilities. The
FORGE PoC provides reusable evidence of this failure pattern.

One deployable Server product does not require every workload to execute in the same
process.

### Distributed scheduler or clustered control plane

Rejected as a V1 requirement. Clustering, leader election, Redis, and distributed
coordination would solve requirements Bamep V1 does not currently have.

## Consequences

- Server responsibilities ship as one deployable product artifact.
- Internal dependency boundaries remain mandatory.
- Heavy workloads remain isolated from the control-plane process path.
- Workers remain release-coupled to Server.
- Microservices, clustering, HA machinery, or distributed scheduling require new approved
  requirements rather than implementation convenience.
- ADR-0003 later selected Rust for Worker; that language choice does not change this topology
  decision.

## Related

- `docs/specifications/m0-stack-and-boundaries-baseline.md` — component/package boundaries.
- ADR-0002 — Server language.
- ADR-0003 — Worker and Agent language.
- `docs/reference/poc-lessons.md` — FORGE PoC evidence.
