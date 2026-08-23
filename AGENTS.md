# AGENTS.md

## Purpose

Mandatory repository-wide guardrails for any AI agent working on Bamep.

Detailed process and authority live in:

- `docs/development/sdd.md` — pre-execution specification and approval;
- `docs/development/workflow.md` — approved work execution and handoff;
- `docs/development/testing.md` — validation strategy and environments;
- `docs/development/documentation-policy.md` — documentation authority and retirement;
- `docs/decisions/README.md` — ADR lifecycle;
- `docs/architecture/` — current implemented architecture;
- `docs/specifications/` — approved normative behavior.

Tool-specific instructions belong in tool-specific files such as `CLAUDE.md`.
`README.md` is public product documentation, not an agent instruction source.

## Repository truth

Never guess repository state.

Before a material change:

- inspect relevant implementation and nearby tests;
- read only the documentation needed for the task;
- inspect applicable Specifications, Architecture, and ADRs;
- verify paths, commands, configuration, dependencies, and conventions from the repository;
- distinguish implemented behavior from planned or approved-but-unimplemented behavior;
- surface contradictions instead of silently reconciling them.

Do not invent files, APIs, requirements, commands, dependencies, conventions, validation
results, or implementation status.

Code and tests are final evidence for currently implemented behavior. Conversation context
must not be the sole durable source of project knowledge.

## Scope and approval

Follow the approved work boundary and `docs/development/sdd.md`.

- Do not silently expand a Work Package, Fix, Refactor, Spike, or reduced-SDD task.
- Do not implement unrelated cleanup.
- Do not turn an implementation detail into a new requirement.
- Do not establish a durable architectural decision silently through code.
- If execution exposes a material out-of-scope requirement, architectural decision, or
  safety question, stop the affected work and surface it.

Use `docs/development/workflow.md` for execution and handoff.

## Repository protection

Preserve existing work.

- Never discard, overwrite, revert, reset, or reformat unrelated changes.
- Inspect a file before replacing or deleting it.
- Do not modify generated/vendor/build/local-configuration files unless required by scope.
- Prefer changing the responsible source or generator.
- Never expose, persist, print, or commit secrets, credentials, signing keys, tokens, or
  private environment values.
- Never weaken checks, warnings, security controls, or tests merely to make work pass.

Inspect ambiguous repository state rather than assuming a clean tree or branch.

## Architecture and dependencies

- `docs/architecture/` describes implemented architecture only.
- Specifications own approved normative behavior; ADRs own accepted decision rationale.
- Preserve accepted dependency and responsibility boundaries.
- Do not inherit stacks, protocols, directories, runtime boundaries, or patterns from FORGE,
  Pascoal, or another project without a Bamep requirement/decision.
- Before adding a dependency, confirm existing capabilities are insufficient and justify its
  maintenance, deployment, security, runtime, and operational cost.

## Safety

Bamep can modify or destroy endpoint data and operating-system installations. Safety takes
precedence over implementation convenience.

- Never weaken identity, inventory, authorization, trust, integrity, or destructive-operation
  safeguards.
- Destructive behavior must consume authoritative safety preconditions from the applicable
  Specification.
- Missing or stale authoritative safety state must fail closed where required.
- A MAC address is inventory evidence, not authentication or permanent Endpoint identity.
- Unrestricted remote shell execution must not replace approved typed Agent actions.
- Generic retry must not be assumed safe for destructive work.
- Automated tests must use safe fakes, fixtures, temporary storage, Simulator scenarios, or
  disposable devices where faithful.

Do not execute real filesystem destruction, partitioning, formatting, deployment, restore,
or equivalent destructive mutation without explicit owner authorization for the exact
environment and target.

Physical/destructive validation that cannot be represented safely belongs to the Integration
Environment defined by `docs/development/testing.md`.

## Development and validation environment

The physical Bamep server/lab is an Integration Environment, not a mandatory development
environment.

Most development must remain possible without physical PXE infrastructure, MikroTik
hardware, real client endpoints, or production storage.

Linux is the Server production target/reference environment. Fakes, containers, WSL2,
temporary storage, and Simulator scenarios prove only the behavior they faithfully model;
they do not prove physical firmware, PXE, Secure Boot, NIC, storage-controller, or WinPE
behavior.

## Git and publication

The repository owner controls Git and publication.

Read-only inspection is allowed when relevant. Unless explicitly authorized for the current
task, do not mutate the working tree/index, branches/tags, commit history, remotes,
synchronization state, pull requests, releases, publication state, or GitHub Project state.

Implementation/review/testing/documentation requests do not implicitly authorize staging,
committing, branch creation, merge/rebase/reset/stash, pull/push, tags, releases, or
equivalent GitHub mutations.

Suggest a Conventional Commit message when useful; do not execute the commit without
authorization.

## Validation integrity

Follow `docs/development/testing.md`.

- Never claim a test, build, lint, check, Spike, Simulator scenario, or Integration
  Environment validation passed unless it actually ran.
- Report failures rather than hiding them.
- Do not skip/weaken tests, add retries, or extend timeouts merely to mask failure.
- Distinguish current-change failures, pre-existing failures, missing prerequisites, and
  environment limitations when evidence permits.
- Report only validation actually performed.
- Never claim owner manual validation on the owner's behalf.

Tests demonstrate implementation behavior; they do not create missing product requirements.

## Documentation and language

Follow `docs/development/documentation-policy.md`: one durable fact should have one
authoritative owner; reference rather than duplicate.

Canonical repository engineering content is English.

User-facing text must use localization boundaries. Product locale requirements are owned by
`docs/specifications/m0-stack-and-boundaries-baseline.md`; do not duplicate locale values
here.

Academic/TCC material may be maintained separately in Brazilian Portuguese but must not
become a competing authoritative engineering copy.

## Final response after repository work

Report:

- what changed and which files changed;
- validation actually performed and results;
- relevant limitations/manual checks;
- relevant out-of-scope findings without implementing them;
- one suggested Conventional Commit message when appropriate.

If no files changed, say so.

## Instruction precedence

When project instructions conflict:

1. safety, data protection, and prevention of destructive operations;
2. explicit owner instructions for the current task;
3. this `AGENTS.md`;
4. tool-specific repository instructions;
5. relevant project documentation;
6. established implementation patterns.

Restricted operations require explicit, specific, task-limited authorization; never infer it
from adjacent requests.
