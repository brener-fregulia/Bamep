# Testing

## Purpose

This document defines the Bamep testing and validation strategy.

It owns:

- test layers and their intended responsibilities;
- validation selection according to behavior and risk;
- test isolation requirements;
- use of fakes, Simulator, and the Integration Environment;
- safety-sensitive validation principles;
- regression and failure-handling policy;
- coverage policy.

It does not define project workflow states, owner handoff, or work completion.

Related authorities:

- `docs/development/sdd.md` — validation obligations required by approved work;
- `docs/development/workflow.md` — execution of validation, recording actual results, and
  owner acceptance;
- `docs/development/documentation-policy.md` — placement of reusable evidence;
- `AGENTS.md` — mandatory repository and destructive-operation guardrails;
- `docs/specifications/m0-simulator-contract-and-validation-strategy.md` — normative
  Simulator fidelity, required scenarios, and M0 validation obligations.

Concrete test commands and tooling must be derived from the current repository rather than
invented or duplicated here.

## Principles

- Test observable Bamep behavior rather than dependency internals.
- Use the smallest test layer that can validate the intended behavior reliably.
- Prefer deterministic, isolated, repeatable tests.
- Test rejected, interrupted, stale, duplicate, and unsafe behavior deliberately.
- Use fakes and simulation at external or destructive boundaries when they faithfully
  preserve the contract under test.
- Use real integration only when the integration itself is the behavior under test.
- Add regression tests for reproducible defects when an active test layer can represent
  them reliably.
- Never weaken a safety invariant to make a test easier to reach.
- Treat coverage as a diagnostic signal, not proof of correctness.
- Do not describe intended validation as evidence that validation occurred.

## Test layers

Bamep uses layered validation:

```text
Unit / Domain
      ↓
Contract
      ↓
Component / Integration
      ↓
Simulator
      ↓
Integration Environment
```

These are validation responsibilities, not mandatory stages for every change.

Select only the layers needed to establish the approved behavior with sufficient
confidence.

Owner acceptance is part of the execution workflow, not a test layer defined by this
document.

## Unit and Domain tests

Use Unit and Domain tests for deterministic logic that does not require infrastructure.

Typical responsibilities include:

- state transitions and rejected transitions;
- validation rules;
- Endpoint identity and reconciliation logic;
- Job, JobStep, and Attempt state behavior;
- retry, cancellation, and idempotency rules;
- scheduler and resource-lease decisions;
- storage-capability selection;
- safety preconditions and invariants;
- protocol-message validation helpers;
- Artifact metadata and integrity logic.

Domain tests should exercise invalid and unsafe cases as deliberately as successful ones.

Pure Domain tests should not require network, database, filesystem, clock, process, or
hardware I/O when those dependencies can be represented explicitly.

## Contract tests

Use contract tests at boundaries shared by independently evolving components or
implementations.

Relevant examples include:

- Agent Protocol;
- Administrative API;
- future extension/plugin protocols;
- Server ↔ Agent messages;
- Server ↔ Web contracts;
- storage and infrastructure ports;
- Artifact metadata;
- domain-event representations.

Contract tests should focus on externally relevant behavior such as:

- serialization and deserialization;
- required and optional fields;
- version handling;
- validation and rejection;
- error representation;
- duplicate or unknown messages;
- incompatible requests.

The authoritative contract remains its Specification. Tests demonstrate that an
implementation conforms to that contract; they do not redefine it.

## Component and integration tests

Use component or integration tests when correctness depends on multiple real internal
responsibilities or a controlled real dependency working together.

Examples include:

- Application + Domain + persistence;
- scheduler + resource leases;
- Agent session handling;
- API + application services;
- Artifact lifecycle across persistence boundaries;
- transport adapters;
- controlled database behavior.

Prefer disposable local dependencies and deterministic setup.

A component/integration test must not require the physical Bamep laboratory unless it is
explicitly an Integration Environment test.

## Simulator

Bamep Simulator is a first-class validation layer for orchestration behavior that requires
a realistic Agent-side participant without physical endpoint hardware.

The Simulator should exercise the real external contracts required by its normative
Specification rather than bypassing them through in-process access to Server business
logic.

Use the Simulator for behaviors such as:

- Endpoint/Agent lifecycle;
- reconnect and interruption;
- protocol sequencing;
- concurrent orchestration;
- scheduling contention;
- retries and cancellation;
- transfer behavior;
- restart/recovery scenarios;
- failure and stale-state handling.

Do not duplicate the authoritative list of required Simulator scenarios here.

Normative fidelity, required scenarios, concurrency obligations, and boundaries between
simulated and physical behavior belong to
`docs/specifications/m0-simulator-contract-and-validation-strategy.md` and the active
milestone Specification when applicable.

Simulator tests must remain reproducible. Randomized scenarios should expose or preserve a
seed when reproducibility is required for diagnosis.

