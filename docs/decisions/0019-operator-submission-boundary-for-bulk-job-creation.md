# ADR-0019: Operator submission boundary for bulk Job creation

Status: Accepted

## Context

M2 introduces the operator plane over the M1 hardware-independent operational core. A core
operator interaction is issuing one logical intent over `1..N` selected Endpoints. Bamep's
durable execution model is unchanged from M0/M1: `Job / JobStep / Attempt`, where one Job
targets exactly one Endpoint (`docs/specifications/m0-job-lifecycle-and-scheduling.md`
"Domain model"; ADR-0006). The only implemented workflow-creation path creates one Job for
one `Enrolled` Endpoint in one transaction; there is no bulk, batch, or multi-Endpoint
semantic.

Two owner-approved M2 architecture Discoveries examined whether the operator-facing grouping
of such work requires new durable state, and what shape:

- the first concluded that an operator-facing `Operation` must not be introduced as a
  durable Domain aggregate on current evidence; `Job / JobStep / Attempt` remain the
  authoritative durable execution model;
- the second defined the semantic boundary between one operator submission and the resulting
  independent per-Endpoint Jobs: command acceptance, retry/idempotency, partial creation,
  and reconstructability.

That Discovery material is investigation only. Its durable conclusions are recorded here
(WHY) and in a dedicated section of
`docs/specifications/m0-persistence-observability-and-domain-events.md` (WHAT); the Discovery
documents are not retained as permanent repository authorities.

The problem this ADR resolves is command acceptance and reconstruction, not Job execution
aggregation. Nothing here is implemented, so `docs/architecture/README.md` is not modified.

## Decision

Bamep introduces a creation-phase operator submission boundary above independent
one-Endpoint Jobs:

- Translating one operator intent over `1..N` Endpoints produces up to N independent
  one-Endpoint Jobs, each created independently within a per-Endpoint persistence
  transaction, preserving the existing one-Endpoint Job boundary. Bulk creation is not
  atomic across Endpoints.
- Partial creation is permitted: some selected Endpoints may produce Jobs while others are
  rejected before any Job exists.
- Each accepted operator submission is a durable creation-phase record. It exists so that one
  accepted command can be processed, retried safely, and reconstructed — including the
  originally requested target set, which cannot be recovered from Jobs alone once some
  targets are rejected.
- Retry identity and durable submission identity are separate: a caller-provided
  `request_key` identifies one logical command across transport retries and exists before
  the first response; a Server-minted `submission_id` is the durable identity of the
  accepted submission. They cannot be one identity, because after a lost first response the
  caller knows only `request_key`.
- The submission record owns only durable command-acceptance and per-target creation state.
  It owns no Job admission, scheduling, dispatch, Attempt, cancellation, reconciliation,
  aggregate progress, or aggregate execution outcome, and it is not a durable Domain
  `Operation` aggregate. Bulk cancellation is Application fan-out to the existing per-Job
  cancellation path, keyed by `submission_id`; the record is a correlation lookup, never a
  cancellation authority.

Exact submission identity, content, acceptance ordering, idempotency and retry-matching
rules, per-target state vocabulary and transitions, per-target finality and resume, and the
atomic-creation invariant are owned by
`docs/specifications/m0-persistence-observability-and-domain-events.md` "Operator submission
persistence and correlation" and are not duplicated here.

Future IAM/audit must be able to attribute a submission to an actor; actor identity
representation and administrative-audit semantics for submissions are outside this decision,
and actor identity is not part of `request_key` or the command-equivalence check. A later
approved IAM, administrative-audit, or product requirement may reopen the durable-`Operation`
classification only if it establishes a genuine invariant above individual Jobs.

## Alternatives considered

### Atomic all-or-nothing bulk creation across N Endpoints

