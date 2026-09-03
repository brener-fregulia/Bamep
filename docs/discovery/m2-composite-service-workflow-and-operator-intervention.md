# Discovery — M2 Composite Service Workflow and Operator-Intervention Model

Status: **Discovery / investigation only — not approved, not normative**

- Source question: Issue #45 — *[Discovery] Define M2 composite service workflow and operator-intervention model*
- Repository baseline: branch `main`, HEAD `360f5106fba494199e9c8df89bf079de268e2681`
- Produced: 2026-09-02
- Read-only Discovery: no repository behavior, Git state, or GitHub state was changed while producing this.
- This document is investigation material under `docs/discovery/README.md`. It is **not** an
  authority for approved behavior, accepted decisions, or implemented architecture. Every
  recommendation here is a proposal for owner review. Once the owner resolves the question,
  durable conclusions move to `docs/specifications/`, `docs/decisions/`, or GitHub work items
  per `docs/development/documentation-policy.md`, and this document is reduced or retired.

---

## 1. Repository baseline inspected

| Item | Value |
|---|---|
| Branch | `main` |
| HEAD SHA | `360f5106fba494199e9c8df89bf079de268e2681` — *docs: define M2 operator submission boundary* |
| Working tree | clean, in sync with `origin/main` |

**Issue #45 body**: read in full from current GitHub state. It matches the Discovery brief and
additionally makes explicit: selective/assisted backup semantics, the assisted-discovery
advisory principle, the HDD→SSD "replacement disk not attached to the Server" scenario, and
**Scenario C** (selective/assisted customer-data backup) as a required third proof scenario. No
disagreement between the brief and the Issue was found.

**Authoritative documents inspected**

- `AGENTS.md`, `CLAUDE.md`
- `docs/development/sdd.md`, `docs/development/documentation-policy.md` (index),
  `docs/development/workflow.md` (index), `docs/development/testing.md` (index)
- ADR-0006 (Job/JobStep/Attempt + scheduling), ADR-0008 (data-plane), ADR-0009
  (driver-provider boundary), ADR-0015 (commercial entitlement boundary), ADR-0016
  (Presentation client), ADR-0019 (operator submission boundary); ADR-0018 referenced
  (isolated Worker) via specs
- Specifications: `m0-stack-and-boundaries-baseline`, `m0-endpoint-identity-lifecycle`,
  `m0-job-lifecycle-and-scheduling`, `m0-data-plane-and-storage-contracts`,
  `m0-persistence-observability-and-domain-events`, `m0-administrative-api-web-read-contract`,
  `m1-simulated-vertical-slice-and-baseline-validation`, `m1-worker-data-plane-control-contract`
  (referenced)
- `docs/discovery/architecture-redesign.md` (active discovery — retains "Future: pre/post
  provisioning diagnostics")

**Implementation areas inspected**

- `crates/domain/src/`: `job.rs`, `artifact.rs`, `transfer.rs`, `transfer_authorization.rs`
  (size/shape), `target_fingerprint.rs`, `final_dispatch.rs`, `chunk_manifest.rs`,
  `hardware_confidence.rs`, `current_boot.rs`, `credential.rs`, `events.rs`, `lib.rs`
- `crates/server/src/application/mod.rs` (single large module),
  `crates/server/src/adapters/postgres/` (repositories),
  `crates/server/migrations/0001_initial_schema.sql`
- `crates/worker/src/data_plane/` (`http.rs`, `upload.rs`, `mod.rs`),
  `crates/worker/src/storage/`
- grep sweeps for `submission`, `Operation`, `Selective`, `Volume`, `BackupStrategy`,
  `debloat`, `restore`, `install`, JobStep "kind"

**Issues inspected**: #41 (closed), #42 (closed), #43 (closed), #44 (open), #45 (open, this
Discovery). All in Milestone #3 "M2 — Operator Plane".

---

## 2. Existing facts already decided (repository-supported)

**F1 — Durable execution authority is `Job / JobStep / Attempt` only.** One Job targets exactly
one Endpoint (ADR-0006; `m0-job-lifecycle-and-scheduling.md` "Domain model"). Confirmed in code:
`crates/domain/src/job.rs` — `Job { steps: Vec<JobStep> }`, `create_workflow(endpoint_id, step_count)`.

**F2 — Baseline workflow is a linear ordered `JobStep` sequence.** "Branching, parallel
JobSteps, partial-success, and skip semantics require explicit future design" (ADR-0006).
`m0-job-lifecycle-and-scheduling.md` "Out of scope": partial-failure/skip and DAG/branching are
explicitly not in the contract.

**F3 — No durable Domain `Operation` aggregate.** ADR-0019 "Decision" and "Alternatives
considered" reject it explicitly; every candidate responsibility above individual Jobs was found
already-owned, derivable, or dependent on future IAM/audit. Not reopened by any later commit.
Not present in code.

