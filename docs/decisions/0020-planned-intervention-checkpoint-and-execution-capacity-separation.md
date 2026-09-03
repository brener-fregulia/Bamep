# ADR-0020: Planned operator-intervention checkpoint and separation of Endpoint exclusivity from automated-execution capacity

Status: Accepted

## Context

M2 introduces the operator plane over the M1 hardware-independent operational core. The
Issue #45 Discovery
(`docs/discovery/m2-composite-service-workflow-and-operator-intervention.md`) investigated
whether technical composed services, planned human intervention mid-workflow, and intentional
hardware replacement require new durable architecture. The owner-approved conclusions relevant
to a durable decision are recorded here (WHY); the normative behavior they imply is owned by
`docs/specifications/m0-job-lifecycle-and-scheduling.md` and
`docs/specifications/m0-endpoint-identity-lifecycle.md` (WHAT).

Two facts from the existing architecture frame the problem:

- **Execution authority is unchanged.** One Endpoint → one Job → ordered JobSteps → Attempts
  (ADR-0006; `m0-job-lifecycle-and-scheduling.md` "Domain model"). ADR-0019 rejected a durable
  Domain `Operation` aggregate; that rejection stands. A composed service is one Endpoint's
  Job with more JobSteps, composed at the Application/submission boundary, not a new
  aggregate.
- **ADR-0015 §6 uses the Endpoint-exclusivity lease as a capacity-accounting proxy.** It
  defines commercial execution capacity as "the maximum number of simultaneously active
  Endpoint Jobs, measured by the number of Job-scoped endpoint-exclusivity leases currently
  granted … Jobs in `Running` or `Cancelling`." Planned intervention introduces a legitimate
  long-lived condition — a Job that is non-terminal and Endpoint-exclusive but executing no
  automated work for an extended period — that this proxy does not account for.

The motivating workflow comes from the intended commercial topology. A representative
appliance provides a fixed number of concurrent downlinks for Endpoints being serviced (the
concrete number is a product parameter — it is **not** Domain vocabulary and must never be
encoded as `Bamep4`, a NIC-port count, a downlink number, or a SKU in Domain, Application, or
Runtime Services):

```text
backup old disk → verify backup → planned human-intervention checkpoint
→ technician disconnects the Endpoint entirely
→ HDD is physically replaced with an SSD (which may take hours or days)
→ Endpoint reconnects → same durable Endpoint recognized
→ fresh machine facts established → destructive target re-resolved
→ execution capacity reacquired → final safety gates revalidated
→ installation continues → restore
```

While the Endpoint is detached for the manual swap its downlink is free and another Endpoint
may be serviced on it.

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

Modeling it as either failure or reconciliation would drag in machinery built for those
cases (redispatch decisions, `StatusQuery`, explicit-operator-`Indeterminate` handling) and
would misrepresent a healthy workflow as broken.

### 2. Endpoint exclusivity and automated-execution capacity are separate authorities

They answer different questions and have different lifetimes:

| Concern | Question | Held from → until |
|---|---|---|
| **Endpoint exclusivity** (Job-scoped) | May another conflicting Job operate on this same Endpoint? | Job admission → Job terminal/cancellation |
| **Automated-execution capacity** (generic commercial slot) | Is this Job consuming one of the finite concurrent automated-execution slots right now? | Execution admission → checkpoint park **or** Job terminal |
| **Attempt-scoped / runtime resource leases** (network, storage, CPU/Worker) | Is a runtime resource reserved for a live Attempt? | Acquired before final pre-dispatch → released at Attempt terminal |

A Job parked at a checkpoint **retains Endpoint exclusivity** (its service context is still
in progress; no second Job may be admitted against that Endpoint), **releases the
automated-execution capacity slot** (nothing automated is executing, so nothing is consumed;
another Endpoint may use it), and **holds no Attempt-scoped lease** (the pre-checkpoint
Attempt already reached a terminal state). Collapsing exclusivity and capacity into one
authority — the pre-intervention model — would either strand a paid concurrency slot on a
machine sitting on a bench with its case open, or force exclusivity to be dropped and let a
second Job act on a half-serviced machine.

### 3. Continuation reacquires a concurrency slot; it is not a new commercial authorization

Resuming automated execution past the checkpoint requires the same durable Endpoint to be
recognized, a recorded operator intervention decision, freshly re-established machine facts,
destructive-target re-resolution where applicable, reacquisition of an automated-execution
slot, and the full final pre-dispatch safety gate. The detailed observable requirements are
owned by `m0-job-lifecycle-and-scheduling.md` "Planned intervention checkpoint" and are not
restated here.

Two points are decisions, not mechanics:

- **A bare operator acknowledgement is never sufficient for destructive continuation.** A
  click asserts intent, not physical reality: it cannot establish that the disk was
  installed, is the right size, is healthy, or is the only plausible target. The machine
  facts must be re-observed and the destructive gate re-run.
- **Reacquiring a slot is a concurrency/readmission operation, not a new commercial
  authorization of the Job.** Parking does not revoke the commercial authorization the Job
  established at admission. ADR-0015 §11 already separates "new gated admission" from
  "continuation of already-authorized work" and shields the latter from entitlement expiry,
  later-discovered invalidity, and commercial-platform unavailability; post-checkpoint
  readmission of a parked Job is in that "continuation" category. The parked Job still waits
  for a free slot, never preempts currently executing work, and — while a currently valid
  capacity policy exists — waits until its continuation fits within that policy. A valid
  capacity reduction delays continuation; it never fails the parked Job. Whether an
  implementation must retain the last valid policy for already-authorized work, and how, is
  implementation-time and not decided here.

