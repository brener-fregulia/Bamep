# ADR-0018: Isolated Worker Process Boundary for the Data Plane

Status: Accepted

## Context

ADR-0001 already requires heavy/risky workloads — explicitly including transfer,
compression, verification, and Artifact movement — to execute behind a separate Worker
process/isolation boundary, while leaving the concrete process/listener/IPC composition
free to evolve. ADR-0003 requires that Worker be Rust and that Server↔Worker contracts
remain explicit and language-independent in authority. ADR-0008 requires bulk Artifact
bytes to travel over HTTPS reusing the trusted Server TLS identity. None of those decisions
selects which process terminates the data-plane HTTPS/TLS listener, how the Worker obtains
TLS identity material, what local Server↔Worker IPC model is used, which process remains
authoritative for durable authorization and business state, or how the two processes are
supervised.

Issue #19 is the first M1 Work Package that must materialize this process boundary in real
code. These choices affect security boundaries (TLS private-key exposure), durability
authority (who may declare a Transfer/Artifact/chunk durably accepted), and backpressure
isolation (the failure mode ADR-0001 exists to prevent), so they cannot safely be
improvised inside #19's implementation.

Technical Spike #34 investigated this composition empirically against a disposable fixture
outside the repository; evidence is preserved in
`docs/reference/worker-data-plane-composition-spike.md`. That Spike is empirical evidence
for a decision, not itself accepted architecture. This ADR is the accepted decision informed
by that evidence and does not reproduce the Spike's fixture, measurements, or findings.

## Decision

### External data-plane listener

The isolated Worker process owns and terminates the external HTTPS data-plane listener
directly. Bulk Artifact request bodies must not be proxied through `bamepd` merely to
centralize TLS termination: doing so would route bulk-transfer backpressure and CPU/I/O
pressure back through the control-plane process path, reintroducing the exact failure mode
ADR-0001 isolates against.

### TLS identity

The Worker uses the same Server TLS identity already trusted by the Agent. No second Server
identity, trust anchor, CA hierarchy, mTLS, or Web PKI is introduced. This is an explicit
security consequence, not an implementation accident: it expands the set of Server-product
processes able to access the Server TLS private-key material, since the Worker must access
that key material to terminate the data-plane TLS connection.

Key material must not be transported through the normal Server↔Worker application IPC
protocol. The exact local provisioning mechanism (protected file access, inherited
descriptor, or another equivalent host-local mechanism) remains implementation-time unless
evidence later requires a stronger decision.

### Server↔Worker IPC

`bamepd` and Worker communicate over a local Unix Domain Socket, with `bamepd` as the UDS
server and Worker as the UDS client. The Worker reconnects after restart. The inter-process
contract must be explicit and independently versioned from Rust implementation types, per
ADR-0003's contract-independence requirement. This ADR does not define the complete IPC
message catalog; a later interoperability-contract Work Package owns the normative message
shapes (illustratively: handshake/version compatibility, authorization query/decision,
verified-chunk/durable-acceptance coordination, error/failure semantics, lifecycle/reconnect
behavior).

### Durable/business authority

`bamepd` remains the sole Domain/Application authority. Worker must not independently
decide: whether a transfer is authorized; whether a capability is semantically valid against
current durable state; credential revocation; Transfer lifecycle; Artifact lifecycle;
Job/Attempt lifecycle; durable chunk acceptance; or durable terminal success.

### PostgreSQL and storage

Worker does not own a PostgreSQL repository Adapter and does not independently mutate Bamep
durable Domain/Application state; durable state transitions remain coordinated through
`bamepd`. This avoids duplicated repository authority, duplicated Domain transition logic,
independent transaction ownership across processes, and Worker drifting toward its own
business-state authority.

This does not mean Worker has no storage access. Worker may use the appropriate storage
Port/Adapter implementation needed for heavy byte I/O (staging, Artifact movement). Storage
I/O and durable business-state persistence are different responsibilities: the former is
execution, the latter is authority reserved to `bamepd`.

### Worker responsibility (mechanism, not authority)

Worker owns execution of heavy/risky data-plane mechanisms: HTTPS request/body handling,
staging, storage I/O, digest computation, chunk byte verification, full Artifact byte
verification, Artifact movement, and (later) compression work. These are execution
responsibilities; they carry no authority over Domain state transitions, which remains with
`bamepd`.