**F4 — One operator submission produces up to N independent one-Endpoint Jobs.** Non-atomic
across Endpoints; partial creation permitted (ADR-0019). The **operator submission** is a
durable **creation-phase Application record** (`request_key` + Server-minted `submission_id` +
requested target set + canonical intent/configuration descriptor + one per-target creation
outcome `Undecided -> Created(job_id) | Rejected(reason)`). It owns **no** admission, scheduling,
dispatch, Attempt, cancellation, reconciliation, or aggregate execution outcome
(`m0-persistence-observability-and-domain-events.md` "Operator submission persistence and
correlation"). **Not implemented** — `grep` finds no submission persistence in `crates/`.

**F5 — `Operation` is permitted product vocabulary, not execution authority** (ADR-0019;
#41/#42/#43 product constraints).

**F6 — Data-plane distinguishes `Volume/Image` from `Selective`.** `m0-data-plane-and-storage-contracts.md`
"Backup strategy boundary": Volume/Image = linear byte-range capture; Selective = file-granular
baseline direction, large files may chunk internally. "Per-file Selective behavior was not
empirically validated by the resumability Spike and must not be presented as such."

**F7 — An Artifact is one atomic integrity/completeness unit.** `Incomplete -> PendingVerification
-> Verified | Failed` (+ `Incomplete -> Failed`). "No subset of a failed Artifact is exposed as
partial success." Terminal states immutable; a failed Artifact is never repaired in place —
later authorized work creates a new Artifact.

**F8 — The contract already anticipates multiple independent Artifacts for a future Selective
workflow**, which "may succeed/fail independently; workflow acceptance of partial success is a
separate policy" (`m0-data-plane-and-storage-contracts.md` "Artifact lifecycle").

**F9 — Integrity != capture consistency.** `capture_consistency in {NotApplicable,
NotEstablished, Established}`; `Established` is never the default and requires positive
confirmation of offline/read-only conditions for the capture duration. Destructive use of a
capture-consistency-requiring Artifact needs **both** `Verified` **and** `capture_consistency ==
Established`. The concrete establishing mechanism is out of scope of that Specification.

**F10 — V1 capture is offline.** Endpoint boots the Linux maintenance environment, installed
Windows not running, sources read-only. Offline capture establishes *source stability during
capture*, not filesystem/application semantic health. Live/VSS capture is outside V1.

**F11 — Source provenance and destructive target identity are independent.** "A valid workflow
may back up an old disk, replace it, revalidate the new disk, provision it, and restore retained
data. The destination fingerprint therefore need not equal the Artifact source fingerprint"
(ADR-0008; `m0-data-plane-and-storage-contracts.md` "Artifact provenance and target identity").
Fingerprint *inequality* is never itself a provenance failure; the fail-closed case is
*intra-Transfer* source-reproducibility inconsistency.

**F12 — M1 `SourceProvenance` is immutable descriptive provenance bound to the Transfer** —
fixed at Transfer creation, never rewritten, **not** an independently re-observed
hardware-identity credential. Confirmed in code: `crates/domain/src/transfer.rs` —
`SourceProvenance(String)`, opaque, bound in `Transfer`. M1 explicitly does **not** define
WWN/serial/GPT/composite `SourceIdentity`/re-observation. A concrete re-observed physical source
identity is deferred to a future physical-disk/hardware-integration milestone.

**F13 — Endpoint durable identity is Server-assigned and independent of MAC/NIC/disk/device-path.**
Four independent state dimensions (persistent identity, credential/session validity, hardware
confidence, authoritative current boot); none inferable from another
(`m0-endpoint-identity-lifecycle.md`).

**F14 — Hardware/inventory facts are evidence, never authentication/trust anchors** (`AGENTS.md`
"Safety"; `m0-endpoint-identity-lifecycle.md` "Identity model").

**F15 — Seven independent destructive-operation preconditions, all fail-closed, none inferable:**
(1) `Enrolled` identity; (2) `CredentialActive` **and** independently-held authenticated Agent
session/presence; (3) authorized Job/action; (4) fresh inventory (authorized revision == current
revision); (5) target-disk revalidation immediately before execution; (6) hardware confidence
`Consistent` (both `LoweredConfidence` and `Conflict` fail); (7) trusted current bootstrap
`Established` via independent Server verification. The Job lifecycle composes/revalidates the
complete gate at final pre-dispatch; data-plane/Artifact gates are additive and may not narrow
it. Implemented and tested at deterministic small scale (`crates/domain/src/final_dispatch.rs`;
m1 RF-004).

**F16 — Destructive work is never blindly retried/resumed.** No generic automatic-retry path for
destructive JobSteps; timeout/reconnect/restart/`Unknown`/`Indeterminate` never imply redispatch
permission; an `Indeterminate` destructive Attempt requires an explicit recorded operator
decision before another Attempt (ADR-0006; `m0-job-lifecycle-and-scheduling.md`).

**F17 — Job-scoped Endpoint-exclusivity lease** is held from `Pending -> Running` until the Job
is genuinely terminal, across JobSteps/Attempts/retries/`Cancelling`/`AwaitingReconciliation`.
Bamep never interleaves active Jobs against one Endpoint.

**F18 — Commercial entitlement boundary (ADR-0015) is fully decided.**
Domain/Application/Runtime Services carry **zero** commercial vocabulary (no
`Edition`/`Bamep4`/`customer`/`contract`/SKU). Commercial capacity means exactly *"maximum
number of simultaneously active Endpoint Jobs, measured by Job-scoped Endpoint-exclusivity
leases currently granted"* — i.e. Jobs in `Running`/`Cancelling`. Core receives only a generic
numeric `ExecutionCapacityPolicy` / `CapabilitySet`; offline-verifiable, fail-closed for new
admission, never terminating active Jobs; the commercial platform is never required online in
the destructive hot path. Not implemented; anticipated architecturally.

**F19 — Storage is capability-based** (`roles in {SYSTEM, CACHE, ARCHIVE}`), no
RAID/filesystem/device-path assumptions in Domain/Application (ADR-0008;
`m0-data-plane-and-storage-contracts.md`). `target_fingerprint.rs`: `TargetFingerprint(String)`
is opaque; "Equality is the only operation this checkpoint needs."

**F20 — Driver application already has an accepted boundary (ADR-0009):** operator-staged local
driver repository consumed through a Port/Adapter; Bamep injects via DISM/`drvload`-equivalent;
Bamep does not bundle/fetch/redistribute proprietary driver packs; no Internet dependency in the
provisioning phase.

**F21 — Presentation is a static client over Administrative API v1 reads only** (ADR-0016;
`m0-administrative-api-web-read-contract.md`). The read contract exposes Endpoint and Job (with
nested JobStep/Attempt/progress/Transfer-Artifact summaries). It defines no Job creation, no
`Operation` resource, no submission surface. M1 completion is headless; Presentation is approved
future work.

**F22 — "Future: pre/post provisioning diagnostics" is already a recorded, undesigned future use
case** (`docs/discovery/architecture-redesign.md`), explicitly *not* an M0/M1 requirement, with
a stated forward-compatibility expectation that the Job/JobStep model composes with `Preflight ->
Backup -> Provision -> Configure -> Postflight -> Report` **without** adding JobStep types or
changing the Job lifecycle contract yet.

---

## 3. Current implementation reality

**What backup / data-plane infrastructure exists in code now**

- Domain: `Transfer` (`transfer_id`, `direction in {AgentToServer}`, immutable
  `SourceProvenance`, binding to Endpoint/Attempt/Artifact), `Artifact` (`ArtifactState`,
  `CaptureConsistency`, transitions), `ChunkManifest` (sealing, immutable expected chunk
  identities, incremental `artifact_digest`), `transfer_authorization` (large: Ed25519 ephemeral
  proof key, capability bindings, per-request proof transcript, replay/freshness),
  `transfer_dispatch`.
- Server: `transfer_repository.rs` (durable Transfer/Artifact/chunk state, resume state),
  Application transfer orchestration, atomic reconciliation composition for `StatusReport{Failed}`
  / cancellation -> `Incomplete -> Failed` (commits `e61417f`, `408952a`, `46a7cd2`).
- Worker (ADR-0018 isolated process): `data_plane/http.rs` + `upload.rs` implementing
  `PUT .../chunks/{i}`, `GET .../chunks`, `POST .../seal`; `storage/fs_store` +
  `storage/full_artifact`; UDS control path to `bamepd` for authorization + acceptance +
  seal/verify commits.
- Tests: `data_plane_transfer_failure_matrix.rs`, `transfer_terminal_evidence.rs`,
  `data_plane_transfer_vertical.rs`, `headless_scale_validation.rs`.

**What is simulated only**

- The transfer action itself: `bamep.m1.data-plane-transfer` (v1) is a **Simulator-only,
  non-destructive, read-only** capture against **disposable local data** (m1 RF-005). It reads
  "Volume/Image or Selective source bytes" *in classification wording only* — there is no real
  disk reader, no real filesystem enumerator, no maintenance-environment integration.
- The destructive action: `bamep.m1.simulated-execution` (v1, closed empty params) — "no
  physical hardware effect, performs no disk operation, no provisioning, and no data-plane
  transfer." All destructive-labeled effects are simulated; the seven-gate is real, its
  downstream effect is not.
- `capture_consistency` is always constructed `NotEstablished` in M1; no component establishes
  `Established` (`crates/domain/src/artifact.rs` comments; F9).

**What remains contract-only (accepted, not implemented)**

- Operator submission persistence and correlation (ADR-0019 / persistence spec) — **zero code**.
- Server->Agent transfer direction; the non-destructive transfer Attempt-commit path (m1 RF-005
  explicitly notes it "does not yet exist in the repository").
- Volume/Image vs Selective as anything more than a documentation boundary — no `BackupStrategy`
  type, no Selective mechanism.
- Commercial entitlement / `ExecutionCapacityPolicy` / `CapabilitySet` (ADR-0015) — anticipated
  only.
- Administrative API v1, Presentation Web, IAM/audit actor attribution.
- Any typed provisioning JobStep (install OS, debloat, drivers, restore, validate) — none exist.
  `JobStep` has no "kind"; it carries only `order`, `state`, an optional `destructive_intent`
  snapshot, and an optional `failure_reason`.
- Planned-hardware-change authorization workflow (`m0-data-plane-and-storage-contracts.md` "Out
  of scope").
- Independently re-observed physical source/target hardware identity (F12).

**What selective-backup functionality does not exist yet: essentially all of it.** No offline
filesystem inspection, no path selection model, no assisted discovery/classification, no
resolved capture set, no per-group Artifact mapping, no required/optional preservation policy, no
selective restore correlation, no estimation. The only Selective-relevant artifact in the
repository is the one-sentence "Backup strategy boundary" paragraph and a classification
adjective in m1 RF-005.

---

## 4. New product pressure

`Capturar imagem do sistema` is **one uniform, optional-free, non-destructive, single-step,
hardware-stable, whole-source intent**. It exercised: Endpoint multi-selection, mixed Endpoint
conditions, product vocabulary, one immutable submission, independent per-target creation. It
deliberately avoided everything below.

| Pressure | What the mock intent never exercised | Why it matters now |
|---|---|---|
| **Composition** | A Job with >1 meaningfully different JobStep; ordering constraints between steps | `backup -> install -> debloat -> drivers -> restore -> validate` is the primary workflow; `step_count` is currently just an integer |
| **Optionality / conditionality** | Steps present for some targets, absent for others; steps enabled by configuration | ADR-0006 has no skip/branch semantics; "optional" must be resolved *somewhere* |
| **Per-Endpoint divergence within one request** | Different treatment per target under shared defaults | #42/#43 kept one intent uniform across LAB-03/07/09 |
| **Destructive composition** | A destructive step gated by a *prior* step's Artifact result | Backup verification -> permission to format is a cross-step invariant |
| **Human intervention** | Execution pausing on a physical-world action, then resuming | No JobStep state, no Attempt state, and no Application concept represents this |
| **Hardware change mid-workflow** | The authorized target-disk fingerprint (precondition 5) becoming *intentionally* stale | HDD->SSD invalidates preconditions 4–7 by design; nothing says "wait, don't fail" |
| **Selective / assisted backup** | Backup as anything other than `true`; a data-selection decision; discovery evidence; multiple Artifacts; required vs optional data | F6/F8 anticipate it; none of it is modeled |
| **"What was actually preserved?"** | Evidence connecting operator request -> resolved selection -> captured Artifacts -> verified result -> restore | Artifact only knows its own bytes verified; no higher correlation exists |
| **Commercial capacity as a product promise** | "up to 4 machines concurrently" as a first-class constraint | ADR-0015 already answers this generically; pressure is only that it now has a concrete product name (`Bamep4`) |
| **Before/after value** | Neutral operational evidence that a future report could consume | Already recorded as future work (F22); pressure is only "don't accidentally make it impossible" |

---

## 5. Missing invariants (genuine — not "lacks a friendly name")

**MI-1 — Resolved per-target execution plan as durable, reconstructable creation-phase content.**
ADR-0019's submission records "a canonical intent/configuration descriptor sufficient to verify
retry equivalence." A composed service with common defaults + per-Endpoint overrides means the
descriptor is no longer one flat intent, and the *resolved* plan for each target (which optional
steps, which configuration) must be reconstructable — otherwise, after a partial creation or a
Server restart mid-processing, Bamep cannot re-derive what LAB-02's Job was supposed to contain.
**This is a content/Specification gap, not a new aggregate**: the resolved plan becomes the
target's JobSteps at `Undecided -> Created(job_id)` (which already commits atomically with
Job/JobStep creation, per persistence spec "Atomic target creation").

**MI-2 — Typed JobStep classification.** `JobStep` has no durable "kind". A composed service
needs steps that are *semantically* backup / OS-install / debloat / driver-install / restore /
validate, because destructive classification, preconditions, retry policy, and Artifact linkage
all differ per kind. The existing rule "concrete action types belong to the Specification that
introduces them" (m0-agent-protocol-contract) already frames how to add them; the missing piece
is a durable JobStep-kind concept and per-kind precondition/postcondition hooks.

**MI-3 — A durable operator-intervention checkpoint.** The Job/JobStep/Attempt model **cannot
today represent** "execution is intentionally suspended pending an external human physical action
and Bamep's re-establishment of required machine facts." It is not `JobStep.Pending` (that's
pre-start eligibility), not `AwaitingReconciliation` (that's uncertain execution of a live
Attempt), and not `Cancelling`. The missing invariant: *a JobStep may be durably suspended;
resuming past it requires (a) a recorded operator authorization decision AND (b) positive
re-establishment of the machine-fact preconditions that the physical change is expected to have
invalidated — a bare acknowledgement is insufficient.*

**MI-4 — Cross-step "preservation sufficiency" policy for destructive continuation.** An Artifact
knows only whether *it* verified (F7). When backup is composed of required and optional
preservation groups (possibly multiple Artifacts, F8), *something* must own the decision "may the
destructive step proceed given these results", fail-closed by default when operator-required
preservation cannot be proven. This is neither Artifact state nor a bypass flow — it is a
JobStep precondition owned by the workflow/destructive-dispatch composition.

**MI-5 — Resolved selective capture set + provenance-of-selection evidence.** Between "operator
wants these paths" and "Artifact bytes captured" there is no durable object recording: the
operator request, the discovery evidence it was based on (and *when*), the reviewed resolved
selection, and the mapping to resulting Artifact(s). Without it, "what did Bamep actually
preserve vs. what was asked?" cannot be answered, and stale-selection detection (source changed
between discovery and capture) has nothing to compare against. Note: F12's intra-Transfer
chunk-reproducibility fail-closed protects *bytes already being captured*; it does **not**
protect against the *selection* being resolved against a filesystem state that then changed
before capture began.

