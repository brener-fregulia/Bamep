# Bamep Architecture

This directory describes architecture **implemented in the current repository**. Code and
tests are final evidence for current behavior; Specifications own normative behavior and
ADRs own decision rationale.

## Current workspace

Bamep currently has five Rust crates:

| Crate | Implemented responsibility |
| --- | --- |
| `bamep-trusted-bootstrap` | Trusted-bootstrap primitives, assertion parsing/transcript, and verification |
| `bamep-agent-protocol` | Rust wire model/codec for the implemented Agent Protocol v1 slice |
| `bamep-domain` | Pure Endpoint identity, boot-context, trusted-bootstrap, runtime-credential, and Job/JobStep workflow business logic |
| `bamep-server` | Application services, Ports, PostgreSQL/transport Adapters, and Agent session handling |
| `bamep-simulator` | Simulated Agent participant using real trusted-bootstrap and WSS/Agent Protocol boundaries |

Planned components remain outside Architecture until corresponding code exists.

## Dependency boundaries

The implemented structure preserves these rules:

- `bamep-trusted-bootstrap` owns only trusted-bootstrap contract representations/operations;
  it has no Domain, Server, Simulator, Agent Protocol, async-runtime, TLS, or WebSocket
  dependency.
- `bamep-agent-protocol` is a transport-independent Rust representation of the normative
  Agent Protocol Specification.
- `bamep-domain` contains pure business logic: transitions take time/secrets explicitly and
  perform no I/O or persistence.
- `bamep-simulator` depends on Agent Protocol and trusted-bootstrap, not Domain or Server; it
  exercises the external Agent-side boundary.
- `bamep-server` contains `application`, `ports`, and `adapters`; Application coordinates
  through Ports and Domain, while infrastructure-specific dependencies stay in Adapters.
- PostgreSQL/SQLx and Agent transport/gateway implementations are Server Adapter concerns.

Infrastructure must not leak into Domain transitions.

## Implemented Agent-side path

The current Simulator/Server slice:

1. establishes trusted bootstrap from simulated bootstrap material;
2. establishes the expected Server certificate fingerprint before Agent authentication;
3. connects through WSS with exact Server-certificate pinning;
4. exchanges Agent Protocol v1 authentication over the real WebSocket transport;
5. sends retained trusted-bootstrap evidence after session establishment;
6. evaluates Endpoint identity, credential, BootContext, and trusted-bootstrap state through
   Server Application/Domain logic, alongside the durable hardware-confidence dimension every
   newly created Endpoint now carries;
7. accepts post-session opaque `InventoryReport` snapshots and records a Server-owned current
   inventory revision only on semantic change;
8. persists durable state and required domain events atomically through the PostgreSQL Adapter
   boundary.

Production boot-chain inputs are still represented by Simulator fixtures where the physical
Integration Environment is not implemented. The WSS and Agent Protocol boundary itself is
not replaced by an in-process fake.

## Implemented internal workflow-creation path

A structurally separate internal Application control path
(`bamep_server::application::JobService::create_workflow`, exercised directly by tests today)
creates one durable `Pending` Job with one or more ordered `Pending` JobSteps for an existing
`Enrolled` Endpoint, atomically, through the `JobRepository` Port and its PostgreSQL Adapter.
This path never runs through Agent Protocol message handling. It stops at durable creation;
admission into `Running`, preliminary JobStep eligibility, and final destructive-dispatch
commitment are implemented separately — see "Implemented Job admission and scheduling
baseline" and "Implemented final destructive-dispatch commitment" below.

## Implemented destructive JobStep intent authorization

A structurally separate internal Application control path
(`bamep_server::application::DestructiveIntentService::authorize`) attaches one durable
destructive-operation authorization snapshot — the Server's current inventory revision and
current target-disk fingerprint at authorization time — to one eligible `Pending` JobStep,
atomically, through the `JobRepository` Port and its PostgreSQL Adapter. The caller identifies
only the Job/JobStep; the evidence itself always comes from the real `InventoryRepository` and
`TargetRevalidationPort`, never from a caller-supplied value. The snapshot is single-assignment:
once attached it is never refreshed from later inventory/target observations, and a JobStep
remains `Pending` throughout. This path stops at the durable snapshot; final dispatch
revalidation and Attempt creation are implemented separately — see "Implemented final
destructive-dispatch commitment" below.

## Implemented safe-dispatch evidence inputs

Three independent evidence inputs for the destructive-dispatch gate exist structurally, now
composed by the final destructive-dispatch commitment path below:

- durable hardware confidence is a fourth `EndpointAggregate` dimension, persisted with the
  Endpoint and initialized to `Consistent` at creation, independently of enrollment approval;
