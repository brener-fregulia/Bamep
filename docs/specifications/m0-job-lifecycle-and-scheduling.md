# M0 — Job Lifecycle and Scheduling

Status: **Approved**

This Specification defines the normative Job/JobStep/Attempt lifecycle, resource-lease scheduling, dispatch authorization, cancellation, retry, and reconciliation contract. Decision rationale belongs to ADR-0006.

## Domain model

- **Job** — one workflow targeting one Endpoint, composed of an ordered sequence of JobSteps.
- **JobStep** — one stage of that workflow; may have more than one Attempt.
- **Attempt** — one concrete execution of a JobStep, corresponding 1:1 to one Agent Protocol `action_id` lifecycle.

The baseline workflow is linear; branching/parallel JobSteps are outside this contract.

## Job lifecycle

States: `Pending`, `Running`, `Cancelling`, `Succeeded`, `Failed`, `Cancelled`.

Transitions:
- `Pending -> Running` — acquire the Job-scoped Endpoint-exclusivity lease.
- `Pending -> Cancelled` — cancellation before work begins; release any granted exclusivity lease.
- `Running -> Succeeded` — every ordered JobStep is `Succeeded`.
- `Running -> Failed` — a JobStep reaches terminal `Failed`.
- `Running -> Cancelling` — cancellation is requested while active.
- `Cancelling -> Cancelled` — no active/uncertain execution remains and no further Attempt is authorized.
- `Succeeded`, `Failed`, and `Cancelled` are terminal.

While `Cancelling`:
- no new JobStep or Attempt may begin;
- an Attempt in `Dispatched`, `InProgress`, or `AwaitingReconciliation` receives `CancelAction`;
- if no active/uncertain Attempt exists, cancellation may complete immediately;
- `CancelAck{Cancelled}` makes the active Attempt/JobStep `Cancelled`;
- `CancelAck{AlreadyCompleted}` preserves an already-known terminal result; otherwise the Attempt enters `AwaitingReconciliation` and requires `StatusQuery`;
- `CancelAck{CannotCancel}` does not imply cancellation; wait for the actual terminal outcome;
- `CancelAck{Unknown}` requires reconciliation.

The Job remains `Cancelling` until execution is known terminal or explicitly resolved to `Indeterminate`. It then becomes `Cancelled` even when the final known Attempt outcome is `Succeeded`, `Failed`, `Rejected`, or `Indeterminate`, provided no further Attempt is authorized.

## Resource leases

**Endpoint exclusivity is Job-scoped.**
- Acquired at `Pending -> Running`; a competing Job for that Endpoint remains `Pending`.
- Retained across JobSteps, Attempts, retries, cancellation, and `AwaitingReconciliation`.
- Released only at terminal Job state; never while execution outcome is uncertain.

Bamep does not interleave active Jobs against one Endpoint.

**Other constrained resources are Attempt-scoped**, including network, storage read/write, and CPU/Worker capacity.
- Required leases are acquired before final dispatch revalidation.
- Released when the Attempt reaches `Succeeded`, `Failed`, `Cancelled`, `Rejected`, or `Indeterminate`.
- Retention during `AwaitingReconciliation` is implementation-time policy.

## JobStep lifecycle

States: `Pending`, `PreconditionsSatisfied`, `Dispatching`, `Succeeded`, `Failed`, `Cancelled`.

Transitions:
- `Pending -> PreconditionsSatisfied` — preliminary JobStep-specific eligibility passes; this does not establish workflow authorization or the destructive gate.
- `Pending | PreconditionsSatisfied -> Failed{PreconditionNotMet}` — initial declared preconditions fail.
- `PreconditionsSatisfied -> Dispatching` — Attempt leases are held, final revalidation passes, and an Attempt is durably committed for dispatch.
- `PreconditionsSatisfied -> Pending` — final revalidation fails; no Attempt is created and newly acquired Attempt leases are released.
- `Dispatching -> Dispatching` — another Attempt is permitted by retry policy.
- `Dispatching -> Succeeded` — the current Attempt succeeds.
- `Dispatching -> Failed` — the current Attempt ends `Failed`, `Rejected`, or `Indeterminate` and no further Attempt is authorized; use `ExecutionFailed`, `DispatchRejected`, or `ReconciliationIndeterminate`.
- `Dispatching -> Cancelled` — the active Attempt is authoritatively cancelled.
- `Succeeded`, `Failed`, and `Cancelled` are terminal.