**MI-6 — Post-hardware-change target re-resolution.** Precondition 5 checks the target
fingerprint "matches the authorized target immediately before execution." After an intended disk
replacement the authorized fingerprint is *deliberately* obsolete. The missing invariant: *after
an intervention checkpoint, the destructive target is re-resolved from targeting intent to a
concrete `TargetFingerprint` against current inventory, a fresh destructive-authorization
snapshot is created, and ambiguity (two plausible target disks) / capability shortfall
(replacement too small) fail closed.* This is the concrete shape of the already-deferred
"planned hardware-change authorization workflow."

**Not missing** (already covered, do not re-invent): grouping of per-Endpoint work (F3/F4),
partial-acceptance semantics (F4), Endpoint identity continuity across reboot/hardware change
(F13), the seven-gate and its fail-closed re-evaluation (F15), Artifact integrity/transfer
authorization/capture consistency (F7/F9), source != target identity (F11), commercial capacity
semantics (F18), no-Internet-in-hot-path (F18/F20).

---

## 6. Candidate models

Only credible alternatives compared. All assume the operator-facing word "Operation" / "service"
stays as vocabulary regardless of model.

| Concern | **C1: Job construction only** (resolve everything at creation; no new concept) | **C2: Application-level service composition** (submission descriptor carries service-intent + defaults + overrides; a resolver produces per-target JobSteps) | **C3: Durable `ServiceIntent` / `OperationPlan` Domain aggregate** above Jobs | **C4: Durable `WorkflowTemplate` Domain aggregate** (reusable named templates) |
|---|---|---|---|---|
| Authority owner | Application creation path + existing Job model | Application (submission record extended) + existing Job model | New Domain aggregate | New Domain aggregate + template store |
| Durable / transient / configuration | Job/JobStep durable; the *request* is transient/UI | Submission record **durable** (already required by F4); resolved per-target plan **durable as JobSteps**; editing state transient | New durable aggregate + lifecycle | Durable template (configuration) + durable instantiation state |
| Relationship to Job | Job is the only artifact of a request | Same; submission correlates via `submission_id` (F4) | Aggregate *supervises* / groups Jobs | Template *instantiates* Jobs |
| Relationship to JobStep | JobSteps are the resolved plan | JobSteps are the resolved plan; resolver is the only new logic | Aggregate may own step definitions -> duplicates JobStep | Template owns step definitions -> duplicates JobStep |
| Relationship to Attempt | none | none | risk of aggregate-level progress/outcome (ADR-0019 rejected this) | none directly |
| Relationship to ADR-0019 | Compatible; under-specifies the descriptor | **Extends ADR-0019's descriptor** — same boundary, richer content | **Reopens** the rejected durable-`Operation` classification without a new invariant | Adjacent; introduces a second durable authority for "what a workflow is" |
| Common defaults | Resolved and discarded (only per-target result kept) | Resolved; **defaults retained in submission descriptor** for reconstruction/retry-equivalence | Held on aggregate | Held on template |
| Per-Endpoint overrides | Resolved and discarded | Resolved; retained as per-target resolved plan + recorded override delta | Held on aggregate per target | Held on instantiation |
| Human intervention | Needs MI-3 regardless | Needs MI-3 regardless (as a JobStep concept) | Could model as aggregate state — wrong layer (execution, not creation) | Template can *declare* a checkpoint step; execution semantics still need MI-3 |
| Selective backup | Needs MI-5 regardless | Needs MI-5 regardless (bound to transfer JobStep / Transfer) | Aggregate could hold selection — wrong layer | Template can't hold a per-run selection |
| Partial backup result | Needs MI-4 regardless | Needs MI-4 regardless (JobStep precondition) | Tempting to put on aggregate -> hidden cross-Job coupling | n/a |
| Restart safety | Weak: request not durable -> interrupted creation cannot reconstruct intended per-target plan | Strong: submission commit precedes any Job; `Undecided` targets resume; resolved plan commits atomically with its Job | Strong but heavier (aggregate recovery + Job recovery) | Medium |
| Complexity | Low code, **high hidden risk** (lost authoritative intent — exactly ADR-0019's rejected "correlation on created Jobs only") | Low–moderate; reuses an already-required record; one new resolver | High; new aggregate, lifecycle, events, and the ADR-0019 re-litigation | High; a whole template subsystem for a need not yet evidenced |
| Primary failure mode | Unreconstructable operator intent after partial creation / restart | Descriptor schema + resolver correctness; must not leak execution authority into the submission | Aggregate silently accretes execution responsibilities (progress, outcome, cancellation) it was told not to own | Premature abstraction; templates become a parallel spec for workflows |

**C4 verdict**: no invariant requires reusable named templates today; a "template" is a UI/config
convenience that can sit entirely in the operator client or as inert Application configuration
later. Reject for this Discovery.

**C3 verdict**: adds a durable aggregate with **no new genuine invariant above individual
Jobs** — the exact test ADR-0019 set. Human-intervention, selective backup, and partial-result
policy are all *execution-layer* or *JobStep-layer* concerns, not "above Jobs." Reject.

**C1 verdict**: correct that no new *aggregate* is needed, but under-delivers on reconstruction —
it repeats ADR-0019's rejected "correlation identifier on successfully created Jobs only"
alternative.

**C2 verdict**: minimal, reuses the record F4 already mandates, keeps execution authority exactly
where it is. **Recommended.**

---

## 7. Recommended minimum model

**Adopt C2. Do not introduce any new Domain aggregate.**

### Proposed concepts

| Concept | Layer | Durable / transient / configuration | New? |
|---|---|---|---|
| **Composed service intent** ("Operation" to the operator) | Presentation vocabulary; **Application** as the submission descriptor's shape | Durable *as submission content* (F4 already requires a canonical descriptor) | Extends ADR-0019 descriptor; **no new aggregate** |
| **Common service defaults** | Application (submission descriptor) | Durable within the submission record | Extends ADR-0019 descriptor |
| **Per-Endpoint override delta** | Application (submission descriptor, per requested target) | Durable within the submission record | Extends ADR-0019 descriptor |
| **Resolved per-target plan** | Application resolver -> **Domain** `Job` + ordered `JobStep`s | Durable **as the target's JobSteps**, committed atomically with `Undecided -> Created(job_id)` | Uses existing Job/JobStep; resolver is new Application logic |
| **JobStep kind** (backup-volume / backup-selective / os-install / debloat / driver-install / restore / validate / **operator-intervention**) | **Domain** `JobStep` | Durable field on `JobStep` | **New durable field** (MI-2) — introduced incrementally, each concrete action by its own Specification |
| **Operator-intervention checkpoint** | **Domain** `JobStep` (a kind) + a suspended/blocked JobStep semantic | Durable JobStep state; Job stays `Running`, keeps its Endpoint-exclusivity lease | **New JobStep semantic** (MI-3) |
| **Preservation-sufficiency precondition** | **Domain**/workflow — a declared precondition on a destructive JobStep | Durable as part of the resolved plan + evaluated at final pre-dispatch | **New precondition category** (MI-4), composes with the seven-gate, never narrows it |
| **Resolved selective capture set** | **Application/Domain** bound to the transfer JobStep / `Transfer` (peer of immutable `SourceProvenance`) | Durable, immutable once capture begins; carries discovery-evidence reference + timestamp | **New durable descriptor** (MI-5) |
| **Targeting intent -> target re-resolution** | **Domain**/workflow at/after the intervention checkpoint | Durable: a fresh destructive-authorization snapshot for the re-resolved `TargetFingerprint` | **New re-resolution step** (MI-6) |

### Relationship to Job / JobStep / Attempt

- **Job**: unchanged. Still one Endpoint, still a linear ordered `JobStep` list. A "composed
  service" for a target = that target's Job with more JobSteps.
- **JobStep**: gains a durable `kind` and per-kind precondition/postcondition hooks.
  **Optionality is resolved at creation** — an "optional" step that the operator disabled for
  LAB-02 simply *is not a JobStep in LAB-02's Job*. This deliberately avoids ADR-0006's
  unspecified skip/branch semantics. Conditional-on-earlier-result steps (e.g. restore only if
  backup verified) are expressed as a **precondition on the later JobStep**, not runtime
  branching.
- **Attempt**: entirely unchanged. Retry policy still per-kind (destructive kinds: no automatic
  retry, F16).
- **ADR-0019**: preserved. Independent per-target Jobs, non-atomic bulk creation, partial
  acceptance, `submission_id` correlation. Only the *descriptor content* grows.

### Relationship to Artifact / Transfer

- Volume/Image and Selective both flow through the **existing** Artifact/Transfer/chunk-manifest/
  transfer-authorization/capture-consistency contracts — unchanged.
- A backup JobStep produces one or more Artifacts (see section 8). A restore JobStep **consumes
  verified Artifact identities** plus, when selective, the resolved capture set for destination
  mapping.
- Source != target identity (F11) is relied on directly for HDD->SSD.

### Explicitly NOT introduced

- No durable Domain `Operation` / `ServiceIntent` / `OperationPlan` / `WorkflowTemplate`
  aggregate.
- No aggregate-level progress, outcome, status, or cancellation authority (bulk cancel stays
  Application fan-out keyed by `submission_id`, per ADR-0019).
- No DAG / parallel JobSteps / runtime skip / runtime branching.
- No commercial vocabulary anywhere in Core (F18).
- No final backup/snapshot format, no production heuristic catalog, no path schema, no
  WWN/serial/GPT schema — all deferred (F12, Issue #45 out-of-scope).
- No break-glass / bypass for preservation-sufficiency (none exists in current authority; do not
  invent).

---

## 8. Selective backup model

**Volume/Image vs Selective (F6)** — both are `backup` JobStep *kinds*, not a boolean.
`backup=true` is never a universal semantic; a target either has a `backup-volume` JobStep, a
`backup-selective` JobStep, both, or neither.

**Manual selection** — the operator supplies explicit paths/roots. The selection is **transient
UI editing state** until submission, then part of the **immutable submitted operator command**
(submission descriptor), then a **resolved capture set** bound to the transfer JobStep before
capture starts.

**Assisted discovery** — Bamep inspects the offline filesystem and **proposes** preservation
groups (user profiles, Desktop/Documents/Pictures/Downloads, savegames, app-data, user archives,
project dirs, large personal files) and **suggests** exclusions (system, caches, reproducible
software). Governing principle, to be recorded normatively:

> Assisted discovery may suggest. It must never silently override explicit operator preservation
> intent. Explicit inclusion wins over any automatic exclusion heuristic. File extension alone is
> never preservation authority (`.exe`/`.dll` may be portable apps, mods, customer binaries,
> irreplaceable content).

Flow: `inspect -> classify/suggest -> operator reviews -> operator modifies -> resolve final
capture set -> capture -> verify`. Heuristic = advisory input only. Operator review + resolution
= authority. Resolved capture set + Artifacts + verification = evidence.

**Resolved selection (MI-5)** must retain enough to later answer: (a) what the operator asked to
preserve; (b) what discovery evidence + timestamp it was based on; (c) what Bamep resolved that
into (concrete roots, required/optional flags per group); (d) which Artifact(s) it maps to; (e)
what verified. This is a **durable descriptor bound to the transfer JobStep / `Transfer`**, a
peer of immutable `SourceProvenance` — **not** a new Domain aggregate.

**Artifact granularity — recommend Model B (independent Artifact per operator-meaningful
preservation group), not per file, not one monolith.**

| Trade-off | Model A (1 Artifact) | **Model B (N Artifacts by group)** |
|---|---|---|
| Atomicity | Whole backup is all-or-nothing (F7) | Each group atomic; groups independent (F8) |
| Partial success | Impossible — one failing file fails everything | Natural — `Documents ok / Pictures ok / LegacyProject failed` |
| Retry | Re-capture everything | Re-capture only the failed group |
| Failure isolation | None | Strong |
| Required/optional (MI-4) | Cannot express per-group | Directly expressible per Artifact |
| Restore | One unit | Restore per verified group; skip failed |
| Operator comprehension | "backup failed" | "3 of 4 groups preserved" |
| Retention | 1 object | N objects (bounded by group count, not file count) |
| Durable object count | Lower | Higher but bounded and meaningful |

Choose B **because MI-4 requires per-group required/optional outcomes anyway**, not for UI
convenience. Bound the split to operator-meaningful groups; a single manual selection with no
grouping is one Artifact.

**Required vs optional / partial outcomes (MI-4)** — policy lives in the **destructive JobStep's
preconditions** (composed by the workflow, additive to the seven-gate, never narrowing it):

- every **required** preservation group's Artifact `Verified` (+ `capture_consistency ==
  Established` where applicable) -> destructive continuation permitted;
- any **required** group not provably preserved -> **fail closed**, JobStep does not dispatch;
  operator sees which group and why;
- **optional** group failure -> workflow may proceed; operator is explicitly informed the
  optional group was not preserved.

No bypass flow (not in current authority).

**Stale selection (MI-5, Question 11)** — F10 establishes source stability *during capture*; it
does **not** cover the gap between discovery/resolution and capture start. Minimum invariant:
*discovery and capture should occur within the same offline read-only maintenance session; the
resolved capture set records the discovery-session reference; if capture cannot be proven to run
against the same offline session/source state, re-discovery is required before capture, and
explicit paths that no longer resolve fail closed for required groups.* The precise mechanism
(session token, source-version evidence, re-enumeration) is a **Spike** question (section 15) —
do not design it here.

**Restore correlation (Question 12)** — a selective restore JobStep consumes **verified Artifact
identities** for the groups to restore, plus the **resolved capture set** for destination
mapping. It does not need a higher-level "preservation-set aggregate" identity if the resolved
capture set + Artifact IDs are durably linked. Destination-path determination after a clean OS
install, user-profile SID changes, and reparse/ADS/ACL handling are **Spike / later-WP**
questions, not Discovery.

---

## 9. Scenario A walkthrough — routine formatting service

**Operator-facing**: selects `LAB-01..LAB-03` -> "New operation" -> picks service
*"Reinstalacao Windows"* -> sets common defaults (Windows edition, debloat profile, driver
policy) -> per-Endpoint overrides (LAB-01: backup volume + restore; LAB-02: clean install;
LAB-03: selective backup + restore) -> Review (shows **resolved plan per target**) -> **Enviar
operacao**.

**Immutable submission boundary**: on `Enviar`, one operator submission is durably accepted —
`request_key` (client-provided) + `submission_id` (Server-minted) + requested target set
`{LAB-01,LAB-02,LAB-03}` + canonical descriptor `{service intent, common defaults, per-target
override deltas}` + three `Undecided` per-target creation states. **No Job exists yet**
(persistence spec "Acceptance ordering").

**Target-specific resolved configuration**: the resolver expands the descriptor per target into
an ordered JobStep list:

- LAB-01: `backup-volume -> os-install -> debloat -> driver-install -> restore -> validate`
- LAB-02: `os-install -> debloat -> driver-install -> validate`
- LAB-03: `backup-selective -> os-install -> debloat -> driver-install -> restore-selective ->
  validate`

**Job creation**: per target, `Undecided -> Created(job_id)` commits **atomically** with that Job
+ its JobSteps (persistence spec "Atomic target creation"). If LAB-02 is rejected (e.g. not
`Enrolled`), it commits `Rejected(reason)`; LAB-01/03 still get Jobs. Partial acceptance (F4).

**Optional-step handling**: "optional" was resolved away — LAB-02 simply has no backup/restore
JobStep. "Restore only if backup verified" is a **precondition on the restore JobStep** (MI-4
style), not runtime branching.

**Artifact participation**: `backup-volume` (LAB-01) -> one Artifact via existing
Transfer/chunk-manifest/authorization/verification. `backup-selective` (LAB-03) -> N Artifacts by
preservation group (section 8). `os-install` on each is a **destructive** JobStep — at final
pre-dispatch it revalidates the complete seven-gate (F15) **plus** the preservation-sufficiency
precondition (required backup Artifacts `Verified`).

**Failure behavior**: LAB-01 backup Artifact `Failed` -> `os-install` precondition fails closed
-> LAB-01's Job fails at that JobStep; LAB-02/03 unaffected (independent Jobs, F17). A
non-destructive step (`driver-install` if classified non-destructive) may bounded-retry;
destructive steps never auto-retry (F16).

**Cancellation / restart**: bulk cancel = Application fan-out to each Job's cancellation path
keyed by `submission_id` (ADR-0019). Server restart mid-creation -> `Undecided` targets resume,
`Created`/`Rejected` are never re-evaluated. Server restart mid-execution -> per-Job
reconciliation (F16); destructive `Indeterminate` needs explicit operator decision.

---

## 10. Scenario B walkthrough — HDD -> SSD replacement

**Setup**: operator configures LAB-01 service = `backup-volume (required) -> operator-intervention
-> os-install -> restore -> driver-install -> validate`. Submission accepted; LAB-01 Job created
with those JobSteps.

1. **`backup-volume`** runs (non-destructive, offline capture) -> Artifact `Verified`,
   `capture_consistency == Established`. Provenance = old disk (immutable `SourceProvenance`,
   F12).
2. **`operator-intervention` checkpoint (MI-3)**: JobStep enters the **suspended** semantic. Job
   stays `Running`; Endpoint-exclusivity lease **and** the commercial-capacity slot remain held
   (owner note in section 17). Operator sees "Aguardando substituicao de disco em LAB-01".
3. **Technician replaces HDD with SSD.** Endpoint powers off / reboots -> Agent session drops; on
   return: genuine reboot -> new `BootContext`, `trusted_bootstrap` reset to `NotEstablished`;
   fresh credential chain `E2->...`; **durable Endpoint identity persists** (`Enrolled`, F13);
   disk change -> hardware confidence `LoweredConfidence` (meaningful change) or `Conflict`.
4. **Server may restart while waiting** — the suspended JobStep state is durable, no live
   Attempt, so it simply stays suspended; lease persists.
5. **Endpoint returns** -> recognized as the **same Endpoint** via Server-assigned identity +
   credential-chain lookup (F13), *not* via MAC/disk (F14).
6. **Resume past the checkpoint requires BOTH** (MI-3):
   - (a) recorded operator authorization ("continuar") — an audited `OperatorDecisionRecorded`;
   - (b) re-established machine facts: fresh inventory revision observed and adopted
     (precondition 4); hardware confidence explicitly resolved back to `Consistent` by an audited
     operator review (precondition 6 — this is a *separate* audited decision, not implied by
     "continuar"); trusted current bootstrap re-`Established` via independent Server verification
     (precondition 7); authenticated Agent session present (precondition 2).
7. **Target re-resolution (MI-6)**: the destructive `os-install` target is re-resolved from
   targeting intent ("primary system disk") to a concrete `TargetFingerprint` against **current**
   inventory; a **fresh** destructive-authorization snapshot is created (the pre-swap snapshot is
   void).
8. **`os-install`** dispatches only when the complete seven-gate (F15) holds against post-swap
   reality + preservation-sufficiency (backup Artifact still `Verified`). Source (old HDD) !=
   target (new SSD) is **expected and allowed** (F11).
9. **`restore`** consumes the verified Artifact; `driver-install`; `validate`; completion
   evidence via domain events + audit.

**Negative cases (all fail closed):**

- *Wrong Endpoint returns* -> credential-chain lookup resolves to a different Endpoint or fails;
  the suspended Job's Endpoint never re-authenticates; no resume.
- *Same Endpoint, HDD not replaced* -> hardware confidence returns `Consistent` unchanged /
  target fingerprint == old; operator's "continuar" cannot satisfy target re-resolution to a
  *new* disk if that's what the plan requires -> precondition fails, or the operator explicitly
  cancels. (If plan doesn't strictly require a new disk, it proceeds against the same disk —
  that's a legitimate operator choice, but the intervention step's expected-change assertion
  should be surfaced.)
- *Replacement SSD too small* -> capability/capacity precondition on `os-install` (new declared
  precondition) fails closed.
- *Two plausible target disks* -> target re-resolution is ambiguous -> fail closed, operator must
  disambiguate.
- *Insufficient inventory confidence* -> precondition 4 (fresh inventory) / 6 (hardware
  confidence) not satisfiable -> fail closed.
- *Endpoint never returns* -> checkpoint has a bounded abandonment/timeout that composes with
  existing Job cancellation -> Job -> `Cancelled` (or `Failed`), Endpoint-exclusivity lease
  released **only** at terminal state (F17); no destructive work ever ran.
- *Server restarts during intervention* -> step 4, safe.
- *"Continuar" pressed too early* -> operator authorization recorded, but (b) machine-fact
  re-establishment fails -> JobStep stays suspended / dispatch blocked. A single click **cannot**
  authorize destruction because preconditions 4–7 are independent and fail-closed (F15) and
  hardware-confidence resolution is its own audited decision.
- *Unrelated critical hardware also changed* -> hardware confidence escalates to `Conflict`
  (breaks identity continuity) or `LoweredConfidence` -> precondition 6 fails until an audited
  operator review; destructive continuation blocked.

---

## 11. Scenario C walkthrough — selective / assisted customer-data backup

**Setup**: LAB-01 service = `backup-selective -> os-install -> restore-selective -> validate`.
Endpoint boots the offline maintenance environment.

1. **Inspect offline filesystem** (non-destructive, read-only, F10).
2. **Manual selection and/or suggestions**: operator picks `C:\Users\Cliente\Desktop`,
   `C:\Users\Cliente\Documents`, `D:\Projetos`, `C:\Games\SomeGame\save`; Bamep *suggests* adding
   the full user profile and *suggests* excluding `C:\Windows`, caches, and `C:\PortableApps`
   (flagged "reproducible software").
3. **Operator reviews / edits**: keeps `C:\PortableApps` **explicitly** (it's a customer-modded
   toolset) -> **explicit inclusion overrides the exclusion heuristic** (section 8 principle).
   Marks `Documents` + `Desktop` + `Projetos` **required**, `SomeGame\save` **optional**.
4. **Resolve final capture set (MI-5)**: durable descriptor bound to the transfer JobStep —
   operator request, discovery-session reference + timestamp, resolved roots with required/optional
   flags, group -> planned-Artifact mapping (Model B: `Documents`, `Desktop`, `Projetos`,
   `PortableApps`, `SomeGameSave`).
5. **Estimate** selected data size (Spike-grade accuracy caveat, section 15).
6. **Capture** -> one Artifact per group via existing
   Transfer/chunk-manifest/authorization/verification contracts.
7. **Verify** every **required** Artifact.
8. **Decide destructive continuation** (MI-4): all required groups `Verified` + `capture_consistency
   == Established` -> `os-install` precondition satisfied.
9. **Later `restore-selective`**: consumes verified Artifact identities + resolved capture set for
   destination mapping.

**Required negative/edge cases:**

- *Manual arbitrary folders* -> step 2, fully supported; no whole-volume assumption.
- *Heuristic disagreement* -> step 3, `C:\PortableApps` preserved; heuristic is advisory only.
- *Misleading extension* -> a `.exe` portable app / `.dll` mod inside a selected root is captured
  because the **root was explicitly selected**; no extension-only rule may drop it.
- *Partial required failure* -> `Projetos` Artifact `Failed` -> `os-install`
  preservation-sufficiency precondition **fails closed**; destructive work blocked; operator told
  exactly which required group failed. The **workflow/JobStep** owns this decision, not the
  Artifact (Artifact only knows *it* failed, F7).
- *Optional failure* -> `SomeGameSave` Artifact `Failed`, all required `Verified` -> workflow
  **may proceed**; operator explicitly informed the optional group was not preserved.
- *Source changes between discovery and capture* -> F10 covers stability *during* capture; the
  discovery->capture gap needs MI-5's same-offline-session invariant. If capture can't be proven
  against the same offline session, re-discovery is required; explicit paths that vanished fail
  closed for required groups. Intra-capture byte mutation already fails closed via immutable
  chunk identity (F12) — no new mechanism there.
- *Captured set differs from proposal* -> the resolved capture set records both the discovery
  evidence and the operator-resolved selection, so the delta is reconstructable.
- *Evidence — "what did Bamep preserve?"* -> answerable from: resolved capture set (requested +
  resolved) + per-group Artifact states (`Verified`/`Failed`) + `capture_consistency` + domain
  events (`ArtifactCreated`/`ArtifactVerified`) + audit. Not merely "Backup completed."

---

## 12. Open Core / commercial boundary

**What Core knows (generic facts only):**

- **Execution capacity** — a generic numeric `ExecutionCapacityPolicy` limiting simultaneously
  active Job-scoped Endpoint-exclusivity leases (Jobs in `Running`/`Cancelling`). One serviced
  machine = one Endpoint = one Job = one active lease, **regardless of how many JobSteps the
  composed service has**. "Service up to 4 machines concurrently" = `max_active_endpoint_jobs =
  4` — **exactly ADR-0015 section 6, no change**.
- **Capability availability** — a generic `CapabilitySet` / `has_capability(CapabilityId)`.
  Assisted-discovery, or an expanded provisioning-action catalog, *could* be gated by a generic
  capability id (e.g. `backup.assisted-discovery`) if the commercial layer wants that — Core only
  evaluates the technical capability contract, never *why*.
- **Generic resource constraints** — existing Attempt-scoped leases (network, storage,
  CPU/Worker), unchanged.

**What Core must NOT know:** `Bamep4`/`Bamep8`, SKU, edition names, customer, contract,
subscription, price, ERP tenant — no enum, no branch, anywhere in Domain/Application/Runtime
Services (ADR-0015 section 1, section 5; `m0-stack-and-boundaries-baseline.md`).

**Distinctions to keep separate:**

| Term | Meaning | Owner |
|---|---|---|
| Capability | technical feature is present/enabled | Core evaluates generic `CapabilityId` |
| Entitlement | *why* a capability/capacity is granted | commercial platform (translated to generic facts) |
| Compatibility | Endpoint hardware/firmware can run the workflow | Core (inventory, preconditions) |
| Eligibility | this Endpoint may be targeted now (`Enrolled`, not `Retired`, confidence OK) | Core (Endpoint lifecycle) |
| Resource capacity | physical network/storage/CPU headroom | Core (Attempt-scoped leases) |
| Operator authorization | this operator/action is permitted | future Core IAM (not this Discovery) |

**Hot-path rule preserved**: the commercial platform is never required online during destructive
execution; missing/expired/invalid entitlement fails closed for *new* admission only and never
terminates active Jobs (ADR-0015 section 11). Composed services and intervention checkpoints do
not change this — a Job parked at a checkpoint keeps its slot but is never killed by entitlement
changes.

**Commercial translation `Bamep4 -> generic facts` is the commercial layer's job. Do not design
SKU/pricing mapping here (Issue #45 out-of-scope).**

---

## 13. Future before/after evidence seam

**Recommendation: introduce no new durable storage now.** Confirm that the operational evidence
Core already produces for its own reasons is sufficient raw material for a future report layer,
and stop there.

| Category | Already justified by Core behavior? | Action |
|---|---|---|
| Hardware inventory before/after | Yes — inventory is durable on revision change; `InventoryRevisionRecorded` event (persistence spec "Inventory persistence") | None — already captured |
| Workflow outcomes / step results | Yes — `JobStarted`/`JobSucceeded`/`JobFailed`/`JobStepFailed`, Attempt states, audit | None |
| Timings | Partially — `occurred_at` on domain events + audit gives coarse timing | None now; finer timing is a future-report concern |
| Artifact / preservation evidence | Yes — Artifact lifecycle, `ArtifactCreated`/`ArtifactVerified`, resolved capture set (MI-5) | None beyond MI-5 |
| Storage health / SMART / diagnostics | **No** — Core has no current reason to collect this | **Do not add.** This is exactly `docs/discovery/architecture-redesign.md` "Future: pre/post provisioning diagnostics" — its own future Discovery |
| Benchmarks / performance deltas | No | Same — future Discovery |
| Customer-facing presentation / branding / business logic | No — commercial | Out of Core entirely |

**Principle to record**: Bamep may preserve neutral operational facts *that Core already needs*;
it must not store arbitrary data speculatively because a report might use it. The Job/JobStep
model is expected to remain forward-compatible with `Preflight -> ... -> Postflight -> Report`
**without** adding JobStep types or changing the lifecycle contract *until that future work is
approved* (F22).

---

## 14. M2 UX impact

**The accepted mental model `Endpoints -> New operation -> Review -> Send -> Monitor` remains
valid.** No redesign. Product implications only:

1. **"New operation" grows from one uniform intent to**: `service intent + common defaults +
   per-Endpoint overrides`. This is a configuration-surface evolution, **not** a new architecture
   — it mirrors C2's richer submission descriptor. #41 (Endpoints list) is unaffected. #42/#43's
   *structure* (targets visible throughout, intent-first, config -> Review separation, "Enviar
   operacao" != guaranteed execution) all still hold; only the payload is richer.
2. **Review** must show the **resolved per-target plan** (which optional steps, which config per
   Endpoint), not just one shared intent — because per-Endpoint divergence is now real and must
   not be normalized away (consistent with #43's "must not silently normalize" constraint).
3. **Selective / manual / assisted data selection** belongs in a **focused sub-flow / drawer
   inside New operation**, before submission — because that is where authoritative configuration
   becomes immutable (the submission boundary). It should **not** be a post-submit step: the
   resolved capture set is part of the immutable operator command. Assisted discovery *requires
   the Endpoint to be in the offline maintenance environment*, which may mean the selection
   sub-flow is a **two-phase** interaction (configure intent now -> Endpoint boots maintenance
   env -> operator returns to resolve selection -> then the destructive steps proceed). This
   two-phase reality is itself a product-design question for a future UX WP, flagged here, not
   solved.
4. **Human-intervention checkpoints** surface in **Monitor** as a first-class "waiting for you"
   state per Endpoint — distinct from "in progress" and "failed."
5. **Commercial capacity** ("Bamep4 -> 4 concurrent") surfaces as a generic "N of 4 active"
   indicator; never as an edition name in Core-served data.

### Recommendation for #44: Option B

> #44 keeps its invariant, but its prototype scenario should change from `Capturar imagem do
> sistema` to a representative composed formatting-service scenario.

**Why B (not A, C, or D):**

- **Not D** — #44's invariant (one submitted request -> independent per-Endpoint *creation*
  outcomes; partial acceptance; no aggregate `Operation` status; creation-acceptance != execution
  success) is **exactly right** and is *strengthened*, not weakened, by this Discovery. It is
  still the correct next WP after #43.
- **Not A** — proceeding unchanged would entrench `Capturar imagem do sistema` as the reference
  mental model across the *fourth* consecutive prototype. The post-submit result screen recaps
  "what was submitted"; if that recap is a single uniform intent, it silently trains the product
  on a model the primary workflow contradicts (composed service + per-Endpoint overrides). The
  result screen should recap a *service* ("Reinstalacao Windows — Laboratorio A") with per-target
  resolved plans.
- **Not C (narrow scope adjustment)** — the change isn't a scope trim; it's a scenario
  substitution. Same surface, same acceptance criteria, same UX questions; only the mock request
  becomes representative (e.g. targets `LAB-01` backup+restore / `LAB-02` clean install / `LAB-03`
  selective backup; outcomes `2 accepted + 1 not accepted`).
- **B** gives #44 a representative scenario at **zero invariant cost** and prevents a fifth
  prototype (Operation Detail/Monitor) inheriting the wrong frame.

**Do not mutate #44** — this is a recommendation for owner action.

---

## 15. Technical Spike needs

**Recommend exactly one narrow Spike now**, plus one clearly-scoped deferral.

**Spike S-1 — Offline NTFS selective discovery/capture feasibility** (blocks the Selective
Specification delta, not the composition model):

- offline NTFS enumeration from the Linux maintenance environment (tooling, reliability);
- path representation when the filesystem is offline / not mounted at a Windows drive letter;
- reparse points / junctions / symlinks / mount-like boundaries — traversal and capture
  semantics;
- ACL / alternate data streams / EFS — what is preserved vs. lost in offline file-granular
  capture;
- user-profile discovery (locating `C:\Users\*`, distinguishing real users from service/default
  profiles);
- selected-data size estimation accuracy;
- restore onto a *fresh* Windows install — destination path/SID mapping reality;
- detecting source change between discovery and capture (same-offline-session evidence).

Scope guard: S-1 answers *"what must the resolved-capture-set descriptor and the Selective
Artifact contract know, and what is unsafe to promise"* — it is **not** a production heuristic
catalog and **not** a restore engine.

**Deferral D-1 — independently re-observed physical disk/hardware identity** (WWN/serial/GPT/
composite `SourceIdentity` / `TargetFingerprint` schema): already deferred by F12 to the future
physical-disk/hardware-integration milestone. MI-6 (target re-resolution) can be specified at the
*invariant* level now (re-resolve, fresh snapshot, ambiguity fails closed) without picking the
schema. Do not pull D-1 forward.

**Not a Spike**: the composition model (section 7), human-intervention checkpoint invariant
(MI-3), preservation-sufficiency policy (MI-4), commercial boundary (section 12) — these are
architecture-reasoning + Specification/ADR work, decidable without new empirical evidence.

---

## 16. Required durable changes (classification only — none created or edited here)

| # | Recommendation | Classification |
|---|---|---|
| R1 | No durable Domain `Operation`/`ServiceIntent`/`OperationPlan`/`WorkflowTemplate` aggregate | **No durable change** — reaffirms ADR-0019 |
| R2 | Composed service = per-Endpoint linear Job with resolved JobSteps; optionality resolved at creation; conditionality as JobStep preconditions | **No durable change** to the Job model; **Specification delta** to `m0-job-lifecycle-and-scheduling.md` to state that optional/conditional steps are creation-time resolution, not runtime skip/branch |
| R3 | Submission descriptor extended: `service intent + common defaults + per-target override deltas + resolved per-target plan`; resolved plan commits atomically with `Created(job_id)` | **Specification delta** to `m0-persistence-observability-and-domain-events.md` "Operator submission persistence and correlation"; **ADR-0019 amendment** if "resolved per-target plan as durable submission content" is judged a genuine decision |
| R4 | `JobStep.kind` durable field + per-kind precondition/postcondition hooks (MI-2) | **ADR** (introduces a durable classification with alternatives) + **Specification delta** to `m0-job-lifecycle-and-scheduling.md`; each concrete action type still introduced by its own Specification + **later implementation WPs** |
| R5 | Operator-intervention checkpoint: suspended-JobStep semantic; resume requires recorded operator authorization **and** re-established machine-fact preconditions (MI-3) | **ADR** + **Specification delta** to `m0-job-lifecycle-and-scheduling.md` (JobStep lifecycle) and `m0-endpoint-identity-lifecycle.md` (which preconditions must be re-established) |
| R6 | Post-hardware-change destructive-target re-resolution: targeting intent -> concrete `TargetFingerprint` against current inventory; fresh authorization snapshot; ambiguity/capability-shortfall fail closed (MI-6) | **Specification delta** to `m0-endpoint-identity-lifecycle.md` (precondition 5) + `m0-job-lifecycle-and-scheduling.md`; this is the concrete shape of the already-listed "planned hardware-change authorization workflow" (data-plane out-of-scope). Possibly folded into the R5 ADR |
| R7 | Preservation-sufficiency precondition on destructive JobSteps: required groups provably `Verified`(+`Established`); optional-group failure informational; fail closed by default; no bypass (MI-4) | **Specification delta** to `m0-data-plane-and-storage-contracts.md` "Destructive-use composition" + `m0-job-lifecycle-and-scheduling.md` "Destructive dispatch" |
| R8 | Selective backup: `backup-volume` / `backup-selective` JobStep kinds; Model B (independent Artifact per operator-meaningful preservation group); resolved capture set as a durable immutable descriptor bound to the transfer JobStep / `Transfer` (peer of `SourceProvenance`); required/optional flags; discovery-evidence + timestamp; same-offline-session invariant (MI-5) | **Specification delta** to `m0-data-plane-and-storage-contracts.md` "Backup strategy boundary" + "Artifact lifecycle" (partial-success workflow policy) + **Spike S-1** for the empirics that bound it |
| R9 | Assisted-discovery authority principle: advisory only; explicit operator inclusion overrides exclusion heuristics; extension alone is never authority; `inspect -> suggest -> review -> modify -> resolve` | **Specification** (thin, principle-level, in the Selective section from R8); production heuristic catalog explicitly **deferred**; capability-gating already covered by ADR-0015 |
| R10 | Selective restore correlation: restore JobStep consumes verified Artifact identities + resolved capture set; destination mapping empirics | **Specification delta** (R8 doc) at invariant level; destination/SID/reparse mechanics deferred to **Spike S-1** / **later WP** |
| R11 | Commercial boundary for composed services: one Endpoint Job = one capacity slot regardless of JobStep count; checkpoint-parked Jobs hold their slot; entitlement never in destructive hot path | **No durable change** — ADR-0015 already sufficient; at most a one-line cross-reference clarification |
| R12 | Before/after evidence: no new durable storage now; confirm existing events/audit/inventory suffice; storage-health/diagnostics/benchmarks remain a separate future Discovery | **No durable change** now |
| R13 | Independently re-observed physical disk/hardware identity schema | **Deferred** to the future physical-disk/hardware-integration milestone (D-1); not this Discovery |
| R14 | #44 prototype scenario -> representative composed formatting service (Option B) | **GitHub WP scope note** (owner action on #44); no repository durable change |

**Sequencing suggestion for owner**: R1/R2/R3 (composition model + submission descriptor) and
R5/R6 (intervention + target re-resolution ADR) are decidable now and unblock the M2 operator
plane. R4 (JobStep kinds) is the umbrella for R7–R10. R8's *implementation* depends on Spike
S-1. None of this blocks #44 proceeding under Option B.

---

## 17. Owner decisions required

1. **Accept C2 (Application-level service composition; no new Domain aggregate) as the
   direction?** — or request a different candidate from section 6.
2. **Authorize a Specification delta + (likely) ADR-0019 amendment** to extend the
   operator-submission descriptor to `service intent + common defaults + per-target overrides +
   resolved per-target plan` (R3)?
3. **Authorize an ADR** for the operator-intervention checkpoint + post-hardware-change target
   re-resolution invariants (R5 + R6), covering: what is durable, what a bare "Continue" does
   *not* authorize, checkpoint timeout/abandonment composition with Job cancellation, and the
   fact that a checkpoint-parked Job **retains its Endpoint-exclusivity lease and its
   commercial-capacity slot**?
4. **Authorize an ADR** introducing `JobStep.kind` as a durable classification (R4), with
   concrete action types added incrementally per existing Specification rules?
5. **Authorize Technical Spike S-1** (offline NTFS selective discovery/capture feasibility,
   section 15), scoped to bound the Selective Specification delta and explicitly excluding a
   production heuristic catalog and restore engine?
6. **Confirm Model B** (independent Artifact per operator-meaningful preservation group) as the
   Selective direction, with the required/optional preservation-sufficiency policy owned by the
   destructive JobStep and **fail-closed with no bypass** (R7/R8)?
7. **Confirm #44 proceeds under Option B** — same invariant and surface, representative
   composed-service scenario — and that this Discovery does **not** block #44?
8. **Confirm the commercial boundary needs no ADR-0015 change** — only, at most, a one-line
   clarification that composed services and checkpoints do not change the "active Endpoint Job =
   one capacity slot" semantic (R11)?
9. **Confirm before/after reporting stays a separate future Discovery** and this Discovery adds no
   speculative evidence storage (R12)?

---

## Resumo em pt-BR

**Conclusao central**: o modelo `Job / JobStep / Attempt` **ja suporta** servicos tecnicos
compostos — um servico = um `Job` linear por Endpoint com mais `JobStep`s, resolvidos no momento
da criacao (passos "opcionais" simplesmente nao existem no Job daquele Endpoint; passos
"condicionais" viram pre-condicoes). **Nenhum agregado novo de Dominio e necessario** (ADR-0019
ja rejeitou `Operation` duravel e continua valido).

**O que realmente falta** (invariantes, nao nomes bonitos): (MI-2) `JobStep` nao tem "tipo"
duravel; (MI-3) nao ha checkpoint duravel de intervencao humana — retomar exige autorizacao
registrada **e** re-estabelecimento dos fatos de maquina, um clique em "Continuar" nao basta;
(MI-4) politica de "backup suficiente para destruir" pertence ao workflow, nao ao Artifact, e
falha fechada; (MI-5) falta um "conjunto de captura resolvido" duravel ligando pedido do operador
-> descoberta -> selecao -> Artifacts -> verificacao; (MI-6) apos troca de disco, o alvo
destrutivo precisa ser re-resolvido (intencao -> `TargetFingerprint`), com ambiguidade falhando
fechada.

**Backup seletivo**: `backup-volume` e `backup-selective` como *tipos* de JobStep (nao um
booleano); descoberta assistida e **apenas sugestao** — inclusao explicita do operador sempre
vence heuristica de exclusao; extensao de arquivo nunca e autoridade; recomendo **um Artifact por
grupo de preservacao** (Modelo B), com grupos obrigatorios/opcionais.

**Fronteira Open Core / comercial**: ADR-0015 ja resolve tudo. `Bamep4` -> `max_active_endpoint_jobs
= 4` generico. Um Endpoint em servico = um Job = um slot, independentemente de quantos passos.
Nada de SKU/edicao no Core.

**#44**: **Opcao B** — mesma invariante e mesma tela, mas trocar o cenario mock `Capturar imagem
do sistema` por um servico composto representativo. Nao bloqueia #44.

**Mudancas duraveis**: nenhuma criada. Recomendo: 1 Spike (S-1, NTFS offline), 2 ADRs (checkpoint
+ re-resolucao de alvo; `JobStep.kind`), e deltas de Specification em job-lifecycle, persistence
e data-plane. Detalhes e classificacao na secao 16; decisoes do owner na secao 17.

---

*Discovery complete. No files (other than this investigation document), Git state, or GitHub
state were changed. Awaiting owner review.*
