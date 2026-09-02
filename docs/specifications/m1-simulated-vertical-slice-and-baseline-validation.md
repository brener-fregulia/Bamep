# M1 — Hardware-Independent Operational Core

Status: **Approved**

## Classification

Type: Feature/Epic-level Specification for the first post-M0 implementation milestone.

Execution grouping: GitHub Milestone #2.

## Context

M0 closed the architecture-and-contract phase before implementation. Its retained completion
record is `docs/specifications/m0-architecture-baseline.md`; current detailed behavior remains
owned by the individual M0 Specifications and ADRs.

M1 implements and empirically validates the hardware-independent successor slice recorded by M0.
At the time M1 was originally approved no product implementation existed and the successor slice
included a minimal Administrative API/Web observation checkpoint. The owner-approved M1 roadmap
rebaseline later removed that Presentation checkpoint from M1 completion so the milestone reflects
the operational core actually being proven. This is historical context, not a claim about current
repository state.

ADR-0013 later superseded ADR-0007's SQLite backend selection and made PostgreSQL the current
persistence baseline without removing M1's persistence-load validation obligation.

## Goal

Implement and validate Bamep's first integrated hardware-independent operational core across its
real internal protocol and process boundaries:

```text
Simulated Endpoint connects
-> authenticated/enrolled
-> inventory reported
-> Job created
-> scheduler evaluates resources
-> typed action dispatched
-> simulated transfer executed
-> progress observed
-> durable state/events persisted
-> disconnect/reconnect handled
-> Job reaches terminal state
```

The milestone remains entirely hardware-independent and headless. It includes empirical validation
of the PostgreSQL persistence baseline and the **20–24 concurrent Simulated Endpoint** target; it
does not require a human Presentation layer or physical Endpoint hardware.

## Scope

M1 covers:

- the minimum repository/tooling bootstrap needed by each Work Package;
- Endpoint identity, enrollment, credential, hardware-confidence, and current-boot behavior;
- real Agent Protocol v1 transport for Simulator-level Agent participation;
- trusted-bootstrap fixture semantics for the simulated environment;
- Job/JobStep/Attempt persistence, scheduling, resource leases, cancellation, and reconciliation;
- the complete destructive-operation authorization gate;
- PostgreSQL-backed durable/transient persistence, domain events, and audit records;
- authenticated chunked data-plane transfer and Artifact lifecycle;
- required Simulator scenarios at deterministic scale or at the explicit 20–24 Endpoint target.

This Specification does not prescribe a particular crate/package/module layout beyond the
accepted M0 boundaries.

## Out of scope

M1 does not implement or validate:

- physical PXE/DHCP/UEFI/GRUB/WinPE/Alpine delivery;
- real firmware Secure Boot execution;
- the physical/operator ceremony for ADR-0011 site-key pairing;
- real Windows deployment or destructive physical-disk operations;
- MikroTik-specific production integration;
- production operator Web or final operator UX;
- product Administrative API implementation;
- human identity and access management;
- production administrative authentication/RBAC;
- Web-originated Job creation, enrollment approval, cancellation, or other write operations;
- live-Windows backup or the final production backup/snapshot format;
- HA, multi-site, or ERP integration.

Actions required by the headless vertical slice are originated by the Simulator/test harness or
another internal development control path.

## Functional requirements

### RF-001 — Trusted bootstrap and Agent session

A Simulated Endpoint uses the deterministic Simulator trusted-bootstrap fixture while preserving
the real security ordering defined by the M0 trusted-bootstrap, Endpoint, and Agent Protocol
Specifications:

1. the simulated trusted-bootstrap stage establishes an authenticated expected Server TLS
   certificate fingerprint for the current boot;
2. only then does the Agent open WSS and verify the presented Server certificate against that
   authenticated fingerprint;
3. only after successful Server authentication does `AuthRequest` credential authentication
   proceed;
4. on valid authentication, the Server durably commits the applicable credential state and, on
   first contact, creates the Endpoint as `PendingEnrollment` before attempting to deliver
   `SessionEstablished`;
5. `PendingEnrollment` and the authenticated session lifecycle remain independent of
   `BootstrapEvidence`;
6. only after `SessionEstablished` may the Agent report `BootstrapEvidence`;
7. the Server independently verifies that evidence before marking trusted bootstrap established
   for the authoritative current boot.

Absent, malformed, or rejected `BootstrapEvidence` does not undo the durable authentication/
enrollment state; trusted bootstrap simply remains `NotEstablished` for that boot.

