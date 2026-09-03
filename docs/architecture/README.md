# Bamep Architecture

This directory describes architecture **implemented in the current repository**. Code and
tests are final evidence for current behavior; Specifications own normative behavior and
ADRs own decision rationale.

## Current workspace

Bamep currently has seven Rust crates:

| Crate | Implemented responsibility |
| --- | --- |
| `bamep-trusted-bootstrap` | Trusted-bootstrap primitives, assertion parsing/transcript, and verification |
| `bamep-agent-protocol` | Rust wire model/codec for the implemented Agent Protocol v1 slice |
| `bamep-domain` | Pure Endpoint identity, boot-context, trusted-bootstrap, runtime-credential, and Job/JobStep workflow business logic |
| `bamep-server` | Application services, Ports, PostgreSQL/transport Adapters, Agent session handling, and the `bamepd` Worker-supervision composition root |
| `bamep-simulator` | Simulated Agent participant using real trusted-bootstrap and WSS/Agent Protocol boundaries |
| `bamep-worker-protocol` | Rust wire model/codec/framing for the implemented Worker Protocol v1 handshake + business-message catalog |
| `bamep-worker` | The isolated Worker process: concurrent UDS control client, fail-closed authority tracking, Server TLS identity, local chunk storage + full-Artifact reconstruction, and the HTTPS `/api/data/v1/` data plane |

Planned components remain outside Architecture until corresponding code exists.

## Bamep Web (Presentation foundation)

`web/` is the Bamep operator-console Presentation client, an independently versioned
component outside the Rust workspace. It is implemented with Svelte 5 and SvelteKit under
strict TypeScript, styled with Tailwind CSS v4, and built by `@sveltejs/adapter-static`
into a fully static, client-rendered SPA (`ssr = false`, no prerendering). The build
output (`web/build/`) is a single `index.html` application-shell fallback plus
fingerprinted `_app/immutable/` assets and `static/` files — no server bundle or runtime
handler. This is the shape ADR-0017's future `bamepd` rule expects (HTML navigation misses
resolve to the application shell, which the client router then routes, including future
deep links such as `/endpoints/LAB-03`). `npm` with a committed `package-lock.json` is the
dependency boundary; Vitest is the test runner.

Implemented at this stage:

- the operator-console application shell — product identity, a persistent sidebar, and
  route-aware active navigation among `Endpoints`, `Operações`, `Atenção`, and
  `Configurações`, with a fluid content area that carries no global `max-width`;
- a small local localization boundary (`src/lib/i18n/`) rendering `pt-BR`, structured so a
  later `en-US` catalog needs no call-site changes;
- a local design-token layer (`src/lib/styles/app.css`) covering only what the shell uses,
  dark by default with light values under `prefers-color-scheme`;
- self-hosted fonts, so the build needs no runtime network access.

The component operates entirely on local foundation/placeholder state. It performs no HTTP
requests, defines no Administrative API client, and mirrors no Server/Domain types.
Administrative API v1 integration (contract owned by
`m0-administrative-api-web-read-contract.md`) and the `bamepd` static-serving integration
(ADR-0017) remain unimplemented; feature routes contain localized placeholders only, and
the #41–#44 product flows are not implemented.

ADR-0016 owns the Presentation stack, static-delivery model, and platform-boundary
rationale; this section records only what now exists in `web/`.

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
- `bamep-worker-protocol` is a transport-independent Rust representation of the Worker
  Protocol v1 handshake + business-message catalog defined by
  `m1-worker-data-plane-control-contract.md`; it has no Domain, Server, PostgreSQL, or
  HTTP-framework dependency, only `serde`/`serde_json`/`uuid`/`thiserror` and `tokio`'s
  `io-util` feature (the generic `AsyncRead`/`AsyncWrite` framing traits — no
  `net`/`rt`/`process`).
- `bamep-worker` (both the `bamep_worker` library and its `bamep-worker` binary) depends on
  `bamep-worker-protocol` and `bamep-trusted-bootstrap`, plus `axum`/`axum-server` (the same
  Axum 0.8 stack ADR-0017 accepts, reused so there is no second Rust HTTP framework) and
  `rustls`/`sha2`/`base64`/`rustix` for TLS serving, chunk hashing, and Unix-safe storage
  primitives. It has no `bamep-domain`, `bamep-server`, or SQLx/PostgreSQL dependency, owns no
  PostgreSQL repository Adapter, and holds no Domain/Application authority (ADR-0018).