## Workflow/scheduler authorization

Every Attempt requires all of:
1. Job is `Running`, holds its Endpoint-exclusivity lease, and is not cancelling/terminal;
2. JobStep is the current active step;
3. retry/reconciliation policy permits this Attempt;
4. all required Attempt-scoped leases are held;
5. no unresolved prior Attempt requires an explicit decision before another Attempt may exist.

This authorization is one independent destructive-operation precondition; it must not recursively include the complete destructive gate.

## Destructive dispatch

A destructive JobStep additionally requires the **complete current seven-item destructive-operation gate** defined by `docs/specifications/m0-endpoint-identity-lifecycle.md`. This Specification intentionally does not duplicate that list; the workflow/scheduler authorization above is the Job-owned authorization dimension within it.

All seven conditions are independent. In particular, authenticated credential/session state does not imply trusted current bootstrap.

A non-destructive JobStep requires workflow/scheduler authorization plus its own time-sensitive declared preconditions.

## Final pre-dispatch revalidation

`PreconditionsSatisfied` is preliminary only. After Attempt-scoped leases are acquired and immediately before the durable dispatch commitment:
1. revalidate workflow/scheduler authorization;
2. for destructive work, revalidate the complete seven-item Endpoint destructive gate;
3. revalidate any other time-sensitive JobStep preconditions.

Only after success may the Attempt/dispatch commitment be persisted. Transmission is attempted only after that commit, following the persistence contract's persist-before-send ordering.

If any required condition fails:
- no Attempt is created;
- no durable dispatch commitment is persisted;
- no `ActionDispatch` is sent;
- newly acquired Attempt-scoped leases are released;
- the JobStep returns to `Pending`.

This includes the case where the trusted-current-bootstrap precondition alone fails while the other six Endpoint destructive preconditions hold. No safety precondition may be inferred from another. There is no second safety evaluation between durable commit and the immediate transmission attempt.

## Attempt lifecycle

States: `Dispatched`, `InProgress`, `AwaitingReconciliation`, `Succeeded`, `Failed`, `Cancelled`, `Rejected`, `Indeterminate`.

`Dispatched` means the Server has durably committed the Attempt and transmission may or may not have occurred; it does not prove Agent receipt.

Transitions:
- `Dispatched -> InProgress` — `ActionAck{Accepted}`.
- `Dispatched -> Rejected` — `ActionAck{Rejected}`.
- `Dispatched -> Succeeded | Failed` — matching terminal `ActionResult` received without an observed `ActionAck{Accepted}`.
- `Dispatched -> AwaitingReconciliation` — expected acknowledgment does not arrive.
- `InProgress -> Succeeded | Failed` — matching `ActionResult`.
- `InProgress -> Cancelled` — authoritative cancellation result.
- `Dispatched | InProgress -> AwaitingReconciliation` — connection loss or Server restart leaves execution/delivery uncertain.
- `AwaitingReconciliation -> InProgress` — `StatusReport{Accepted | Running}`.
- `AwaitingReconciliation -> Succeeded | Failed | Cancelled` — matching terminal `StatusReport`.
- `AwaitingReconciliation -> Indeterminate` — an explicit reconciliation decision closes an outcome that cannot be established.
- `Succeeded`, `Failed`, `Cancelled`, `Rejected`, and `Indeterminate` are terminal.

One `StatusReport{Unknown}` does not automatically produce `Indeterminate`. `Indeterminate` never means success, failure, cancellation, or proof that execution did not occur.

A correctly authenticated and correlated terminal `ActionResult` is stronger authoritative execution evidence than the absence of an observed `ActionAck{Accepted}`; the `Dispatched -> Succeeded | Failed` transition covers a lost or delayed `Accepted` Ack followed by authoritative terminal evidence and does not require a synthetic persisted `InProgress` step first. It must not be modeled as automatic retry, a fabricated receipt, blind redispatch, or implicit failure inferred merely from a missing Ack. The normal `Dispatched -> InProgress -> Succeeded | Failed` path remains equally legitimate.

## Duplicate and delayed evidence

`action_id` is the domain/protocol idempotency key. While an Attempt's current authoritative state remains known — no connection loss or restart involved — the Server:

