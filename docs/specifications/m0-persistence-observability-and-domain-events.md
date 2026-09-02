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
- operator submission acceptance and per-target creation outcomes (see "Operator submission
  persistence and correlation").

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

### Required M1 normal-terminal Job/JobStep events

For the M1 normal Job/JobStep/Attempt execution path, the following are required, not
merely representative:

- `JobSucceeded` when Job enters `Succeeded`;
- `JobFailed` when Job enters `Failed`;
- `JobStepFailed` when JobStep enters `Failed`.

No event is required merely because `ActionAck{Accepted}` was observed, an Attempt enters
`InProgress`, `ActionProgress` was observed, an Attempt succeeds, or a JobStep succeeds.
`ActionAckAccepted`, `AttemptStarted`, `AttemptSucceeded`, and `JobStepSucceeded` are
deliberately not defined events. This is deliberate, not an omission: events remain
coarse-grained domain facts, not raw protocol history.

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

## Operator submission persistence and correlation

One operator intent over `1..N` Endpoints is accepted as a single operator submission and
then translated into independent one-Endpoint Jobs. This section is the normative owner of
what a submission persists and how it correlates to Jobs. ADR-0019 owns the rationale and the
rejected alternatives. This section defines no Administrative API, HTTP, or schema.

### Submission identity and content

An accepted submission durably records:

- `request_key` — the caller-provided logical-command identity (below);
- `submission_id` — the Server-minted durable identity of the accepted submission (below);
- the requested target Endpoint set;
- a canonical intent/configuration descriptor sufficient to verify retry equivalence;
- the accepted-at timestamp;
- one creation outcome per requested Endpoint (below).

### `request_key`

- exists before the first request completes;
- identifies one logical operator command across transport retries;
- is supplied by the caller / operator-plane client, not minted by the Server;
- a retry carrying the same `request_key` and an equivalent canonical command resolves to
  the same already-accepted submission;
- the same retained `request_key` presented with a non-equivalent target set, configuration,
  or intent is rejected;
- a deliberately new operator command uses a new `request_key`.

The future actor/IAM identity is not part of the canonical command-equivalence descriptor.

For the period during which idempotency is guaranteed, one retained `request_key` must not
identify more than one accepted submission. The concrete idempotency-retention duration and
any post-retention `request_key` reuse policy remain implementation/operations policy unless
a later product requirement constrains them.

Concrete wire representation of `request_key` is owned by future Administrative API work.

### `submission_id`

- is minted by the Server/Application when the submission is durably accepted;
- is the durable identity of that accepted submission;
- is distinct from any HTTP request, connection, or session identity;
- carries no Job admission, scheduling, dispatch, cancellation, reconciliation, or execution
  authority.

### Acceptance ordering

```text
accept the submission durably
→ then process individual targets
```

The acceptance commit must establish the immutable submission core (identities, the complete
requested target set, the canonical descriptor, accepted-at) and one durable per-target
creation state initialized to `Undecided` for every requested Endpoint. No Job for a
submission may be durably created before that submission is itself durable and discoverable.

### Per-target creation state

Each requested Endpoint has one durable per-target creation state. `Undecided` is durable
non-terminal creation-phase state; it is not transient.

```text
Undecided → Created(job_id)
Undecided → Rejected(reason)
```

- the state transitions exactly once;
- `Created(job_id)` is final for that submission;
- `Rejected(reason)` is final for that submission;
- a transport retry never re-evaluates a target that already holds a final outcome;
- only a target still durably `Undecided` — because processing was interrupted before it was
  decided — may be resumed;
- trying a previously `Rejected` Endpoint after circumstances change requires a new operator
  command and a new submission, not a re-drive of the settled one.

The concrete rejection-reason vocabulary is follow-up contract work and is not defined here.

### Atomic target creation

For one target, the `Undecided → Created(job_id)` transition and the durable creation of
that target's Job and its JobSteps commit in the same persistence transaction. Therefore:

- no durable Job exists for a submission target whose outcome is not `Created(job_id)`;
- no `Created(job_id)` outcome exists without that Job;
- a rolled-back attempt leaves the target `Undecided`, so resume is safe.

This is a normative atomicity requirement. It prescribes no repository type, transaction
API, statement, or schema.

### Rejected target

`Undecided → Rejected(reason)` is independently durable. `AuditRecord` is not the
authoritative source for ordinary submission creation outcomes; operational reconstruction
comes from authoritative submission and Job state. Security-relevant audit may later
correlate to a submission without becoming that authoritative source.

### Settled submission

Once no target remains `Undecided`, creation processing is settled. Settlement is not Job
success, execution success, cancellation state, or reconciliation state, and creates no
supervisory execution lifecycle. An aggregate "k of N created" view is derived, not
authoritative state.

### Correlation and events

`submission_id` is durable correlation state owned by the submission and carried by every
Job successfully created for that submission. Job-creation paths that are not operator
submissions do not carry it.

This contract does not add `submission_id` to the generic durable correlation-identifier
list in "Correlation" and does not extend the domain-event envelope. An event correlated to
a Job created from a submission is reachable only through that Job. A future requirement for
direct submission correlation on domain events, an outbox, or external integrations must
extend the relevant contract explicitly; it is not implied by this section. This section
adds no new domain event.

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

For a destructive Attempt, the required terminal audit record for a known authoritative
outcome (`Rejected`, `Succeeded`, or `Failed`) commits atomically with the durable
Attempt/JobStep/Job terminal state that outcome affects. That audit represents known Server
state derived from authenticated Agent Protocol evidence; it does not prove more than that
evidence establishes. No audit record is required merely for an `ActionAck{Accepted}`, an
`ActionProgress` tick, or a duplicate message; `ActionProgress` remains
transient/high-frequency by default.

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
- fixed numeric persistence-performance thresholds;
- Administrative API routes, HTTP methods, payloads, and the `request_key` wire form;
- the concrete submission rejection-reason vocabulary;
- the canonical intent/configuration descriptor format and its equivalence algorithm;
- exact idempotency-retention duration and post-retention `request_key` reuse policy;
- actor/IAM attribution of operator submissions.

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

**Operator submission**
- the acceptance commit establishes the immutable core and an `Undecided` state for every
  requested Endpoint before any Job for that submission is durably created;
- a retry with the same `request_key` and an equivalent canonical command resolves to the
  same submission and creates no duplicate Job;
- a retained `request_key` presented with a non-equivalent command is rejected;
- `Undecided → Created(job_id)` and that target's Job/JobStep creation commit atomically; a
  rolled-back target remains `Undecided`;
- `Created` and `Rejected` are never re-evaluated by a transport retry; only `Undecided`
  targets resume;
- per-target creation outcomes are reconstructable from submission and Job state without the
  audit trail.

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
- ADR-0019 — operator submission boundary for bulk Job creation (rationale and rejected
  alternatives).
- `docs/development/testing.md` — test-layer responsibilities.
- Issue #21 — M1 persistence-load validation.
