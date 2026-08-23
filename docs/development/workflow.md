# Development Workflow

## Purpose

This document defines how **approved Bamep work is executed** from `Ready` through owner
acceptance.

It does not define how work is discovered, specified, classified, or decomposed, and it
does not define the testing strategy.

Related authorities:

- `docs/development/sdd.md` — Discovery, Specification, approval, decomposition, and the
  boundary for work becoming `Ready`;
- `docs/development/testing.md` — test layers, validation selection, isolation, Simulator,
  and Integration Environment policy;
- `docs/development/documentation-policy.md` — where durable information belongs;
- `docs/decisions/README.md` — ADR lifecycle;
- `AGENTS.md` — mandatory repository, safety, Git, and publication guardrails.

The workflow exists to execute approved decisions reliably without hidden scope or
architecture changes.

## Operational flow

Approved work follows:

```text
Ready
  ↓
In Progress
  ↓
Validation
  ↓
Done
```

A failed validation returns the affected work to `In Progress`.

GitHub Projects own the operational status. Repository documentation must not mirror the
changing state of individual work items.

## Ready

`Ready` means the SDD boundary has been satisfied and another execution session can start
without reconstructing intent from conversation history.

The criteria for becoming `Ready` belong to `docs/development/sdd.md`.

Execution must not begin when a material requirement, dependency, architecture question,
or safety question is still unresolved unless the approved work is itself the investigation
that resolves it.

## Starting execution

Before editing:

1. identify the approved GitHub work item;
2. read its scope, out of scope, acceptance criteria, dependencies, and validation
   obligations;
3. follow its links to authoritative Specifications, ADRs, Architecture, and Reference
   evidence;
4. inspect the current implementation and relevant tests;
5. verify that repository reality has not invalidated an assumption in the approved work.

If the approved context is insufficient or contradictory, do not guess. Return the
affected question to the appropriate SDD stage.

## In Progress

`In Progress` means active execution of approved scope.

During execution:

- implement only the approved result;
- preserve established architecture and dependency boundaries;
- preserve unrelated working-tree changes;
- make the smallest coherent change that advances the approved work;
- add or update validation required by the affected behavior;
- record meaningful deviations instead of silently changing the plan;
- report useful out-of-scope findings separately;
- keep `main` in a known-good state when working directly on it.

Do not silently add unrelated:

- cleanup;
- refactoring;
- dependencies;
- formatting;
- release work;
- architecture changes;
- future capabilities.

Execution may include code, documentation, tests, Simulator work, a Technical Spike, or
another artifact when that is the approved work.

## Checkpoints

A checkpoint is a bounded execution or review step inside an already-approved Work Package.

Use checkpoints when one Work Package needs several focused implementation or review steps
to remain understandable and resumable.

A checkpoint:

- is execution granularity, not planning hierarchy;
- does not become a Feature, Work Package, or GitHub Issue by default;
- is not a new approval boundary when it remains inside approved scope;
- is not a GitHub Project status;
- may span one focused development or agent session;
- may result in one coherent commit when useful.

Do not create checkpoint ceremony merely to subdivide straightforward work.

If a checkpoint discovers new scope, a new architectural decision, or a new safety
requirement, stop the affected execution and return to SDD.

## New decisions or scope discovered during execution

Execution must not silently become the place where architecture or requirements are
invented.

When a material issue appears:

```text
execution
   ↓
new requirement / architectural decision / safety question
   ↓
stop affected work
   ↓
return to Discovery / Spike / Specification / ADR as appropriate
   ↓
owner approval
   ↓
resume execution
```

Unrelated execution may continue only when it does not depend on the unresolved question.

Accepted ADRs remain constraints until explicitly reconsidered through the ADR process.

## Safety-sensitive execution

For destructive, privileged, identity, authentication, trust, or recovery-sensitive work:

- consume the safety contract from the authoritative Specification or approved work item;
- fail closed when required preconditions or authoritative state are unavailable;
- do not weaken a safety invariant to make later execution or testing easier;
- do not assume generic retry makes destructive work replay-safe;
- use safe test boundaries whenever real destructive execution is unnecessary;
- use the Integration Environment when the required behavior cannot be represented
  faithfully through local automation or simulation.

Detailed validation and safe-target selection belong to
`docs/development/testing.md`.

Real destructive operations remain subject to the explicit authorization rules in
`AGENTS.md`.

## Validation during execution

Relevant automated validation is part of execution completeness.

Use `docs/development/testing.md` to select the appropriate test layer and environment.

Operationally:

1. run the narrowest relevant validation first;
2. investigate unexpected failures;
3. correct failures caused by the current work;
4. broaden validation according to affected scope and risk;
5. record only checks that actually ran and their actual results.

