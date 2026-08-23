# ADR-0003: Worker and Agent implementation language strategy

Status: Accepted

## Context

After ADR-0002 selected Rust for the Bamep Server, M0 still required independent evaluation
of the implementation language for two different responsibilities:

- the **Worker** boundary established by ADR-0001 for transfer, compression, verification,
  and artifact-movement workloads;
- the future endpoint-resident **Agent**, responsible for participating in the versioned
  control protocol and executing only approved typed actions in the maintenance
  environment.

Issue #1 (`[WP] Define product, runtime, and stack architecture baseline`) explicitly
required that these choices not be inferred automatically from the Server language.

The owner direction recorded there was:

- Rust and Go were the primary Worker candidates;
- Go must not be selected merely as a learning opportunity;
- the Agent must be evaluated against its own runtime and deployment constraints;
- the cost of maintaining a polyglot stack matters for a primarily solo-maintained
  project.

This ADR records that evaluation and the resulting decision.

The original text also referenced M0 Discovery material that has since been reduced after
its durable conclusions were promoted. Issue #1 and Git history preserve the planning
history; this ADR preserves the architectural rationale.

## Evaluation

### Worker

The Worker is release-coupled to the Server by ADR-0001 but executes heavy workloads behind
a separate process/isolation boundary.

#### Rust

Advantages considered:

- shared toolchain and dependency ecosystem with the Server;
- shared CI/build knowledge;
- static native binaries suitable for the Linux appliance model;
- no garbage-collected runtime;
- strong fit for CPU- and I/O-intensive systems workloads;
- direct reuse of internal Rust libraries where doing so does not collapse an external
  contract boundary.

Because Workers ship with the Server release, using the same language also reduces
operational complexity for build and release.

#### Go

Go was considered technically viable.

Advantages included:

- simple concurrency primitives;
- fast compilation;
- straightforward static-binary deployment;
- mature support for network and systems services.

The principal cost was introducing a second toolchain, dependency ecosystem, and build
path for a component already release-coupled to a Rust Server.

No Worker-specific requirement identified during M0 materially outweighed that maintenance
cost.

### Agent

The Agent has different constraints from the Server and Worker and therefore required its
own evaluation.

Relevant requirements included:

- operation inside a Linux maintenance/live environment;
- small and predictable runtime footprint;
- typed protocol participation;
- authentication and reconnect behavior;
- action state handling;
- process/tool supervision;
- cancellation and result reporting;
- suitability for static or self-contained deployment.

#### Rust

Rust provides:

- static musl-compatible build options appropriate for Alpine-style environments;
- no garbage-collected runtime;
- strong compile-time representation of protocol and state-machine behavior;
- consistency with the Server implementation ecosystem.

#### Go

Go was also technically viable for an Agent:

- easy cross-compilation;
- static-binary deployment;
- a mature runtime for daemon-style software.

Its additional runtime footprint was not demonstrated to be a blocker, but selecting it
would introduce a second implementation ecosystem without a concrete Agent requirement
that demanded doing so.

Reusable PoC evidence about resource pressure in diskless maintenance environments remains
in `docs/reference/poc-lessons.md`. That evidence informed caution about footprint but did
not by itself prove either Rust or Go unsuitable.

### Cross-cutting maintenance cost

A split-language design is architecturally possible because Bamep contracts are explicitly
versioned.

However, for a primarily solo-maintained project, another implementation language creates
real recurring cost:

- another toolchain;
- another dependency ecosystem;
- another set of build/release conventions;
- another security/update surface;
- duplicated implementation knowledge.

No identified Worker or Agent requirement justified paying that cost in M0.

## Decision

Rust is the implementation language for both the Worker and the Agent.

The principal reasons are:

- consistency with the Rust Server selected by ADR-0002;
- reduced operational and maintenance cost;
- suitable deployment/runtime characteristics for both evaluated responsibilities;
- no identified Worker- or Agent-specific requirement that materially favored Go.

This is an implementation-language decision, not a contract-definition decision.

## Contract independence

**Externally relevant contracts must remain independent from the shared Rust implementation
language.**

Using Rust across Server, Worker, and Agent must not make Rust types, shared crates, or
internal APIs the sole definition of a wire or inter-process contract.

In particular:

- Agent Protocol is defined normatively by
  `docs/specifications/m0-agent-protocol-contract.md`;
- Administrative API behavior is defined by its applicable Specification;
- future externally relevant Worker or extension contracts must receive their own explicit
  contract authority when introduced.

Shared Rust representations are allowed as implementation conveniences.

For example, components may:

- share an internal crate;
- generate Rust representations from an explicit schema;
- reuse validation or codec libraries.

But a contract participant must remain implementable from the authoritative contract
without requiring inspection of another component's Rust source.

The single-language stack is a maintenance and deployment choice. It is not the
load-bearing definition of interoperability.

## Alternatives considered

### Go for Worker and/or Agent

Viable, but not selected.

No concrete technical blocker eliminated Go. The decision instead reflects the absence of
a requirement strong enough to justify a second implementation ecosystem.

A later materially different runtime requirement may justify reconsidering one component's
language independently.

### Split-language stack

Examples include Rust Server + Worker with a Go Agent, or Rust Server + Agent with a Go
Worker.

Technically valid because explicit versioned contracts permit independent implementations.

Rejected for the baseline because it introduces polyglot maintenance cost without an
identified project requirement that benefits from it.

### Python for Worker

Not selected as a primary candidate.

Python had historical value in the FORGE PoC, but CPU-intensive Worker responsibilities
and the new Rust Server baseline did not provide a concrete reason to add Python as another
production implementation ecosystem.

This is not a general claim that Python cannot perform such work; it was simply not the
preferred fit for this Bamep boundary.

## Consequences

- Server, Worker, and Agent share Rust as their selected implementation language.
- Build and release infrastructure should avoid unnecessary language-specific duplication
  across those components.
- Internal Rust crates may be shared where useful and where doing so preserves the
  responsibility boundaries of the architecture.
- External contracts remain explicit and independently versioned.
- A future non-Rust implementation must remain possible from the relevant contract alone.
- Changing the language of one component does not inherently require changing the others;
  such a change would require its own architectural justification.
- This ADR does not authorize or imply that Worker or Agent implementation already exists.

## Current implementation relationship

The current repository already implements Rust for Server and the shared protocol/trust
libraries.

Worker and production Agent implementation are not implied by this ADR and must not be
described as implemented until code exists.

`docs/architecture/README.md` is authoritative for the currently implemented repository
structure.

## Related specifications and decisions

- ADR-0001 — Server runtime topology and Worker/process isolation.
- ADR-0002 — Server/backend implementation language.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — normative responsibility,
  dependency, packaging, and versioning baseline.
- `docs/specifications/m0-agent-protocol-contract.md` — normative Agent Protocol contract.

## Related evidence

- `docs/reference/poc-lessons.md` — reusable lessons from the FORGE PoC, including
  maintenance-environment resource constraints.

## Related work

- Issue #1 — `[WP] Define product, runtime, and stack architecture baseline`.
- Issue #3 — historical M0 Work Package that produced the Agent control/action contract
  baseline now persisted in its durable Specification and ADRs.
