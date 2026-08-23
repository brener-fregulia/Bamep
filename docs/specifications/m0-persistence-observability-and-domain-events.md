# M0 — Persistence, Observability, and Domain-Event Model

Status: **Approved**

## Purpose and authority

This Specification is the normative persistence contract for Bamep durable state,
domain events, correlation, auditability, and recovery-relevant persistence behavior.

It defines **what must be durable and how persistence semantics compose with domain
transitions**.

It does not own:

- the choice of persistence backend — ADR-0013 owns PostgreSQL;
- SQLx, query style, migration mechanics, schema layout, or current Adapter conventions —
  `docs/development/persistence.md` owns those implementation conventions;
- lifecycle state machines owned by other Specifications;
- wire-protocol message semantics;
- current implementation structure.

ADR-0007 remains the historical origin of several persistence decisions but is
`Superseded` by ADR-0013. No current normative behavior should require reconstructing
requirements from ADR-0007.

## Durable versus transient/high-frequency boundary

Bamep persists meaningful domain state and state transitions, not every observation,
message, progress update, or telemetry sample.

### Durable state

The following are durable when applicable:

- Job, JobStep, and Attempt state and transitions;
- Endpoint identity, credential, and hardware-confidence state and transitions;
- the Endpoint authoritative current-boot projection and trusted-bootstrap state;
- inventory revisions, written on change;
- Artifact/Snapshot lifecycle metadata;
- domain events;
- safety-relevant audit records;
- correlation required to relate durable workflow, protocol, and transfer state.

Durability means the state required to preserve correctness across Server restart is
stored in the adopted durable persistence backend.

### Transient or high-frequency state

The following must not become one durable row per observation/message/sample by default:

- Agent connection and presence state;
- `ActionProgress` ticks;
- general application logs;
- high-frequency telemetry and metrics.

`ActionProgress` may keep only a latest-value representation when useful. General logs
and telemetry may use their own retention or aggregation mechanisms, but they are not
domain-history records merely because they are observable.

### Classification rule

Any new state introduced by later work must be classified deliberately as durable or
transient.

The default must not be "persist everything."

The deciding question is whether the information is required as durable domain,
security, audit, recovery, or correlation state — not how frequently the originating
message arrives.

## Authoritative current-boot persistence

The Endpoint authoritative current-boot projection is durable because it determines which
boot context is current across restart/reconnect and participates in the destructive
operation safety model.

The projection includes the durable identity/correlation required by the Endpoint
Specification, including:

- `boot_context_id`;
- the current 32-byte `boot_nonce`;
- trusted-bootstrap state (`NotEstablished` or `Established`).

Historical `BootContext` records do not become current merely because they resolve to the
same Endpoint.

If no authenticated current boot can be established for pre-existing/unknown data, the
state remains current-boot-absent and trusted bootstrap is not established. Persistence
must not fabricate a historical nonce or compatibility path that creates trust.

Detailed current-boot state transitions, old-boot rejection, and trusted-bootstrap
authorization semantics belong to `docs/specifications/m0-endpoint-identity-lifecycle.md`
and `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md`.

## Domain-event model

Domain events are durable, coarse-grained facts describing committed domain transitions.

They are not:

- raw protocol-message history;
- telemetry;
- general application logs;
- an event-sourcing replay log.

Events are useful for Bamep's own durable history and for future integrations without
exposing the internal database as an integration contract.

### Baseline event catalog

The catalog is representative and extensible as later Specifications introduce additional
domain transitions.

