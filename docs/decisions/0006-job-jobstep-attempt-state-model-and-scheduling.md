# ADR-0006: Job/JobStep/Attempt state model and resource-lease scheduling

Status: Accepted

## Context

During M0, Bamep required an architectural model for durable workflow execution and
resource-aware scheduling before implementation could begin.

Issue #4 (`[WP] Define Job lifecycle and scheduling model`) had to resolve several related
questions:

- how an overall provisioning/recovery workflow is represented;
- how repeated execution of one workflow stage is represented;
- how uncertain execution after disconnect or restart is represented;
- how endpoint exclusivity and other constrained resources are arbitrated;
- how retry policy composes with Agent Protocol retry mechanics;
- what workflow/scheduler authorization means for destructive dispatch.

Earlier decisions had deliberately left parts of this open:

- ADR-0004 required an authorized Job/action as one destructive-operation precondition but
  did not define that authorization;
- ADR-0005 defined the Agent Protocol retry **mechanism** using a fresh `action_id`, but did
  not define when a retry is safe or permitted.

The owner review of the original model also identified important corrections:

- endpoint exclusivity must be Job-scoped rather than Attempt-scoped;
- time-sensitive dispatch preconditions must be revalidated immediately before durable
  dispatch commitment;
- uncertain execution needs an explicit terminal `Indeterminate` outcome;
- cancellation needs a non-terminal `Cancelling` state rather than assuming that a
  cancellation request proves execution stopped;
- Agent Protocol action states and Server workflow states must remain distinct models.

The original ADR contained detailed state tables and destructive-precondition enumeration.
Those normative details are now owned by
`docs/specifications/m0-job-lifecycle-and-scheduling.md` and related Specifications.

This ADR preserves **why the model was selected**.

## Decision

### Three-tier workflow model

Bamep models durable workflow execution using three distinct concepts:

- **Job** — the overall workflow targeting one Endpoint;
- **JobStep** — one ordered stage of that workflow;
- **Attempt** — one concrete execution attempt of a JobStep.

An Attempt corresponds to one Agent Protocol `action_id` lifecycle.

A retry is therefore not mutation or reuse of an earlier Attempt. It creates a new Attempt
with a fresh action identity while retaining explicit correlation to the previous
execution.

This separation is required because JobStep lifecycle and execution-attempt lifecycle have
different semantics:

- one JobStep may require more than one execution attempt;
- an individual Attempt may be rejected, fail, be cancelled, or become uncertain without
  necessarily defining the final JobStep outcome;
- retry and reconciliation history must remain durable and auditable.

The exact state machines and transitions are normative in
`docs/specifications/m0-job-lifecycle-and-scheduling.md`.

### Linear JobStep sequence for the baseline

The baseline uses an ordered linear sequence of JobSteps.

A DAG or branching workflow model is not part of this decision because no M0 requirement
required parallel or branching JobSteps within one Job.

A future requirement for branching, parallel JobSteps, partial completion, or skip
semantics must be introduced explicitly rather than inferred from this model.

### Job-scoped endpoint exclusivity

Endpoint exclusivity is owned by the **Job**, not by an individual JobStep or Attempt.

Once a Job is admitted to active execution, another active Job must not interleave work
against the same Endpoint.

This scope was selected because:

- the baseline does not require two active Jobs to interleave against one Endpoint;
- Job-scoped ownership is simpler than repeatedly releasing and reacquiring exclusivity
  between JobSteps or Attempts;
- retaining exclusivity across retries and reconciliation provides the safer boundary while
  execution outcome may still be uncertain.

Scheduler admission therefore arbitrates the endpoint-exclusivity lease.

The exact Job states, acquisition/release transitions, and queuing behavior belong to the
Job lifecycle Specification.

### Attempt-scoped constrained-resource leases

Resources other than endpoint exclusivity may be scoped to individual Attempts.

Examples include:

- network capacity;
- storage read/write capacity;
- CPU/Worker capacity;
- future constrained technical resources.

This separates long-lived endpoint ownership from resources that only need to be consumed
while a concrete execution attempt requires them.

The resource model is extensible. This ADR does not select a fairness algorithm, lease
ordering algorithm, priority model, or fixed global concurrency limit.

### Resource-aware scheduling instead of one global concurrency number

Bamep does not model scheduling as one fixed global number of concurrent endpoints.

