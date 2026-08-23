# AGENTS.md

## Purpose

This file defines mandatory repository-wide guardrails for any AI agent working on Bamep.

It is not a second SDD, workflow, testing, architecture, or documentation handbook.

Detailed authorities:

- `docs/development/documentation-policy.md` — documentation ownership, promotion, and
  retirement;
- `docs/development/sdd.md` — Discovery, Specification, approval, and work decomposition;
- `docs/development/workflow.md` — execution from `Ready` through owner acceptance;
- `docs/development/testing.md` — testing and validation strategy;
- `docs/decisions/README.md` — ADR lifecycle;
- `docs/architecture/` — current implemented architecture;
- `docs/specifications/` — normative approved behavior and contracts.

Tool-specific instructions belong in files such as `CLAUDE.md` and `.claude/`.

`README.md` is public product documentation and must not be used as an agent instruction
source.

## Repository truth

Never guess repository state.

Before proposing or making a material change:

- inspect the relevant implementation and nearby tests when they exist;
- read only the documentation needed for the task;
- inspect applicable Specifications, Architecture, and ADRs;
- verify paths, commands, configuration, dependency versions, and conventions from the
  repository;
- report contradictions between the request, approved contracts, documentation, and
  implementation;
- distinguish implemented behavior from planned or approved-but-unimplemented behavior.

Do not invent:

- files or paths;
- APIs or protocol behavior;
- requirements;
- commands;
- dependencies;
- repository conventions;
- validation results;
- implementation status.

Code and tests are the final evidence for currently implemented behavior.

Conversation history may provide context but must not become the sole durable source of
project knowledge.

## Scope and approval

Follow the approved work boundary.

- Non-trivial planned implementation requires sufficient approved scope according to
  `docs/development/sdd.md`.
- Do not silently expand a Work Package, Fix, Refactor, Spike, or reduced-SDD task.
- Do not implement unrelated cleanup merely because nearby code could be improved.
- Do not turn an implementation detail into a new requirement without returning to SDD.
- Do not establish a durable architectural decision silently through code.
- If execution discovers a material requirement, architectural decision, or safety
  question outside approved scope, stop the affected work and surface it.

Use `docs/development/workflow.md` for execution and handoff rules.

## Repository protection

Preserve the user's existing work.

- Never discard, overwrite, revert, reset, or reformat unrelated changes.
- Inspect a file before replacing or deleting it.
- Do not modify generated files, vendored dependencies, build output, or local
  configuration unless the approved task requires it.
- Prefer changing the responsible source or generator rather than generated output.
- Do not expose, persist, print, or commit secrets, credentials, signing keys, tokens, or
  private environment values.
- Do not weaken checks, warnings, security controls, or tests merely to obtain a passing
  result.

When repository state is ambiguous, inspect it rather than assuming a clean tree or a
particular branch state.

## Architecture and dependencies

Consume existing architecture instead of reconstructing it from historical projects.

- `docs/architecture/` describes implemented architecture only.
- Approved future behavior belongs to Specifications and accepted decisions belong to
  ADRs.
- Preserve accepted dependency and responsibility boundaries.
- Before adding a dependency, confirm that the existing platform or workspace does not
  already provide the required capability.
- Introduce a dependency only when its maintenance, deployment, security, runtime, and
  operational costs are justified by the approved work.

Bamep must not inherit stacks, directories, protocols, runtime boundaries, or design
patterns from FORGE, Pascoal, or another project merely because they were previously used.

Historical projects may provide evidence or process lessons. Bamep requirements and
accepted decisions remain authoritative for Bamep.

## Safety

Bamep can modify or destroy endpoint data and operating system installations.

Safety takes precedence over implementation convenience.

Mandatory rules:

- never weaken identity, inventory, authorization, trust, integrity, or
  destructive-operation safeguards to make a workflow pass;
- destructive behavior must consume its authoritative preconditions and safety invariants
  from the applicable Specification;
- a MAC address is an inventory signal, not authentication and not permanent Endpoint
  identity;
- unrestricted remote shell execution must not replace approved typed Agent actions;
- generic retry must not be assumed safe for destructive work;
- missing or stale authoritative safety state must fail closed where the applicable
  contract requires it;
- automated tests must use safe fakes, fixtures, temporary storage, Simulator scenarios, or
  disposable devices when those can represent the required behavior faithfully.

