# ADR-0003: Worker and Agent language strategy

Status: Accepted

## Context

After ADR-0002 selected Rust for the Server, Bamep still needed an independent language
decision for:

- the **Worker** boundary from ADR-0001;
- the endpoint-resident **Agent**.

Rust and Go were the primary candidates. The choice had to account for each component's
runtime constraints and the maintenance cost of a polyglot stack.

## Decision

Rust is the implementation language for both Worker and Agent.

### Worker

Rust is selected because:

- the Worker is release-coupled to the Rust Server;
- one toolchain/CI/release ecosystem reduces maintenance cost;
- native static deployment fits the appliance model;
- Rust is suitable for transfer, compression, verification, and other CPU/I/O-heavy work.

Go is technically viable, but no Worker requirement justified introducing a second
production ecosystem.

### Agent

The Agent requires a small native binary suitable for the Linux maintenance environment,
typed protocol/state handling, authentication/reconnect logic, action supervision, and
cancellation/result reporting.

Rust fits those constraints, including static/musl-compatible deployment, without adding
another runtime/toolchain.

Go is also viable; its runtime footprint was not proven to be a blocker. It was not selected
because no Agent-specific requirement outweighed the recurring cost of a second ecosystem.

### Contract independence

Using Rust across Server, Worker, and Agent must **not** make shared Rust types or crates the
sole definition of an external/inter-process contract.

In particular:

- Agent Protocol remains explicitly specified and independently versioned;
- future Administrative API or Worker/external contracts require their own authoritative
  contract;
- shared Rust crates/types are implementation conveniences only.

A participant must remain implementable from the authoritative contract without reading
another component's Rust source.

## Alternatives considered

### Go for Worker and/or Agent

Viable, but not selected. No concrete requirement justified its additional toolchain,
dependency, CI, release, and maintenance cost.

A future materially different runtime requirement may reopen the decision for one component
without requiring the others to change.

### Split-language stack

Technically viable because contracts are versioned independently, but rejected for the
baseline due to unnecessary polyglot maintenance cost.

### Python for Worker

Not selected as a primary production candidate. The FORGE PoC provides historical lessons,
but did not establish a reason to add Python alongside the Rust Server for the Worker
boundary.

## Consequences

- Server, Worker, and Agent use Rust as their selected implementation language.
- Shared build/release infrastructure and internal crates may be reused where responsibility
  boundaries remain intact.
- External contracts stay explicit and language-independent.
- A future non-Rust participant remains possible from the relevant contract alone.
- This ADR selects languages only; it does not imply that Worker or production Agent
  implementation currently exists.

## Related

- ADR-0001 — runtime topology and Worker isolation.
- ADR-0002 — Server language.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — component/package boundaries.
- `docs/specifications/m0-agent-protocol-contract.md` — Agent Protocol authority.
- `docs/reference/poc-lessons.md` — reusable FORGE PoC evidence.