The Simulator does not prove physical firmware, PXE, NIC, storage-controller, Secure Boot,
or WinPE behavior.

## Fakes and controlled test boundaries

Use a fake when the external system is not the behavior under test and the fake can
preserve the relevant contract.

Possible fake boundaries include:

- network or Agent peers;
- storage devices;
- boot/discovery infrastructure;
- switch integrations;
- filesystem operations;
- process execution;
- clocks and timers;
- Artifact stores;
- external download sources.

A fake should model the contract relevant to the scenario, including meaningful failure
behavior where needed.

Do not build mocks that reproduce private implementation steps line by line.

When correctness depends on the real integration, move to the appropriate integration
layer instead of increasing fake complexity until it merely imitates the real system.

## Destructive-operation safety

Automated tests must not operate on real user or production data.

Storage- or disk-mutating tests should use safe targets such as:

- temporary files and directories;
- virtual disk images;
- disposable loop devices when explicitly appropriate and isolated;
- temporary filesystems;
- isolated virtual machines;
- controlled Integration Environment devices when real hardware behavior is required.

Safety-sensitive tests should cover the authoritative contract, including negative cases
such as, when applicable:

- identity mismatch;
- stale authoritative state;
- target/disk mismatch;
- missing authorization or trust;
- failed destructive preconditions;
- interrupted execution;
- invalid retry/reconciliation;
- required recovery states;
- failed integrity or verification gates.

Do not hardcode a duplicate list of normative destructive preconditions here. Their
authoritative definition belongs to the applicable Specification.

A test must never bypass or weaken one safety condition merely to exercise a later stage.

## Data-plane and Artifact validation

Data-plane validation should deliberately exercise interruption and integrity behavior
defined by the authoritative data-plane Specification.

Depending on the behavior under test, relevant scenarios may include:

- interrupted transfer;
- incomplete Artifact state;
- digest or size mismatch;
- duplicate transfer request;
- storage exhaustion;
- producer or consumer disconnect;
- restart;
- atomic completion;
- verified Artifact promotion;
- invalid transfer authorization or binding;
- failed verification before destructive use.

Resumability tests must represent only capabilities supported by the real transfer model.

Do not simulate arbitrary byte-offset resume if the producer cannot reproduce data with
the semantics required by the approved contract.

## Persistence and recovery validation

Durable workflow state must be validated independently from transient runtime presence.

Relevant scenarios may include:

- process restart;
- Agent reconnect;
- incomplete JobStep or Attempt;
- durable state with no active connection;
- duplicate result delivery;
- stale revisions;
- interrupted workflows;
- recovery reconciliation;
- invalid or inconsistent persisted state.

Tests must demonstrate the crash/reconnect semantics defined by the authoritative
Specifications.

In particular, restart or reconnect must not be treated as evidence that destructive work
is automatically safe to replay.

Use a real disposable database when persistence semantics themselves are under test.

## Frontend validation

When Bamep Web behavior is implemented, validate behavior owned by the frontend at the
appropriate layer.

Relevant areas may include:

- components and user-visible state;
- forms and validation;
- loading and error states;
- localization boundaries;
- controlled API interaction;
- Endpoint and Job presentation;
- destructive-action confirmations;
- accessibility-relevant behavior.

Prefer observable assertions over private component structure.

Frontend unit/component tests should not require a production Bamep Server.

## Regression tests

A reproducible defect should receive a regression test when an active test layer can
represent it reliably and the test provides durable protection against recurrence.

A useful regression test:

1. represents the failure condition;
2. asserts the intended behavior from an authoritative requirement or established behavior;
3. avoids unrelated private implementation detail;
4. would detect the defective behavior when practical.

If no automated layer can represent the defect faithfully, the required Integration
Environment or manual validation belongs in the work item's validation obligations rather
than in a misleading automated test.

Do not create a new inappropriate testing abstraction solely to cover one isolated defect.

## Test isolation

Automated tests must not depend on:

- personal files or developer-specific mutable state;
- real endpoint/customer data;
- production storage;
- production credentials;
- mutable external infrastructure;
- public Internet availability;
- physical Bamep hardware unless the test is explicitly an Integration Environment test.

Prefer:

- temporary directories;
- deterministic fixtures;
- isolated databases;
- fake adapters;
- local test servers;
- disposable Artifacts;
- explicit setup and teardown.

Resources created by tests should be cleaned up after success and failure when practical.

## Development environments

Linux is the reference environment for Bamep Server, Agent, Worker, Simulator, and
Linux-specific integration behavior.

When validation is performed from Windows:

- WSL2 may be used for Linux-targeted builds, tests, scripts, process behavior, filesystem
  behavior, and local simulation when it represents the responsibility faithfully;
