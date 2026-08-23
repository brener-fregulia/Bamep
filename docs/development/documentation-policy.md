# Documentation Policy

## Purpose

This document defines where Bamep information is authoritative, how durable knowledge
moves between documentation layers, and when material should leave the repository HEAD.

It does not define the full engineering lifecycle or execution workflow. Those belong to:

- `docs/development/sdd.md` — Discovery, Specification, approval, and work decomposition;
- `docs/development/workflow.md` — execution workflow and handoff;
- `docs/development/testing.md` — testing and validation strategy;
- `docs/decisions/README.md` — ADR format and lifecycle.

## Core rule

Each durable fact should have one authoritative owner.

Prefer:

```text
one fact
→ one authoritative source
→ short references everywhere else
```

over independently maintained copies.

A document may provide local context for a fact owned elsewhere, but it must not become a
second normative or historical authority for that fact.

## Authority map

| Information | Authoritative source | Responsibility |
| --- | --- | --- |
| Implemented behavior | Code and tests | What the repository actually does |
| Public product overview | `README.md` | Project purpose, status, stable capabilities, requirements, and entry points |
| Mandatory contributor/agent guardrails | `AGENTS.md` | Repository-wide rules and pointers to detailed authorities |
| Unresolved investigation | `docs/discovery/` | Open questions, alternatives, uncertainty, and investigation context |
| Normative system behavior | `docs/specifications/` | Approved behavior, contracts, invariants, and requirements |
| Current implemented architecture | `docs/architecture/` | How the implemented system is structured and interacts now |
| Architectural decision rationale | `docs/decisions/` | Why a durable choice was made, alternatives, trade-offs, and consequences |
| Engineering process and conventions | `docs/development/` | Development policy and repository engineering conventions |
| Reusable empirical evidence | `docs/reference/` | Validated technical facts, experiments, compatibility findings, and limitations |
| Approved actionable work | GitHub Issues | Scope, acceptance criteria, dependencies, execution context, and outcome |
| Operational work state | GitHub Projects | Current workflow state |
| Milestone or release scope | GitHub Milestones | Grouping of approved work |

The repository is the permanent technical source of truth.

GitHub is the operational source of truth for approved and materialized work.

Conversation history is supplemental context only and must not be the sole source of
required project knowledge.

## Authority boundaries

### Specifications own WHAT

Specifications own normative system behavior: contracts, required states, transitions,
invariants, failure semantics, and other approved behavior that implementations must
satisfy.

ADRs, Issues, Architecture, Discovery, and Reference documents may point to those
contracts, but should not maintain independent normative copies.

A Specification may legitimately describe behavior not yet implemented. It must not be
presented as proof that the behavior already exists.

### ADRs own WHY

ADRs preserve durable architectural decisions, meaningful alternatives, trade-offs, and
consequences.

They should contain enough context to make the decision understandable, but should not
become a second Specification for detailed normative behavior already owned elsewhere.

Accepted ADRs are historical records. When a decision changes, preserve that history and
use the ADR lifecycle defined in `docs/decisions/README.md` rather than rewriting the past
to look current.

### Architecture owns HOW IT WORKS NOW

Architecture documentation describes implemented structure, boundaries, components,
data flows, deployment topology, persistence boundaries, and security boundaries when
those details remain useful beyond the code itself.

It must reflect repository reality. Planned or merely approved architecture belongs in
Discovery, Specifications, or ADRs until implemented.

Code and tests remain the final verification of implemented behavior. If Architecture
documentation disagrees with the implementation, the documentation is stale and must be
corrected.

### Reference owns EVIDENCE

Reference documentation preserves reusable empirical facts and evidence whose value
survives the work item that produced them.

Examples include tested hardware or firmware behavior, compatibility results, protocol or
tool constraints, reproducible experiments, negative findings, and validated limitations.

Reference evidence may justify an ADR or Specification, but it does not become a
requirement merely because it was observed.

### Discovery owns unresolved investigation

