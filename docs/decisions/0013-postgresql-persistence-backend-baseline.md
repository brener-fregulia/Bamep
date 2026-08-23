# ADR-0013: PostgreSQL persistence backend baseline

Status: Accepted

Supersedes: ADR-0007

## Context

ADR-0007 originally selected SQLite for the M0 standalone persistence baseline.

That decision was reasonable under the assumptions available at the time:

- Bamep V1 was a single-node deployment;
- expected durable writes were bounded by meaningful domain-state transitions rather than
  raw message or telemetry volume;
- SQLite offered minimal operational overhead;
- the Repository Port/Adapter boundary preserved the ability to replace the backend later.

ADR-0007 also established several persistence invariants that were independent from the
backend choice, including the durable/transient boundary, atomic persistence expectations,
and persist-before-send behavior.

Those invariants are now normative in
`docs/specifications/m0-persistence-observability-and-domain-events.md`.

This ADR does not redefine them.

The persistence-backend decision was reconsidered while executing Issue #17
(`[WP] Establish simulated Endpoint trust, enrollment, and Agent session`) during the first
real post-M0 implementation work.

At that point:

- no production release or installed customer database existed;
- no persistent schema-upgrade compatibility obligation existed;
- the Repository Port/Adapter boundary had already kept persistence-specific code out of
  Domain and Application;
- replacing the backend therefore had very low migration cost;
- Bamep had become concretely shaped as a persistent Server/appliance product rather than a
  lightweight embedded desktop application;
- the control plane was async/Tokio-based;
- PostgreSQL had mature native-async Rust integration;
- upcoming Job/JobStep/Attempt, Artifact, audit, and Administrative API work would add real
  relational query and concurrent-write needs;
- deferring the backend switch until after those layers existed would increase migration
  cost without producing a clear benefit.

The owner also had meaningful PostgreSQL operational experience, reducing the ongoing
operational cost of adopting it in a primarily solo-maintained project.

The reconsideration did **not** establish that SQLite had failed the 20–24 endpoint target.
That measurement had not been completed.

This ADR changes the backend because the overall architecture, implementation timing,
operational fit, and evolution cost favored PostgreSQL before a production compatibility
burden existed.

## Decision

### PostgreSQL is the persistence backend baseline

PostgreSQL is the only production/baseline persistence backend supported by Bamep.

SQLite is no longer a supported production persistence backend.

Bamep does not maintain dual SQLite/PostgreSQL implementations for speculative portability.

The Domain and Application remain isolated from the backend through the persistence
Port/Adapter boundary defined by the product/component Specification.

### Distribution-managed supported major

Bamep does not permanently pin one PostgreSQL major version in architecture.

The baseline policy is:

> Use a supported PostgreSQL major supplied by the reference Linux distribution unless a
> concrete Bamep requirement requires a different supported major.

This keeps PostgreSQL lifecycle management aligned with the reference operating-system
baseline rather than turning a temporary distribution version into a permanent
architectural constraint.

A distribution upgrade that changes the default supported PostgreSQL major does not by
itself require a new ADR.

A concrete incompatibility or product requirement that cannot be satisfied by this policy
does.

### Local standalone topology

For the V1 standalone deployment profile, PostgreSQL is a local Server dependency on the
same appliance/host by default.

This ADR does not introduce:

- remote database operation;
- PostgreSQL HA;
- database clustering;
- multi-site persistence;
- a separately administered database tier.

Those require explicit future requirements.

Exact local connection mechanics, service ordering, role creation, credential provisioning,
backup integration, and packaging procedures are implementation/operations concerns unless
a later Specification or ADR constrains them.

### One active backend

Bamep intentionally supports one active persistence backend rather than maintaining
multiple adapters for hypothetical future portability.

The Repository Port/Adapter boundary exists to keep Domain and Application independent from
PostgreSQL implementation details.

It does not create a requirement to implement unused database backends.

If a future requirement justifies a different backend, that change should be evaluated at
that time rather than paid for continuously in advance.

### Relational-first modeling

PostgreSQL adoption does not imply document-oriented persistence.

Queryable lifecycle, correlation, scheduling, reconciliation, audit, and safety-relevant
state should be represented relationally when those properties participate in:

- constraints;
- joins;
- filtering;
- ordering;
- uniqueness;
- foreign-key relationships;
- lifecycle transitions;
- safety decisions.

`JSONB` is appropriate for genuinely variable or opaque payloads where relational
structure provides no useful invariant or query behavior.

Whole aggregate serialization into JSON is not the baseline persistence model merely
because PostgreSQL supports JSONB.

The normative set of durable records, events, correlation fields, and audit requirements
belongs to `docs/specifications/m0-persistence-observability-and-domain-events.md`.

Concrete table and index design remains implementation work.

### Persistence invariants remain contract-owned

Changing from SQLite to PostgreSQL does not reopen the persistence semantics established
during M0.

In particular, the current normative Specification remains authoritative for:

- durable versus transient/high-frequency state;
- inventory write-on-revision-change behavior;
- domain-event semantics;
- correlation requirements;
- atomic state + event + audit persistence;
- auditability;
- current durable state as the source of truth rather than event sourcing;
- persist-before-send ordering for Agent dispatch;
- restart/recovery persistence expectations.

This ADR consumes those requirements but does not duplicate their detailed definitions.