| Event | Emitted when |
|---|---|
| `EndpointPendingEnrollment` | an Endpoint enters `PendingEnrollment` |
| `EndpointEnrolled` | an Endpoint enters `Enrolled` |
| `EndpointHardwareConfidenceChanged` | hardware-confidence state changes |
| `EndpointRetired` | an Endpoint enters `Retired` |
| `InventoryRevisionRecorded` | a new durable inventory revision is recorded |
| `JobStarted` | a Job transitions to `Running` |
| `JobSucceeded` / `JobFailed` / `JobCancelled` | a Job reaches the matching terminal state |
| `JobStepFailed` | a JobStep reaches `Failed` |
| `AttemptIndeterminate` | an Attempt is explicitly closed `Indeterminate` |
| `ArtifactCreated` / `ArtifactVerified` | the applicable Artifact lifecycle transition occurs |
| `OperatorDecisionRecorded` | a safety-relevant operator decision is durably recorded |

An event is emitted from the underlying committed transition, not reconstructed from
high-frequency observations.

Artifact-specific event semantics belong to
`docs/specifications/m0-data-plane-and-storage-contracts.md`.

### Domain-event envelope

Every durable domain event carries at least:

- `event_id` — unique and immutable;
- `event_type` — the event name;
- `event_version` — the schema version for that event type;
- `occurred_at` — the time associated with the committed transition;
- applicable correlation identifiers;
- `payload` — event-type-specific data.

Event-type versions evolve independently.

This Specification does not define an external event transport, webhook, message broker,
or ERP-facing publication API.

## Correlation model

Durable state and domain events carry whichever identifiers are applicable so Bamep can
relate Endpoint, workflow, protocol, and transfer activity.

The baseline correlation set is:

- `endpoint_id`;
- `job_id`;
- `jobstep_id`;
- `attempt_id`;
- `action_id`;
- `transfer_id`.

### Identity separation

`attempt_id` and `action_id` are distinct:

- `attempt_id` is Server-side Domain identity for one JobStep execution attempt;
- `action_id` is Agent Protocol wire identity.

An Agent-executed Attempt currently relates them 1:1, but they must not be merged into one
identifier or coupled to the same identity scheme.

`transfer_id` is the durable identity of one logical data-plane transfer and is likewise
distinct from HTTP request/connection identity and from `attempt_id`. Its full lifecycle
belongs to `docs/specifications/m0-data-plane-and-storage-contracts.md`.

Additional correlation identifiers may be introduced by the Specification that owns a new
concept. They must compose with, rather than silently replace, the existing identifiers.

## Transactional consistency

When a durable domain transition requires a domain event and/or audit record, all required
parts commit atomically in the same persistence transaction:

- the durable domain-state mutation;
- its required domain event;
- its required audit record.

A crash must never leave:

- committed domain state without a required event/audit record; or
- a committed event/audit record for a transition that did not commit.

An audit record associated with that transition is not a best-effort side write.

### This is not event sourcing

Current durable domain state remains the source of truth for Bamep operation.

Domain events describe committed transitions and become durable as part of the same atomic
transaction. Bamep does not reconstruct current state by replaying the event stream.

Adopting an external publication mechanism in the future does not change this invariant
unless a later approved architecture decision explicitly does so.

## Persist-before-send ordering

A persistence transaction and a network send cannot be atomic with each other.

Whenever Bamep creates durable state that authorizes or establishes a later outbound
protocol effect, the required durable transaction commits **before** the Server attempts
that outbound delivery.

### Agent action dispatch

For an Agent-executed Attempt:

1. the applicable final dispatch preconditions pass;
2. the Attempt/action correlation and `Dispatched` commitment are created;
3. required domain event(s) and audit record(s) are included in the same durable
   transaction;
4. the transaction commits;
5. only then may the Server attempt `ActionDispatch`.

There must never be a path where the Agent can receive an `ActionDispatch` for which the
Server has no durable Attempt/correlation/audit state required by this contract.

A crash after commit but before or during transmission is an uncertain delivery outcome.
It is reconciled through the Job lifecycle and Agent Protocol contracts; it must not cause
blind redispatch of destructive work.

### Credential/session establishment

Durable Endpoint/credential/current-boot changes required for a successful authentication
exchange commit before the Server attempts to send the corresponding
`SessionEstablished`.

A dropped connection after commit is therefore a delivery-recovery case, not permission to
roll back or fabricate durable state.

