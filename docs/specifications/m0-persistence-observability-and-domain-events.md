# M0 — Persistence, Observability, and Domain-Event Model

Status: **Approved**

This Specification is the normative contract for Bamep durable/transient state, domain
events, correlation, auditability, and recovery-relevant persistence behavior. ADR-0013
owns the PostgreSQL backend decision; `docs/development/persistence.md` owns SQLx/schema/
migration conventions.

## Durable versus transient state

Persist meaningful domain state and transitions, not every observation.

**Durable when applicable:**
- Job/JobStep/Attempt state;
- Endpoint identity, credential, hardware-confidence, and authoritative current-boot state;
- inventory revisions on change;
- Artifact lifecycle metadata;
- domain events;
- safety-relevant audit records;
- correlation needed for recovery and authorization.

**Transient/high-frequency by default:**
- Agent connection/presence;
- `ActionProgress` ticks;
- general logs;
- high-frequency telemetry/metrics.

New state must be classified explicitly. The default is not "persist everything."

### Authoritative current boot

The Endpoint current-boot projection is durable because it participates in safety decisions
across restart/reconnect. It includes at least:

- `boot_context_id`;
- current 32-byte `boot_nonce`;
- trusted-bootstrap state (`NotEstablished | Established`).

Historical `BootContext` rows never become current merely because they resolve to the same
Endpoint. Unknown/pre-existing data without an authenticated current boot remains
current-boot-absent / not trusted; no historical nonce is fabricated.

Lifecycle and old-boot rejection semantics belong to `m0-endpoint-identity-lifecycle.md`
and `m0-trusted-bootstrap-and-server-fingerprint-contract.md`.

## Domain events

Domain events are durable coarse-grained facts about committed domain transitions. They are
not raw protocol history, telemetry, general logs, or an event-sourcing replay log.

Representative catalog:

| Event | Emitted when |
|---|---|
| `EndpointPendingEnrollment` | Endpoint enters `PendingEnrollment` |
| `EndpointEnrolled` | Endpoint enters `Enrolled` |
| `EndpointHardwareConfidenceChanged` | hardware-confidence state changes |
| `EndpointRetired` | Endpoint enters `Retired` |
| `InventoryRevisionRecorded` | a new inventory revision is committed |
| `JobStarted` | Job enters `Running` |
| `JobSucceeded` / `JobFailed` / `JobCancelled` | Job reaches matching terminal state |
| `JobStepFailed` | JobStep reaches `Failed` |
| `AttemptIndeterminate` | Attempt is closed `Indeterminate` |
| `ArtifactCreated` / `ArtifactVerified` | applicable Artifact transition occurs |
| `OperatorDecisionRecorded` | a safety-relevant operator decision is committed |

Events are emitted from the underlying transition, not reconstructed from observations.

### Event envelope

Every durable event carries at least:

- unique immutable `event_id`;
- `event_type`;
- independently versioned `event_version`;
- `occurred_at`;
- applicable correlation identifiers;
- event-specific `payload`.

External event delivery/webhooks/brokers are outside this Specification.

## Correlation

Durable state/events carry whichever identifiers apply:

- `endpoint_id`;
- `job_id`;
- `jobstep_id`;
- `attempt_id`;
- `action_id`;
- `transfer_id`.

`attempt_id` (Server Domain identity) and `action_id` (Agent Protocol wire identity) remain
distinct even when related 1:1.

`transfer_id` is the durable logical transfer identity defined by
`m0-data-plane-and-storage-contracts.md`, not an HTTP request/connection identity.

## Atomic persistence

When a durable transition requires an event and/or audit record, the required:

- domain-state mutation;
- domain event;
- audit record

commit atomically in the same persistence transaction.

A crash must not leave committed state without its required event/audit record, or a
committed event/audit record for a transition that did not commit.

Current durable state is the source of truth; Bamep is not event-sourced.

## Persist-before-send

A database transaction and network send cannot be atomic. Therefore required durable state
must commit **before** the Server attempts the corresponding outbound protocol effect.

### Agent action dispatch

For Agent-executed Attempts:

1. final dispatch preconditions pass;
2. Attempt/action correlation and `Dispatched` commitment are created;
3. required event/audit records are included;
4. the transaction commits;
5. only then may `ActionDispatch` be sent.

A crash after commit but before/during delivery is reconciled through the Job lifecycle and
Agent Protocol contracts; it never permits blind destructive redispatch.

### Session establishment

Endpoint/credential/current-boot changes required by successful authentication commit
before `SessionEstablished` is attempted.

Dropped delivery after commit is a recovery case, not permission to roll back durable
state.

