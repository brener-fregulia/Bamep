# Specification-Driven Development

## Purpose

Bamep uses Specification-Driven Development (SDD) to make important intent, constraints,
decisions, safety requirements, and acceptance boundaries explicit before execution.

SDD defines how work moves from an idea or unresolved problem to approved, materialized
work that is ready for execution.

It does not define the execution workflow itself.

Related authorities:

- `docs/development/documentation-policy.md` — where information is authoritative and how
  durable knowledge is promoted or retired;
- `docs/development/workflow.md` — how approved work is executed, validated, handed off,
  and completed;
- `docs/development/testing.md` — testing and validation strategy;
- `docs/decisions/README.md` — ADR format and lifecycle;
- `AGENTS.md` — mandatory repository-wide guardrails.

Use only as much process as necessary to preserve intent, evidence, scope, safety,
architectural reasoning, and continuity.

## SDD flow

The normal pre-execution flow is:

```text
Idea or problem
      ↓
Discovery, when needed
      ↓
Technical Spike, when evidence is insufficient
      ↓
Specification or scoped work definition
      ↓
Owner approval
      ↓
Work decomposition, when needed
      ↓
GitHub materialization
      ↓
Ready for execution
```

Not every change requires every step or artifact.

Once approved work is ready for execution, `docs/development/workflow.md` owns the
operational lifecycle.

## Discovery

Discovery is investigation, not implementation.

Use Discovery when the requested outcome, current behavior, constraints, architecture,
safety implications, or viable solution space are not yet understood well enough to define
approved work confidently.

Discovery should inspect, as relevant:

1. the requested outcome and explicit non-goals;
2. current implementation and nearby tests;
3. current Architecture documentation;
4. applicable Specifications and ADRs;
5. relevant Reference evidence and prior work;
6. affected responsibilities and boundaries;
7. safety and destructive-operation implications;
8. unresolved architectural questions;
9. evidence gaps that may require a Technical Spike;
10. expected validation obligations.

Do not invent requirements to fill gaps.

When evidence is incomplete, preserve the uncertainty explicitly rather than choosing a
solution silently.

Discovery may conclude that:

- a Specification can be proposed;
- a Technical Spike is required;
- an ADR question must be resolved;
- work can be defined directly with reduced SDD;
- no implementation work is currently justified.

Persistent Discovery material must follow
`docs/development/documentation-policy.md`. Resolved Discovery does not remain authoritative
after its durable conclusions have been promoted.

## Technical Spikes

A Technical Spike is a focused investigation used when empirical evidence is required
before a Specification or architectural decision can be completed confidently.

Typical examples include:

- boot or firmware viability;
- transfer resumability;
- Secure Boot or trust-chain behavior;
- hardware or driver compatibility;
- resource or throughput measurement;
- storage, network, or protocol behavior that cannot be resolved by inspection alone.

A Spike should define, as applicable:

- the question;
- why existing evidence is insufficient;
- constraints and assumptions;
- method or experiment;
- evaluation or success criteria;
- evidence collected;
- conclusion;
- remaining uncertainty.

Spike code and experiments are evidence gathering. They do not become accepted
architecture or production implementation merely because they work.

A completed Spike may feed a Specification, ADR, Reference document, or later work item.
Durable output placement follows `docs/development/documentation-policy.md`.

## Specification

A Specification defines approved intended behavior or constraints for a scope that needs a
durable normative contract.

Before owner approval, it is a proposal.

After owner approval, it is normative for its scope until changed through the appropriate
SDD process.

Useful sections may include:

- classification;
- context;
- current behavior;
- goal;
- scope;
- out of scope;
- functional or non-functional requirements;
- business rules;
- safety invariants;
- acceptance criteria;
- architecture impact;
- related ADRs;
- Technical Spikes or evidence;
- validation expectations;
- work decomposition;
- open questions.

Use only sections that add information.

Do not create empty RF, RNF, RN, architecture, or validation sections merely to satisfy a
template.

Requirements describe intended behavior or constraints. They should not encode incidental
implementation choices unless those choices are already accepted architectural
constraints.

Repository-level Specifications are justified when the contract has durable system value.
Routine work may be specified directly in GitHub when a second repository copy would add
no durable authority.

## Work classification

Use the smallest classification that accurately describes the work.

| Classification | Purpose |
| --- | --- |
| Epic | Optional grouping for a larger objective containing multiple related Features |
| Feature | Introduces one coherent product capability or behavior |
| Fix | Corrects behavior that does not match its intended or specified behavior |
| Refactor | Changes internal structure while preserving intended behavior unless explicitly specified otherwise |
| Technical Spike | Gathers evidence needed before a decision or Specification can be completed |
| Work Package | Smallest approved and tracked unit of delivery and acceptance |

Do not create hierarchy for ceremony.

A parent Feature, Fix, or Refactor may contain Work Packages when decomposition improves
scope control, review, validation, or continuity.

## Work decomposition

Decompose work only when smaller units create a clearer delivery and acceptance boundary.

A Work Package should normally represent **one concrete, reviewable result** with a
coherent acceptance boundary.

Split work when:

- independent outcomes can be delivered or accepted separately;
- one unit would span unrelated responsibilities;
- safety-critical work benefits from isolation;
- one unit depends on evidence or decisions another unit must produce first;
- validation would otherwise mix materially different concerns;
- the resulting work would be too broad to review or resume reliably.

Do not split merely because implementation may require several commits or sessions.

Do not combine transversal changes across many unrelated authorities into one Work Package
only because they share a general theme.

A checkpoint is an execution detail inside already-approved work. It is not a planning
unit, a new approval boundary, or a GitHub Issue by default. Checkpoint execution semantics
belong to `docs/development/workflow.md`.