### RF-002 — Explicit enrollment approval

An explicit operator-approval action transitions `PendingEnrollment -> Enrolled`.

The approval control path is independent from the Simulated Agent participant. The Agent cannot
approve its own enrollment, and simulation must not collapse this into automatic enrollment.
The decision is durable and auditable according to the Endpoint/persistence contracts.

### RF-003 — Inventory persistence

Inventory is durably recorded on change only.

### RF-004 — Job lifecycle and safe dispatch

A Job/JobStep/Attempt is created through an internal Simulator/harness path, scheduled,
persisted-before-send, dispatched through typed Agent Protocol actions, reconciled across
disconnect/Server restart, and reaches the correct terminal state.

M1 introduces one concrete Simulator-only typed action, owned here under
`m0-agent-protocol-contract.md`'s rule that concrete action types belong to the
Specification that introduces them:

```text
action_type: "bamep.m1.simulated-execution"
action_version: "1"
parameters: {}
```

The v1 `parameters` schema is closed and empty. The action exists only to validate normal
M1 orchestration/execution: it has no physical hardware effect, performs no disk operation,
no provisioning, and no data-plane transfer, and exposes no arbitrary command/shell
execution. Execution outcome is never requested through action parameters; deterministic
Simulator scenario configuration instead controls accept-then-succeed, accept-then-fail,
reject, duplicate evidence, and delayed evidence.

For this action, `ActionAck{Rejected}.error.code` is one of the following closed values,
used only as needed to express the behavior above:

- `UNSUPPORTED_ACTION`;
- `UNSUPPORTED_ACTION_VERSION`;
- `INVALID_PARAMETERS`;
- `ACTION_NOT_AVAILABLE`.

`ActionResult.detail` is minimal and deterministic:

- `Succeeded` — `{ "code": "SIMULATED_COMPLETION" }`;
- `Failed` — `{ "code": "SIMULATED_FAILURE" }`.

`Cancelled` remains part of the generic Agent Protocol vocabulary; Issue #27 owns its
action-specific handling for this action.

The Simulator emits `ActionProgress` for this action using only `percent`, with a
deterministic example progression of `0`, `50`, `100`. This is action-specific Simulator
behavior, not a universal Agent Protocol requirement; generic wire shape, correlation, and
idempotency rules for `ActionDispatch`, `ActionAck`, `ActionProgress`, and `ActionResult`
remain owned by `m0-agent-protocol-contract.md`.

The complete **seven** destructive-operation preconditions owned by
`m0-endpoint-identity-lifecycle.md` are implemented and tested at deterministic small scale.

The required negative case is explicit: when preconditions 1–6 hold and only precondition 7
(trusted current bootstrap) fails, no destructive Attempt, durable dispatch commitment, or
`ActionDispatch` may be created/sent.

All destructive-labeled effects remain simulated.

### RF-005 — Authenticated resumable data-plane transfer

A simulated data-plane JobStep completes end to end with transfer authorization,
sender-constrained transfer authentication, chunk resume, Artifact lifecycle, and Artifact
verification against disposable local data.

M1's proven direction is **Agent -> Server** simulated capture. This narrows only which
direction M1 itself validates; it does not narrow the generic bidirectional M0 data-plane
contract owned by `m0-data-plane-and-storage-contracts.md`, which remains direction-agnostic.
A future milestone may prove Server -> Agent without requiring a contract change here.

