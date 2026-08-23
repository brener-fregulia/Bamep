# ADR-0002: Bamep Server/backend implementation language — Rust

Status: Accepted

## Context

During M0, Bamep required a durable implementation-language decision for the Server/backend
before implementation began.

Issue #1 (`[WP] Define product, runtime, and stack architecture baseline`) recorded explicit
owner direction:

> Rust is the preferred language for the Bamep Server/backend and should be treated as the
> default candidate unless Discovery identifies a concrete architectural blocker.

This ADR records the evaluation of that direction and the resulting architectural
decision.

At the time of the decision, Bamep needed a Server language suitable for a long-running
Linux service responsible for:

- endpoint authentication and coordination;
- concurrent Job orchestration;
- protocol handling;
- persistence;
- scheduling and resource arbitration;
- safety-sensitive decisions around destructive operations.

The previous FORGE PoC used Python/FastAPI. That prior implementation is evidence and
experience, not a requirement that Bamep preserve or reject the same stack. Reusable PoC
lessons remain in `docs/reference/poc-lessons.md`.

The original ADR also referenced M0 Discovery and ADR-triage documents that have since been
reduced or retired after durable conclusions were promoted. Issue #1 and Git history
preserve the planning history; this ADR preserves the language-decision rationale.

## Decision

Rust is the implementation language for the Bamep Server/backend.

The decision is based on the absence of a concrete architectural blocker and on Rust's fit
for Bamep's Server responsibilities:

- strong static typing for protocol, state, and safety-sensitive code;
- memory and thread safety without a garbage-collected runtime;
- mature async/concurrency support for a long-running control plane;
- good support for Linux-native service deployment;
- suitability for explicit Domain/Application/Port/Adapter boundaries;
- one implementation ecosystem that can support network, persistence, orchestration, and
  systems-facing responsibilities without requiring a second Server language.

This ADR chooses the Server implementation language. It does not define the detailed
runtime topology, persistence backend, protocol, component responsibility model, or
packaging contract; those decisions belong to their own authorities.

## Alternatives considered

### Go

Viable, but not selected.

Go provides:

- straightforward concurrency primitives;
- fast compilation;
- simple deployment;
- a mature ecosystem for network services.

No Server-specific Bamep requirement identified during M0 materially favored Go over Rust,
and the owner explicitly rejected selecting Go merely as a learning opportunity.

Given Rust as the preferred default and no architectural blocker against it, introducing a
different Server language would have required a concrete project benefit that was not
identified.

### Python

Not selected for the Bamep Server.

Python had already been used successfully enough to support the FORGE PoC, but Bamep was
being designed as a longer-lived systems/orchestration product with stronger compile-time
contracts, explicit state models, and destructive-operation safety requirements.

The PoC also demonstrated that blocking or heavy work can starve control-plane behavior.
That evidence does **not** establish Python itself as the cause; it reinforces the need for
careful workload isolation and execution design, which is addressed separately by
ADR-0001.

Choosing Python would also retain a runtime and typing model the project did not prefer for
the new Server baseline when Rust had no identified blocker.

### Rust

Accepted.

Rust matched the owner's stated default and the evaluated Server requirements without
introducing a concrete architectural limitation that justified another language.

## Consequences

- Bamep Server implementation uses Rust.
- New Server-side code, libraries, and tooling should fit the Rust implementation baseline
  unless this ADR is explicitly reconsidered.
- Rust raises the contribution and learning barrier for contributors unfamiliar with the
  language; that cost was accepted.
- The language decision does not allow Rust implementation types to become the sole
  definition of externally relevant contracts. Agent Protocol, Administrative API, and
  other external contracts remain explicit and independently versioned.
- Runtime topology remains owned by ADR-0001.
- Component responsibility and dependency boundaries remain owned by
  `docs/specifications/m0-stack-and-boundaries-baseline.md`.
- Persistence-backend choice is independent from this ADR.

Later accepted decisions refined adjacent questions without changing this ADR:

- ADR-0003 selected Rust for Worker and Agent as well;
- ADR-0013 selected PostgreSQL as the persistence backend baseline.

Those are subsequent decisions, not retroactive reasons for selecting Rust for the Server.

A future concrete requirement that Rust cannot satisfy adequately may justify
reconsideration through the ADR process. Implementation convenience alone is not sufficient
to silently override this decision.

## Current implementation relationship

The current repository implements the Server as the Rust crate `bamep-server` inside the
Cargo workspace.

`docs/architecture/README.md` owns the description of the currently implemented Rust
workspace and Server boundaries. This ADR explains **why Rust is the selected Server
language**; it must not duplicate current crate/module structure.

## Related specifications and decisions

- `docs/specifications/m0-stack-and-boundaries-baseline.md` — normative product,
  responsibility, dependency, packaging, and versioning baseline.
- `docs/specifications/m0-architecture-baseline.md` — M0 baseline that required the stack
  decision.
- ADR-0001 — Server runtime topology and Worker/process isolation.
- ADR-0003 — Worker and Agent implementation language strategy.
- ADR-0013 — PostgreSQL persistence backend baseline.

## Related evidence

- `docs/reference/poc-lessons.md` — reusable lessons from the FORGE PoC.

## Related work

- Issue #1 — `[WP] Define product, runtime, and stack architecture baseline`.