Credential replacement/recovery semantics belong to the Endpoint identity contract and
the applicable credential ADRs.

## First contact, reboot, and current-boot atomicity

For first contact, the durable transition establishing the initial Endpoint state must
atomically include the persistence state required by the Endpoint contract, including:

- Endpoint `PendingEnrollment` when that is the applicable enrollment path;
- the credential-chain/lookup projection;
- resolved `BootContext` correlation;
- authoritative current-boot selection/current nonce;
- `TrustedBootstrapState::NotEstablished`;
- the required `EndpointPendingEnrollment` event.

That transaction commits before `SessionEstablished` is attempted.

For a genuine reboot of an existing Endpoint, the applicable identity-continuity and
credential transition must atomically include:

- the new `BootContext` correlation;
- replacement of the authoritative current boot/current nonce;
- reset of trusted-bootstrap state to `NotEstablished`;
- any other durable credential/current-boot changes required by the Endpoint contract.

That transaction also commits before `SessionEstablished` is attempted.

Same-boot credential reconnect/rotation preserves the authoritative current-boot
projection according to the Endpoint identity contract.

Rejected authentication must not partially mutate this durable state.

## Trusted-bootstrap event and audit policy

Trusted-bootstrap state is durable security/domain state.

Under the current contract:

- establishing trusted bootstrap does **not** emit a
  `TrustedBootstrapEstablished` domain event;
- rejected bootstrap evidence does **not** emit a
  `TrustedBootstrapRejected` domain event;
- evidence acceptance/rejection does **not** create a new immutable audit record merely
  because evidence was processed.

This is an explicit contract decision.

It does not mean every durable field change generally lacks an event. It means the current
event/audit catalog has no trusted-bootstrap-specific event or audit obligation.

Existing enrollment events and enrollment-approval audit requirements remain unchanged.

A future requirement for trusted-bootstrap event publication or audit history requires an
explicit Specification update.

## Inventory persistence boundary

Inventory is durable **on revision change**, not on every report or poll.

Requirements:

- unchanged observed inventory creates no new durable inventory revision;
- a changed inventory creates a new durable revision;
- the authoritative current inventory-revision identifier is persisted for use by
  lifecycle/safety contracts;
- historical revisions are retained sufficiently for the audit and precondition behavior
  required by the product.

A concrete pruning duration or retention window is not defined here.

The Endpoint and Job lifecycle Specifications own the semantics of "sufficiently fresh
inventory" for destructive dispatch. This Specification owns the persistence behavior of
the revision itself.

## Auditability

Audit records are durable and immutable once written.

They carry applicable correlation identifiers and whichever actor information is known at
the point of recording.

Actor attribution distinguishes, when known:

- an **operator actor** for a human decision;
- a **system actor** for an automated system decision.

The concrete operator authentication/identity model is outside this Specification.

### Safety-relevant operator decisions

Audit records are required for applicable safety-relevant operator decisions, including:

- Endpoint enrollment approval;
- hardware-confidence conflict resolution;
- reconciliation decisions that close an Attempt as `Indeterminate`;
- authorization of a further destructive Attempt where explicit authorization is required;
- Job cancellation decisions.

### Destructive execution

Destructive execution requires durable auditability of:

- the authorization/decision enabling dispatch when one is applicable;
- the destructive dispatch commitment;
- the known terminal outcome, or the eventual `Indeterminate` resolution when the real
  outcome cannot be established.

The dispatch-commitment audit record represents the Server's durable authorization and
commitment to transmit. It does **not** prove that the network frame was sent, received, or
executed.

Actual Agent-side knowledge remains represented by Agent Protocol acknowledgement/result/
status evidence and the resulting Attempt lifecycle state.

## Observability responsibilities

The structural observability baseline is correlation.

Domain events provide durable transition-level history.

High-frequency telemetry provides a different kind of operational visibility and must not
be treated as a substitute for domain events.

