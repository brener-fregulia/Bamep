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

## Maintenance rule

Update this directory only for durable structure visible in implemented code. Do not copy
planned contracts, ADR rationale, empirical evidence, or GitHub execution history here.

If this document disagrees with code/tests, it is stale.
