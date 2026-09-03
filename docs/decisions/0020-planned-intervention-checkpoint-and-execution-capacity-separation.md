# ADR-0020: Planned operator-intervention checkpoint and separation of Endpoint exclusivity from automated-execution capacity

Status: Proposed

## Context

M2 introduces the operator plane over the M1 hardware-independent operational core. The
Issue #45 Discovery
(`docs/discovery/m2-composite-service-workflow-and-operator-intervention.md`) investigated
whether technical composed services, planned human intervention mid-workflow, and intentional
hardware replacement require new durable architecture. The owner-approved conclusions relevant
to a durable decision are recorded here (WHY); the normative behavior they imply is owned by
`docs/specifications/m0-job-lifecycle-and-scheduling.md` and
`docs/specifications/m0-endpoint-identity-lifecycle.md` (WHAT). This ADR is filed `Proposed`
pending the owner's explicit acceptance of this record; it does not mark itself Accepted.

Two facts from the existing architecture frame the problem:

- **Execution authority is unchanged.** One Endpoint → one Job → ordered JobSteps → Attempts
  (ADR-0006; `m0-job-lifecycle-and-scheduling.md` "Domain model"). ADR-0019 rejected a durable
  Domain `Operation` aggregate; that rejection stands. A composed service is one Endpoint's
  Job with more JobSteps, composed at the Application/submission boundary, not a new
  aggregate.
- **ADR-0015 §6 currently equates two things.** It defines commercial execution capacity as
  "the maximum number of simultaneously active Endpoint Jobs, measured by the number of
  Job-scoped endpoint-exclusivity leases currently granted … Jobs in `Running` or
  `Cancelling`." At the time ADR-0015 was written every non-terminal admitted Job was also
  actively executing automated work, so the equivalence was harmless.

The intended commercial topology breaks that equivalence. A representative appliance provides
a fixed number of concurrent downlinks for Endpoints being serviced (the concrete number is a
product parameter — it is **not** Domain vocabulary and must never be encoded as `Bamep4`, a
NIC-port count, a downlink number, or a SKU in Domain, Application, or Runtime Services). A
required physical-service workflow is:

```text
backup old disk → verify backup → planned human-intervention checkpoint
→ technician disconnects the Endpoint entirely
→ HDD is physically replaced with an SSD (which may take hours or days)
→ Endpoint reconnects → same durable Endpoint recognized
→ fresh machine facts established → destructive target re-resolved
→ execution capacity reacquired → final safety gates revalidated
→ installation continues → restore
```

While the Endpoint is physically detached for the manual disk swap, its former downlink is
free and another Endpoint may be serviced on it. The Job is legitimately non-terminal,
Endpoint-exclusive, and **not executing any automated work** for an extended period — a
combination the previous model could not represent.

## Decision

### 1. A planned intervention checkpoint is a first-class workflow boundary

A JobStep may declare a planned point at which automated execution stops and waits for an
external human physical action before the workflow continues. This boundary is distinct from
failure and from reconciliation:

- it is **not** `Failed` / `Cancelling` — the workflow is healthy and paused at a known-good
  point (e.g. immediately after a verified backup);
- it is **not** `AwaitingReconciliation` / `Indeterminate` — those exist because Bamep lost
  certainty about a **live** Attempt's outcome; a checkpoint has no live Attempt and no
  uncertainty.

The checkpoint state is durable and survives Server restart with no reconciliation, because
no Attempt is in flight.

### 2. Endpoint exclusivity and automated-execution capacity are separate authorities

They answer different questions and have different lifetimes:

| Concern | Question | Held from → until |
|---|---|---|
| **Endpoint exclusivity** (Job-scoped) | May another conflicting Job operate on this same Endpoint? | Job admission → Job terminal/cancellation |
| **Automated-execution capacity** (generic commercial slot) | Is this Job consuming one of the finite concurrent automated-execution slots right now? | Execution admission → checkpoint park **or** Job terminal |
| **Attempt-scoped / runtime resource leases** (network, storage, CPU/Worker) | Is a runtime resource reserved for a live Attempt? | Acquired before final pre-dispatch → released at Attempt terminal |