Likewise, domain events are not a substitute for high-frequency runtime telemetry.

Telemetry retention/aggregation, if implemented, is an implementation/operations policy
outside this Specification unless a later product requirement constrains it.

## Persistence-backend relationship

PostgreSQL is the current backend selected by ADR-0013.

The semantic requirements in this Specification are not PostgreSQL implementation details.

Domain/Application code must consume persistence through the appropriate Port boundary
rather than making this contract depend on PostgreSQL/SQLx APIs.

Current PostgreSQL/SQLx schema, query, migration, and Adapter conventions belong to
`docs/development/persistence.md`.

## Out of scope

This Specification does not define:

- concrete PostgreSQL schema/table layout;
- indexes or query plans;
- SQLx API usage;
- migration tooling or migration-history policy;
- concrete database connection/pool configuration;
- operator authentication/identity implementation;
- telemetry retention/aggregation policy;
- external domain-event delivery or publication;
- Artifact-specific event payloads beyond the generic event/correlation contract;
- Job/JobStep/Attempt lifecycle transitions;
- Endpoint identity/credential/current-boot lifecycle semantics;
- Agent Protocol wire semantics;
- fixed numeric persistence-performance thresholds.

## Validation expectations

Validation must cover the persistence semantics defined here.

### Unit/domain and contract validation

At minimum:

- domain events are emitted according to the applicable transition contract without
  unintended duplication;
- domain-event envelope/version expectations are validated;
- unchanged inventory does not create a new durable revision;
- changed inventory does;
- trusted-bootstrap state changes follow the explicit current event/audit policy.

### Persistence and recovery validation

At minimum:

- required durable state survives Server restart;
- transient presence/progress loss after restart is not interpreted as durable-state loss;
- state + required event + required audit commit together or fail together;
- persist-before-send ordering is preserved;
- first-contact/reboot persistence does not leave partial credential/current-boot state.

Implementation-level persistence tests must use the real adopted backend according to
`docs/development/testing.md` and `docs/development/persistence.md`.

### Representative load validation

The adopted persistence baseline must be measured under the M1 20–24 concurrent Simulated
Endpoint target.

The measurement records actual:

- durable write volume;
- contention;
- latency;
- backpressure.

No numeric pass/fail threshold is invented in this Specification before evidence exists.

Issue #21 (`[WP] Validate Simulator concurrency and M1 persistence baseline`) owns execution
and recording of this empirical M1 validation.

If the representative result is unacceptable, the persistence-backend decision in
ADR-0013 must be reconsidered explicitly rather than silently worked around.

## Acceptance mapping

This Specification satisfies the M0 persistence/observability contract by defining:

- the durable/transient boundary;
- domain-event semantics and envelope;
- correlation requirements;
- atomic transition/event/audit persistence;
- persist-before-send behavior;
- inventory revision persistence;
- auditability;
- validation obligations.

Issue #5 is the historical M0 Work Package that produced this contract.

## Related specifications and decisions

- ADR-0013 — current PostgreSQL persistence-backend decision.
- ADR-0007 — superseded historical SQLite persistence decision.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — Endpoint, credential,
  current-boot, inventory-safety, and destructive-precondition semantics.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — Job/JobStep/Attempt,
  dispatch, reconciliation, and retry semantics.
- `docs/specifications/m0-agent-protocol-contract.md` — Agent wire correlation,
  acknowledgement, progress, status, and result semantics.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — Artifact and transfer
  lifecycle/correlation semantics.
- `docs/specifications/m0-simulator-contract-and-validation-strategy.md` — Simulator
  validation contract.
- `docs/development/persistence.md` — current PostgreSQL/SQLx implementation and migration
  conventions.
- `docs/development/testing.md` — validation strategy and test-layer responsibilities.

## Related work

- Issue #5 — historical M0 persistence/observability/domain-event Work Package.
- Issue #21 — current M1 concurrency and persistence-load validation Work Package.

Status: Approved.