## Why PostgreSQL

### Better fit for the Server/appliance model

Bamep is a continuously running Server/appliance product.

The operational simplicity advantage of an embedded database therefore carries less weight
than it would for a single-user desktop application.

A local PostgreSQL service is an acceptable dependency in this deployment model.

### Better async integration

Bamep's Server control plane is async/Tokio-based.

PostgreSQL has mature native-async Rust drivers and connection-pool support that integrate
naturally with that runtime.

The first SQLite checkpoint required synchronous database work to be mediated inside an
async Server and serialized through a guarded connection.

That implementation was workable, but PostgreSQL provides a cleaner structural fit for the
runtime architecture.

### Concurrent relational workloads

The relevant scaling variable is not simply endpoint count.

ADR-0007 correctly observed that:

> endpoint concurrency is not equivalent to database writer concurrency.

That observation remains valid.

The persistence load depends on the actual number and timing of durable state transitions.

However, Bamep's expected evolution includes concurrent transitions across:

- Endpoint state;
- credentials and boot context;
- Job/JobStep/Attempt;
- inventory revisions;
- Artifact metadata;
- events and audit records.

PostgreSQL's MVCC and concurrent-writer model provide a stronger baseline for that evolving
workload without requiring the project to prove SQLite insufficient first.

### Lower migration cost now than later

At the reconsideration point, there was:

- no production installed base;
- no supported historical schema;
- no customer data migration requirement;
- no release compatibility burden.

Changing the Adapter then was near the minimum possible migration cost.

Waiting until more Work Packages and real installations depended on SQLite would add:

- production data conversion;
- upgrade/rollback paths;
- backup/restore conversion;
- integrity verification;
- support burden.

Deferring the decision therefore increased future cost without an identified technical
benefit.

### Maintainer operational experience

PostgreSQL was already familiar operational territory for the project owner.

For a primarily solo-maintained project, existing operational competence materially lowers
the real cost of introducing and maintaining infrastructure.

This was treated as a legitimate engineering factor, not as a substitute for the technical
evaluation above.

## Alternatives considered

### Keep SQLite as the long-term baseline

Rejected.

SQLite remained technically plausible, and this ADR does not claim it could not meet the
M0 concurrency target.

It was not selected because PostgreSQL better matched:

- the async Server runtime;
- expected relational evolution;
- concurrent durable-state changes;
- the Server/appliance deployment profile;
- the owner's operational environment;
- the opportunity to switch before compatibility costs accumulated.

### SQLite now, PostgreSQL later

Rejected.

The Repository abstraction reduces Domain/Application coupling to the backend but does not
eliminate future production migration cost.

Once real deployments exist, a backend migration would require substantially more than
rewriting an Adapter.

Because the switch was already justified and cheap at the current stage, postponing it did
not buy enough benefit.

### Dual SQLite/PostgreSQL support

Rejected.

No accepted requirement needs two persistence backends.

Maintaining both would duplicate implementation, testing, migration behavior, and support
surface for speculative portability.

### Another relational database

Not selected.

No current requirement justified adding another backend evaluation once PostgreSQL
satisfied the architectural needs and operational constraints.

A future concrete requirement may reopen the backend decision.

## Consequences

- ADR-0007 remains `Superseded` and preserves the original SQLite decision history.
- PostgreSQL is the only current production persistence backend.
- Domain and Application must remain independent from PostgreSQL- and driver-specific
  APIs.
- The project does not maintain a lowest-common-denominator persistence model merely for
  hypothetical backend portability.
- Relational modeling is preferred for queryable lifecycle/correlation/safety state;
  flexible opaque payloads may use JSONB selectively.
- The PostgreSQL major follows the supported reference-distribution baseline unless a
  concrete requirement overrides that policy.
- PostgreSQL is local to the standalone appliance by default; this decision does not imply
  remote database operation or HA.
- Backend performance still requires empirical validation under representative workload.
  PostgreSQL adoption is not evidence that persistence performance is automatically
  sufficient.
- Persistence semantics remain defined by the persistence Specification rather than by this
  ADR.
- SQL driver, query style, migration implementation, and current schema conventions are
  implementation concerns rather than part of the backend-selection rationale.

## Current implementation relationship

The current Server implementation uses PostgreSQL through SQLx inside the PostgreSQL
Adapter.

That is current implementation, not a reason to redefine this ADR around SQLx.

Current implementation and schema-evolution conventions belong to
`docs/development/persistence.md` and `docs/architecture/README.md`.

The normative persistence contract belongs to
`docs/specifications/m0-persistence-observability-and-domain-events.md`.

## Related decisions and specifications

- ADR-0007 — superseded historical SQLite persistence decision.
- ADR-0001 — standalone Server/runtime topology.
- ADR-0002 — Rust Server implementation language.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Port/Adapter and product
  deployment boundaries.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — normative
  persistence, event, correlation, audit, and recovery semantics.
- `docs/development/persistence.md` — current PostgreSQL/SQLx implementation and migration
  conventions.

## Related work

- Issue #5 — historical M0 Work Package that established the original persistence,
  observability, and domain-event baseline.
- Issue #17 — M1 Work Package during which the persistence backend was reconsidered before a
  production compatibility burden existed.
- Issue #21 — M1 work responsible for representative persistence-load validation at scale.