A work item may move from `In Progress` to `Validation` when:

- approved execution scope is complete;
- required automated validation has been performed as far as the current environment
  permits;
- no known required automated validation is failing because of the current work;
- remaining owner-manual or Integration Environment checks are explicit;
- known limitations or deviations are reported.

Do not describe intended validation as completed validation.

## Validation

`Validation` is the owner acceptance stage.

Before handoff, provide the owner with the information needed to validate the result
without reconstructing the execution session.

Report, as applicable:

- what changed;
- acceptance criteria addressed;
- automated validation actually performed and results;
- known limitations or unresolved non-blocking findings;
- exact manual checks remaining;
- Integration Environment requirements;
- relevant deviations from the approved plan.

Agents may define manual validation procedures.

Agents must not claim that owner validation has been completed on the owner's behalf.

If owner validation finds a defect or unmet acceptance criterion:

```text
Validation
    ↓
In Progress
    ↓
correction + relevant validation
    ↓
Validation
```

## Done

`Done` means the owner accepted the work.

Before completion, ensure that durable information produced by execution is stored at the
correct authority according to `docs/development/documentation-policy.md`.

The GitHub work item should preserve only useful execution/outcome context, such as:

```text
Outcome
- delivered or validated result;
- relevant deviation from the approved plan.

Validation
- relevant checks and actual results;
- owner validation result when provided.

Follow-up
- unresolved or separately materialized work, if any.
```

Do not reproduce code diffs, conversation transcripts, or full copies of durable
Specifications/ADRs in the outcome.

Parent work is complete only when its own acceptance criteria and required child work are
complete.

## Session handoff

Unfinished work must remain resumable without relying on conversation history.

Before ending an execution session, update the existing authoritative operational source
when needed with:

- what is complete;
- what remains;
- current validation state;
- relevant evidence or failure information;
- blockers;
- newly discovered questions that require SDD.

Prefer the existing GitHub Issue or other established authority.

Do not create a separate handoff document when the current work item can carry the needed
execution state.

## Branch model

For the current owner-driven, sequential Bamep workflow, approved work may be performed
directly on `main` when that is the simplest safe path.

Working directly on `main` requires small coherent changes and a known-good repository
state. Do not intentionally leave `main` broken between checkpoints.

Use a branch when isolation provides real value, such as:

- parallel work;
- risky or broad changes;
- discardable Technical Spike experiments;
- external contributions or pull requests;
- work the owner explicitly asks to isolate.

When a branch is used, use the established prefixes:

```text
feature/<name>
fix/<name>
refactor/<name>
spike/<name>
docs/<name>
```

Do not create a branch per Work Package or checkpoint unless isolation justifies it.

Git operations and publication remain owner-controlled according to `AGENTS.md`.

## Commit strategy

Prefer small, coherent commits that leave the repository understandable and, when
applicable, validated.

Use Conventional Commits.

Examples:

```text
feat(agent): add enrollment handshake
fix(scheduler): release lease after cancellation
refactor(jobs): separate transition validation
test(protocol): cover duplicate acknowledgement
docs(workflow): clarify execution handoff
```

One coherent implementation change and its directly related tests may share a commit.

Use a separate `test(...)` commit when the testing work has independent value.

Do not split or combine commits merely for ceremony.

Commit messages summarize the change. Detailed execution history belongs in the GitHub
work item when it is useful.

Agents may suggest commit messages but must not commit, push, merge, tag, or publish unless
explicitly authorized.

## Review

Review compares the executed result against its authoritative inputs and acceptance
boundary.

Prioritize findings involving:

- correctness;
- data loss or destructive-operation safety;
- identity, authorization, and trust;
- protocol interoperability;
- persistence or recovery correctness;
- stale state;
- regressions;
- architectural violations;
- missing failure handling;
- missing required validation;
- unintended coupling;
- scope expansion.

Review is read-only unless corrections are explicitly requested.

## Reduced workflow

Work approved through reduced SDD still uses the same execution principles.

For a small isolated change, the operational path may be:

```text
Ready
  ↓
In Progress
  ↓
proportional validation
  ↓
Validation, when owner acceptance is material
  ↓
Done
```

Reduced workflow does not bypass:

- approved scope;
- architecture constraints;
- safety requirements;
- relevant validation;
- owner-controlled Git and publication rules.

If meaningful ambiguity, architecture impact, or safety risk appears, return to normal SDD.

## Guiding rule

Execution should consume approved decisions, not create hidden ones.

Keep work small enough to review and resume, record actual validation, and return to SDD
whenever execution discovers a material question outside the approved boundary.