Different operations consume different constrained resources. A JobStep/Attempt therefore
competes for the technical leases required by that operation rather than only for a single
repository-wide concurrency slot.

A later capacity policy may constrain admission through this resource model without
changing the core decision recorded here.

### Workflow/scheduler authorization is one independent gate

Workflow/scheduler authorization is a distinct decision produced by Job lifecycle and
resource arbitration.

It answers whether the current workflow is allowed to create and dispatch an Attempt at
that point in time.

It is **not** a synonym for the entire destructive-operation safety gate and must not be
defined recursively in terms of that full gate.

The precise workflow/scheduler authorization conditions are normative in
`docs/specifications/m0-job-lifecycle-and-scheduling.md`.

### Destructive dispatch consumes the complete normative safety gate

This ADR does not own the list of destructive-operation preconditions.

A destructive Attempt may be committed for dispatch only when the **complete current
normative precondition set** defined by the applicable Specifications holds.

The current Job lifecycle Specification composes the seven independent destructive
preconditions defined by the Endpoint identity/trust baseline, including trusted current
bootstrap context.

The original ADR text enumerated an older six-item set. That enumeration is intentionally
removed here so the ADR cannot compete with the normative Specification as the safety
contract evolves.

No destructive precondition may be inferred from another.

### Revalidation immediately before durable dispatch commitment

Preliminary JobStep eligibility is not sufficient authorization to dispatch.

Time-sensitive conditions and workflow/scheduler authorization must be evaluated at the
final pre-dispatch boundary required by the Job lifecycle Specification.

This revalidation gates creation of the durable dispatch commitment.

The ordering is important because:

- leases or authoritative state may change after an earlier eligibility check;
- a stale eligibility result must never authorize destructive work;
- persistence and network transmission cannot be one atomic operation.

The current persistence baseline requires the dispatch commitment to be durably persisted
before the Server attempts delivery to the Agent. Current authority for those persistence
invariants is ADR-0013 and the persistence Specification.

### Attempt is the Server-side interpretation of execution

Attempt state is a Server-side durable workflow model.

It is not a 1:1 copy of the Agent Protocol's local action-state vocabulary.

The Agent Protocol communicates execution evidence; the Job lifecycle translates that
evidence into durable workflow state according to its normative Specification.

This separation allows the protocol and Domain model to evolve at their appropriate
boundaries without requiring identical state machines.

### Uncertain execution is first-class

Loss of acknowledgement, connection loss, Agent restart, or Server restart can make the
actual outcome of an Attempt uncertain.

The model therefore distinguishes:

- an Attempt that is still awaiting reconciliation;
- a terminal `Indeterminate` Attempt whose real execution outcome cannot be established.

`Indeterminate` must never mean:

- success;
- failure;
- cancellation;
- proof that the action never executed.

Closing uncertainty as `Indeterminate` requires the explicit reconciliation semantics
defined by the Job lifecycle Specification.

This distinction is especially important for destructive work, where assuming
"not executed" could lead to unsafe duplicate execution.

### Destructive work is never blindly retried

ADR-0005 provides the protocol mechanism for a retry: a new action identity correlated to
the previous one.

This ADR establishes that the existence of that mechanism does not authorize its use.

Destructive JobSteps have no generic automatic-retry path.

A subsequent destructive Attempt after failure or indeterminate execution requires the
explicit authorization semantics defined by the Job lifecycle Specification.

Reconnect, timeout, Server restart, Agent restart, `Unknown`, or `Indeterminate` must never
be treated as implicit permission to redispatch destructive work.

### Cancellation represents intent separately from outcome

Requesting cancellation does not prove execution stopped.

The Job model therefore includes a non-terminal `Cancelling` state while active or
uncertain execution is resolved.

This prevents Bamep from:

- reporting cancellation prematurely;
- releasing endpoint exclusivity while execution may still exist;
- starting another Job or Attempt based on an unproven stopped state.

The detailed `CancelAction` / `CancelAck` / reconciliation transitions belong to the Job
lifecycle and Agent Protocol Specifications.

## Alternatives considered

### Two-tier model without Attempt

Rejected.

Representing repeated execution directly on JobStep would require ad hoc fields or mutable
history to distinguish multiple dispatches.

A first-class Attempt provides:

- one durable record per concrete execution;
- direct correlation to one `action_id`;
- explicit retry lineage;
- independent reconciliation and terminal outcome.

### DAG or branching workflow baseline

Rejected for M0.