- `bamep-server` depends on `bamep-worker-protocol` (for the `bamepd`-side UDS handshake) but
  not on `bamep-worker` itself in production code, preserving one-directional isolation; the
  `bamepd` binary spawns the compiled `bamep-worker` executable as a separate OS process
  rather than linking against its crate.

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
  is currently correlated to. That `action_id` is threaded through as `mark_endpoint_uncertain`'s
  own parameter and compared, inside the same `MarkUncertainDecision` closure the Adapter already
  invokes under its Attempt lock, against the freshly locked candidate Attempt's own `action_id` —
  a mismatch is a safe no-op. This closes a cross-Attempt race a purely Endpoint-scoped correlation
  left open: `OutboundSessionDirectory` records only one `(SessionId, ActionId)` pair per Endpoint,
  so a session that only ever carried an earlier, now-terminal Attempt can otherwise still be read
  as "dispatch-relevant" for a later, unrelated Attempt already dispatched through a different (or
  the same) session by the time this trigger's own PostgreSQL call actually runs. Comparing the
  exact `action_id` — not just Endpoint identity — makes that window safe without a second Attempt,
  a persisted `SessionId`, or a new `JobRepository` method. Both registries unregister
  synchronously, with no `.await` in between, strictly before this (or any) reconciliation call —
  never leaving a stale outbound-ready window a concurrent final-dispatch attempt could observe.

  The `(SessionId, ActionId)` correlation itself has two writers, both on
  `OutboundSessionDirectory`: `ActionDispatch` transmission unconditionally establishes it
  (`CancelAction`/`StatusQuery` transmission never does — neither proves the resolved session owns
  the action's execution); and `bind_dispatch_relevant_session` REBINDS it to whichever
  authenticated session supplied evidence the Application/Repository layer actually accepted as
  authoritative non-terminal knowledge (Issue #28 third corrective pass "Session-relevance transfer
  after authoritative non-terminal evidence") — `AgentControlGateway::handle_status_report` for an
  accepted `StatusReport{Accepted|Running}` (`AwaitingReconciliation -> InProgress`), and
  `handle_action_ack` for an accepted `ActionAck{Accepted}` (`Dispatched -> InProgress`), each
  gated on the real `ApplyReconciliationResult`/`ApplyActionEvidenceResult::Applied` with
  `terminal: false` the Repository already returned — never merely because untrusted wire input
  claimed `Running`/`Accepted`. This is consistent with the wider evidence-application contract,
  which already correlates by `action_id` + authenticated Endpoint alone, never exact `SessionId`
  identity (`JobRepository::apply_action_evidence`/`apply_status_report`). Without this transfer, a
  session that legitimately became the one currently relevant to an action — by successfully
  reporting/acking it after the original dispatching session disconnected or another session raced
  ahead of it — could later disconnect without its own loss ever being considered
  reconciliation-relevant, silently stranding the Attempt `InProgress` forever.

  `bind_dispatch_relevant_session` is compare-and-swap-like, not a blind overwrite (Issue #28
  fourth corrective pass "Late stale rebind ordering"): it returns `BindOutcome::Bound` when no
  correlation currently exists for the Endpoint or the current one already names this exact
  `action_id`, and `BindOutcome::StaleActionIgnored` (no mutation) when the current correlation
  names a DIFFERENT `action_id`. This closes the gap the unconditional version left: the Gateway
  task calling it resumes from its own evidence-application `.await` with no guarantee the
  Endpoint's correlation hasn't since moved on — a genuinely later `ActionDispatch` for the next
  ordered JobStep's Attempt can commit and transmit through a different session while this
  continuation was still in flight. Since a later action always obtains its own correlation through
  its own `ActionDispatch`, once the correlation has genuinely advanced past a given `action_id`,
  any surviving continuation for it is stale and must never move the correlation backward —
  mirroring the durable no-regression rules used elsewhere (newer authoritative lifecycle identity
  is never overwritten by delayed evidence for an earlier one).
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
  `apply_cancel_ack`, and calls `bamep_domain::apply_status_report`. `Unknown` evidence always
  decides `NoOp` (`bamep_domain::reconciliation` module docs) and therefore never reaches the
  `bind_dispatch_relevant_session` rebind above — it never proves current execution knowledge.
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

## Implemented pre-dispatch Transfer/Artifact/ChunkManifest durable model

`bamep-domain` adds three pure modules for the M1 data-plane
(`m0-data-plane-and-storage-contracts.md`; Issue #36): `transfer` (`TransferId`,
`TransferDirection` — currently only `AgentToServer` —, `SourceProvenance`, `Transfer`,
`create_transfer_context`, `bind_attempt`), `artifact` (`ArtifactId`, `ArtifactState`,
`CaptureConsistency`, `Artifact`, and the `begin_verification`/`complete_verification`/
`fail_incomplete`/`set_capture_consistency` transitions), and `chunk_manifest`
(`DigestAlgorithm` — currently only `Sha256` —, `Digest`, `ChunkSize`, `ChunkIndex`,
`ChunkManifest`, `record_expected_chunk`, `seal`, `validate_verified_chunk`). `Transfer` carries
no state machine of its own — only an optional `attempt_id`, `None` until a later dispatch
boundary binds it — and `ArtifactState`/`CaptureConsistency` transition independently of each
other on the same `Artifact`.

`bamep_server::ports::TransferRepository` is a new Port mirroring `JobRepository`'s lock/decide/
persist discipline: every mutating method locks the named `Transfer`'s row before invoking a
caller-supplied `decide` closure over `TransferLockedFacts` (the current `Transfer`, `Artifact`,
`ChunkManifest`, and durably held chunk indices), so Domain remains the sole owner of transition
legality. `bamep_server::application::TransferService` is the thin Application layer calling
exactly one `bamep_domain` decision per method and handing it to the Port; it performs no
hashing, file, storage, or network I/O.

`bamep_server::adapters::postgres::PostgresTransferRepository` implements this Port against four
new relational tables folded into `0001_initial_schema.sql` (pre-baseline phase):
`artifacts` (state, capture consistency), `transfers` (Endpoint/Job/JobStep correlation,
direction, digest algorithm, chunk size, source provenance, nullable/unique `attempt_id`),
`chunk_manifests` (sealed/chunk_count/artifact_digest, all-or-none), and `chunk_identities`
(per-chunk expected size/digest plus a `held` column distinguishing "expected identity recorded"
from "matching bytes durably accepted"). No bulk Artifact bytes are stored in PostgreSQL. The
module's `load_locked_facts`/`persist_attempt_binding` functions are `pub(crate)` primitives a
later Work Package (#40) can compose directly into its own transaction to commit the Transfer ->
Attempt binding atomically alongside its JobStep/Attempt commitment, without requiring #36 to
decide that future transaction's shape now.

This durable model supports creating a pre-dispatch `Transfer`/`Artifact`/empty `ChunkManifest`
for an existing Endpoint/Job/JobStep correlation with no Attempt — a `Transfer` with
`attempt_id: None` is never treated as transfer-authorized. No Attempt/action identity is created,
no JobStep is transitioned, and the destructive-operation gate is never evaluated by this path.
Binding an existing Transfer to an owning Attempt exactly once, and rejecting a conflicting
rebind, is implemented and tested; #40 (below) is the first consumer that actually commits that
owning Attempt. Agent Protocol transfer authorization (#38) is implemented — see
"Sender-constrained transfer authorization" below. The authenticated HTTPS chunk transport,
storage, and Artifact verification through the Worker (#39) are implemented — see "Implemented
Worker data-plane chunk transport" below. The Agent-side C1 transfer participant (#19), the
Server-side C2 terminal-`ActionResult` consumption (#19), the C3 end-to-end RF-005
happy-path vertical, and the C4 interruption/resume + fail-closed matrix (interruption and
legitimate resume, real WSS disconnect/reconnect, corrupted-chunk rejection, source-mutation
reproducibility failure, invalid/replayed/wrong-binding authorization, Worker runtime
restart, `bamepd`-restart transient-authorization invalidation + startup reconciliation,
`Verified`/`Failed`-but-`ActionResult`-lost reconciliation, `capture_consistency`
independence, source-vs-target identity independence, transfer cancellation, a
still-`Incomplete` Artifact reconciled by `StatusReport{Failed}`, and a lock-order
concurrency regression) are implemented — see "Implemented transfer terminal-result
consumption", "Implemented transfer cancellation / reconciliation Artifact-failure
composition", and "Implemented integrated RF-005 transfer matrix" below. #19's integrated
functional matrix is implemented; physical firmware/PXE/Secure-Boot/WinPE behaviour,
20–24-Endpoint scale, physical-disk capture, and the production backup format are not in its
scope. The isolated
Worker process/control boundary itself (#37) is implemented — see "Implemented isolated Worker
runtime and control boundary" below.

## Implemented non-destructive M1 transfer dispatch-commit path

`bamep-domain` adds a fourth pure module, `transfer_dispatch`
(`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005; Issue #40), structurally
separate from `final_dispatch` (the destructive gate, Issue #25): its `TransferDispatchInputs`
carries no Endpoint/credential/presence/inventory/target-fingerprint/hardware-confidence/
trusted-bootstrap evidence at all, so `evaluate_transfer_dispatch` cannot reach the seven-item
destructive-operation gate even by accident. It checks only generic workflow/scheduler
authorization (Job `Running`, current ordered step, no unresolved prior Attempt), rejects a
JobStep that already carries a `DestructiveIntent` (Issue #31) as structurally out of scope for
this action, verifies the presented `Transfer` correlates to the exact Job/JobStep/Endpoint under
evaluation, and composes `bamep_domain::transfer::bind_attempt` to bind the fresh `Attempt` to
that `Transfer` in the same decision — never regenerating `TransferId`/`ArtifactId`.

`bamep_server::ports::JobRepository` gained one sibling method to `commit_destructive_dispatch`,
`commit_transfer_dispatch`, taking a `TransferDispatchLockedFacts` (Job/JobStep/existing-Attempt
facts plus the locked `Transfer` — no `EndpointAggregate` field exists on this type, so the
destructive gate's evidence is structurally unreachable from the Adapter side too).
`PostgresJobRepository::commit_transfer_dispatch` locks `jobs` -> `job_steps` -> `attempts`
(existence check) -> `transfers`, extending its own existing lock order by one leaf; it reuses
`adapters::postgres::transfer_repository`'s `pub(crate)` `load_locked_facts`/
`persist_attempt_binding` primitives directly (never through `TransferRepository::bind_attempt`,
which owns its own separate transaction) so the `JobStep -> Dispatching` transition, the new
`attempts` row, and the `transfers.attempt_id` binding commit in one PostgreSQL transaction. No
destructive-dispatch audit record or new `DomainEvent` is created for this non-destructive
commitment. `bamep_server::application::TransferDispatchService` is the non-destructive sibling of
`FinalDispatchService`: it composes the same `TechnicalResourceArbiter` reservation
acquire/release discipline around the pure Domain gate and never depends on an `AgentDispatchPort`
at all, so sending `ActionDispatch` is structurally unreachable from it.

`bamep_server::application::ActionDispatchService` gained `dispatch_transfer`, sharing its
existing guard/registration/exactly-once-send logic with `dispatch` through one private helper.
It sends `bamep.m1.data-plane-transfer` v1 with `parameters` reconstructed only from the durably
bound `Transfer` (`transfer_id`, `artifact_id`, `direction`, `digest_algorithm`, `chunk_size`) —
never a caller-supplied replacement. The pre-existing `dispatch`/`bamep.m1.simulated-execution`
path is unchanged.

## Implemented transfer terminal-result consumption

Issue #19 checkpoint C2 wires the Server side of the M1 transfer terminal `ActionResult`
(`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005;
`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle" — `Incomplete -> Failed`
ownership and ordering).

`AgentControlGateway::handle_action_result` now classifies the owning action from **durable
Server facts**, never from `ActionResult.detail`: it asks the new
`bamep_server::application::TransferTerminalEvidenceService::classify` whether a `Transfer`
(Issue #40) is bound to the action's Attempt (the durable discriminator —
`attempts` stores no `action_type`). A `bamep.m1.data-plane-transfer` result is validated
against the closed RF-005 detail vocabulary (`parse_transfer_result_detail`) and consumed
through `TransferTerminalEvidenceService::apply`; the RF-004 `bamep.m1.simulated-execution`
path stays exactly on `ActionEvidenceService` with the unchanged `m1_result_detail_matches`
check. A malformed transfer detail is answered with the existing generic `ProtocolError`.

`TransferTerminalEvidenceService::apply` composes the pure Domain decision
(`decide_transfer_terminal_evidence`, which reuses `bamep_domain::apply_action_evidence` for
the generic Attempt/JobStep/Job math and `bamep_domain::fail_incomplete` for CASE C) with the
new `JobRepository::apply_transfer_terminal_evidence` atomic boundary:

- CASE A (`TRANSFER_VERIFIED`) commits Attempt/JobStep/Job success **only** when the durably
  bound Artifact is independently confirmed `Verified` under lock; any other state fails
  closed with no mutation.
- CASE B (`ARTIFACT_VERIFICATION_FAILED`) requires the durably bound Artifact to already be
  `Failed` (the #39 seal/verification path committed it) and performs the normal
  `ResultFailed` workflow transition with no further Artifact transition.
- CASE C (`CHUNK_VERIFICATION_FAILED` / `TRANSFER_ABANDONED`) drives
  `Artifact Incomplete -> Failed` **in the same PostgreSQL transaction** as the terminal
  Attempt/JobStep/Job transition and its already-required events/audit. A failed transaction
  leaves neither side durably committed; an idempotent resend recovers it; a matching
  duplicate is a `NoOp`; conflicting late evidence never overwrites the first committed
  terminal outcome; a terminal Artifact is never rewritten (`Verified` never to `Failed`,
  `Failed` never to `Verified`). Issue #27's "while `Cancelling` → Job `Cancelled`"
  composition is inherited unchanged through `apply_action_evidence`.

`PostgresJobRepository::apply_transfer_terminal_evidence` locks
`transfers -> artifacts -> attempts -> job_steps -> jobs` — the `transfers -> artifacts`
prefix matches `PostgresTransferRepository`'s chunk/seal/verification family (no deadlock with
a concurrent `commit_chunk_acceptance` etc.), and the `attempts -> job_steps -> jobs` suffix
matches `apply_action_evidence`; the opposite `transfers`/`jobs` order versus
`commit_transfer_dispatch` is not a reachable cycle because a JobStep is never dispatched
while its Attempt is being evidenced (linear workflow). `bamep_server::adapters::postgres::transfer_repository`
gained the `pub(crate)` `load_locked_transfer_and_artifact` and
`persist_incomplete_artifact_failed` primitives it composes.

No wire contract changes (Agent Protocol catalog, `ActionResult` shape, closed RF-005 detail
codes, Worker Protocol v1, the 137-byte proof transcript, and the HTTPS `/api/data/v1/`
surface are all unchanged), no new Domain event, and the C1 Simulator participant is
unchanged.

## Implemented transfer cancellation / reconciliation Artifact-failure composition

Issue #19 checkpoint C4 closes the one gap C2/C3 recorded: the
`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle" clause names an authoritative
`Cancelled` Attempt outcome for a still-`Incomplete` Artifact as a `bamepd`-driven
`Incomplete -> Failed` trigger, atomic with the unchanged Issue #27 cancellation terminal
transition. `TransferTerminalEvidenceService::apply_cancel_ack` implements it: it reuses
`bamep_domain::apply_cancel_ack` (Issue #27, byte-for-byte unchanged) for the workflow
decision, and — only when that decision drives the owning Attempt to a terminal `Cancelled`
while the bound Artifact is `Incomplete` — adds `bamep_domain::fail_incomplete`'s
`Incomplete -> Failed` into the **same** transaction as the #27 terminal
Attempt/JobStep/Job transition, reusing `JobRepository::apply_transfer_terminal_evidence`'s
existing lock/decide/persist boundary (no new repository method). A terminal Artifact
(`Verified`/`Failed`) is never rewritten; a `PendingVerification` Artifact keeps its own
seal-path outcome; a non-terminal (`AwaitingReconciliation`) CancelAck outcome never touches
the Artifact. `AgentControlGateway::handle_cancel_ack` routes a `CancelAck` to this path only
when the action has a bound `Transfer` (classified from durable facts, never from the wire);
every non-transfer `CancelAck` stays exactly on `CancellationService` (Issue #27,
unchanged). No new event, no new state, no new wire message.

The same transfer-aware transaction also consumes an authoritative Issue #28
`StatusReport{Cancelled}` **or `StatusReport{Failed}`** after session-loss reconciliation —
`TransferTerminalEvidenceService::apply_status_report_cancelled` /
`apply_status_report_failed`. Both reuse `bamep_domain::apply_status_report` for the unchanged
#28 workflow decision and atomically add the Specification-owned `Artifact Incomplete ->
Failed` transition when that decision terminates the owning transfer Attempt (as `Cancelled`
or `Failed`) while its bound Artifact is still `Incomplete` — the reconciliation equivalent of
the `ActionResult{Failed}` `CHUNK_VERIFICATION_FAILED` / `TRANSFER_ABANDONED` path, per
`m0-data-plane-and-storage-contracts.md` "`Incomplete -> Failed` ownership and ordering"
(`StatusReport` carries no failure code; a still-`Incomplete` Artifact makes `Incomplete ->
Failed` the only safe transition). Durable action-to-Transfer classification controls this
routing; non-transfer Attempts and every other `StatusReport` state (`Running`, `Succeeded`,
`Unknown`) remain on the generic Issue #28 path. Terminal Artifacts (`Verified`/`Failed`) are
never rewritten and `PendingVerification` keeps its seal/verification-path outcome; the
workflow still transitions per #28 in those cases.

## Implemented integrated RF-005 transfer matrix

Issue #19 checkpoints C3 and C4 compose the already-implemented C1 and C2 pieces into an
integrated matrix — one deterministic successful capture plus the interruption/resume +
fail-closed adversarial set — with every boundary real. They add the cancellation
composition above and otherwise no Server business logic and no Simulator code: a shared
harness (`crates/server/tests/support/transfer_vertical.rs`) plus the happy-path vertical
(`data_plane_transfer_vertical.rs`) and the matrix
(`data_plane_transfer_failure_matrix.rs`), which drive the committed
`bamep_simulator::DataPlaneTransferAgent` (and its committed deterministic
`TransferRunOptions`/`InMemoryTransferSource` hooks) from a real Agent Protocol v1 WSS
session, exactly as `action_dispatch_wss.rs` drives `SimulatedActionAgent`.

The one run crosses: a durable non-destructive `commit_transfer_dispatch`
(`Attempt{Dispatched}`, the seven-item destructive gate never evaluated — the JobStep
carries no `authorized_inventory_revision_id`/`authorized_target_fingerprint`); a real
loopback TCP -> pinned TLS 1.3 -> WebSocket delivery of the RF-005 `ActionDispatch` through
the same `OutboundSessionDirectory`/`ActionDispatchService.dispatch_transfer` path #26/#40
use; C1 `accept` -> `ActionAck{Accepted}`; a real WSS `TransferAuthorizationRequest`
carrying a freshly generated ephemeral Ed25519 proof public key ->
`TransferAuthorizationService::issue` (the same instance the Worker UDS side consumes, its
`data_plane_base_url` resolved to the real in-process Worker HTTPS origin) ->
`TransferAuthorizationGrant{token, data_plane_base_url}`; C1 `run` performing real hyper-1
HTTPS GET-resume / PUT-chunks / POST-seal against the real
`bamep_worker::data_plane::DataPlane`, its real IPC client + D1 staging + D2 reconstruction,
the real `WorkerControlPlane` over AF_UNIX (`bamep-worker-protocol` v1), into real
PostgreSQL-backed chunk acceptance / manifest seal / independent SHA-256 verification ->
durable Artifact `Verified`; `ActionProgress{bytes_processed}` streamed over the same WSS
session as C1 produces it, consumed by the real gateway `handle_action_progress`; then
`ActionResult{Succeeded, TRANSFER_VERIFIED}` over the same session, consumed by C2's
`TransferTerminalEvidenceService` through the real gateway -> `Attempt`/`JobStep`/`Job`
`Succeeded`.

Ordering is proven directly: at the instant C1's `run` returns, the harness observes the
Artifact already `Verified` while the workflow is still `Running` and no `ActionResult` has
crossed the wire — which, with C2's CASE A durable-`Verified`-under-lock gate, establishes
that workflow success cannot precede `Verified`. The Worker never touches a
workflow table; the sole terminal workflow transition rides the Agent's WSS `ActionResult`
through C2 (one terminal audit record). The Simulator pins exactly one
`ServerCertFingerprint` — the value it verified for WSS is the value it reuses for Worker
HTTPS (both listeners present the same leaf).

The matrix reuses the same harness for every adversarial case: C1's interrupt/corruption
hooks and `InMemoryTransferSource::mutate_chunk` drive the inputs; resume always obtains a
fresh WSS `TransferAuthorizationGrant` for the same durable `transfer_id`/`artifact_id`/
`action_id` and never redispatches; a real WSS `drop_ungracefully` exercises #28's
connection-loss path (`AwaitingReconciliation`) and a `StatusReport{Running}` returns the
Attempt to `InProgress` so the data plane can resume; `Verified`/`Failed`-but-lost
`ActionResult` resolves through #28's `StatusReport` path without re-running the transfer or
rewriting the Artifact; `restart_worker` tears the HTTPS listener + IPC client + control
plane down and rebuilds them against restart-stable staging and unchanged durable state;
`restart_bamepd_transient_authority` + `reconcile_on_startup` proves an in-flight Attempt
becomes `AwaitingReconciliation` and pre-restart capabilities fail closed with the single
non-enumerable `401`; a proof against the wrong chunk/operation binding and a replayed
`proof_id` reach the real `CapabilityStore`/`ReplayCache`/Worker `AuthorizationQuery` and
produce that same `401`. Every terminal-failure case asserts the Artifact is `Failed` (never
`Verified`, `Incomplete`, or `PendingVerification`) for `CHUNK_VERIFICATION_FAILED`/
`TRANSFER_ABANDONED`, and a durable per-chunk recorded identity is never rewritten. A
dedicated concurrency regression drives `commit_transfer_dispatch` and
`apply_transfer_terminal_evidence` back to back many times under real PostgreSQL and
observes no deadlock (SQLSTATE `40P01`).

The Worker runs as an in-process `DataPlane` + IPC-client runtime rather than a spawned
`bamep-worker` process, matching `worker_data_plane_transfer_interop.rs` (Issue #39 Phase
E2B); process/runtime isolation is proven separately by `worker_process_supervision.rs` and
`worker_runtime_ownership.rs`. The vertical does not exercise `bamepd` as a complete production
composition root; no current implementation composes all Server responsibilities there.
`bamepd`/Server restart is represented by #28's `reconcile_on_startup` plus transient-authority
replacement, not an OS process restart.

## Implemented isolated Worker runtime and control boundary

Issue #37 materializes the process/runtime boundary ADR-0001/ADR-0003/ADR-0018 require, and
the M1 Worker IPC handshake slice `m1-worker-data-plane-control-contract.md` defines. Issue #38
adds sender-constrained transfer authorization on top of it — capability/proof cryptography,
freshness, replay protection, and the `AuthorizationQuery`/`AuthorizationDecision` exchange over
the same UDS boundary (see "Sender-constrained transfer authorization" below). Issue #39 then
adds the rest of the Worker Protocol v1 catalog, the Worker-local chunk storage and
full-Artifact reconstruction mechanisms, and the Worker-owned HTTPS `/api/data/v1/` data plane
that composes them — see "Implemented Worker data-plane chunk transport" below.

**Process topology.** `bamepd` (a new `[[bin]]` target on the `bamep-server` package,
`crates/server/src/bin/bamepd.rs`) is the minimal Server daemon composition root: it binds the
Worker UDS listener, supervises the Worker OS process, forwards Worker configuration/TLS
identity paths through the child process environment, and — since Issue #38, extended by #39 —
connects to PostgreSQL and constructs the durable Application services the Worker control plane
needs (`TransferAuthorizationService`, plus the `ChunkAcceptanceService`/`ManifestSealService`/
`ArtifactVerificationService` for the rest of the v1 catalog). The Worker process — not
`bamepd` — serves the external HTTPS `/api/data/v1/` data plane. `bamepd` still owns nothing
else — no Administrative API, Agent WSS listener, Web, or scheduler workflows — so `bamepd`
remains only a *partial* composition root; those existing/future responsibilities remain wired
through their own Application/Adapter boundaries until their own composition-root work requires
integration. `bamep-worker` (`crates/worker`, package `bamep-worker`) is a genuinely separate
Rust crate/binary: `bamepd` spawns it as a real child OS process (`tokio::process`), never an
in-process task.

**Worker IPC v1.** `bamep-worker-protocol` implements the explicit u32be-length-prefixed
JSON framing, the common envelope (`protocol_version`/`message_id`/`type`, `in_reply_to` on
responses), and the handshake/error message slice
(`WorkerHello`/`ServerHello`/`HandshakeRejected`/`ProtocolError`) from
`m1-worker-data-plane-control-contract.md`. Since #39 the full v1 business catalog is also
represented — the `AuthorizationQuery`/`ChunkAcceptanceRequest`/`ResumeDiscoveryQuery`(+
`ResumeDiscoveryContinue`)/`ManifestSealRequest`/`ArtifactVerificationReport` request messages
and their paired decisions/pages/acks, with shape validators — with `protocol_version` and the
137-byte proof transcript unchanged. `crate::framing` provides the length-prefix codec generic
over `tokio::io::AsyncRead`/`AsyncWrite`, reused by both the `bamepd`-side listener and the
Worker client so the framing logic exists in exactly one place. `crate::codec::decode`
distinguishes an unrecognized top-level `type` (`DecodeError::UnknownType`) from every other
malformed-JSON case, so a receiver can answer it with a stable `ProtocolError` rather than
silently closing the connection. `is_uuid_v4` requires both the version-4 nibble and the
RFC4122/RFC9562 standard variant.

**Worker control-boundary lifetime ownership.** Before touching the Worker UDS socket
pathname at all, `bamepd` validates/creates the trusted runtime directory
(`bamep_server::adapters::worker_runtime_ownership::TrustedRuntimeDir`: a real, non-symlink,
owner-only-mode directory owned by the effective UID running `bamepd`, whose configured path
must be absolute and whose *complete* ancestor chain up to the filesystem root — not just the
immediate parent — cannot be replaced by an untrusted principal: every ancestor must be a real,
non-symlink directory that is root-owned or effective-UID-owned, and not group/other-writable
unless it also carries the sticky bit) and acquires an exclusive, non-blocking advisory
lock (`flock(LOCK_EX | LOCK_NB)` via `rustix`) on a dedicated lock file inside it
(`RuntimeOwnershipLock`), held for the entire daemon lifetime. A second `bamepd` targeting the
same runtime directory fails at lock acquisition — before it ever inspects, probes, or attempts
to bind the socket pathname. This lock is the primary exclusivity guarantee; the
directory/socket filesystem checks below remain defense in depth. Shutdown ordering is
explicit: stop handlers, stop Worker, clean up the Worker socket, release the ownership lock
last.

**`bamepd`-side UDS control plane.** `bamep_server::adapters::worker_control_plane::WorkerControlPlane`
binds/listens on a configurable UDS path (`BAMEP_WORKER_UDS_PATH`) only after the ownership
lock above is held, restricting the bound socket to owner-only access (`0600`). A pre-existing
path is removed only after confirming it is actually a socket
(`std::os::unix::fs::FileTypeExt::is_socket`) — an unrelated file at that path is refused,
never deleted. It accepts each connection, performs the handshake, and hands the resulting
`worker_instance_id` to `bamep_server::runtime::worker_authority::WorkerAuthorityRegistry` — an
in-process (never PostgreSQL-durable) Runtime Service, mirroring `presence`/`outbound_sessions`,
that tracks the single current connection generation. A newer successful handshake always
supersedes the previous one; a superseded generation's later disconnect is a no-op against the
now-current generation. This registry is the narrow readiness/control-connection seam #39
consumes rather than inventing another notion of Worker authority. The accept
loop drains completed connection-handler tasks during normal operation, not only at shutdown.
Since #38 the per-connection handler is a genuine loop, not a single-shot receive; since #39 it
serves the full Worker Protocol v1 catalog sequentially in place — authorization, durable chunk
acceptance, resume discovery pagination, the atomic manifest seal, and the independent Artifact
verification commit — each request answered before the next frame is read, with generation-
scoped transient handles linearised against connection supersession. On `bamepd` shutdown, the
control plane stops accepting new connections and removes the socket file.

**Worker-side reconnecting client.** `bamep_worker::ipc::control::worker_control` returns a
cloneable `WorkerControlHandle` plus a `ControlDriver` that owns the whole connect →
handshake → read/write/correlate → reconnect lifecycle in one task, sleeping a configurable
bounded delay before reconnecting — never busy-spinning. Since #39 the driver runs the full
Worker Protocol v1 control client: many concurrent outstanding requests correlated by
`message_id`/`in_reply_to`, generation-scoped follow-up tickets (`acceptance_handle`/
`verification_handle`/resume cursors) rejected locally after a reconnect, a bounded per-request
timeout, and every operation reporting only what `bamepd` decided — never a fabricated decision,
a local `Verified`/`Failed` verdict, or a replayed proof/handle across a reconnect. The
superseded `ipc::client`/`ipc::authorization_client` single-outstanding-query modules are gone.
`bamep_worker::ipc::authority::AuthorityTracker` exposes the fail-closed
`AuthorityPhase` (`Disconnected`/`Connecting`/`Handshaking`/`Ready`) plus a per-process
monotonic connection-generation counter over a `tokio::sync::watch` channel:
`AuthoritySnapshot::is_available()` is `true` only for a current-generation successful
handshake, and becomes `false` immediately on disconnect. `worker_instance_id` is a UUID v4
generated once per Worker process start (`bamep-worker`'s `main.rs`) and stays stable across
that process's reconnects; a new Worker process always gets a new one. Unix Domain Sockets are
Unix-only: both the `bamepd`-side listener and the Worker client keep their real
`tokio::net::Unix*`-based implementation behind `#[cfg(unix)]`, with a narrow non-Unix stub
that never becomes available (no fake TCP/localhost substitute) — Linux/WSL2 remains the
reference/production environment and the only environment that exercises the real code path.

**Worker process supervision.** `bamep_server::runtime::worker_supervisor::WorkerSupervisor`
spawns the configured Worker executable via `tokio::process::Command` (`kill_on_drop(true)` as
a defense-in-depth backstop), observes its exit through `Child::wait()`, and respawns it after
a fixed configurable delay (minimum `100`ms) — distinguishing a spawn/configuration failure
(`SupervisorEvent::SpawnFailed`) from an observed child exit
(`SupervisorEvent::WorkerExited`) without treating either as `bamepd` itself crashing.
Diagnostic events are sent over a bounded channel via `try_send`, dropping on backpressure
rather than letting a stalled log consumer block Worker supervision. On controlled shutdown it
kills and reaps the current child before returning. Startup ordering is ownership lock, then
UDS listener bind, then supervisor/Worker start, so Worker never races a listener that failed
to bind.

**TLS identity provisioning.** ADR-0018 requires Worker to reuse the exact same Server TLS
identity without private-key bytes crossing the ordinary Worker IPC protocol. `bamep-worker`
loads the Server certificate chain from a host-local PEM file
(`BAMEP_WORKER_TLS_CERT_PATH`, parsed with `rustls-pemfile`) and the private key from
`BAMEP_WORKER_TLS_KEY_PATH` by opening it once with `O_NOFOLLOW` (via `rustix`), validating
regular-file/owner-only-mode from that same file descriptor's `fstat`, and reading its bytes
from that same descriptor — never a separate `symlink_metadata`-then-`read` path resolution —
paths are forwarded through process configuration, never key material through UDS JSON — and
proves the pair is rustls-usable (TLS 1.3, `ring` provider, no client-certificate
authentication, mirroring `adapters::agent_transport::AgentTransportAcceptor`'s existing
configuration shape) before considering itself ready. `bamep_worker::tls::identity::ServerTlsIdentity`
implements a manual (never derived) `Debug` that redacts the private key. Since #39 that same
`rustls::ServerConfig` is what the Worker's HTTPS data-plane listener actually serves (with
`http/1.1` as the sole ALPN protocol) — see "Implemented Worker data-plane chunk transport".

**Sender-constrained transfer authorization (Issue #38).**
`bamep_domain::transfer_authorization` implements the M1 proof-key/capability/transcript
primitives materialized by Issue #35: `ProofPublicKey`/`ProofId`/`ProofSignature` (raw
byte types with strict canonical base64url-no-pad wire encoding, mirroring
`bamep_trusted_bootstrap::BootNonce`'s discipline), `CapabilityToken` (CSPRNG-generated,
opaque, `Debug`-redacted) and its derived `CapabilityId = SHA-256(UTF-8 token bytes)`,
`build_proof_transcript` (the exact 137-byte domain-separated transcript), and
`verify_proof_signature` (strict Ed25519, no prehash/context, via `ed25519-dalek`, mirroring
the site-key verification discipline). `capability_is_current`/`capability_matches_request`
are pure predicates over a `CapabilityBinding`; `proof_is_fresh` enforces a bounded freshness
window (120s past / 30s future skew, both implementation-time constants). Domain performs no
I/O, caching, or clock access of its own.

`bamep_server::runtime::capability_store::CapabilityStore` and `::replay_cache::ReplayCache`
are the process-local Runtime Services holding transient authorization state — never
PostgreSQL-durable, mirroring `presence`/`reservation_registry`. `CapabilityStore` generates a
fresh `ProcessAuthorizationEpoch` once per construction (i.e. once per `bamepd` process
lifetime) and stamps it into every capability it issues; `bamep_domain::capability_is_current`
rejects any capability whose stored epoch does not match the store's current one. Because the
store itself is also never reconstructed across a restart, a fresh `bamepd` process both starts
with an empty capability store *and* mints a new epoch — two independent reasons a pre-restart
capability can never become valid again (`m0-data-plane-and-storage-contracts.md` "Server
restart").

Both Runtime Services are **bounded by explicit finite capacity** as well as by time.
`ReplayCache::check_and_insert` performs its lookup-and-insert as one `HashMap::entry` decision
under a single lock acquisition (never a separate `contains`-then-`insert`); it keys each entry
by the exact instant past which its proof can no longer satisfy `bamep_domain::proof_is_fresh`
(`issued_at + PROOF_FRESHNESS_PAST_WINDOW`, from `proof_replay_valid_until_millis`) and evicts
only genuinely-expired entries — so a proof accepted at the maximum accepted future skew stays
replay-protected for exactly as long as it could still be freshness-valid, and moving the clock
backwards only retains entries longer. Beyond that, it carries `DEFAULT_REPLAY_CACHE_CAPACITY`
(`2^16`) live entries maximum; at saturation a genuinely new `proof_id` is refused
(fail-closed) rather than evicting a still-live entry. `CapabilityStore` likewise carries
`DEFAULT_CAPABILITY_STORE_CAPACITY` (`4096`); `issue` evicts expired capabilities first and then
fails closed if a genuinely new capability would exceed capacity — a live capability is never
evicted to make room. Both saturations collapse to the single generic denial.

`bamep_server::application::TransferAuthorizationService` is the single Application-layer owner
of both directions of this boundary, backed by the `TransferAuthorizationRepository` Port
(`crates/server/src/adapters/postgres/authorization_repository.rs`): `load_authorization_state`
reads the `transfers`/`artifacts` row pair, the owning Artifact's `chunk_manifests` row and its
`chunk_identities` (expected-identity + `held` state), the owning `attempts` row (when bound),
and the `endpoints`/credential row `FOR UPDATE` inside one transaction, then rolls back — a
consistent, read-only locking snapshot (`AuthorizationDurableState`) reused identically by both
call sites below.

- `issue` serves the Agent WSS `TransferAuthorizationRequest`
  (`bamep_server::adapters::agent_gateway::AgentControlGateway::handle_transfer_authorization_request`):
  the authenticated session's `endpoint_id` is authoritative (never the request body). A request
  with no `correlation_id`, or with a syntactically present `correlation_id` that is known-wrong
  once the Transfer/Attempt ownership checks below have already run, is a protocol violation
  answered with generic `ProtocolError` correlated to the request's own `message_id` (never a
  wire-invalid `TransferAuthorizationDenied` carrying anything other than the owning `action_id`,
  and never revealing that authoritative value either). Only a request presenting the exact
  owning `action_id` reaches the semantic decision below, whose denial echoes that same value.
  The owning Attempt must be
  exactly `InProgress` — the durable phase fact that `ActionAck{outcome: Accepted}` has been
  processed (`m0-agent-protocol-contract.md` "Transfer authorization"); a still-`Dispatched`
  Attempt is too early, and a pre-dispatch unbound Transfer, an `AwaitingReconciliation`
  Attempt, or any terminal Attempt state is denied. The Endpoint credential must be
  `CredentialActive`. Renewal is the same call again with a fresh proof key — it creates
  nothing, so it composes for free.
- `decide` serves the Worker UDS `AuthorizationQuery`
  (`bamep_server::adapters::worker_control_plane`), in this order: look up the capability by
  `CapabilityId::from_token_bytes`; check `capability_is_current`/`capability_matches_request`;
  parse `proof_id`/`signature`; check freshness; verify the independently reconstructed canonical
  transcript's signature against the capability's bound `ProofPublicKey`; **re-read current
  durable state** (not the issuance-time snapshot) and re-check Transfer/Attempt(`InProgress`
  only)/credential validity plus current data-plane operation eligibility
  (`bamep_domain::data_plane_operation_is_current` over the Artifact state + manifest sealed
  flag + whether the `chunk_index` already has a durable expected identity); and **only then**,
  when the request would otherwise be approved, perform the atomic replay check-and-insert. A
  request rejected by any earlier check never consumes its `proof_id`. An approved `chunk_upload`
  whose `chunk_index` is already durably recorded carries that recorded expected digest as
  `AuthorizationDecision.expected_chunk_digest` (canonical base64url-no-pad); #38 only carries
  it — the Worker's comparison against the Agent-declared digest and the resulting HTTP `409` is
  #39.

Both directions collapse every internal *semantic* denial cause into one generic outcome
(`TransferAuthorizationOutcome::Denied` / `WorkerAuthorizationOutcome::Denied`) before it
reaches the wire, satisfying the non-enumerable-denial requirement identically on both
boundaries. `issue` additionally distinguishes a known-wrong action-scoped `correlation_id`
as `TransferAuthorizationOutcome::ProtocolViolation`, mapped to generic `ProtocolError` rather
than `TransferAuthorizationDenied` (see above) — a separate case from semantic denial, not an
additional externally enumerable reason within it. `bamepd`'s composition root
(`crates/server/src/bin/bamepd.rs`) now connects to PostgreSQL and constructs one
`TransferAuthorizationService` shared by both boundaries — the
Worker control plane cannot answer `AuthorizationQuery` from current durable state without it,
so `bamepd` is no longer PostgreSQL-free as the #37 architecture note originally described.
`BAMEPD_DATABASE_URL` and `BAMEP_DATA_PLANE_BASE_URL` are new required `bamepd` configuration;
`BAMEP_DATA_PLANE_BASE_URL` is parsed with a real URI parser (`url::Url`) and accepted only as
`https://host[:port]` with no userinfo, path (not even a bare `/`), query, or fragment.

`bamep-agent-protocol` materializes `TransferAuthorizationRequest`/`Grant`/`Denied`
(`transfer_id`/`action_id` as `ProtocolId`, `token`/`data_plane_base_url` opaque strings,
`token` `Debug`-redacted); `bamep-worker-protocol` materializes `AuthorizationQuery`/
`AuthorizationDecision` (opaque `token`/`proof_id`/`signature`, closed `AuthorizationOperation`/
`WireTransferDirection` wire enums, manual `Debug` redaction of every secret field) — neither
wire crate depends on `bamep-domain`; both treat capability/proof material as opaque bytes to be
forwarded, per ADR-0003/ADR-0018.

Since #39 the Worker UDS client is `bamep_worker::ipc::control` (see "Worker-side reconnecting
client" above): the `ControlDriver` publishes the live per-generation request channel over a
`watch` the instant a handshake succeeds and clears it the instant the connection ends, and
`WorkerControlHandle` exposes exactly the five authorizing/follow-up operation pairs the v1
catalog defines — not a generic RPC surface. A disconnect with a request in flight, a
correlation violation, a `bamepd` `ProtocolError`, a per-request timeout, or no connection at
all all fail the caller closed with a typed `ControlError`; the Worker can never fabricate an
`AuthorizationDecision`, `ChunkAcceptanceDecision`, `ResumeDiscoveryPage`,
`ManifestSealDecision`, or `ArtifactVerificationAck` locally. This client is driven by real
HTTPS data-plane traffic since #39 — see "Implemented Worker data-plane chunk transport".

`bamep-simulator` carries the **Agent-side** half of this boundary, since M1's Agent
participant is the Simulated Endpoint: `bamep_simulator::transfer_authorization` independently
generates the ephemeral Ed25519 proof keypair (`AgentProofKey`, private half redacted and never
persisted, regenerated on restart/renewal), exposes only the canonical `proof_public_key` wire
form, computes `CapabilityId = SHA-256(token)` itself, mints a fresh `proof_id` per operation
attempt, and builds and signs its own copy of the exact 137-byte transcript
(`build_proof_transcript`) — it does **not** call `bamep-server`/`bamep-domain`, so
`bamepd`'s verifier reconstructing and accepting that signature is real cross-implementation
interoperability evidence (`crates/simulator/tests/data_plane_proof_interop.rs`). This is the
reusable Agent proof/authorization participant #19 will later compose into a real WSS+HTTPS
transfer; #19 still owns that final composition.

## Implemented Worker data-plane chunk transport (Issue #39)

Issue #39 completes the M1 Worker data plane: the Worker process now terminates the external
HTTPS `/api/data/v1/` transfer surface and executes it as pure mechanism, while every durable
decision stays with `bamepd` (ADR-0018; `m0-data-plane-and-storage-contracts.md`;
`m1-worker-data-plane-control-contract.md`).

**Worker Protocol v1 catalog.** `bamep-worker-protocol` now carries every v1 business message
(authorization, durable chunk acceptance, paginated resume discovery, atomic manifest seal,
independent Artifact verification) alongside the #37 handshake slice, with per-message shape
validators. `protocol_version` and the 137-byte proof transcript are unchanged.
`bamep_worker::ipc::control` is the concurrent client over it (see "Worker-side reconnecting
client"): one reconnecting connection, many correlated outstanding requests, generation-scoped
`acceptance_handle`/`verification_handle`/resume-cursor tickets that a reconnect invalidates
locally before anything is sent, and no local business verdict ever.

**Worker-local chunk storage (`bamep_worker::storage`, Unix-only).** `FilesystemChunkStore`
stages one authorized chunk body under an owner-only tree, hashes it incrementally with
SHA-256, and finalizes it with `flush` → file `fsync` → `link(2)` no-replace placement →
directory `fsync` into a deterministic restart-stable path; a byte-different chunk already at
that path is refused, never overwritten, and recognised leftover staging files are cleared at
startup. A finalized file means only "these exact bytes were staged and hashed here" — never
that `bamepd` accepted or holds the chunk. `FullArtifactHasher` reopens a sealed Artifact's
finalized chunks `0..chunk_count` in order and recomputes the full-Artifact SHA-256 over their
raw concatenation with a fresh hasher — a true independent reread that reuses none of the
per-chunk hashes and reaches no `Verified`/`Failed` verdict. The API is blocking and never
buffers a whole chunk; the HTTP layer drives it from `spawn_blocking`.

**Worker HTTPS data plane (`bamep_worker::data_plane`).** An Axum 0.8 + `axum-server` listener
serves the Worker's existing `rustls::ServerConfig` (the same Server TLS identity the Agent
already trusts; `http/1.1` ALPN; no second certificate, trust anchor, or client auth). It does
**structural** HTTP parsing only — a structurally unrepresentable request is the contract's
fixed `400 MALFORMED_REQUEST`; everything else, including a nonexistent `transfer_id`, goes to
`bamepd` and every authorization or control-transport failure is the single fixed generic
`401 AUTHORIZATION_DENIED`. No `5xx` on `/api/data/v1/`, no redirects. Three route shapes:
`GET .../chunks` (resume discovery → `discover_resume`, one aggregated response, no cursor
escapes); `PUT .../chunks/{n}` (authorize → stream the body into storage bounded by the
authoritative `chunk_size` → mechanical SHA-256 → validate the declared/expected digest →
no-replace finalize → `commit_chunk` → only then the `201 accepted` / `200 already_held`
response); `POST .../seal` (`seal_manifest` first durable commit → independent full-Artifact
reconstruction over the authoritative sealed `chunk_count`/`chunk_size` →
`report_artifact_verification`, whose authoritative `Verified`/`Failed` is the `200` body —
both verdicts are `200`). Bulk bytes never cross the UDS: only authorization inputs, the
Worker-verified digest, the exact received size, and control results do. Any failure after the
seal's first commit (a lost `Ack`, a reread failure, UDS loss) leaves the Artifact
`PendingVerification` and fails the request closed; an idempotent seal retry re-drives
verification.

**`bamepd` durable authority (`bamep_server`).** The `bamepd`-side control plane answers the
full catalog from current PostgreSQL state (see "`bamepd`-side UDS control plane"):
`ChunkAcceptanceService` commits/confirms per-chunk expected identity; `ManifestSealService`
seals `Incomplete → PendingVerification` and mints a `verification_handle` in one transaction;
`ArtifactVerificationService` compares the Worker's reported digest against its own durable
expected value — never trusting a Worker verdict — and commits `PendingVerification → Verified
| Failed`. The `bamepd` binary constructs these services; the Worker process, not `bamepd`,
binds the HTTPS listener.

## Maintenance rule

Update this directory only for durable structure visible in implemented code. Do not copy
planned contracts, ADR rationale, empirical evidence, or GitHub execution history here.

If this document disagrees with code/tests, it is stale.
