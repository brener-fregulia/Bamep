# M2 — Endpoint Capture Service Intent and Source-Reference Contract

Status: **Proposed — pending owner approval**

## Classification

Type: Specification (first M2 product/Application service-intent contract).

This Specification does not itself decompose implementation Work Packages; it is the
normative WHAT that future M2 implementation Work Packages and the future Administrative
API submission-write Specification (#55 follow-up) must consume without re-deciding it.

## Context

Two owner-approved M2 Discoveries produced the conclusions this Specification materializes:

- Discovery #56 selected `bamep.m2.endpoint-capture` (read-only Volume/Image capture) as
  Bamep's first real operator-facing service intent, because the underlying Agent -> Server
  Transfer/Artifact/data-plane machinery is already Approved, implemented, and tested
  (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005), while explicitly
  rejecting a durable Domain `Operation`/`ServiceIntent`/catalog aggregate and confirming
  that `targets` remain owned by the operator-submission envelope (ADR-0019; Discovery #55).
- Discovery #58 selected a typed, revision-scoped capture-source reference ("Model B") as the
  v1 source-selection contract, resolved across four owner-review addenda into a
  source-mapping continuity epoch model: `SourceReference { inventory_revision_id,
  source_observation_id, agent_source_id }`, with durable `SourceProvenance` binding that
  same lineage immutably to the Transfer.

Both Discoveries are investigation only; their durable conclusions are recorded in the
respective GitHub Issues (#56, #58) and are not restated as a competing authority here. This
Specification is the first durable Specification-level materialization of those conclusions.

No new ADR is required. ADR-0019 (operator submission boundary) and ADR-0020 (planned
intervention checkpoint / capacity separation) are consumed without being reopened: this
Specification introduces no durable `Operation` aggregate, no bulk atomicity across
Endpoints, and no change to Job/JobStep/Attempt state or transition vocabulary.

## Goal

Define the minimum authoritative contract for one real, operator-facing, read-only Endpoint
capture service intent — including the source-reference/provenance lineage it requires and
the concrete Agent Protocol action that carries it — so that:

- a future Administrative API operator-submission Specification (#55 follow-up) has one real
  typed intent to reference instead of an opaque JSON blob;
- Application code has an authoritative target for the atomic per-target creation outcome
  ADR-0019 already requires;
- the Agent can distinguish a stale/unresolvable source selection from every other rejection
  reason, and does so before reading any source byte.

## Scope

This Specification defines:

1. the `bamep.m2.endpoint-capture` service-intent identity and the per-target capture
   configuration it requires;
2. the `SourceReference` typed source-selection/authority contract and its wire
   representation;
3. the source-mapping continuity epoch semantics `SourceObservationId` represents;
4. the minimum normative `InventoryReport.inventory` fragment the intent depends on;
5. the durable `SourceProvenance` shape bound to a capture Transfer;
6. the authoritative Application-level outcome of accepting one capture target, composing
   with ADR-0019's atomic target-creation requirement;
7. the concrete `bamep.m2.endpoint-capture-transfer` Agent Protocol action, distinct from and
   non-modifying of `bamep.m1.data-plane-transfer` v1;
8. restart/reconnect/resume semantics for a source-bearing capture, reconciled with
   `m0-data-plane-and-storage-contracts.md`;
9. the retry-equivalence semantics a future submission contract must apply to the selected
   source.

Non-negotiable invariants this Specification preserves (repeated here because they anchor
every section below):

- **One target produces exactly one Job.** No bulk/multi-Endpoint Job.
- **Exactly one concrete source per target.** No implicit "primary disk", no selection by
  device path, ordinal, or enumeration order.
- **`targets` belong to the future operator-submission envelope, not to this intent.** This
  Specification defines only the per-target capture configuration shape carried inside that
  future envelope, not the envelope itself.
- **No durable `Operation`/`ServiceIntent`/catalog aggregate** is introduced.
- **No `JobStep.kind`** is introduced; the existing Transfer -> `job_step_id` correlation
  remains sufficient to identify this one side-tabled action kind.
- **A fresh `SourceObservationId` is minted whenever mapping continuity is in doubt; silent
  rebinding is forbidden.** Genuine reboot is not guaranteed resumable by this first
  contract; no persistent cross-boot physical-source identity is introduced.
- **`bamep.m1.data-plane-transfer` v1 is not extended in place.**
- **A stale-but-structurally-valid source reference is never rejected as
  `INVALID_PARAMETERS`.**
- **`SourceProvenance` is historically immutable and is never destructive target identity.**

## Out of scope

- Windows reinstall, driver installation, debloat, Selective/file-granular preservation,
  restore;
- the Administrative API HTTP envelope, routes, methods, and `request_key` wire form (#55
  follow-up owns this; this Specification defines only the typed semantic material that
  contract must canonicalize/reference);
- the Endpoint collection/inventory read surface (#57);
- a Web API client or replacing current Presentation fixtures;
- post-submit result/monitoring UI;
- IAM/RBAC/MFA/actor attribution;
- a general/future-complete service or action catalog;
- `JobStep.kind` or general typed provisioning-action modeling (`docs/discovery/m2-composite-service-workflow-and-operator-intervention.md`
  group A remains unresolved and is not forced open by this Specification);
- universal WWN/serial/GPT/composite physical-disk identity, or any independently
  re-observed cross-boot physical source identity;
- Commercial Platform concepts.

## RF-1 — Service-intent identity and per-target capture configuration

The service intent is identified by a fixed, versioned pair, shared once per operator
submission (not repeated per target):

```json
{
  "intent": "bamep.m2.endpoint-capture",
  "intent_version": "1"
}
```

- `intent` is a fixed non-empty string identifying this concrete service intent; it is
  opaque outside this Specification's normative meaning.
- `intent_version` is wire type `string`, reusing the existing Bamep versioning idiom already
  used for `action_version` (`m0-agent-protocol-contract.md` "Action field contract") and
  `protocol_version`, rather than introducing a second integer-typed versioning convention.
  `"1"` is this Specification's only currently defined value.

This descriptor owns no target set and no per-target data. `targets` — the requested
Endpoint set and each target's resolved configuration — remain owned by the future
operator-submission envelope (Discovery #55; ADR-0019). This Specification instead defines
the **per-target capture configuration** that envelope must carry for every target
requesting this intent:

```json
{
  "source_reference": {
    "inventory_revision_id": "<uuid-v4>",
    "source_observation_id": "<43-char base64url>",
    "agent_source_id": "<opaque string>"
  }
}
```

Exactly one `source_reference` is required per target requesting `bamep.m2.endpoint-capture`.
There is no implicit/default source. A target submitted without a structurally valid
`source_reference` is a malformed command, not a valid target with an assumed source
(consistent with Discovery #58's rejection of "Model A — implicit single eligible source").

## RF-2 — `SourceReference` authority and lifetime

`SourceReference` identifies the exact source an Endpoint capture reads, without ever
promoting an OS-local device path, enumeration ordinal, drive letter, or display label to
cross-boundary authority.

| Field | Owner | Meaning |
| --- | --- | --- |
| `inventory_revision_id` | Server-minted | the durable `InventoryRevisionId` (`m0-persistence-observability-and-domain-events.md` "Inventory persistence") whose reported snapshot gave this reference its meaning |
| `source_observation_id` | Agent-minted | identifies one source-mapping continuity epoch (RF-3) |
| `agent_source_id` | Agent-minted | opaque label unique and stable only within the owning `source_observation_id` epoch |

A `SourceReference` is meaningful only while all three hold simultaneously against current
authoritative state:

1. `inventory_revision_id` equals the Endpoint's **current** authoritative
   `InventoryRevisionId`;
2. that revision's reported inventory snapshot carries a `capture_source_observation_id`
   (RF-4) equal to `source_observation_id`;
3. that snapshot's `capturable_sources` array contains an entry whose `agent_source_id`
   equals the given value.

Any one of these failing makes the reference **stale**, never a different valid source. Zero
eligible sources, multiple eligible sources with no selection, or a selection that no longer
satisfies all three conditions all fail closed identically — the system never silently
chooses by order or path.

This tuple remains valid from operator selection through submission acceptance, Job/Transfer
creation, and final pre-dispatch, subject to two independent fail-closed re-checks before the
first source byte is read (RF-5, RF-6):

- **Server-side**: `inventory_revision_id` still equals the Endpoint's current authoritative
  revision immediately before dispatch commitment.
- **Agent-side**: the Agent's own current live mapping still recognizes
  `(source_observation_id, agent_source_id)` before it opens the source.

Neither check substitutes for the other.

## RF-3 — Source-mapping continuity epoch

`source_observation_id` represents continuity of the Agent's own
`(source_observation_id, agent_source_id) -> exact local source` mapping. It is **not** a
process-lifetime token and **not** a physical-hardware identity.

- The Agent mints a fresh `source_observation_id` whenever it (re)establishes this mapping
  without being able to prove exact continuity with a previously reported mapping.
- A same-boot Agent process restart may retain the existing epoch **only if** the Agent can
  restore/reconstruct the exact same mapping without ambiguity. Any doubt requires a fresh
  epoch and a fresh inventory observation; silent rebinding to a similar-looking source is
  forbidden.
- Because `source_observation_id` is part of the normative reported inventory content
  (RF-4), minting a fresh epoch necessarily changes the reported snapshot and therefore
  produces a fresh Server `InventoryRevisionId` through the already-approved
  inventory-on-change rule (`m0-persistence-observability-and-domain-events.md` "Inventory
  persistence"; `crates/domain/src/inventory.rs` `record_inventory_on_change`). No new
  Server-side mechanism is introduced to detect this; the existing JSON-equality rule already
  suffices once the epoch identifier participates in the compared content.
- A genuine reboot is **not guaranteed** to preserve the epoch. This first contract
  introduces no persistent cross-boot physical-source identity; if the Agent cannot honestly
  restore the mapping, the previous epoch and every reference minted against it become stale.
- `agent_source_id` is unique and stable **only** inside its owning epoch. The same string
  value under a different `source_observation_id` never means the same source. The Agent
  must never resolve `(source_observation_id, agent_source_id)` against a mapping from a
  different epoch.

## RF-4 — Normative inventory content

`InventoryReport.inventory` remains opaque to Agent Protocol itself
(`m0-agent-protocol-contract.md` "Inventory reporting"). This Specification defines the
minimum fragment of that JSON object relevant to source selection; all other inventory
content remains opaque and out of this Specification's scope (no SMART, capacity, health,
topology, friendly names, or generic hardware schema):

```json
{
  "capture_source_observation_id": "<43-char base64url>",
  "capturable_sources": [
    { "agent_source_id": "<opaque string>" }
  ]
}
```

- `capture_source_observation_id` is the Agent's current `source_observation_id` (RF-3) at
  observation time.
- `capturable_sources` lists every source currently eligible for capture under that epoch,
  each identified only by its `agent_source_id`. An empty array is valid and means no source
  is currently capturable; the Server/Application must never fabricate a synthetic entry.
- The Agent supplies no other cross-boundary source metadata in this fragment. Human-facing
  presentation of sources (labels, size, type) is explicitly deferred to a future Endpoint
  inventory read surface (#57) and is not defined here.

## Wire representation for `source_observation_id` and `agent_source_id`

These are this Specification's concrete interoperable representation choices, made at
Specification level per the smallest existing fitting Bamep wire idiom, not new ADR
material:

**`source_observation_id`** reuses exactly the `boot_nonce` idiom already defined by
`m0-trusted-bootstrap-and-server-fingerprint-contract.md`: 32 raw bytes from a
cryptographically secure source, encoded as canonical RFC 4648 base64url without padding —
exactly 43 ASCII characters — under the identical strict round-trip parsing rule used
throughout the trusted-bootstrap and data-plane Specifications (reject padding, the
standard-base64 alphabet, whitespace, wrong length, or non-canonical trailing bits). This
idiom is the right fit because both values share the same shape: minted fresh exactly when a
new epoch/boot begins, then held stable and reused for the epoch/boot's entire duration, and
compared only for exact equality by the receiving party — never parsed or interpreted.
`boot_nonce`'s own freshness/replay semantics are not imported; only its byte-length and
canonical encoding are reused.

**`agent_source_id`** reuses the opacity idiom already defined for the data-plane capability
`token` (`m0-data-plane-and-storage-contracts.md` "Capability opacity"): an opaque UTF-8
string that the receiving party never parses and compares only for exact equality. It is
non-empty; a concrete maximum length remains implementation-time, mirroring that same
Specification's own deferral of `token`'s exact maximum length, and must remain comfortably
within ordinary JSON field/message size. Unlike `source_observation_id`, `agent_source_id`
carries no freshness requirement — the same value is expected to repeat, unchanged, across
every inventory report while its owning epoch remains current, since it is what lets the
Server/operator refer to "the same source" across repeated observations within one epoch.

**`inventory_revision_id`** carried inside `SourceReference` and inside the Agent action
(RF-6) reuses the existing `InventoryRevisionId` Domain representation
(`crates/domain/src/inventory.rs`: `InventoryRevisionId(pub Uuid)`) under the Agent Protocol
v1 identifier convention already fixed for `transfer_id`/`artifact_id`/`action_id`
(`m0-agent-protocol-contract.md` "Wire encoding and compatibility"): a lowercase hyphenated
UUID v4 string.

## RF-5 — Durable `SourceProvenance`

`SourceProvenance` bound to a capture Transfer becomes, for this intent, the resolved
`SourceReference` tuple recorded at Transfer-creation time:

```json
{
  "inventory_revision_id": "<uuid-v4>",
  "source_observation_id": "<43-char base64url>",
  "agent_source_id": "<opaque string>"
}
```

- It is fixed when the Transfer is created and is **never rewritten**, per the existing M1
  rule that `SourceProvenance` is immutable descriptive provenance bound to the Transfer
  (`m0-data-plane-and-storage-contracts.md` "M1 scope of `SourceProvenance`"). This
  Specification gives that same immutable-descriptive category a concrete structured shape
  for the new M2 action; it remains, exactly as before, **not** an independently re-observed
  hardware-identity credential.
- It continues to describe the captured source **historically**, even after the same
  `agent_source_id`/`source_observation_id`/`inventory_revision_id` combination is no longer
  current or selectable — including across a later planned hardware replacement
  (`m0-endpoint-identity-lifecycle.md` "Planned hardware replacement").
- It is never compared against `TargetFingerprint` and never becomes a precondition for a
  later destructive operation; source identity and destructive target identity remain
  independent (`m0-data-plane-and-storage-contracts.md` "Artifact provenance and target
  identity").

## RF-6 — Application mapping and atomic target creation

For one submission target accepted for `bamep.m2.endpoint-capture`, the Application derives
the following authoritative durable outcome — the required result, not today's helper-call
sequence:

1. **Source-reference freshness check.** Before creating any durable state for this target,
   the Application validates the given `source_reference` against the Endpoint's current
   authoritative `InventoryRevisionId` and its reported `capturable_sources` (RF-2). If it is
   stale or does not resolve, this target's per-target creation outcome settles
   `Undecided -> Rejected(SourceReferenceStale)` and **no** Job/Transfer/Artifact is created
   for it. `SourceReferenceStale` is this Specification's chosen semantic meaning for that
   per-target creation rejection; the exact Administrative API wire string for it remains
   owned by the future submission-write Specification (#55 follow-up), consistent with
   `m0-persistence-observability-and-domain-events.md`'s existing deferral of "the concrete
   rejection-reason vocabulary". This is a target-creation-time rejection like any other
   per-target eligibility failure — it never uses `INVALID_PARAMETERS`, which is reserved for
   a structurally invalid command.
2. **Atomic durable creation.** Otherwise, the following become durable as **one** atomic
   target-creation outcome, composing with — and not narrowing — ADR-0019's existing
   `Undecided -> Created(job_id)` atomicity requirement:
   - exactly one Job for exactly one Endpoint (`create_workflow`-equivalent outcome);
   - the minimum ordered JobStep set this intent requires: exactly **one** capture JobStep.
     No `JobStep.kind` is introduced — the Transfer this JobStep owns is the sufficient
     correlation to identify this one side-tabled action kind (Discovery #56 resolution);
   - the pre-dispatch `Transfer` + `Artifact` (`Incomplete`) + empty/unsealed `ChunkManifest`
     state already required by `m0-data-plane-and-storage-contracts.md`
     (`create_transfer_context`-equivalent outcome), with `source_provenance` populated by
     the resolved `SourceReference` tuple (RF-5) — never a placeholder string;
   - the target's `Undecided -> Created(job_id)` transition itself.

   This Specification states the **required durable outcome**: all of the above commit in the
   same persistence transaction, or none of them do. It prescribes no repository type,
   transaction API, or schema, and it does not claim that today's separate repository helper
   methods already provide this transaction as written (Discovery #56/#58 resolutions).
3. **No premature dispatch.** No Attempt, action identity, or `ActionDispatch` is created at
   target-creation time. The created JobStep begins `Pending` and proceeds through the
   ordinary Job-lifecycle scheduling/precondition boundary
   (`m0-job-lifecycle-and-scheduling.md`) exactly like any other non-destructive JobStep.
4. **Final pre-dispatch revalidation.** Immediately before the Attempt/dispatch commitment for
   the capture JobStep, the Server re-validates `inventory_revision_id` currency again as this
   action's own time-sensitive declared precondition
   (`m0-job-lifecycle-and-scheduling.md` "Final pre-dispatch revalidation" step 3). Failure
   releases newly acquired leases and returns the JobStep to `Pending`, unchanged from the
   existing generic rule; it creates no Attempt and sends no `ActionDispatch`.

This action is **non-destructive** (read-only Volume/Image capture, per
`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005's existing classification
principle applied to this new action): it requires workflow/scheduler authorization and its
own time-sensitive preconditions, but **not** the seven-item destructive-operation gate
(`m0-endpoint-identity-lifecycle.md`).

## RF-7 — Product Agent action contract: `bamep.m2.endpoint-capture-transfer`

`bamep.m1.data-plane-transfer` v1's `parameters` schema is closed and remains M1/Simulator-
scoped; it is **not** extended in place. This Specification introduces a new, distinct,
closed action:

```text
action_type: "bamep.m2.endpoint-capture-transfer"
action_version: "1"
parameters: {
    transfer_id: string,       // UUID v4; the durable logical Transfer identity
    artifact_id: string,       // UUID v4; the durable logical Artifact identity
    direction: "agent_to_server",
    digest_algorithm: "sha256",
    chunk_size: integer,       // positive; bytes; fixed for this Transfer's manifest
    source_reference: {
        inventory_revision_id: string,   // UUID v4, lowercase hyphenated
        source_observation_id: string,   // 43-char base64url, no padding
        agent_source_id: string          // opaque UTF-8, non-empty
    }
}
```

`transfer_id`, `artifact_id`, `direction`, `digest_algorithm`, and `chunk_size` carry exactly
the same meaning, encoding, and interoperability rationale already established by
`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005 and
`m0-data-plane-and-storage-contracts.md`; this Specification does not redefine the chunk
manifest, digest, or resume contract those values participate in. `source_reference` is the
only field this action adds, carrying exactly the `SourceReference` shape defined in RF-2.

**Rejection vocabulary.** `ActionAck{Rejected}.error.code` for this action is one of the
following closed values, mirroring the existing RF-005 set with exactly one addition:

- `UNSUPPORTED_ACTION`;
- `UNSUPPORTED_ACTION_VERSION`;
- `INVALID_PARAMETERS` — a structurally invalid `parameters` object: a missing/wrong-typed
  field, an unknown `direction`/`digest_algorithm` enum value, a non-positive `chunk_size`,
  or a `source_reference` that fails structural validation (missing field, malformed
  `inventory_revision_id`/`source_observation_id` encoding, or empty `agent_source_id`). It
  is never used for a structurally valid `source_reference` that is merely stale;
- `ACTION_NOT_AVAILABLE`;
- `SOURCE_REFERENCE_STALE` — the `source_reference` is structurally valid but the Agent's
  current live source-observation epoch does not recognize it: either
  `source_observation_id` does not match the Agent's current epoch, or `agent_source_id`
  does not resolve within that epoch. The Agent performs this check, and fails closed,
  **before reading any source byte**.

**`ActionResult.detail`** reuses exactly the RF-005 closed vocabulary and ordering rules
unchanged: `{ "code": "TRANSFER_VERIFIED", "artifact_id" }` on success, and
`ARTIFACT_VERIFICATION_FAILED` / `CHUNK_VERIFICATION_FAILED` / `TRANSFER_ABANDONED` (each with
`artifact_id`) on failure, composing with the Artifact lifecycle exactly as RF-005 already
defines. No new terminal code is introduced for lost source-mapping continuity mid-Transfer
(RF-8) — that case is already `TRANSFER_ABANDONED`, consistent with "no new Transfer
lifecycle state is introduced merely for source continuity."

`ActionProgress` reuses the RF-005 meaning of `percent`/`bytes_processed` unchanged. This
action reuses the generic Agent Protocol envelope, correlation (`correlation_id ==
action_id`), and idempotency rules (`m0-agent-protocol-contract.md`) without redefinition.

## RF-8 — Restart/reconnect/resume semantics

This reconciles directly with `m0-data-plane-and-storage-contracts.md` "Disconnect and
restart" and "Authorization lifetime versus transfer identity":

- **Same-boot Agent process restart, mapping restored.** If durable state still authorizes
  the Transfer (existing rule) **and** the Agent can restore the exact
  `(source_observation_id, agent_source_id) -> local source` mapping for this Transfer's
  recorded `SourceProvenance` without ambiguity, the Agent reauthenticates, obtains a new
  transfer-authorization capability, and continues the same `transfer_id`/`artifact_id`,
  resuming only chunks not yet durably held — unchanged from the existing chunk-resume
  contract. No new Transfer lifecycle state is introduced.
- **Same-boot Agent process restart, mapping cannot be restored.** The Agent must not open a
  merely similar source. The capture fails/abandons through the existing
  `Incomplete -> Failed` rule via `ActionResult{Failed, TRANSFER_ABANDONED}` (or the
  equivalent `#28` reconciliation path when that terminal evidence is lost), exactly as
  `m0-data-plane-and-storage-contracts.md` "`Incomplete -> Failed` ownership and ordering"
  already defines. No new lifecycle state or rejection code is introduced for this case.
- **Genuine reboot.** Not guaranteed resumable by this first contract. This Specification
  introduces no persistent cross-boot physical-source identity; a genuine reboot is treated
  under the same "mapping cannot be restored" rule above unless a future milestone defines a
  trustworthy cross-boot source-resolution mechanism.
- **Before the first byte of a not-yet-started Transfer**, source replacement/change is
  covered by RF-2/RF-6's fail-closed checks, not by this section: the Agent's
  `SOURCE_REFERENCE_STALE` `ActionAck{Rejected}` applies, and no Attempt/Transfer progress
  exists yet to "resume".

## RF-9 — Retry-equivalence semantics

The selected source lineage is semantically part of the per-target command. A future
Administrative API submission contract (#55 follow-up) must treat the per-target capture
configuration's `source_reference` (RF-1) as participating in canonical command equivalence
exactly as `m0-persistence-observability-and-domain-events.md` "`request_key`" already
requires generically for "target set, configuration, or intent": reusing one retained
`request_key` with a *different* `source_reference` for the same target is a non-equivalent
command and must be rejected, never silently reinterpreted as continuing the original
command against a different source.

This Specification defines no Administrative API HTTP envelope, route, or wire location for
`request_key` itself; that remains owned by the #55 follow-up.

## Safety invariants

- Zero, one, or multiple eligible capture sources are all handled without ever choosing by
  device order, path, or an implicit "primary disk".
- A stale or unresolvable source reference fails closed before any byte is read, at both the
  Server (`inventory_revision_id` currency) and the Agent (`source_observation_id`/
  `agent_source_id` resolution) boundary, and never through `INVALID_PARAMETERS`.
- Silent rebinding of a `source_observation_id` epoch across a mapping change is forbidden;
  any doubt mints a fresh epoch.
- Genuine reboot is never treated as resumable through invented physical identity.
- `SourceProvenance` is immutable once the Transfer exists and is never later compared to
  `TargetFingerprint` or otherwise treated as destructive target identity.
- `bamep.m1.data-plane-transfer` v1 is unmodified by this Specification.
- No destructive-operation precondition is weakened, invoked, or implied by this
  non-destructive intent.

## Architecture impact

- No new durable Domain aggregate. No ADR reopened or superseded.
- Extends the normative content owned by `m0-data-plane-and-storage-contracts.md`
  (`SourceProvenance` concrete shape for this action only), `m0-agent-protocol-contract.md`
  (one new concrete `action_type`, per its existing generic delegation rule), and
  `m0-persistence-observability-and-domain-events.md` (the canonical intent/configuration
  descriptor content for this one intent, and one new per-target creation rejection
  meaning), without altering any of those Specifications' own generic mechanics.
- `docs/architecture/README.md` is not modified: nothing here is implemented.

## Acceptance criteria

- `bamep.m2.endpoint-capture` expresses one real endpoint-capture service intent without
  borrowing the current Web mock schema (RF-1).
- One target's selected source is unambiguous, represented through the accepted revision +
  source-observation epoch + opaque Agent-source lineage (RF-2, RF-3).
- Zero/one/multiple source behavior is fail-closed and never silently chooses by device
  order/path (RF-2, RF-4).
- The Agent distinguishes stale source selection before reading bytes, including the
  Server/Agent race around a local mapping change (RF-2, RF-6, RF-7).
- Same-boot restart/resume semantics no longer contradict the existing M0 resumability
  contract (RF-8).
- Genuine reboot limitation is explicit and invents no physical identity (RF-3, RF-8).
- Durable provenance remains historically meaningful and independent from destructive target
  identity (RF-5).
- The new product action is concrete, closed, versioned, and does not mutate the M1 action
  contract in place (RF-7).
- ADR-0019 target-creation atomicity is preserved semantically without pretending today's
  helper sequence is already transactional (RF-6).
- Retry-equivalence requirements expose enough typed semantics for the future #55
  submission-write Specification (RF-9).
- No new ADR is introduced.

## Explicitly not decided here

- The exact Administrative API HTTP envelope, `request_key` wire location, and the exact
  wire string for the `SourceReferenceStale` per-target creation rejection reason (#55
  follow-up).
- The Endpoint/inventory read surface exposing `capturable_sources` for human operator
  selection (#57).
- `JobStep.kind` / general typed provisioning-action modeling (unresolved; see
  `docs/discovery/m2-composite-service-workflow-and-operator-intervention.md` group A).
- Any independently re-observed cross-boot physical source identity (WWN/serial/GPT/
  composite) — deferred to a future physical-disk/hardware-integration milestone, unchanged
  from `m0-data-plane-and-storage-contracts.md`.
- Exact maximum length for `agent_source_id` (implementation-time, mirroring `token`'s own
  deferred maximum length).

No genuinely new architectural decision was discovered while writing this Specification
beyond what #56/#58 already approved; nothing here was stopped for owner review on that
basis.

## Validation

Documentation-only Specification; no product code is implemented. Future implementation Work
Packages must validate at minimum:

- `SourceReference`/`source_observation_id`/`agent_source_id` structural validation positives
  and negatives (malformed encoding, empty string, missing field) versus staleness negatives
  (well-formed but non-current), asserting the correct closed rejection code for each;
- fresh-epoch minting on ambiguous mapping reconstruction, and the resulting fresh
  `InventoryRevisionId` through the existing inventory-on-change rule;
- per-target creation: stale reference rejects only that target
  (`Rejected(SourceReferenceStale)`) without a Job/Transfer/Artifact, while a fresh reference
  commits Job + JobStep + Transfer + Artifact + `Undecided -> Created(job_id)` atomically;
- final pre-dispatch revalidation failure returns the JobStep to `Pending` without creating
  an Attempt;
- Agent-side `SOURCE_REFERENCE_STALE` rejection before any chunk is requested;
- same-boot restart with mapping restored resumes; same-boot restart/reboot with mapping
  lost reaches `Incomplete -> Failed` via `TRANSFER_ABANDONED`;
- `SourceProvenance` immutability across the Transfer's lifetime;
- retry with an identical `request_key` but a different `source_reference` for the same
  target is rejected as non-equivalent (once the #55 submission contract exists).

Contract tests for the new `action_type` follow
`docs/development/testing.md` "Contract tests"; Simulator scenarios follow
`m0-simulator-contract-and-validation-strategy.md`.

## Related

- ADR-0019 — operator submission boundary; `targets` ownership and atomic target-creation
  rationale this Specification composes with.
- ADR-0020 — planned intervention checkpoint; unaffected by this non-destructive intent.
- Discovery #56 (Issue #56) — first service-intent selection and descriptor-ownership
  correction.
- Discovery #58 (Issue #58) — source-reference/provenance model and the source-mapping
  continuity epoch resolution.
- `m0-data-plane-and-storage-contracts.md` — Artifact/Transfer/chunk-manifest contract this
  action reuses; owner of `SourceProvenance`'s M1 baseline scope.
- `m0-agent-protocol-contract.md` — generic envelope/correlation/idempotency rules this
  action reuses unchanged.
- `m0-persistence-observability-and-domain-events.md` — operator submission persistence;
  owner of the generic `request_key`/canonical-descriptor/rejection-vocabulary deferrals this
  Specification partially resolves for this one intent.
- `m0-job-lifecycle-and-scheduling.md` — non-destructive JobStep dispatch and final
  pre-dispatch revalidation this action's source-freshness precondition composes with.
- `m0-endpoint-identity-lifecycle.md` — destructive-operation gate this non-destructive
  action does not invoke; planned hardware replacement composition for `SourceProvenance`.
- `m1-simulated-vertical-slice-and-baseline-validation.md` RF-005 — the closed M1/Simulator-
  scoped `bamep.m1.data-plane-transfer` action this Specification does not modify, and the
  non-destructive classification principle this new action follows.
- `m0-trusted-bootstrap-and-server-fingerprint-contract.md` — `boot_nonce` wire idiom reused
  for `source_observation_id`.