Rejected on semantic/architectural grounds, not technical impossibility — PostgreSQL can
execute a multi-row transaction. One transaction spanning N independent Endpoints introduces
unnecessary cross-Endpoint locking and a shared failure domain, conflicts with the
independent Job / independent-scheduling grain established by ADR-0006, and lets one
ineligible Endpoint block unrelated eligible Endpoints. No current product invariant
requires all-or-nothing acceptance.

### Correlation identifier on successfully created Jobs only

Rejected. Rejected targets have no Job, so the original operator intent becomes
unreconstructable after a browser reload, client timeout, or Server restart, and an
uncertain first response cannot be resolved safely from Job state alone. Authoritative
operator intent would be lost for a simpler implementation.

### One server-minted identity used as both retry key and durable submission identity

Rejected. The caller cannot present a server-minted identity on the retry that follows a
lost first response, because it never received it. Retry idempotency requires a
caller-provided identity established before the first request completes, distinct from the
server-owned durable submission identity.

### A durable Domain `Operation` aggregate above Jobs

Rejected — see the first M2 Discovery. No genuine durable invariant above individual Jobs is
evidenced today; every candidate responsibility is either already owned (per-Job/per-Endpoint
audit, `installation_id` accounting, lease-counted capacity, per-Job cancellation and
reconciliation), derivable (partial outcomes, progress), or dependent on future M2 IAM/audit
requirements that have not yet established a genuine invariant above individual Jobs.

### `AuditRecord` as the authoritative store for per-target creation outcomes

Rejected. It would make ordinary operational reconstruction depend on combining business
state with the audit trail and give `AuditRecord` a product-state responsibility it does not
own. Security-relevant audit may later correlate to a submission, but the authoritative
per-target creation outcome is owned by the submission record.

## Consequences

- A new durable Application/operator-submission concept exists, owned normatively by
  `docs/specifications/m0-persistence-observability-and-domain-events.md`. No new broad M2
  Specification is created for it.
- Jobs created through the operator-submission path carry `submission_id` as durable
  correlation state; other Job-creation paths do not.
- Per-target creation outcomes are durable and once-only, so an interrupted submission can be
  resumed without re-deciding already-final targets.
- Implementation will require transactional coordination between one target's successful
  creation outcome and that target's Job/JobStep creation, and will require the submission
  acceptance commit to precede any Job creation for that submission.
- This decision does not extend the domain-event envelope. A future requirement for direct
  submission correlation on domain events, an outbox, or external integrations must extend
  the relevant contract explicitly.
- Future Administrative API work must provide a concrete wire representation of `request_key`
  and the submission read surface; this ADR defines no HTTP method, route, header, or
  payload.
- The concrete rejection-reason vocabulary, the canonical command-equivalence descriptor
  format, persistence schema, and the exact idempotency-retention duration are follow-up
  contract/implementation work, not decided here.
- `docs/architecture/README.md` is not modified: nothing is implemented.

## Related architecture

- ADR-0006 — `Job / JobStep / Attempt` state model and Job-scoped Endpoint exclusivity; the
  one-Endpoint Job semantic this decision builds above, not reopened here.
- ADR-0013 — PostgreSQL persistence backend and Domain/Application boundary the per-target
  atomicity requirement operates within.
- ADR-0015 — commercial entitlement boundary; capacity is measured by live Job-scoped
  Endpoint-exclusivity leases and `installation_id`, neither changed by this decision.
- ADR-0016 / ADR-0017 — Presentation client and Administrative serving boundary; the
  operator plane that will consume this submission boundary, whose API shape is not defined
  here.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` "Operator
  submission persistence and correlation" — normative owner (WHAT).
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — Job lifecycle; amended only with
  a short cross-reference that submission creation-processing state carries no Job lifecycle
  authority.

## Related work

- The two owner-approved M2 architecture Discoveries that produced these conclusions
  (conducted as investigation; not retained as permanent repository documents).
- Future M2 operator-plane Administrative API and IAM Work Packages, which consume this
  boundary and must not re-decide it.