For a Job parked at a planned intervention checkpoint:

- **Endpoint exclusivity: retained.** The original service Job keeps exclusive logical
  ownership of that Endpoint's service context. No second conflicting Job may be admitted
  against that Endpoint merely because it is physically disconnected.
- **Automated-execution capacity: released.** No automated Endpoint work is executing, so no
  slot is consumed. Another Endpoint may use it.
- **Attempt-scoped / runtime leases: none held.** The pre-checkpoint JobStep's Attempt has
  already reached a terminal state; the parked condition creates no long-lived reservation.

### 3. Automated continuation reacquires capacity and revalidates safety

Resuming automated execution past the checkpoint requires **all** of the following, none
inferred from another and **none satisfied by an operator acknowledgement alone** — a bare
click asserts intent, not physical reality, and cannot prove the disk was installed, is the
right size, is healthy, or is the only candidate:

1. the same durable Endpoint is recognized through the existing identity/authentication model;
2. the recorded operator intervention decision exists, plus any intervention-specific evidence
   the JobStep declares;
3. current machine facts are freshly established as applicable: current inventory revision,
   authoritative current bootstrap, an authenticated Agent session, and hardware confidence
   resolved under `m0-endpoint-identity-lifecycle.md`;
4. for a destructive continuation, stale target authorization is invalidated and the
   destructive target is re-resolved and revalidated against current hardware;
5. an automated-execution capacity slot is reacquired; if none is available the Job **waits**
   — lack of capacity is never a Job failure, and a resuming Job never preempts already
   executing work;
6. the complete applicable final pre-dispatch safety gate runs before any Attempt, unchanged
   and additive.

### 4. Intentional hardware replacement does not create a new Endpoint

Replacing an HDD/SSD changes current boot, authenticated Agent session, inventory revision,
hardware confidence, and the current destructive target identity. It does **not** change
durable Endpoint identity, and it does not lose correlation with the parked Job. Disk
identity and inventory are evidence, never the durable Endpoint identity or a trust anchor
(`AGENTS.md` "Safety"; ADR-0004). Artifact source provenance bound to the earlier captured
data is immutable and keeps describing the pre-replacement source (source provenance and
destructive target identity are already independent — ADR-0008).

### 5. Relationship to ADR-0015 — capacity semantic only

When accepted, this ADR **supersedes only** ADR-0015 §6's capacity-measurement equivalence.
The generic capacity unit becomes:

> the maximum number of Endpoints concurrently admitted to automated execution

rather than "the number of granted Job-scoped endpoint-exclusivity leases / Jobs in
`Running` or `Cancelling`". A Job blocked only by capacity — at first admission or at
post-checkpoint readmission — waits, composing with the existing queue semantics
(ADR-0015 §11 "Capacity exceeded").

**Every other ADR-0015 decision is unaffected and is not reopened:** Core knows no
SKU/edition/customer/contract vocabulary (§1, §5); the commercial platform translates
business concepts into generic capacity/capability facts (§3, §4); verification is
offline-capable and fail-closed (§9); the commercial platform is never required in the
destructive hot path (§11); already-executing destructive work is never terminated because
entitlement availability changes (§11). Capacity remains a generic numeric policy the
Scheduler receives without commercial vocabulary.

### 6. Representation is left to the Specification and implementation

This ADR does not decide whether the parked condition is a new JobStep lifecycle state, an
existing state plus durable checkpoint metadata, or another representation, nor does it
define the future physical-disk identity schema (WWN/serial/GPT/composite descriptors —
future physical-hardware work). The Specification defines observable behavior; the
representation is chosen later, alongside the still-open JobStep-classification work.

## Alternatives considered

- **Keep ADR-0015 §6 unchanged; a parked Job keeps its capacity slot.** Rejected: a single
  manual step lasting hours or days would hold a finite concurrent-execution slot with zero
  automated work happening, defeating the concurrency the commercial topology exists to
  provide. This was the earlier review position and is explicitly withdrawn.
