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
  automated-execution capacity; safe continuation after intentional hardware replacement.**
  → ADR-0020 (WHY, `Proposed`);
  `docs/specifications/m0-job-lifecycle-and-scheduling.md` "Planned intervention checkpoint"
  and "Job admission and capacity" (WHAT);
  `docs/specifications/m0-endpoint-identity-lifecycle.md` "Planned hardware replacement"
  (WHAT).
- **ADR-0015 §6 capacity-measurement semantic** is revised by ADR-0020; all other ADR-0015
  commercial-boundary decisions are unchanged.

## Unresolved investigation this document still owns

These are **not** decided and must not be turned into contracts without the narrow NTFS
Technical Spike and explicit owner approval:

1. **Typed JobStep classification.** Whether `JobStep` needs a durable "kind"
   (backup-volume / backup-selective / os-install / debloat / driver-install / restore /
   validate / operator-intervention) and per-kind precondition/postcondition hooks, and how
   that is represented. Runtime `Skipped` / `NotApplicable` semantics are explicitly out of
   scope for now — the working hypothesis is that non-applicable steps are simply absent from
   a target's Job (resolved at creation), not skipped at runtime.
2. **Cross-step preservation-sufficiency policy.** When a backup is composed of required and
   optional preservation groups, which layer owns the fail-closed decision "may the
   destructive step proceed given these results". Working hypothesis: a precondition on the
   destructive JobStep, additive to the seven-item gate, never narrowing it. No bypass flow
   exists in current authority; do not invent one.
3. **Selective backup model.** Volume/Image vs Selective as JobStep kinds rather than a
   boolean; Artifact granularity (candidate: one Artifact per operator-meaningful preservation
   group); the resolved selective-capture-set descriptor (operator request, discovery
   evidence + timestamp, resolved roots with required/optional flags, Artifact mapping);
   same-offline-session / stale-selection detection.
4. **Assisted discovery.** Working principle only: suggestion is advisory; explicit operator
   inclusion overrides any exclusion heuristic; file extension alone is never preservation
   authority. Its authorization timing (a distinct non-destructive discovery submission
   preceding the destructive one) is a future boundary question after the Spike.
5. **Selective restore correlation and NTFS specifics** — restore destination/SID mapping,
   path representation for an offline filesystem, reparse points / ACL / ADS / EFS behavior,
   size estimation accuracy, final backup format, and the re-observed physical disk identity
   model (WWN/serial/GPT) — all deferred to the Spike / a later Work Package.

## Recommended next step for the Selective branch

**One narrow Technical Spike — offline NTFS selective discovery/capture feasibility.** It
bounds what the resolved-capture-set descriptor and a Selective Artifact contract can safely
promise; it is **not** a production heuristic catalog and **not** a restore engine. The
composed-service composition model and the intervention checkpoint do not depend on it.

## #44

Unchanged product conclusion: #44 remains the next M2 UX Work Package under Option B — same
post-submit per-Endpoint result invariant (one submitted request → independent per-Endpoint
creation outcomes, no aggregate success/failure), with a representative composed
reinstall/service mock replacing `Capturar imagem do sistema`, and not depending on
unresolved Selective mechanics. No repository document needs the full prototype scenario.