- an in-process Runtime Presence Registry (`bamep_server::runtime::presence`) tracks currently
  authenticated Agent Protocol sessions per Endpoint; the real `AgentControlGateway` registers
  and unregisters sessions against it and it is never persisted;
- a `TargetRevalidationPort` Port, backed today only by a deterministic in-memory fixture
  (`bamep_server::adapters::target_revalidation_fixture`), exposes an opaque current
  target-disk fingerprint per Endpoint, independently of inventory-revision state.

## Implemented Job admission and scheduling baseline

A structurally separate internal Application control path
(`bamep_server::application::JobSchedulingService`) implements two narrow M1 scheduling
transitions, atomically, through the `JobRepository` Port and its PostgreSQL Adapter:

- `admit`: a `Pending` Job becomes `Running` only after durably acquiring Job-scoped Endpoint
  exclusivity, committed atomically with exactly one `JobStarted` domain event. Durable
  exclusivity is represented by a PostgreSQL partial unique index over `Running`/`Cancelling`
  Job states per Endpoint (`jobs_active_endpoint_exclusivity`) rather than a separate lease
  table; a competing same-Endpoint admission attempt is rejected, never silently admitted;
- `satisfy_current_step_preconditions`: the structurally current ordered `Pending` JobStep of
  a `Running` Job may become `PreconditionsSatisfied`. A later/non-current step cannot skip an
  earlier unfinished one. No JobStep becomes `Dispatching` here, and no Attempt exists.

A separate in-process Runtime Service, `bamep_server::runtime::resource_arbiter::TechnicalResourceArbiter`,
grants/releases deterministic transient Attempt-scoped technical-resource reservations
(opaque `ReservationId` handles over generic named resource kinds and quantities). It is
memory-only, never persisted, and is composed by the final destructive-dispatch commitment
path below.

## Implemented final destructive-dispatch commitment