No accepted requirement required branching or parallel JobSteps inside one Job.

A linear sequence was the smallest model that satisfied the workflow requirements.

### One fixed global concurrency limit

Rejected.

A single number cannot represent endpoint exclusivity, network, storage, Worker/CPU, and
other independently constrained resources.

Resource leases provide a model that can express those constraints without prematurely
choosing a complex scheduling algorithm.

### Attempt- or JobStep-scoped endpoint exclusivity

Rejected after owner review.

Releasing endpoint ownership between Attempts or JobSteps would allow interleaving of two
Jobs against one Endpoint even though the baseline had no requirement for that behavior.

Job-scoped exclusivity is both simpler and safer for the accepted workflow model.

### Treat protocol `Unknown` as an automatic final interpretation

Rejected.

Protocol uncertainty requires workflow-level judgment.

Automatically mapping one unknown report to "not executed", retryable failure, or even
immediate `Indeterminate` would collapse distinct safety-relevant states.

### Immediate Job `Cancelled` on cancellation request

Rejected.

A cancellation request is intent, not execution evidence.

The explicit `Cancelling` state preserves this distinction until active or uncertain work
has been resolved.

### Generic automatic retry for destructive JobSteps

Rejected.

Even a nominally gated generic retry path creates an unsafe default for operations whose
previous execution may have partially or fully occurred.

Destructive retry must remain an explicit workflow decision rather than a generic transport
or scheduler policy.

## Consequences

- Job, JobStep, and Attempt are durable Domain concepts.
- Attempt history must survive Server restart sufficiently to support reconciliation.
- Endpoint exclusivity is arbitrated at Job admission and remains held while the Job is
  active or cancellation/reconciliation uncertainty still exists.
- Attempt-scoped resource leases are separate from Job-scoped endpoint exclusivity.
- Scheduler/resource policy can evolve without collapsing the lifecycle model into a fixed
  global concurrency number.
- Dispatch commitment must respect the current persistence contract and persist-before-send
  ordering.
- Uncertain execution must remain explicit rather than being silently mapped to failure,
  success, or non-execution.
- Destructive work cannot inherit a generic automatic-retry policy.
- Data-plane JobSteps must fit the same JobStep/Attempt and reconciliation model; their
  transfer-specific contract remains separate.
- Simulator validation must be able to exercise reconciliation, `Indeterminate`,
  cancellation, and endpoint-exclusivity contention through the normative contract.
- Future branching workflows, partial-failure/skip semantics, or materially different
  scheduling semantics require explicit approved design work.

## Authority boundary

This ADR owns the **decision rationale** for:

- the Job/JobStep/Attempt split;
- linear baseline workflow shape;
- Job-scoped endpoint exclusivity;
- Attempt-scoped technical resource leases;
- resource-aware scheduling;
- explicit uncertainty and `Indeterminate`;
- non-terminal cancellation;
- prohibition on generic destructive retry.

It does **not** own:

- the exact lifecycle state tables or transitions;
- the current destructive-precondition list;
- Agent Protocol wire messages;
- persistence transaction details;
- data-plane transfer semantics;
- Simulator scenario requirements.

Those are owned by their Specifications.

## Current implementation relationship

Issue #18 (`[WP] Execute durable Job lifecycle, scheduling, and safe action dispatch`) is
the current M1 implementation Work Package for this model.

The current repository does not yet make completion of that Work Package implicit merely
because this ADR is Accepted.

`docs/architecture/README.md` remains authoritative for what is actually implemented.

## Related specifications and decisions

- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — normative Job/JobStep/Attempt
  lifecycle, scheduling, reconciliation, and destructive-dispatch composition.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — normative Endpoint/trust safety
  preconditions consumed by destructive dispatch.
- `docs/specifications/m0-agent-protocol-contract.md` — action identity, status,
  acknowledgement, cancellation, and retry/reconciliation wire contract.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — durable workflow,
  event, audit, and persistence semantics.
- ADR-0004 — Endpoint identity/enrollment decision history.
- ADR-0005 — Agent control-plane and typed-action decision history.
- ADR-0013 — current PostgreSQL persistence baseline and carried-forward persistence
  invariants.

## Related work

- Issue #4 — historical M0 Work Package that produced this decision and the normative Job
  lifecycle Specification.
- Issue #18 — current M1 Work Package implementing and validating the durable Job lifecycle,
  scheduling, reconciliation, and complete destructive-dispatch gate.