- **Model the checkpoint as `AwaitingReconciliation`.** Rejected: conflates a deliberate,
  healthy pause at a known-good boundary with lost-certainty recovery of a live Attempt, and
  drags in `StatusQuery` / `Indeterminate` / explicit-operator-decision machinery for a
  non-failure.
- **Model the checkpoint as `Failed`; the operator creates a new Job to continue.** Rejected:
  loses workflow correlation, the verified backup Artifact linkage, and the resolved plan;
  forces the operator to reconstruct intent; and a "continue" that is really "start over" is
  error-prone immediately before a destructive step.
- **A durable `Operation` aggregate owning the parked/resume lifecycle.** Rejected: ADR-0019
  already rejected a durable Operation aggregate on the same test (no genuine invariant above
  individual Jobs). Parking and resume are per-Endpoint execution-layer state.
- **Release Endpoint exclusivity while parked so the slot is fully free.** Rejected: a second
  Job could then be admitted against an Endpoint whose service context is half-complete,
  violating "Bamep does not interleave active Jobs against one Endpoint" and risking a
  destructive action on a machine mid-service.
- **Reacquire nothing on resume; continue automatically once the operator clicks continue.**
  Rejected: a burst of parked Jobs reconnecting could exceed the concurrent-execution limit
  the instant their operators click, and a click cannot establish the post-swap machine
  facts the destructive gate requires.

## Consequences

- `docs/specifications/m0-job-lifecycle-and-scheduling.md` gains a "Planned intervention
  checkpoint" section and a corrected "Job admission and capacity" paragraph; its capacity
  wording stops equating capacity with the endpoint-exclusivity lease.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` gains a minimal "Planned hardware
  replacement" clarification: the same Endpoint is preserved, and stale destructive-target
  authorization is not reused.
- ADR-0015 §6 carries a note pointing here; ADR-0015 keeps its `Accepted` status and its
  history is not rewritten. Formal partial supersession takes effect when this ADR is
  accepted.
- No code changes. ADR-0015 capacity gating is not implemented, and the intervention
  checkpoint is not implemented; this is a durable-decision and Specification change only.
- Bulk cancellation (Application fan-out keyed by `submission_id`), operator-submission
  correlation, the seven-item destructive gate, and every Job/JobStep/Attempt state and
  transition are unchanged.
- Left open (Discovery / future work): the exact durable representation of the parked
  condition; `JobStep.kind` classification; and all Selective-backup questions (artifact
  granularity, resolved-capture-set schema, NTFS path/ACL/ADS/EFS behavior, assisted-discovery
  authorization timing, restore mapping), which still require the planned narrow NTFS
  Technical Spike.

## Related architecture

- ADR-0006 — Job/JobStep/Attempt state model and Job-scoped Endpoint exclusivity; the
  execution authority this decision builds on, not reopened.
- ADR-0015 — commercial entitlement boundary; §6's capacity-measurement semantic is the only
  part this decision revises, and only when accepted.
- ADR-0019 — operator submission boundary; the composition boundary and the rejection of a
  durable `Operation` aggregate, both preserved.
- ADR-0004 / ADR-0008 — Endpoint identity independent of hardware, and source-vs-target
  identity independence, both relied on directly by §4.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — normative owner of the checkpoint
  behavior and the capacity-admission rule (WHAT).
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — normative owner of Endpoint
  identity continuity and the destructive-operation preconditions (WHAT).

## Related work

- Issue #45 — the Discovery that produced these conclusions.
- The planned narrow NTFS / Selective Technical Spike — still required before any Selective
  Specification delta; not blocked by, and does not block, this ADR.
- Issue #44 — the next M2 UX Work Package; proceeds under Option B (a representative composed
  reinstall/service mock, not `Capturar imagem do sistema`, not dependent on unresolved
  Selective mechanics). Not edited by this ADR.

Status: Proposed.