- treats a duplicate `ActionAck{Accepted}` against an Attempt already `InProgress` as a no-op;
- treats matching duplicate terminal evidence against an Attempt already committed to that same terminal outcome as a no-op;
- never lets a delayed `ActionAck` regress an Attempt past an already-committed authoritative terminal outcome;
- never lets `ActionProgress` regress or reopen a terminal Attempt;
- never overwrites an already-committed authoritative terminal outcome with conflicting terminal evidence;
- never creates a second Attempt from duplicate or delayed evidence.

This governs evidence handling while current authoritative state exists. Connection loss, restart, and uncertain-delivery recovery remain the Reconciliation contract below.

## Agent Protocol mapping

| Agent Protocol evidence | Attempt state |
| --- | --- |
| `ActionAck{Accepted}` | `InProgress` |
| `StatusReport{Accepted}` | `InProgress` |
| `StatusReport{Running}` | `InProgress` |
| `ActionResult{Succeeded}` | `Succeeded` |
| `ActionResult{Failed}` | `Failed` |
| `ActionResult{Cancelled}` / `CancelAck{Cancelled}` | `Cancelled` |
| `ActionAck{Rejected}` | `Rejected` |
| acknowledgment timeout | `AwaitingReconciliation` |
| connection loss / restart while `Dispatched` or `InProgress` | `AwaitingReconciliation` |
| `StatusReport{Unknown}` plus explicit close decision | `Indeterminate` |

Agent-side `Accepted` and `Running` deliberately collapse into Attempt `InProgress`.

## Reconciliation

On Server restart:
1. persisted `Dispatched` and `InProgress` Attempts become `AwaitingReconciliation`;
2. after the relevant Agent session re-establishes, send `StatusQuery{action_id}`;
3. apply returned evidence through the mapping above;
4. never blindly redispatch, resume, or assume success.

A destructive Attempt resolved to `Indeterminate` requires an explicit recorded operator decision before another destructive Attempt may be authorized.

## Retry policy

- **Destructive JobSteps:** never retry automatically after `Failed`, `Rejected`, or `Indeterminate`; every further Attempt requires explicit recorded operator authorization.
- **Non-destructive JobSteps:** bounded automatic retry may be allowed after `Failed`; retry after `Indeterminate` requires explicit opt-in by that JobStep type.
- `Rejected` is not automatically treated as retryable `Failed`.
- Every retry is a fresh Attempt with a fresh Agent Protocol action identity.

Exact retry bounds/backoff are implementation-time policy.

## Job admission and capacity

At `Pending -> Running`, the Job must acquire its Endpoint-exclusivity lease.

Admission may also consume an Application-supplied effective capacity policy limiting active Job-scoped Endpoint leases. The Scheduler receives only generic numeric/technical policy, not commercial edition/license concepts. A Job blocked only by capacity remains `Pending`; later capacity-policy reduction does not terminate already `Running` or `Cancelling` Jobs.

Ordering, fairness, and priority among queued Jobs/leases remain implementation-time.

## Operator submission correlation

Job creation may be correlated to an operator submission that translates one operator intent
over `1..N` Endpoints into independent one-Endpoint Jobs
(`docs/specifications/m0-persistence-observability-and-domain-events.md` "Operator submission
persistence and correlation"; ADR-0019). That submission's creation-processing state owns no
Job admission, scheduling, dispatch, cancellation, reconciliation, or execution authority,
and does not add or change any Job, JobStep, or Attempt state or transition defined here.

## Out of scope

- partial-failure/skip semantics;
- DAG/branching/parallel JobSteps;
- exact scheduling/fairness/priority algorithm;
- non-exclusivity lease retention during `AwaitingReconciliation`;
- exact retry bounds/backoff;
- whether repeated revalidation failure eventually becomes terminal `Failed{PreconditionNotMet}`;
- transfer-specific preconditions/postconditions;
- concrete persistence or Agent transport mechanics.

## Related

- ADR-0006 — Job/JobStep/Attempt and scheduling rationale.
- ADR-0019 — operator submission boundary correlated to Job creation.
- ADR-0015 — effective capacity-policy boundary.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — authoritative destructive-operation gate.
- `docs/specifications/m0-agent-protocol-contract.md` — Agent action/status/cancellation wire contract.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — durable state and persist-before-send contract.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — transfer-specific JobStep semantics.