Discovery is for questions that are still materially unresolved and whose investigation
context is useful beyond one conversation.

It may contain facts, assumptions, alternatives, proposals, and uncertainty, but those
categories must remain distinguishable.

Discovery never overrides an approved Specification, accepted ADR, current Architecture,
or validated Reference evidence for the responsibility those sources own.

### GitHub owns work history

GitHub Issues own actionable work: scope, out of scope, acceptance criteria, dependencies,
validation expectations, execution context, and outcome.

Issues should link to durable repository authorities instead of copying their full
contracts or architectural reasoning.

GitHub Projects and Milestones own operational state and work grouping. Repository
documents must not mirror those changing states.

## Promotion and retirement

Durable conclusions should be promoted to the authority that owns them.

Typical promotion paths are:

```text
empirical finding
→ docs/reference/

architectural decision
→ docs/decisions/

approved normative behavior
→ docs/specifications/

implemented structure
→ docs/architecture/
```

After promotion, the source material should retain only information that still has a
unique responsibility.

Use these rules when deciding whether material remains in HEAD:

- resolved Discovery should be removed or reduced once all durable conclusions have been
  promoted and no unresolved investigation remains;
- Technical Spike execution history may remain in its GitHub Issue; permanent repository
  material is justified only by reusable evidence or another durable authority;
- Reference material should remain when it preserves unique evidence, environment details,
  negative results, limitations, or reproduction context that would be costly to recover;
- accepted and superseded ADRs remain historical records according to the ADR policy;
- Architecture documentation should be updated or replaced as implementation changes;
- milestone Specifications may be retired only after their durable normative content has
  authoritative homes elsewhere and no active work still depends on them;
- closed Issues remain the work-history record and do not require a duplicate repository
  report.

Git history preserves file evolution. A document does not need to remain in HEAD solely to
prove that it once existed.

## Avoiding competing authority

Before adding or repeating information, ask:

1. What kind of information is this?
2. Which source owns that kind of information?
3. Does an authoritative source already exist?
4. Can this document use short context plus a link instead of another full copy?
5. Will this information remain useful after the current work item?
6. Am I preserving durable knowledge or merely copying execution history?

Do not independently maintain the same requirement, decision, implemented-state claim, or
empirical conclusion across multiple documentation categories.

When two documents disagree, resolve the disagreement at the correct authority. Do not
make both copies longer in an attempt to reconcile them.

## When permanent documentation is justified

| Change | Typical durable owner |
| --- | --- |
| Internal implementation with no durable external effect | No new permanent document |
| New or changed normative behavior | Specification |
| New architectural decision | ADR |
| Meaningful implemented architecture change | `docs/architecture/` |
| Reusable empirical or compatibility finding | `docs/reference/` |
| Development process or convention change | `docs/development/` |
| Public capability, requirement, or stable entry point | `README.md` when appropriate |
| Approved actionable work | GitHub Issue |
| Unresolved investigation worth preserving | `docs/discovery/` |

Do not update every documentation category for every change.

## Language

Canonical repository documentation is written in English, including Discovery,
Specifications, Architecture, ADRs, development documentation, Reference material, and
GitHub work items.

User-facing localization is independent from repository documentation language.

Academic or TCC material may be maintained separately in Brazilian Portuguese, but it
must not become a competing authoritative copy of engineering documentation.

## Validation

For documentation changes, verify as applicable that:

- referenced paths and links exist;
- terminology is consistent;
- claims match the authority that owns them;
- implemented-state claims match repository reality;
- planned behavior is not presented as implemented;
- ADR status and history are preserved;
- reusable evidence is not discarded while removing execution history;
- GitHub operational state is not duplicated into permanent documentation;
- required context is not left only in conversation history.

Documentation-only changes normally do not require product test suites unless they modify
executable examples, generated documentation, configuration, schemas, or another testable
artifact.

## Guiding rule

Store information where its future reader will expect its authority to live.

If the same fact is fully maintained in several places, the documentation model is wrong.