## Work Package definition

A Work Package is the smallest approved project unit worth tracking independently through
execution and acceptance.

It should contain enough persistent context for another session to understand what must be
delivered without depending on the conversation that created it.

Useful sections include:

```text
Objective
Scope
Out of scope
Authoritative inputs / related architecture
Dependencies
Safety constraints
Acceptance criteria
Validation
Implementation notes, only when they constrain execution
```

Not every Work Package needs every section.

A Work Package should:

- have one coherent result;
- have explicit acceptance criteria;
- identify dependencies that materially block execution;
- identify authoritative Specifications, ADRs, Architecture, or Reference evidence rather
  than copying them;
- preserve safety constraints required for its own execution;
- state validation obligations relevant to acceptance;
- avoid absorbing unrelated future work.

Implementation detail that does not constrain the approved outcome should normally be left
to execution.

If a proposed Work Package still contains unresolved questions that can materially change
its scope or architecture, return to Discovery or create a Technical Spike instead of
pretending the work is ready.

## Architecture decisions

A durable architectural choice with meaningful alternatives, trade-offs, or long-term
constraints requires ADR handling.

During Discovery or Specification:

1. identify the architectural question;
2. inspect existing ADRs;
3. gather relevant alternatives and evidence;
4. use a Technical Spike when empirical evidence is required;
5. obtain owner approval;
6. create or update the ADR according to `docs/decisions/README.md`;
7. make Specifications and work items consume the accepted decision without duplicating its
   reasoning.

Do not establish architectural policy implicitly through a Specification detail, Work
Package, or implementation plan when the decision itself deserves an ADR.

Accepted ADR history must remain historically honest.

## Safety-sensitive specification

Bamep can modify or destroy endpoint data and operating system installations.

Discovery, Specifications, and Work Packages must explicitly surface safety implications
when scope involves, for example:

- disk preparation, partitioning, formatting, deployment, restore, or artifact deletion;
- Endpoint identity, enrollment, authentication, or trust;
- destructive storage mutation;
- privilege boundaries;
- retry or reconciliation of destructive work.

Define the safety contract at the authority that owns the behavior.

As applicable, specify:

- preconditions;
- identity and authorization assumptions;
- stale-state handling;
- interruption and reconciliation behavior;
- retry and cancellation semantics;
- integrity requirements;
- verification required before destructive execution;
- Integration Environment obligations where simulation cannot represent the risk.

Do not infer that a generic retry policy makes a destructive operation safe to replay.

## Validation expectations

SDD defines **what must be demonstrated**, not the detailed mechanics of running tests.

Specifications and Work Packages should identify validation obligations that are material
to acceptance, especially for:

- safety invariants;
- protocol interoperability;
- persistence and crash correctness;
- destructive-operation behavior;
- Simulator fidelity;
- Integration Environment behavior.

The testing layers and selection strategy belong to `docs/development/testing.md`.

The execution and recording of validation results belong to
`docs/development/workflow.md`.

Do not use tests to invent requirements that the approved contract does not define.

## Owner approval

Owner approval is the boundary between proposed work and approved work.

Approval is required before:

- beginning non-trivial planned implementation;
- treating a Specification as approved;
- accepting a significant architectural decision;
- materializing proposed work as approved execution scope;
- materially expanding or changing approved scope.

Approval may be explicit in conversation or represented by an approved persistent project
state.

Do not infer approval merely because an option was discussed.

If a material requirement, decision, or scope boundary changes after approval, surface the
change and return to the appropriate SDD stage.

## GitHub materialization

GitHub materialization converts approved work into operational work items.

Materialize only approved work.

GitHub Issues may represent Features, Fixes, Refactors, Technical Spikes, and Work
Packages. Parent items group work; child Work Packages carry concrete delivery and
acceptance boundaries.

An Issue owns actionable scope, acceptance, dependencies, execution context, and outcome.
It should link to durable repository authorities instead of copying their full contracts
or architectural reasoning.

GitHub Projects own operational status. GitHub Milestones own milestone or release
grouping.

The detailed status lifecycle begins once work is ready and belongs to
`docs/development/workflow.md`.

## Ready boundary

SDD is complete for a work item when another execution session can start without guessing
about intent or relying on prior conversation history.

Before work is considered ready, confirm that:

- scope and out of scope are sufficiently explicit;
- acceptance criteria are reviewable;
- authoritative inputs are identifiable;
- blocking architectural decisions are resolved or explicitly isolated;
- required evidence exists or is delegated to a prior Spike;
- safety constraints are explicit where applicable;
- validation obligations are known;
- material dependencies are identified.

If those conditions are not met, the work is not ready for execution.

## Reduced SDD

Small, isolated, low-risk work may use a reduced process when intent and scope are already
clear and there is no meaningful architecture, safety, or evidence gap.

Examples include:

- typo corrections;
- broken links;
- minor documentation corrections;
- trivial configuration maintenance;
- small reproducible fixes with obvious scope and no architectural impact.

A reduced path may be:

```text
scope confirmation
→ owner approval when required
→ execution
```

Execution and validation still follow `docs/development/workflow.md` and
`docs/development/testing.md`.

Reduced SDD must not bypass:

- meaningful scope ambiguity;
- safety analysis;
- architectural decisions;
- significant requirements;
- required empirical investigation;
- destructive-operation constraints.

If any of those appear, return to the normal SDD path.

## Guiding rule

SDD exists to make the important questions explicit **before** execution becomes the place
where hidden decisions are made.

Use enough structure to preserve intent, evidence, scope, architecture, safety, and
acceptance.

Do not produce documents or hierarchy that have no unique authority or project value.