A structurally separate internal Application control path
(`bamep_server::application::FinalDispatchService::commit_destructive_dispatch`) performs the
final destructive-dispatch authorization gate and durable commitment. It acquires the required
technical-resource reservation from `TechnicalResourceArbiter` first; on success it locks the
Job/JobStep/Endpoint state through the `JobRepository` Port and its PostgreSQL Adapter, resolves
current Runtime Presence Registry and `TargetRevalidationPort` evidence and "now" only after
that lock is held, and calls the pure Domain decision
`bamep_domain::evaluate_final_destructive_dispatch` (`bamep-domain`'s `final_dispatch` module),
which composes workflow/scheduler authorization with the complete seven-item destructive gate
without inferring any one precondition from another.

On success, one PostgreSQL transaction atomically commits the candidate JobStep's
`PreconditionsSatisfied -> Dispatching` transition, one fresh `attempts` row
(`bamep_domain::Attempt`/`AttemptId`/`ActionId`, currently only ever `Dispatched`), and a
destructive-dispatch `audit_records` row correlating `endpoint_id`/`job_id`/`job_step_id`/
`attempt_id`/`action_id`. On revalidation failure the JobStep returns to `Pending` and the
reservation is released; on resource unavailability nothing is touched. No new `DomainEvent` is
introduced for this commitment.

This path never sends `ActionDispatch` and never opens an Agent Protocol/WebSocket connection —
transmission of the already-committed Attempt remains unimplemented.

## Implemented normal typed-action dispatch and evidence completion

A structurally separate transient outbound-delivery Runtime Service,
`bamep_server::runtime::outbound_sessions::OutboundSessionDirectory`, tracks the most recently
registered live authenticated `SessionId` per Endpoint and its bounded outbound command channel
(`OutboundCommand`), independently of `PresenceRegistry`. `AgentControlGateway`'s authenticated-
session task registers/unregisters the same exact `SessionId` in both registries on every exit
path, splits its owned WebSocket into read/write halves, and runs one `tokio::select!` loop
serving inbound Agent Protocol frames and outbound `OutboundCommand`s from the same task — the
sole serialized writer of that session's socket. `OutboundSessionDirectory` implements the
`bamep_server::ports::AgentDispatchPort` Port; Application depends on that Port, never on
`tokio-tungstenite` directly.

`bamep_server::application::ActionDispatchService` registers a transient
`bamep_server::runtime::reservation_registry::AttemptReservationRegistry` mapping
(`AttemptId -> ReservationId`, composing #32's `TechnicalResourceArbiter`) before transmitting
`ActionDispatch` for the single M1 concrete action (`bamep.m1.simulated-execution`, version `1`,
closed empty `parameters`), converting the committed `Attempt.action_id`'s exact UUID into the
Agent Protocol wire identity without generating a replacement.

`bamep_server::application::ActionEvidenceService` applies inbound `ActionAck`/`ActionResult`
evidence: it locks Attempt -> JobStep -> Job (in that order) through
`PostgresJobRepository::apply_action_evidence`, calls the pure Domain decision
`bamep_domain::apply_action_evidence` (`bamep-domain`'s `action_evidence` module), and — on an
`Applied` outcome — persists the resulting Attempt/JobStep/Job state, the required
`JobStepFailed`/`JobFailed`/`JobSucceeded` domain events, and (for a terminal outcome) one audit
record, all atomically. Only after that commit does it remove the Attempt's reservation mapping
exactly once and release it through the arbiter. Duplicate/delayed/conflicting evidence is
resolved by the Domain decision into `NoOp` (idempotent, matches already-committed state) or
`Conflict` (ignored, never overwrites a different already-committed terminal outcome) — the
Adapter persists nothing for either. `ActionProgress` never reaches this service: the Gateway
treats it as transient advisory metadata only.

`AgentControlGateway` correlates every inbound `ActionAck`/`ActionResult` to the authenticated
Endpoint of the session it arrived on; an unknown `action_id` and one belonging to another
Endpoint's Job both resolve identically, so the Server never reveals which case occurred.

## Implemented Job cancellation

`bamep_agent_protocol` adds `CancelAction{action_id}` (Server -> Agent) and
`CancelAck{action_id, outcome}` (Agent -> Server, `Cancelled | AlreadyCompleted | CannotCancel |
Unknown`) to `AgentProtocolMessage`, both carrying `correlation_id == action_id` and never a
replacement action identity.

`bamep_server::application::CancellationService` holds two structurally distinct
responsibilities on one shared instance:

- `request` — the internal operator/harness cancellation-request control path (never callable
  from inbound Agent Protocol handling). It locks the target Job and, when one currently exists,
  the JobStep-current Attempt in `Dispatched`/`InProgress`/`AwaitingReconciliation`, in the same
  Attempt -> JobStep -> Job order `apply_action_evidence` already uses
  (`PostgresJobRepository::request_cancellation`), then calls the pure Domain decision
  `bamep_domain::request_cancellation`. `Running` with an active Attempt commits `Cancelling` plus
  an immutable operator cancellation audit; `Running` with none commits `Cancelled` directly plus
  one `JobCancelled` event and the audit. Only after that transaction commits does it attempt
  `CancelAction` transmission, reusing `OutboundSessionDirectory`/`AgentDispatchPort::cancel_action`
  — the same outbound session path `ActionDispatchService` uses. A repeated request against an
  already-`Cancelling` Job is a persisted no-op: the durable `Running -> Cancelling` transition
  itself is the send-once gate, so no separate dedup registry is needed.
- `apply_cancel_ack` — inbound `CancelAck` evidence, invoked only by `AgentControlGateway`. It
  locks Attempt -> JobStep -> Job by `action_id`, exactly like `apply_action_evidence`, and calls
  the pure Domain decision `bamep_domain::apply_cancel_ack`. `Cancelled` against an active/
  uncertain Attempt commits Attempt/JobStep `Cancelled`, Job `Cancelled`, and one `JobCancelled`
  event. `Unknown`/`AlreadyCompleted` against `Dispatched`/`InProgress` commit
  `AwaitingReconciliation` with no event. `CannotCancel` never mutates state. On any terminal
  outcome it removes the Attempt's reservation mapping exactly once and releases it through the
  arbiter, exactly like `ActionEvidenceService`.

`bamep_domain::apply_action_evidence` (the same function `ActionEvidenceService` calls) now reads
the owning Job's current state to decide the terminal Job outcome: normal `ActionAck{Rejected}`/
`ActionResult{Succeeded|Failed}` evidence arriving while the Job is already `Cancelling` still
preserves the Attempt/JobStep result exactly, but resolves the Job to `Cancelled` with
`JobCancelled` instead of `Failed`/`Succeeded` with `JobFailed`/`JobSucceeded` — no JobStep is ever
scheduled past that point. This is the same lock/read/decide/persist transaction
`apply_action_evidence` already used before #27; no new lock order was introduced, so a
concurrent cancellation request and terminal evidence commitment for the same Attempt serialize
through the existing Attempt -> JobStep -> Job ordering rather than racing under a competing one.

## Implemented uncertain-execution reconciliation

`bamep_agent_protocol` adds `StatusQuery{action_id}` (Server -> Agent) and
`StatusReport{action_id, known_state}` (Agent -> Server, `Accepted | Running | Succeeded |
Failed | Cancelled | Unknown`) to `AgentProtocolMessage`, both carrying
`correlation_id == action_id` and never a replacement action identity. `StatusQuery` is never an
`ActionDispatch` retry.

`bamep_domain::reconciliation` adds three pure decisions, deliberately separate from
`action_evidence` (which never owns `AwaitingReconciliation`/`Indeterminate`) and from
`cancellation` (which owns only `CancelAck` evidence): `mark_awaiting_reconciliation`
(`Dispatched`/`InProgress -> AwaitingReconciliation`), `apply_status_report` (the closed
`StatusReport` vocabulary applied against an `AwaitingReconciliation` Attempt — one `Unknown`
never produces `Indeterminate`; `Cancelled` evidence completes cancellation only when the Job is
already `Cancelling`, mirroring `cancellation::apply_cancel_ack`'s identical authority guard so an
Agent-reported `Cancelled` can never itself initiate Job cancellation), and `close_indeterminate`
(the explicit reconciliation decision that closes an `AwaitingReconciliation` Attempt
`Indeterminate`, with `JobStep -> Failed{ReconciliationIndeterminate}` and one
`AttemptIndeterminate` domain event, composing with a Job already `Cancelling` exactly like every
other terminal reconciliation outcome).

`bamep_server::application::ReconciliationService` holds five structurally distinct
responsibilities on one shared instance, extending `JobRepository` with
`mark_endpoint_active_attempt_uncertain`, `reconcile_all_active_attempts_on_startup`,
`find_reconciliation_candidate`, `apply_status_report`, and `close_indeterminate`:

- `mark_endpoint_uncertain` — the connection-loss trigger. `AgentControlGateway` calls it once
  its authenticated-session task exits (normal disconnect or a Gateway error), after that task's
  own message loop, so it never blocks the outbound channel it depends on — and only when
  `OutboundSessionDirectory::dispatch_relevant_action` returns the exact `action_id` this session
  actually carried via `ActionDispatch`. That `action_id` is threaded through as
  `mark_endpoint_uncertain`'s own parameter and compared, inside the same `MarkUncertainDecision`
  closure the Adapter already invokes under its Attempt lock, against the freshly locked candidate
  Attempt's own `action_id` — a mismatch is a safe no-op. This closes a cross-Attempt race a purely
  Endpoint-scoped correlation left open: `OutboundSessionDirectory` records only one
  `(SessionId, ActionId)` pair per Endpoint (only for `ActionDispatch`, never `CancelAction`/
  `StatusQuery` transmission), so a session that only ever carried an earlier, now-terminal Attempt
  can otherwise still be read as "dispatch-relevant" for a later, unrelated Attempt already
  dispatched through a different (or the same) session by the time this trigger's own PostgreSQL
  call actually runs. Comparing the exact `action_id` — not just Endpoint identity — makes that
  window safe without a second Attempt, a persisted `SessionId`, or a new `JobRepository` method.
  Both registries unregister synchronously, with no `.await` in between, strictly before this (or
  any) reconciliation call — never leaving a stale outbound-ready window a concurrent
  final-dispatch attempt could observe.
- `reconcile_on_startup` — Server-restart recovery: locks and reconciles every currently
  `Dispatched`/`InProgress` Attempt across every Endpoint in one pass. No test/harness in this
  repository runs a persistent Server process, so this is exercised by calling it directly
  against durable state, standing in for an actual restart.
- `reconcile_on_session_start` — issues `StatusQuery` for any `AwaitingReconciliation` Attempt
  once a session (re-)establishes for its Endpoint. `AgentControlGateway` spawns this as a
  background task immediately after registering outbound delivery/presence, concurrently with
  (not before) its own message loop starting — the outbound send it performs awaits an ack only
  that loop's `outbound_rx` ever fulfills.
- `apply_status_report` — inbound `StatusReport` evidence, invoked only by `AgentControlGateway`.
  Locks Attempt -> JobStep -> Job by `action_id`, exactly like `apply_action_evidence`/
  `apply_cancel_ack`, and calls `bamep_domain::apply_status_report`.
- `close_indeterminate` — the explicit reconciliation-close control path (never callable from
  inbound Agent Protocol handling, mirroring `CancellationService::request`'s identical
  separation). Locates the target Job's current `AwaitingReconciliation` Attempt, locks it, and
  calls `bamep_domain::close_indeterminate`; the required operator-decision audit always commits
  atomically alongside the terminal transition.

Every terminal outcome from `apply_status_report`/`close_indeterminate` removes the Attempt's
transient reservation mapping exactly once and releases it through
`TechnicalResourceArbiter`, exactly like `ActionEvidenceService`/`CancellationService`; entering
`AwaitingReconciliation` itself never releases it, and the mapping's absence after a Server
restart (a fresh, empty in-memory registry) is a safe no-op release, never a correctness problem
for the durable Attempt lifecycle.

## Maintenance rule

Update this directory only for durable structure visible in implemented code. Do not copy
planned contracts, ADR rationale, empirical evidence, or GitHub execution history here.

If this document disagrees with code/tests, it is stale.
