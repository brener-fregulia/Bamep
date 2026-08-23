# ADR-0002: Bamep Server/backend implementation language — Rust

Status: Accepted

## Context

Bamep needed an implementation language for a long-running Linux Server handling endpoint
coordination, protocol/state logic, persistence, scheduling, and safety-sensitive workflow
decisions.

Rust was the owner's preferred default unless a concrete architectural blocker was found.
The FORGE Python/FastAPI implementation is historical evidence, not a Bamep stack
requirement.

## Decision

Rust is the implementation language for the Bamep Server/backend.

Reasons:

- strong static typing for protocol, state, and safety-sensitive code;
- memory/thread safety without a garbage-collected runtime;
- mature async/concurrency support;
- good Linux-native deployment fit;
- suitable ecosystem for persistence, networking, orchestration, and systems integration;
- no concrete Bamep requirement identified a blocker or a stronger reason to choose another
  Server language.

This ADR selects the Server language only. Runtime topology, persistence backend, component
boundaries, and protocols are separate decisions/contracts.

## Alternatives considered

### Go

Viable, with strong concurrency support, simple deployment, and fast compilation.

Not selected because no Server-specific requirement materially favored it over Rust, and
choosing it merely as a learning opportunity was not a valid project reason.

### Python

Not selected for the Bamep Server baseline.

Python supported the FORGE PoC, but the new project preferred stronger compile-time
contracts and a native systems-oriented implementation model.

The PoC showed that blocking/heavy work can starve control-plane behavior; it did **not**
prove Python itself was the cause. Workload isolation is addressed separately by ADR-0001.

## Consequences

- Server implementation uses Rust unless this ADR is explicitly reconsidered.
- Rust's higher learning/contribution barrier is accepted.
- Rust implementation types must not become the sole definition of externally relevant
  contracts; Agent Protocol, Administrative API, and similar contracts remain explicit and
  independently versioned.
- ADR-0003 later selected Rust for Worker and Agent; ADR-0013 later selected PostgreSQL.
  Those are subsequent decisions, not reasons retroactively added to this one.

## Related

- ADR-0001 — runtime topology and Worker isolation.
- ADR-0003 — Worker and Agent language.
- ADR-0013 — PostgreSQL backend.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — component/package boundaries.
- `docs/reference/poc-lessons.md` — FORGE PoC evidence.