- successful native-Windows execution does not prove Linux-specific behavior;
- native Windows tooling is appropriate for responsibilities that are intentionally
  portable.

Containers may be used when they improve isolation and reproducibility, for example for:

- disposable databases;
- local service dependencies;
- controlled protocol peers;
- repeatable integration fixtures.

Containers and WSL2 are development/testing techniques, not assumed production
architecture.

They must not be treated as faithful substitutes for behavior that materially depends on
physical PXE/DHCP networks, firmware/UEFI, Secure Boot, physical NICs, real storage
hardware, WinPE, or hardware-specific compatibility.

## Integration Environment

The Bamep Integration Environment exists for behavior that cannot be established
faithfully through local automation or simulation.

Examples include:

- PXE and DHCP behavior on the provisioning network;
- physical UEFI firmware;
- bootloader/network-boot behavior;
- Secure Boot and the physical trusted-boot chain;
- physical NIC behavior;
- real storage tooling where hardware behavior matters;
- Windows or WinPE deployment;
- hardware-specific compatibility;
- destructive end-to-end provisioning.

An Integration Environment procedure should identify, as applicable:

- required hardware/topology;
- environment preparation;
- exact target;
- safety precautions;
- steps or stimulus;
- expected observable result;
- cleanup or recovery procedure.

Real destructive execution requires the explicit authorization defined in `AGENTS.md`.

Reusable empirical findings from Integration Environment work belong in
`docs/reference/` according to the documentation policy.

## Selecting validation

Choose validation according to the responsibility and risk being changed.

| Change | Typical validation |
| --- | --- |
| Pure Domain rule | Focused Unit/Domain tests |
| State machine | Valid and rejected transition tests |
| Shared protocol/API contract | Contract tests |
| Persistence behavior | Domain + real disposable persistence integration |
| Infrastructure adapter | Adapter/contract tests + controlled integration when relevant |
| Scheduler/resource model | Domain tests + Simulator concurrency when required |
| Agent lifecycle | Contract + component/Simulator reconnect and failure scenarios |
| Data-plane transfer | Integration + interruption/integrity scenarios |
| Frontend behavior | Relevant frontend tests |
| Hardware-specific behavior | Automated layers where useful + Integration Environment |
| Destructive workflow | Safety tests + required Integration Environment evidence |
| Documentation only | Terminology, links, paths, examples, and factual consistency |

Shared or cross-cutting changes normally require broader validation than an isolated pure
function or document.

The approved Specification or Work Package may require stronger validation than this table;
when it does, the approved requirement wins.

## Coverage

Coverage helps identify weakly exercised:

- branches;
- state transitions;
- error paths;
- safety logic;
- shared Domain behavior;
- protocol handling.

Coverage percentage is not proof of correctness.

Do not introduce arbitrary repository-wide thresholds without an established baseline and
a reasoned policy.

Before enforcing a threshold:

1. establish stable coverage tooling;
2. establish a meaningful baseline;
3. verify what files/generated code are included;
4. inspect important uncovered behavior;
5. choose a value that protects useful regression detection.

Do not lower an established threshold merely to make a change pass.

Safety-critical behavior may require strong targeted coverage even when repository-wide
coverage remains lower.

## Handling validation failures

When a test or validation scenario fails:

1. reproduce with the narrowest useful test or scenario;
2. determine whether the cause is the current change, the environment, a missing
   prerequisite, flaky behavior, or pre-existing state;
3. correct failures caused by the current work;
4. preserve and report unrelated or unresolved failures accurately.

Do not, without understanding and justifying the cause:

- delete the failing test;
- skip or disable it;
- weaken the assertion;
- increase timeouts;
- add retries;
- disable safety checks.

A changed test is acceptable when the authoritative expected behavior changed; the reason
must be the changed contract, not the desire to obtain a passing suite.

## Validation evidence

Only executed validation produces validation evidence.

When recording results, distinguish:

- the layer or scenario exercised;
- the environment used;
- actual pass/fail outcome;
- relevant limitations;
- missing prerequisites;
- reusable empirical findings.

The execution workflow owns where work-item results and owner handoff are recorded.

Reusable technical evidence that survives the work item belongs in `docs/reference/`.

## Current repository state

Bamep currently has a Rust Cargo workspace containing the implemented Domain, Server,
Agent Protocol, Simulator, and trusted-bootstrap crates.

Do not hardcode a permanent repository-wide command here unless the repository explicitly
defines one as authoritative.

Derive concrete commands from the current workspace, package, build, CI, or test
configuration for the scope being validated.

As additional components such as Bamep Web are implemented, extend this policy only when a
new stable testing responsibility is required.

## Guiding rule

Choose the smallest trustworthy layer that can prove the behavior under test.

Use deterministic automation for what can be represented faithfully, Simulator validation
for orchestration through its real contract boundary, and the Integration Environment for
physical behavior that simulation cannot establish.