## First contact and reboot atomicity

First contact atomically persists the state required by the Endpoint contract, including
the applicable:

- `PendingEnrollment`;
- credential-chain/lookup projection;
- resolved `BootContext`;
- authoritative current boot/nonce;
- `TrustedBootstrapState::NotEstablished`;
- required `EndpointPendingEnrollment` event.

A genuine reboot atomically persists the new boot correlation/current boot, credential
changes, and reset to `NotEstablished`.

Both commit before `SessionEstablished`.

Same-boot reconnect/rotation preserves current-boot state. Rejected authentication must not
partially mutate durable identity/credential/current-boot state.

## Trusted-bootstrap event/audit policy

Trusted-bootstrap state is durable security/domain state, but under the current contract:

- establishment emits no `TrustedBootstrapEstablished` event;
- rejected evidence emits no `TrustedBootstrapRejected` event;
- evidence acceptance/rejection alone creates no immutable audit record.

Enrollment events and enrollment-approval audit requirements are unchanged. Adding
trusted-bootstrap-specific event/audit history requires an explicit contract update.

## Inventory persistence

Inventory is durable **on revision change**, not per report/poll.

- unchanged inventory creates no revision;
- changed inventory creates a new revision;
- the authoritative current revision identifier is durable;
- historical revisions are retained sufficiently for required audit/safety behavior.

Concrete pruning/retention duration is implementation-time.

Freshness semantics for destructive dispatch belong to Endpoint/Job lifecycle contracts.

## Auditability

Audit records are durable and immutable and carry applicable correlation plus known actor
information (`operator` or `system`).

Required safety-relevant operator decisions include:

- enrollment approval;
- hardware-confidence resolution;
- closing an Attempt `Indeterminate`;
- authorizing further destructive work where required;
- Job cancellation.

Destructive execution additionally requires durable auditability of:

- applicable authorization/decision;
- destructive dispatch commitment;
- known terminal outcome or eventual `Indeterminate` resolution.

The dispatch-commitment audit record proves the Server durably authorized/committed the
dispatch; it does **not** prove network transmission, receipt, or execution. Agent-side
knowledge comes from Agent Protocol evidence and resulting Attempt state.

Required audit records participate in the same atomic transaction as their transition/event.

## Observability

Correlation is the structural observability baseline.

Domain events provide durable transition history. High-frequency telemetry provides
runtime detail. Neither substitutes for the other.

Telemetry retention/aggregation is an implementation/operations policy unless a later
product requirement constrains it.

## Backend boundary

PostgreSQL is selected by ADR-0013, but this contract is backend-independent at the
Domain/Application boundary.

PostgreSQL/SQLx schema, query, migration, and Adapter conventions belong to
`docs/development/persistence.md`.

## Out of scope

- concrete schema/tables/indexes/query plans;
- SQLx APIs and migration mechanics;
- database pool/connection configuration;
- operator authentication implementation;
- telemetry retention policy;
- external event publication;
- lifecycle/wire semantics owned by other Specifications;
- fixed numeric persistence-performance thresholds.

## Validation

At minimum:

**Domain/contract**
- required events are emitted once for their transitions;
- event envelope/version rules hold;
- unchanged inventory creates no revision; changed inventory does;
- trusted-bootstrap state follows the explicit no-event/no-audit policy.

**Persistence/recovery**
- durable state survives Server restart;
- transient presence/progress loss is not interpreted as domain-state loss;
- required state + event + audit commit together or fail together;
- persist-before-send ordering holds;
- first-contact/reboot persistence cannot leave partial identity/credential/current-boot
  state.

Implementation-level persistence tests use the real adopted backend.

**Representative load**
Issue #21 owns M1 validation at 20–24 concurrent Simulated Endpoints, measuring actual
durable write volume, contention, latency, and backpressure. No numeric threshold is
invented here before evidence exists; unacceptable results require explicit reconsideration
of the persistence baseline.

## Related

- ADR-0013 — PostgreSQL backend decision.
- ADR-0007 — superseded historical SQLite decision.
- `m0-endpoint-identity-lifecycle.md` — Endpoint/current-boot semantics.
- `m0-job-lifecycle-and-scheduling.md` — Job/Attempt dispatch and reconciliation.
- `m0-agent-protocol-contract.md` — Agent wire correlation/evidence.
- `m0-data-plane-and-storage-contracts.md` — Artifact/transfer semantics.
- `docs/development/persistence.md` — PostgreSQL/SQLx implementation conventions.
- `docs/development/testing.md` — test-layer responsibilities.
- Issue #21 — M1 persistence-load validation.