Do not execute real filesystem destruction, partitioning, formatting, deployment,
restore, destructive storage mutation, or equivalent real-world destructive operations
without explicit and specific owner authorization for the exact environment and target.

Physical or destructive validation that cannot be represented safely in local automation
belongs to the Integration Environment according to `docs/development/testing.md`.

## Development and validation environments

The physical Bamep server and laboratory are an Integration Environment, not a mandatory
development environment.

Most development must remain possible without requiring physical PXE infrastructure,
MikroTik hardware, real client endpoints, or production storage.

Linux is the production target for Bamep Server and the reference environment for
Linux-specific behavior.

Use WSL2, containers, fakes, fixtures, temporary storage, and Simulator scenarios only
where they faithfully represent the responsibility being tested. Do not treat them as
proof of physical firmware, PXE, Secure Boot, NIC, storage-controller, or WinPE behavior.

Detailed environment and test-boundary policy belongs to
`docs/development/testing.md`.

## Git and publication

The repository owner retains control of Git and publication.

Read-only inspection such as status, diff, log, show, file inspection, and repository
search is allowed when relevant.

Unless explicitly and specifically authorized for the current task, do not perform
operations that modify:

- the working tree or index;
- branches or tags;
- commit history;
- remotes or synchronization state;
- pull requests;
- releases;
- publication state;
- GitHub Project state.

This restriction includes staging, committing, amending, checkout/restore that changes
files, branch creation, merge, rebase, reset, stash, pull, push, tag creation, release
publication, and equivalent GitHub mutations.

A request to implement, test, review, or document something does not implicitly authorize
Git or publication changes.

When useful, suggest a Conventional Commit message after a coherent change. Do not execute
the commit unless explicitly authorized.

## Validation integrity

Follow `docs/development/testing.md` for validation strategy and
`docs/development/workflow.md` for execution/handoff.

Mandatory reporting rules:

- never claim that a test, build, lint, check, Spike experiment, Simulator scenario, or
  Integration Environment validation passed unless it actually ran;
- report failures rather than hiding them;
- do not delete, skip, weaken, retry, or extend timeouts merely to mask a failure without
  understanding the cause;
- distinguish current-change failures, pre-existing failures, missing prerequisites, and
  environment limitations when evidence permits;
- report only the validation actually performed;
- never claim owner manual validation on the owner's behalf.

Tests demonstrate implementation behavior. They must not invent missing product
requirements.

## Documentation

Follow `docs/development/documentation-policy.md`.

In particular:

- one durable fact should have one authoritative owner;
- use short context and links instead of maintaining competing copies;
- Specifications own normative behavior;
- ADRs own decision rationale and history;
- Architecture owns implemented structure;
- Reference owns reusable empirical evidence;
- Discovery owns unresolved investigation;
- GitHub Issues own actionable work and outcome history.

Do not preserve a document in HEAD solely because it is historical when Git history or the
GitHub work item already preserves that history and no unique durable responsibility
remains.

## Language

Use English for canonical repository engineering content, including source code,
identifiers, comments, documentation, ADRs, Specifications, Discovery, Reference material,
internal logs, schemas, protocol fields, domain events, and GitHub work items.

User-facing UI strings must use localization boundaries rather than scattered hardcoded
text.

Until the product-level localization requirement is promoted to its durable normative
owner, preserve the currently established direction:

- initial UI locale: `pt-BR`;
- planned additional locale: `en-US`.

Academic or TCC-facing material may be maintained separately in Brazilian Portuguese but
must not become a competing authoritative copy of engineering documentation.

## Final response after repository work

After changing repository files, report at minimum:

- what changed;
- files changed;
- validation actually performed and results;
- relevant limitations or manual checks still required;
- relevant out-of-scope findings without silently implementing them;
- one suggested Conventional Commit message when appropriate.

When no files were changed, say so clearly.

## Instruction precedence

When project instructions conflict, use this order:

1. safety, data protection, and prevention of destructive operations;
2. explicit owner instructions for the current task;
3. this `AGENTS.md`;
4. tool-specific repository instructions;
5. relevant project documentation;
6. established implementation patterns.

An operation restricted by these guardrails requires explicit, specific, task-limited
authorization. Do not infer that authorization from adjacent requests.