### 4. Intentional hardware replacement does not create a new Endpoint

Replacing an HDD/SSD changes current boot, authenticated Agent session, inventory revision,
hardware confidence, and the current destructive target identity. It does **not** change
durable Endpoint identity, and it does not lose correlation with the parked Job. Disk
identity and inventory are evidence, never the durable Endpoint identity or a trust anchor
(`AGENTS.md` "Safety"; ADR-0004). Artifact source provenance bound to earlier captured data
is immutable and keeps describing the pre-replacement source (source provenance and
destructive target identity are already independent — ADR-0008). Any destructive-target
authorization resolved against the pre-replacement disk is therefore stale and cannot be
reused; the target is re-resolved and revalidated against current hardware before any
destructive Attempt.

### 5. Relationship to ADR-0015 — capacity-accounting semantic only

This ADR **partially supersedes ADR-0015 §6**: the equivalence between commercial capacity
and the count of granted Job-scoped endpoint-exclusivity leases (Jobs in `Running` or
`Cancelling`) no longer holds. The generic capacity unit becomes:

> the maximum number of Endpoints concurrently admitted to automated execution

A Job blocked only by capacity — at first admission or at post-checkpoint readmission —
waits, composing with the existing queue semantics (ADR-0015 §11 "Capacity exceeded").

**Every other ADR-0015 decision is unaffected and is not reopened:** Core knows no
SKU/edition/customer/contract vocabulary (§1, §5); the commercial platform translates
business concepts into generic capacity/capability facts (§3, §4); verification is
offline-capable and fail-closed, and platform connectivity is never itself a continuation
gate (§9, §11); the commercial platform is never required in the destructive hot path (§11);
already-authorized work is never terminated because entitlement availability changes (§11).
Capacity remains a generic numeric policy the Scheduler receives without commercial
vocabulary. ADR-0015 keeps its `Accepted` status; §6 carries a revision note recording this
partial supersession.

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
  provide. This was an earlier review position and is explicitly withdrawn.
- **Model the checkpoint as `AwaitingReconciliation`.** Rejected: conflates a deliberate,
  healthy pause at a known-good boundary with lost-certainty recovery of a live Attempt.
- **Model the checkpoint as `Failed`; the operator creates a new Job to continue.** Rejected:
  loses workflow correlation, the verified backup Artifact linkage, and the resolved plan;
  forces the operator to reconstruct intent; and a "continue" that is really "start over" is
  error-prone immediately before a destructive step.
- **A durable `Operation` aggregate owning the parked/resume lifecycle.** Rejected: ADR-0019
  already rejected a durable Operation aggregate on the same test (no genuine invariant above
  individual Jobs). Parking and resume are per-Endpoint execution-layer state.
- **Release Endpoint exclusivity while parked so the slot is fully free.** Rejected: a second
  Job could then be admitted against an Endpoint whose service context is half-complete,
  violating "Bamep does not interleave active Jobs against one Endpoint".
- **Re-evaluate the Job's commercial authorization on resume.** Rejected: a machine
  half-way through an already-authorized service would be stranded by an entitlement that
  expired or became unreachable while a technician was swapping a disk — contradicting
  ADR-0015 §11's protection of already-authorized work.
- **Reacquire nothing on resume; continue automatically once the operator clicks continue.**
  Rejected: a burst of parked Jobs reconnecting could exceed the concurrent-execution limit
  the instant their operators click, and a click cannot establish the post-swap machine
  facts the destructive gate requires.

## Consequences

- `docs/specifications/m0-job-lifecycle-and-scheduling.md` owns the "Planned intervention
  checkpoint" behavior, the corrected "Job admission and capacity" wording, and the
  parked-Job continuation rule; its capacity wording no longer equates capacity with the
  endpoint-exclusivity lease.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` owns the "Planned hardware
  replacement" clarification: the same Endpoint is preserved, and stale destructive-target
  authorization is not reused.
- ADR-0015 §6 carries a revision note recording the partial supersession; ADR-0015 keeps its
  `Accepted` status and its original decision text is preserved.
- No code changes. ADR-0015 capacity gating is not implemented, and the intervention
  checkpoint is not implemented; this is a durable-decision and Specification change only.
- Bulk cancellation (Application fan-out keyed by `submission_id`), operator-submission
  correlation, the seven-item destructive gate, and every Job/JobStep/Attempt state and
  transition are unchanged.
- Left open: the exact durable representation of the parked condition; JobStep semantic
  classification; and all Selective-backup questions — see the Issue #45 Discovery.

## Related architecture

- ADR-0006 — Job/JobStep/Attempt state model and Job-scoped Endpoint exclusivity; the
  execution authority this decision builds on, not reopened.
- ADR-0015 — commercial entitlement boundary; §6's capacity-accounting semantic is the only
  part this decision partially supersedes; §11's continuation-of-already-authorized-work
  separation is relied on directly by §3.
- ADR-0019 — operator submission boundary; the composition boundary and the rejection of a
  durable `Operation` aggregate, both preserved.
- ADR-0004 / ADR-0008 — Endpoint identity independent of hardware, and source-vs-target
  identity independence, both relied on directly by §4.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — normative owner of the checkpoint
  behavior, the capacity-admission rule, and the parked-Job continuation rule (WHAT).
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — normative owner of Endpoint
  identity continuity and the destructive-operation preconditions (WHAT).

## Related work

- Issue #45 — the Discovery that produced these conclusions.
- The planned narrow NTFS / Selective Technical Spike — still required before any Selective
  Specification delta; not blocked by, and does not block, this ADR.
- Issue #44 — the next M2 UX Work Package; not edited by this ADR.

Status: Accepted.