### Authorization interaction

The authoritative authorization decision remains in `bamepd`. Worker may perform local
cryptographic/mechanical validation only as allowed by the later explicit Worker/data-plane
contract, but token/proof validity alone must never be converted into business
authorization by Worker. Every operation requiring current durable authorization must
obtain/consume an authoritative decision from `bamepd`. This ADR does not define the final
capability/proof protocol.

### Durable chunk acceptance ordering

The architecture must preserve: Worker receives/verifies bytes, then `bamepd` authoritatively
commits the required durable acceptance/state, and only then may the operation be reported
as durably accepted/successful. A Worker-local verified buffer/file is not itself
authoritative durable Artifact state. If Worker/`bamepd` IPC becomes uncertain around this
boundary, the system must preserve idempotent retry semantics and must never fabricate
durable success.

### IPC loss is fail-closed

Loss of Worker↔`bamepd` control IPC is fail-closed. While authoritative communication is
unavailable, Worker must not authorize new work from stale local assumptions, fabricate
durable chunk acceptance, fabricate Transfer/Artifact success, or independently advance
Domain state. The exact process policy after prolonged IPC loss (stay alive/reconnect versus
self-terminate and be respawned) is implementation-time; the architectural invariant is
fail-closed loss of authority.

### Supervision

`bamepd` supervises the Worker process. Worker crash/restart must not require restarting the
control-plane process. Worker process failure alone does not automatically prove that an
owning Attempt/Transfer succeeded or failed; existing uncertainty/resume contracts remain
authoritative.

### Same product, not a microservice

Despite process separation, Worker remains part of the Bamep Server product: it ships with
the Server, uses the Server release/version, is host-local, and introduces no independently
deployed/scaled service and no distributed-system/HA contract.

## Alternatives considered

### `bamepd` terminates data-plane TLS and proxies bulk bodies to Worker

Rejected. Bulk transport/backpressure would remain on the control-process path, weakening
ADR-0001's isolation purpose; `docs/reference/poc-lessons.md` already documents this coupling
failure mode from the FORGE PoC. Spike #34 demonstrated that direct Worker listener ownership
is viable. This candidate was rejected by architecture/evidence analysis building on already
-accepted evidence, not by an equivalent new empirical benchmark of this candidate itself.

### File-descriptor passing / inherited connections / kTLS-style composition

Rejected for V1: added lifecycle and platform complexity with no current Bamep requirement
justifying it; a direct Worker-owned listener satisfies the requirement more simply.

### Worker owns PostgreSQL/business authority

Rejected: fragments Domain/Application authority, creates duplicate persistence/transaction
responsibility, and pushes Worker toward independent service architecture, none of which V1
requires.

Same-process execution instead of a separate Worker is not reconsidered here; ADR-0001
already rejected it.

## Consequences

- `bamepd` and Worker are independently supervised OS processes sharing one product release.
- Worker gains the ability to access Server TLS private-key material; this expands the
  process attack surface for that identity, so key access must use host-local
  least-privilege mechanisms and must never cross ordinary Worker IPC messages.
- All durable Domain/Application authority, PostgreSQL access, and business-state
  transitions remain exclusively in `bamepd`.
- A later interoperability-contract Work Package must define the concrete IPC message
  catalog, handshake/versioning, and authorization/acceptance message shapes.
- Worker's HTTP framework choice (Axum/Tower per ADR-0017's Administrative precedent, or a
  lower-level rustls composition) is not selected here and remains implementation-time.
- No Specification, `docs/architecture/README.md`, or Issue #19 is modified by this ADR.

## Related architecture

- ADR-0001 — Worker process isolation (not reopened here).
- ADR-0003 — Worker language and contract-independence requirement.
- ADR-0008 — data-plane HTTPS/TLS-identity-reuse and chunk/resume contract this composition
  must satisfy.
- ADR-0013 — PostgreSQL persistence backend and Domain/Application isolation boundary this
  composition preserves.
- ADR-0017 — Axum/Tower precedent for the Administrative surface; not extended to Worker by
  this ADR.
- `docs/reference/worker-data-plane-composition-spike.md` — empirical evidence supporting
  this decision.

## Related work

- Issue #34 — Spike that produced the empirical evidence for this decision.
- Issue #19 — the M1 Work Package this decision makes architecture-ready for decomposition.
