# Discovery — M2 composite service workflow: unresolved Selective-backup and step-modeling questions

Status: **Discovery / investigation only — not approved, not normative.**

- Source question: Issue #45 — *[Discovery] Define M2 composite service workflow and
  operator-intervention model*.
- Investigation material under `docs/discovery/README.md`. It does not override an approved
  Specification or an accepted ADR.

## What has been promoted out of this Discovery

The stable, owner-approved conclusions now live in their durable owners; this document no
longer restates them:

- **No durable Domain `Operation` / `ServiceIntent` / `OperationPlan` / `WorkflowTemplate`
  aggregate.** A composed service is one Endpoint's Job with more ordered JobSteps, composed
  at the Application/submission boundary. Reaffirms ADR-0019.
- **Planned operator-intervention checkpoint; separation of Endpoint exclusivity from
  automated-execution capacity; parked work as already-authorized continuation across
  entitlement expiry/unavailability; safe continuation after intentional hardware
  replacement.** → ADR-0020 (WHY, Accepted);
  `docs/specifications/m0-job-lifecycle-and-scheduling.md` "Planned intervention checkpoint"
  and "Job admission and capacity" (WHAT);
  `docs/specifications/m0-endpoint-identity-lifecycle.md` "Planned hardware replacement"
  (WHAT).
- **ADR-0020 partially supersedes ADR-0015 §6's capacity-accounting semantic**; all other
  ADR-0015 commercial-boundary decisions are unchanged.

## Unresolved investigation this document still owns

None of the following is decided. They must not be turned into contracts without the relevant
work below and explicit owner approval.

### A. Does NOT depend on the NTFS Spike — resolve when a real provisioning action needs it

Through ordinary ADR / Specification work when the first concrete provisioning action forces
the question, not before:

- whether `JobStep` needs durable semantic classification at all;
- the exact `JobStep.kind` representation;
- `Skipped` / `NotApplicable` lifecycle semantics (working hypothesis: non-applicable steps
  are simply absent from a target's Job, resolved at creation, not skipped at runtime — but
  this is not fixed);
- general typed provisioning-action modeling (os-install, debloat, driver-install, restore,
  validate) and per-kind precondition/postcondition hooks;
- which layer owns the fail-closed "may the destructive step proceed given the backup
  results" decision (working hypothesis: a precondition on the destructive JobStep, additive
  to the seven-item gate, never narrowing it; no bypass flow — do not invent one).

### B. DOES depend on the NTFS Spike

The Spike bounds what a Selective contract can safely promise:

- offline Selective filesystem discovery (feasibility, tooling, reliability);
- Artifact granularity for Selective (candidate: one Artifact per operator-meaningful
  preservation group);
- resolved capture-scope evidence (operator request, discovery evidence + timestamp, resolved
  roots with required/optional flags, Artifact mapping);
- offline path representation (filesystem not mounted at a Windows drive letter);
- ACL / alternate data streams / EFS behavior in offline file-granular capture;
- reparse-point / junction / symlink traversal and capture semantics;
- Selective restore mapping onto a fresh Windows install (destination paths, profile/SID);
- stale-selection / same-offline-session evidence (source changed between discovery and
  capture);
- selected-data size estimation accuracy;
- feasible preservation metadata;
- assisted-discovery authorization timing (whether discovery is a distinct non-destructive
  submission preceding the destructive one). Working principle only: suggestion is advisory;
  explicit operator inclusion overrides any exclusion heuristic; file extension alone is never
  preservation authority.

### C. Does NOT belong to the NTFS Spike — future physical-hardware integration

- re-observed physical disk identity: WWN, serial, GPT / composite target identity.

## Recommended next step for the Selective branch

**One narrow Technical Spike — offline NTFS selective discovery/capture feasibility**, scoped
to group B above. It is **not** a production heuristic catalog and **not** a restore engine.
The composed-service composition model, the intervention checkpoint, and the capacity
decision do not depend on it.
