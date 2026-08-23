# ADR-0006: Job/JobStep/Attempt state model and resource-lease scheduling

Status: Accepted

## Context

Bamep needs a durable workflow model that can survive disconnect/restart, represent repeated
execution safely, and schedule constrained resources without reducing capacity to one global
concurrency number.

ADR-0005 defines the Agent Protocol retry mechanism; this ADR defines the workflow model and
the conditions under which another execution attempt may exist.

Normative lifecycle states, transitions, destructive preconditions, reconciliation details,
and cancellation semantics belong to
`docs/specifications/m0-job-lifecycle-and-scheduling.md`.

## Decision

### Three-tier workflow model

Bamep models:

- **Job** — one overall workflow targeting an Endpoint;
- **JobStep** — one ordered stage of that workflow;
- **Attempt** — one concrete execution of a JobStep.

One Attempt corresponds to one Agent Protocol `action_id` lifecycle. A retry creates a new
Attempt with a fresh action identity and explicit lineage to the prior execution.

This keeps retry/reconciliation history durable and separates workflow-stage state from
individual execution outcomes.

### Linear baseline workflow

The baseline Job is an ordered linear sequence of JobSteps.

Branching, parallel JobSteps, partial-success, and skip semantics require explicit future
design.

### Job-scoped Endpoint exclusivity

Endpoint exclusivity belongs to the Job, not an individual JobStep or Attempt.

Once a Job is active, another Job must not interleave work against the same Endpoint.

The lease remains held across JobStep boundaries, retries, cancellation handling, and
reconciliation uncertainty until the Job is genuinely terminal.

### Attempt-scoped technical resource leases

Other constrained resources may be acquired per Attempt, including:

- network capacity;
- storage read/write capacity;
- CPU/Worker capacity;
- future constrained technical resources.

This keeps long-lived Endpoint ownership separate from resources needed only for one
execution.

Fairness, priority, lease ordering, and exact capacity numbers are not selected here.

### Resource-aware scheduling

Bamep does not use one fixed global concurrency limit as its scheduling model.

Different work consumes different resources, so admission/dispatch competes for the leases
required by that operation.

Capacity policy may evolve without changing the Job/JobStep/Attempt model.

### Workflow authorization is one independent gate

Workflow/scheduler authorization answers whether the current Job/JobStep is allowed to
create and dispatch an Attempt at that point in time.

It is one independent dispatch condition, not a synonym for the complete destructive-safety
gate.

The exact normative conditions belong to the Job lifecycle Specification.

### Revalidate immediately before durable dispatch commitment

Earlier eligibility is not sufficient.

Time-sensitive preconditions and workflow authorization are revalidated at the final
pre-dispatch boundary before the durable dispatch commitment is created.

If revalidation fails, no Attempt/dispatch commitment is created and no action is sent.

Persist-before-send ordering is owned by
`m0-persistence-observability-and-domain-events.md`.

### Destructive dispatch uses the complete current safety contract

This ADR does not enumerate destructive-operation preconditions.

A destructive Attempt may be committed only when the complete current normative gate in the
Endpoint/Job Specifications holds.

No precondition may be inferred from another.

### Attempt is Server-side execution interpretation

Attempt state is a durable Server Domain concept, not a copy of Agent Protocol local action
states.

Agent Protocol supplies execution evidence; the Job lifecycle maps that evidence into
durable Attempt state.

### Uncertain execution is first-class

Disconnect, timeout, Agent restart, or Server restart may leave execution outcome unknown.

The model therefore distinguishes:

- execution still awaiting reconciliation;
- terminal `Indeterminate`, meaning the real outcome cannot be established.

`Indeterminate` means neither success, failure, cancellation, nor proof that execution never
occurred.

This prevents uncertainty from being converted into unsafe duplicate execution.

### Destructive work is never blindly retried

A fresh `action_id` is a protocol mechanism, not authorization to retry.

Destructive JobSteps have no generic automatic-retry path. Another destructive Attempt
requires the explicit workflow/operator authorization defined by the Job lifecycle
Specification.

Timeout, reconnect, restart, `Unknown`, or `Indeterminate` never imply permission to
redispatch destructive work.

### Cancellation intent is not execution outcome

Requesting cancellation does not prove that execution stopped.

The Job therefore has a non-terminal `Cancelling` condition while active or uncertain work
is resolved.

Endpoint exclusivity is not released and replacement work is not started merely because a
cancel request was issued.

## Alternatives considered

### Two-tier Job/JobStep model

Rejected. Repeated executions would require mutable/ad-hoc history instead of one durable
record per concrete Attempt.

### DAG/branching baseline

Rejected because no accepted baseline requirement needs branching or parallel JobSteps.

### Fixed global concurrency limit

Rejected because Endpoint exclusivity, network, storage, and Worker/CPU capacity are
independent constraints.

### Attempt- or JobStep-scoped Endpoint exclusivity

Rejected after owner review. It would allow two Jobs to interleave against one Endpoint
between steps/retries despite no requirement for that behavior.

### Treat protocol `Unknown` as a final workflow result

Rejected. Protocol uncertainty needs workflow-level reconciliation and cannot safely imply
"not executed", failure, or retryability.

### Immediate `Cancelled` on cancel request

Rejected because the request expresses intent, not proof that execution stopped.

### Generic destructive retry

Rejected because the previous execution may have partially or fully occurred.

## Consequences

- Job, JobStep, and Attempt are durable Domain concepts.
- Attempt/retry history survives restart sufficiently for reconciliation.
- Endpoint exclusivity is Job-scoped.
- Technical resource leases are independently Attempt-scoped.
- Scheduling remains resource-aware rather than one global concurrency value.
- Dispatch requires final revalidation and the persistence contract's persist-before-send
  ordering.
- Execution uncertainty remains explicit through reconciliation/`Indeterminate`.
- Destructive work cannot inherit a generic retry policy.
- Cancellation cannot release exclusivity or start replacement work before execution state
  is resolved.
- Data-plane work uses the same JobStep/Attempt model; transfer semantics remain in the
  data-plane Specification.

## Related

- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — normative lifecycle,
  scheduling, reconciliation, cancellation, and dispatch contract.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — Endpoint/trust safety
  preconditions.
- `docs/specifications/m0-agent-protocol-contract.md` — action/status/cancellation wire
  contract.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — durability and
  persist-before-send.
- ADR-0004 — Endpoint identity decision.
- ADR-0005 — Agent Protocol/action decision.
- Issue #18 — current M1 implementation Work Package.