**Classification: non-destructive.** `bamep.m1.data-plane-transfer` in its M1 Agent -> Server
capture direction is a **non-destructive, read-only** transfer action: it reads Volume/Image
or Selective source bytes without writing to Endpoint storage, consistent with the offline
read-only capture model `m0-data-plane-and-storage-contracts.md` "V1 capture consistency"
already requires. It is therefore dispatched and committed through the generic
**non-destructive** JobStep path owned by `m0-job-lifecycle-and-scheduling.md`: workflow/
scheduler authorization, applicable Attempt-scoped resource leases, this action's own
time-sensitive declared preconditions, final pre-dispatch revalidation, and the
persist-before-send Attempt/dispatch commitment
(`m0-persistence-observability-and-domain-events.md` "Agent action dispatch"). It MUST NOT
require, consume, or be gated by the seven-item destructive-operation precondition gate owned
by `m0-endpoint-identity-lifecycle.md` — that gate governs destructive JobSteps specifically,
and this action does not become one merely because the currently implemented
`bamep.m1.simulated-execution` Attempt-commit path (Issue #25/#26) happens to materialize the
destructive dispatch path for the action it serves. A future Server -> Agent direction of this
same `action_type` performing a destructive write is out of scope for M1 and would need its
own explicit classification when defined.

This Work Package does not design or implement the non-destructive transfer Attempt-commit
path itself. That path does not yet exist in the repository — the only currently implemented
Attempt-commit path materializes the destructive gate for `bamep.m1.simulated-execution` — and
remains a repository implementation dependency for #19's end-to-end RF-005 integration. This
classification is recorded only so that dependency is not later implemented by reusing or
narrowing the destructive path.

M1 introduces one concrete data-plane transfer action, owned here under
`m0-agent-protocol-contract.md`'s rule that concrete action types belong to the
Specification that introduces them, and distinct from `bamep.m1.simulated-execution`
(`RF-004`), which explicitly has no data-plane transfer:

```text
action_type: "bamep.m1.data-plane-transfer"
action_version: "1"
parameters: {
    transfer_id: string,       // UUID v4; the durable logical Transfer identity
    artifact_id: string,       // UUID v4; the durable logical Artifact identity
    direction: "agent_to_server",
    digest_algorithm: "sha256",
    chunk_size: integer        // positive; bytes; fixed for this Transfer's manifest
}
```

`transfer_id` and `artifact_id` are created durably by `bamepd` before dispatch (they are not
Agent-originated) and delivered to the Agent exclusively through this action's `parameters`,
which is the single channel that exists for the Agent to learn them; they are not duplicated
elsewhere on the wire merely because they are also durable Server state. `direction` is a
closed enumeration with exactly one M1 v1 value, `"agent_to_server"`; the field exists because
the per-request data-plane proof defined by `m0-data-plane-and-storage-contracts.md` binds
direction, not because M1 exercises more than one value. `digest_algorithm` is a closed
enumeration with exactly one M1 v1 value, `"sha256"`; this is an M1 interoperability choice
carried explicitly on the wire, not a universal, permanently fixed Bamep digest algorithm.
`chunk_size` is Server-selected per Transfer and carried explicitly on the wire; this
Specification does not fix a universal chunk size, and an M1 implementation choosing a
concrete value (for example, matching `docs/reference/transfer-resumability-spike.md`'s 4 MiB
experimental value) does so as an M1-scoped operational choice, not as durable architecture.

The concrete disposable source bytes the Simulated Agent captures for Agent -> Server capture
are supplied through Simulator/test-harness configuration, not through this action's
`parameters` or any other Agent Protocol message — the same fixture boundary already used for
trusted-bootstrap material (`m0-trusted-bootstrap-and-server-fingerprint-contract.md`
"Simulator contract"). This keeps the action's parameters limited to values the Agent could not
otherwise obtain and that are required by the interoperability contract itself.

Beyond `parameters`, the Agent obtains everything else it needs from the already-required
protocol exchanges this action triggers: the ephemeral proof keypair is Agent-generated
locally (`m0-data-plane-and-storage-contracts.md` "Ephemeral proof key"); the Worker
data-plane HTTPS origin is delivered via `TransferAuthorizationGrant.data_plane_base_url`
(`m0-agent-protocol-contract.md` "Transfer authorization"); the exact chunk-request/resume
HTTPS surface is `m0-data-plane-and-storage-contracts.md`'s "HTTPS data-plane v1 contract".
No value required to execute this action is obtainable only by reading Server Rust source.

For this action, `ActionAck{Rejected}.error.code` is one of the following closed values, used
only as needed to express the behavior above (the same closed set used by
`bamep.m1.simulated-execution` for consistency; no data-plane-specific code is currently
required):

- `UNSUPPORTED_ACTION`;
- `UNSUPPORTED_ACTION_VERSION`;
- `INVALID_PARAMETERS` — used for a structurally invalid `parameters` object, including an
  unknown `direction`/`digest_algorithm` enum value or a non-positive `chunk_size`;
- `ACTION_NOT_AVAILABLE`.

`ActionResult.detail` composes with the authoritative Artifact lifecycle
(`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle") rather than merely reporting
that bytes were sent:

- `Succeeded` — sent only after `bamepd` has durably committed the owning Artifact to
  `Verified`: `{ "code": "TRANSFER_VERIFIED", "artifact_id": "<uuid>" }`;
- `Failed` — one of the following closed values:
  - `{ "code": "ARTIFACT_VERIFICATION_FAILED", "artifact_id": "<uuid>" }` — full-Artifact
    verification failed (`PendingVerification -> Failed`);
  - `{ "code": "CHUNK_VERIFICATION_FAILED", "artifact_id": "<uuid>" }` — a required chunk
    could not be reproduced/verified;
  - `{ "code": "TRANSFER_ABANDONED", "artifact_id": "<uuid>" }` — capture was abandoned or
    cancelled before completion.

  Failure ordering differs by code. For `ARTIFACT_VERIFICATION_FAILED` the durable
  `PendingVerification -> Failed` commit precedes the `ActionResult` — the seal/verification
  path (`m1-worker-data-plane-control-contract.md`) already committed it, and the Agent
  observes it as the seal HTTP response's `artifact_status`. For `CHUNK_VERIFICATION_FAILED`
  and `TRANSFER_ABANDONED` no preceding operation commits the Artifact to `Failed`: the
  Agent's terminal `ActionResult{Failed}` is the authoritative evidence `bamepd` consumes to
  drive `Incomplete -> Failed`, atomically with the terminal workflow transition — or, when
  that terminal `ActionResult` was lost, an authoritative `#28` `StatusReport{Failed}`
  reconciliation outcome for the owning Attempt while the Artifact is still `Incomplete`
  drives the same atomic `Incomplete -> Failed`. `m0-data-plane-and-storage-contracts.md`
  "Artifact lifecycle" owns the correlation validation, atomicity, retry/duplicate,
  conflicting-evidence, and terminal-Artifact-immutability semantics for both channels,
  including why the code-less `StatusReport{Failed}` is sufficient when the Artifact is
  `Incomplete`.

`Cancelled` remains part of the generic Agent Protocol vocabulary, composing with the Job
lifecycle `Cancelling` contract (Issue #27, unchanged); cancellation does not roll back
chunks already durably accepted. An Artifact still `Incomplete` when the owning Attempt's
authoritative cancellation/abandonment outcome means it can no longer reach `Verified`
follows the same `Incomplete -> Failed` rule — driven by `bamepd` and committed atomically
with the existing cancellation terminal transition
(`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle") — not a data-plane-specific
cancellation state.

`ActionProgress` for this action uses `percent` and/or `bytes_processed`; when used,
`bytes_processed` means cumulative bytes of chunks `bamepd` has durably accepted for this
Transfer so far. This is action-specific meaning for the otherwise generic optional field
defined by `m0-agent-protocol-contract.md`, which itself defines no transfer-specific
semantics for it.

Source provenance in M1 composes with — and never extends — the data-plane contract. RF-005
keeps three easily-conflated things distinct: (A) **source mutation / reproducibility
failure**, already an M1 operational failure case — when source bytes previously associated
with the same logical Transfer can no longer reproduce a durably recorded chunk identity, the
transfer fails closed via `CHUNK_VERIFICATION_FAILED` above without rewriting that identity;
(B) **immutable descriptive `SourceProvenance`**, which stays bound to the Transfer and is
never rewritten; and (C) an **independently re-observed physical source identity**, which M1
does not select, require, or exercise, and which composes no field or message into Agent
Protocol or Worker Protocol. `m0-data-plane-and-storage-contracts.md` "Artifact provenance
and target identity" owns the full rule, including the deferral of (C) to the future
physical-disk / hardware-integration milestone; the same authority keeps source provenance
distinct from any later destructive target identity.

Required fail-closed cases remain those owned by the data-plane and Simulator Specifications.

### RF-006 — Headless completion boundary

M1 completion does not require Administrative API or Web implementation. Presentation clients and
their versioned Administrative API boundary remain approved future product work governed by
`m0-administrative-api-web-read-contract.md`, ADR-0016, and ADR-0017; this milestone neither
implements nor supersedes those Presentation decisions.

### RF-007 — Concurrency target

The Simulator exercises **20–24 concurrent Simulated Endpoints** for the three scenario categories
explicitly tied to that target:

- scheduler/resource contention;
- PostgreSQL persistence-load validation;
- chunked data-plane transfer at scale.

Other required Simulator scenarios may be proven at deterministic smaller scale unless
concurrency is necessary to exercise the behavior under test.

## Non-functional requirements

### NF-001 — Persistence-load empirical validation

PostgreSQL is the accepted persistence baseline under ADR-0013.

At the 20–24 Endpoint target, M1 records actual:

- durable write volume;
- contention;
- persistence latency;
- backpressure.

No numeric acceptance threshold is invented before measurement. If representative behavior is
unacceptable, ADR-0013 must be explicitly revisited rather than silently bypassed or weakened.

### NF-002 — Reference environment

Linux is Bamep's development and production reference environment.

Validation of Linux-specific responsibilities must run in an environment that faithfully
represents those responsibilities. Native Linux is valid; WSL2 may be used from Windows when it
faithfully represents the responsibility under test. Native-Windows-only execution is not
sufficient evidence for Linux-specific behavior.

`docs/development/testing.md` owns the general validation-environment policy.

## Safety invariants

M1 must preserve the M0 safety contracts, especially:

- none of the seven destructive-operation preconditions may be inferred from another;
- `CredentialActive` never implies trusted current bootstrap;
- reconnect, timeout, missing acknowledgement, `Unknown`, or `Indeterminate` never justify blind
  destructive redispatch;
- transfer authorization fails closed and does not expose enumerable internal denial reasons;
- destructive-labeled Simulator operations never touch physical hardware or physical disks.

The authoritative detailed gates remain in the Endpoint, Job, data-plane, trusted-bootstrap, and
Simulator Specifications.

## Architecture constraints

M1 follows the accepted architecture without revalidating it:

- modular monolith with Worker process isolation — ADR-0001;
- Server in Rust — ADR-0002;
- Agent/Worker in Rust with wire-contract independence — ADR-0003;
- PostgreSQL behind the `repositories` Port — ADR-0013;
- Presentation / Application / Domain / Runtime Services / Ports / Adapters / Workers remain
  dependency boundaries rather than mandated crate/package/module layout.

Current implemented structure is described only by `docs/architecture/README.md`.

## Authoritative inputs

M1 directly consumes:

- `m0-endpoint-identity-lifecycle.md` — Endpoint/enrollment/current-boot state and destructive gate;
- `m0-agent-protocol-contract.md` — real Agent Protocol v1 wire behavior;
- `m0-trusted-bootstrap-and-server-fingerprint-contract.md` — trusted-bootstrap and fixture
  semantics;
- `m0-job-lifecycle-and-scheduling.md` — Job/JobStep/Attempt, scheduling, cancellation, retry, and
  reconciliation;
- `m0-persistence-observability-and-domain-events.md` — durable state/events/audit contract;
- `m0-data-plane-and-storage-contracts.md` — transfer/Artifact contract;
- `m0-simulator-contract-and-validation-strategy.md` — Simulator fidelity, scenarios, concurrency,
  and persistence-load validation;
- `m0-stack-and-boundaries-baseline.md` — product/component dependency boundaries;
- `m1-worker-data-plane-control-contract.md` — Server↔Worker UDS contract for RF-005;
- ADR-0004, ADR-0005, ADR-0006, ADR-0008, ADR-0010, ADR-0012, ADR-0013, and ADR-0018 for the
  rationale behind the directly exercised contracts.

ADR-0007 is historical (`Superseded by ADR-0013`). ADR-0009 and the real ADR-0011 ceremony remain
outside M1 execution.

## Validation

M1 uses the validation model defined by `docs/development/testing.md`.

Each Work Package selects the applicable Unit/Domain, Contract, Component/Integration, Simulator,
and owner-manual validation from the normative contracts it implements. M1 does not redefine
per-contract test cases already owned by those Specifications.

The Integration Environment is not required to complete M1 because physical boot, firmware,
network-boot, hardware, and real-disk behavior are outside this milestone.

Issue #21 owns the final M1 empirical evidence boundary: 20–24 concurrent Simulated Endpoint
scheduler/resource contention, PostgreSQL persistence-load measurement, and chunked data-plane
transfer at scale, together with confirmation of the remaining required headless deterministic
Simulator coverage. No Presentation implementation or observation is required for that evidence.

## Integration Environment boundary

Passing M1 does not validate physical provisioning.

PXE, DHCP, physical UEFI/Secure Boot, GRUB/WinPE/Alpine delivery, physical NIC behavior,
MikroTik integration, real disk tooling, Windows deployment, the real ADR-0011 pairing ceremony,
and hardware-specific compatibility remain Integration Environment responsibilities according to
the Simulator and trusted-bootstrap boundaries.

## Open questions

1. Concrete implementation of the operator-approval control path (harness, CLI, development
   fixture, or equivalent); its semantic independence from the Agent is already decided.
2. Concrete implementation of Job-creation origination outside a product Administrative API.
3. Numeric persistence-load acceptance thresholds, which may only be established from NF-001
   empirical evidence.
