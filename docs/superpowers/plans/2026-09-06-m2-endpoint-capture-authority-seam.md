# M2 Endpoint Capture Authority Seam Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task after owner approval. Steps use checkbox syntax for tracking. Do not implement, commit, publish, or change GitHub state merely because this plan exists.

**Goal:** Implement Issue #62's Server/Application seam for validated, atomic creation and fresh dispatch of one M2 endpoint-capture target.

**Architecture:** Add one capture-creation Port using the repository's lock/read/decision/persist pattern and transaction-local PostgreSQL helpers. Reuse the existing Job scheduling and non-destructive Transfer dispatch path, with an obligatory freshness check for structured M2 provenance. Preserve M1 descriptive provenance and its five-field action, and construct M2's six-field action from the committed Transfer through the existing outbound service.

**Tech stack:** Existing Rust 2021 Cargo workspace; Domain value types and pure decisions; Server Application/Ports; PostgreSQL with SQLx 0.9 runtime-checked queries and embedded migrations; Tokio, async-trait, serde/serde_json, uuid, base64, thiserror. No new dependency, crate, HTTP surface, or Agent implementation.

**Spec:** `docs/specifications/m2-endpoint-capture-service-intent-and-source-reference-contract.md` (Approved), RF-2/RF-4/RF-5/RF-6/RF-7 dispatch construction. RF-3/RF-8 are compatibility constraints, not Agent implementation scope.

## Global constraints

- Planning baseline: `2bb4a1d978c58c1f1940679a27261016cdc221e3`; initial `git status --short` was empty.
- This is a proposed execution plan, pending owner review. Revalidate HEAD, worktree, instructions, and Issue #62 before execution. Stop for unexpected tracked dirt.
- "Exactly one concrete source per target." No implicit source, path interpretation, enumeration-order selection, or persistent physical identity.
- "No `JobStep.kind`" and no durable Operation/ServiceIntent/catalog aggregate.
- "`bamep.m1.data-plane-transfer` v1 is not extended in place."
- "`SourceProvenance` is historically immutable and is never destructive target identity."
- Source failure at creation means `Rejected(SourceReferenceStale)` without capture resources; structural input errors remain distinct from staleness. No Administrative API rejection string is selected here.
- Capture is non-destructive. Preserve existing enrollment eligibility, ordinary scheduling, resource leases, and unresolved-Attempt checks. Do not import or weaken the destructive gate.
- No Attempt/action at target creation. Failed final freshness revalidation returns the step to Pending and releases newly acquired Attempt reservations.
- Domain/Application never accept `PgPool`, SQLx transactions, or row types. Only PostgreSQL adapters share transaction-local SQL helpers.
- No Administrative API/submission envelope, collection/source-selection read, Web, physical source access, Agent package/action handler, SOURCE_REFERENCE_STALE Agent handling, restore, Selective, reinstall, drivers, debloat, IAM, or lab validation.
- Use real disposable PostgreSQL for atomicity and locking claims. No production/physical targets; no fake transaction test accepted as persistence evidence.
- No staging, commits, branch changes, GitHub changes, or publication without explicit task-specific authorization. Review checkpoints below are local reviews, not implicit permission to commit.

## Grounding and architectural feasibility

Authoritative inputs inspected: Issue #62 in full; #59 body and owner approval/final status comments; #61 final Outcome B comment; AGENTS.md; development SDD, workflow, documentation, testing and persistence policies; ADR-0019; the Approved M2 Specification; M0 Agent Protocol, Job lifecycle, persistence, and data-plane contracts; M1 RF-005; relevant implemented Architecture and code/tests below.

#61 demonstrated physical read/data-plane behavior through M1, not the missing M2 authority. No physical repetition belongs here.

### RF-6 fits one WP

`JobService::create_workflow` calls a transaction-owning Job repository. `TransferService::create_transfer_context` calls a different transaction-owning Transfer repository. Calling these in sequence is insufficient.

However, `PostgresJobRepository::commit_transfer_dispatch` already calls `transfer_repository::load_locked_facts` and `persist_attempt_binding` with the **same** transaction. Capture creation can follow exactly that pattern: one new purpose-specific Port, one adapter transaction, and reusable insert helpers that neither begin nor commit. No generic unit-of-work framework or cross-backend transaction abstraction is needed.

There is **no implemented submission aggregate, submission table, or target-outcome persistence Port at this HEAD**. The #62 phrase "target creation result ... where applicable" is implemented here as a typed standalone result returned after commit. It is not represented as an already-durable `Undecided -> Created` submission transition. The transaction-local capture operation must remain callable by a future submission adapter: that future operation must lock its existing Undecided target, invoke capture creation within its transaction, record Created/Rejected and Job correlation, then commit once. Calling the standalone transaction-owning method and recording a submission result afterward is expressly invalid. This plan neither implements submission persistence nor claims full ADR-0019 submission acceptance/idempotency.

### RF-5 compatible representation

Keep `SourceProvenance(String)`, its `new`/`as_str` behavior, and M1 constructor signatures intact. Introduce a small `TransferSourceProvenance` sum type with M1 descriptive and M2 endpoint-capture variants. Change the Transfer field to this sum type, retaining M1 string serialization and giving M2 the exact structured object; do not reinterpret JSON-looking M1 strings as M2.

In PostgreSQL, retain the existing `source_provenance` TEXT column for M1 and add three typed nullable M2 columns, constrained so exactly one representation is present. This avoids parsing descriptive text into authority, a placeholder M1 string on M2, whole-aggregate JSONB, or a generic action catalog. Fold this into the initial migration under the current pre-baseline persistence policy. Do not execute changes against a retained lab database.

### Dispatch and lock ownership

Reuse `TransferDispatchService`, `JobRepository::commit_transfer_dispatch`, `evaluate_transfer_dispatch`, and `ActionDispatchService::dispatch_transfer`. The provenance variant determines whether a capture freshness precondition and M2 action apply; callers cannot opt out by choosing the old service.

For M2, the dispatch transaction reads and locks the Endpoint current revision **after** its existing Job/JobStep/Transfer locks and holds it through commit. This preserves the Transfer-before-Endpoint order of transfer authorization. Inventory recording already locks the Endpoint before replacing its current pointer. Capture creation locks the Endpoint then inserts only fresh Job/Transfer identities, not existing dispatched resources. Document and test these specific interactions; do not reverse all repository lock orders.

No architectural blocker was found. If execution discovers a required submission implementation or Agent-side coupling beyond this boundary, stop affected work and return to owner review.

## Exact file map

Paths marked Create do not exist at the planning baseline. All other paths were inspected.

| Action | Path | Responsibility |
| --- | --- | --- |
| Create | `crates/domain/src/endpoint_capture.rs` | SourceReference/value types, structural parsing, RF-2 pure decision, immutable capture provenance |
| Modify | `crates/domain/src/lib.rs` | Export the new narrow Domain types/decision |
| Modify | `crates/domain/src/transfer.rs` | Two provenance representations and M2 context construction; preserve M1 constructors |
| Modify | `crates/domain/src/transfer_dispatch.rs` | M2-only final currency precondition before Attempt minting |
| Create | `crates/server/src/application/endpoint_capture.rs` | Capture creation orchestration and exact M2 action parameter builder |
| Modify | `crates/server/src/application/mod.rs` | Module export; pass locked freshness facts; select M1/M2 action through existing outbound path; update affected unit fixtures |
| Create | `crates/server/src/ports/endpoint_capture.rs` | Purpose-specific capture creation transaction Port, locked facts and typed results |
| Modify | `crates/server/src/ports.rs` | Export new Port; extend transfer-dispatch locked facts; prevent M2 through legacy standalone Transfer creation |
| Create | `crates/server/src/adapters/postgres/endpoint_capture_repository.rs` | One atomic capture transaction and transaction-local composition entry point |
| Modify | `crates/server/src/adapters/postgres/mod.rs` | Register/export new adapter |
| Modify | `crates/server/src/adapters/postgres/inventory_repository.rs` | Share current-revision row mapping/read inside an already-owned transaction |
| Modify | `crates/server/src/adapters/postgres/job_repository.rs` | Extract insertion-only workflow helper; load M2 current revision during existing dispatch commitment |
| Modify | `crates/server/src/adapters/postgres/transfer_repository.rs` | Typed provenance mapping in both readers; insertion-only context helper; reject legacy M2 creation |
| Modify | `crates/server/migrations/0001_initial_schema.sql` | Relational M2 source lineage with representation and ownership constraints |
| Create | `crates/server/tests/support/endpoint_capture.rs` | Local capture fixture composition and read-only row assertions |
| Modify | `crates/server/tests/support/mod.rs` | Export the new test support module only |
| Create | `crates/server/tests/endpoint_capture_creation.rs` | Real PostgreSQL creation, rejection, rollback and provenance tests |
| Create | `crates/server/tests/endpoint_capture_dispatch.rs` | Real PostgreSQL final freshness/race tests and outbound capture assertions |
| Modify | `crates/server/tests/transfer_repository.rs` | Legacy constructor rejection test for structured M2 input; retain M1 tests |
| Modify | `crates/server/tests/transfer_dispatch_commit.rs` | Replace two legacy field `as_str` assertions with equivalent typed-provenance equality; retain all behavioral assertions |
| Modify after implementation | `docs/architecture/README.md` | Record implemented capture Port, provenance and dispatch boundaries; correct only directly affected M1-only descriptions |

Read/reuse without planned edits: `crates/domain/src/inventory.rs`, `job.rs`, `artifact.rs`, `chunk_manifest.rs`; `crates/server/src/adapters/postgres/authorization_repository.rs`; `crates/server/tests/job_workflow_creation.rs`, `inventory_report_wss.rs`, `support/transfer_vertical.rs`; `crates/agent-protocol/src/messages.rs`, `codec.rs`; `crates/simulator/src/transfer_action.rs`. No Worker/Simulator/protocol production edit is planned.

## TDD execution rules

Each numbered behavior task has its own RED -> GREEN sequence. Add one test or tightly related parameterized test at a time, run the named command, confirm the intended missing behavior, implement only that behavior, and rerun. A missing symbol is an initial compile RED, not proof of behavioral correctness; after introducing signatures, require the relevant assertion to fail before completing its implementation. A connection/prerequisite/compiler error unrelated to the new seam is not a valid behavioral RED. Never deliberately break working production behavior to manufacture a second RED for a regression test that already passes.

All commands run from repository root. PostgreSQL tests use `support::TestDatabase::setup/teardown`, which creates fresh `bamep_wp1_test_*` databases through embedded migrations. Existing Unix-socket peer authentication or `BAMEP_TEST_PG_ADMIN_URL` supplies access; never print the variable or change roles/configuration automatically. Record prerequisites as missing if unavailable.

## Task 1 — Typed source input and one RF-2 validation decision

**Files:** Create `crates/domain/src/endpoint_capture.rs`; modify `crates/domain/src/lib.rs`. Tests are colocated under `endpoint_capture::tests`.

**Consumes:** `InventoryRevision`, `InventoryRevisionId`, `InventorySnapshot`, `EndpointId`; existing base64/serde/uuid dependencies. Inventory remains otherwise opaque.

**Produces:**

```rust
// All fields private. Derive equality/Clone/Debug; validated Deserialize, exact Serialize.
pub struct SourceObservationId([u8; 32]);
pub struct AgentSourceId(String);
pub struct SourceReference {
    inventory_revision_id: InventoryRevisionId,
    source_observation_id: SourceObservationId,
    agent_source_id: AgentSourceId,
}
pub enum SourceReferenceError { InvalidRevision, InvalidObservation, EmptySourceId }
pub struct SourceReferenceStale;

// Required APIs:
// SourceObservationId::parse_wire_value(&str) -> Result<Self, SourceReferenceError>
// SourceObservationId::to_wire_value(&self) -> String
// SourceObservationId::from_bytes([u8; 32]) -> Self; as_bytes(&self) -> &[u8; 32]
// AgentSourceId::new(String) -> Result<Self, SourceReferenceError>; as_str(&self) -> &str
// SourceReference::new(InventoryRevisionId, SourceObservationId, AgentSourceId)
//     -> Result<Self, SourceReferenceError>
// SourceReference getters return InventoryRevisionId and references to the two other values.
pub fn validate_capture_source(
    endpoint_id: EndpointId,
    current: Option<&InventoryRevision>,
    selected: &SourceReference,
) -> Result<(), SourceReferenceStale>;
```

- [ ] **RED — structural input tests.** Add `source_reference_round_trips_exact_wire_shape` and `source_reference_rejects_structural_errors`. Use this concrete valid JSON fixture; mutate individual fields in the negative test.

```rust
let value = serde_json::json!({
    "inventory_revision_id": "12345678-1234-4234-8234-123456789abc",
    "source_observation_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "agent_source_id": "source-α"
});
let selected: SourceReference = serde_json::from_value(value.clone()).unwrap();
assert_eq!(serde_json::to_value(selected).unwrap(), value);
```

Negative table: missing each field; wrong JSON types; nil/non-v4 UUID; uppercase/unhyphenated UUID; padded/standard-alphabet/whitespace/wrong-length/noncanonical-trailing-bit observation; empty source id. Accept a nonempty Unicode source and preserve exact bytes, including whitespace: no trim, case-fold, path parsing, or invented hardware syntax. Keep unrelated unknown fields compatible with the existing known-message rule.

Run RED: `cargo test -p bamep-domain endpoint_capture::tests::source_reference -- --nocapture`.

- [ ] **GREEN — structural types.** Implement strict observation decode/length/re-encode equality using `base64::engine::general_purpose::URL_SAFE_NO_PAD`, validating constructor plus serde decoding through that constructor. UUID decoding checks version 4 and equality with lowercase hyphenated rendering before constructing `InventoryRevisionId`; do not tighten the existing generic InventoryRevisionId globally. Reuse the encoding idiom, never BootNonce's identity/freshness semantics. Do not add Agent epoch generation.

Run GREEN: the same command.

- [ ] **RED — RF-2 matrix.** Add `capture_source_requires_exact_current_tuple` with table-driven assertions. Build `InventoryRevision` directly using the selected id, owning Endpoint, `InventorySnapshot` from the following object, and a fixed timestamp.

```rust
let snapshot = serde_json::json!({
    "capture_source_observation_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "capturable_sources": [{"agent_source_id": "a"}, {"agent_source_id": "b"}],
    "unrelated_inventory": {"anything": true}
});
// selected targets "b", not the first entry.
assert_eq!(validate_capture_source(endpoint, Some(&current), &selected), Ok(()));
assert_eq!(validate_capture_source(endpoint, None, &selected), Err(SourceReferenceStale));
```

Each negative uses a clone of this fixture: wrong Endpoint ownership; stale revision; different valid observation; unknown selected id; empty array; duplicate selected id; missing/wrong-typed fragment; malformed observation; non-object source entry; missing/wrong-typed/empty entry id. Assert `Err(SourceReferenceStale)`, never fallback or structural-command rejection. Also test multiple distinct sources with one explicit selection, and an unrelated duplicated id with a uniquely selected id: RF-4 forbids selecting the duplicated id, not an invented global hardware-inventory rule.

Run RED: `cargo test -p bamep-domain endpoint_capture::tests::capture_source_requires_exact_current_tuple -- --exact --nocapture`.

- [ ] **GREEN — one pure validator.** Check owner and revision first. Parse only the RF-4 fragment; validate entry structure, compare observation exactly, and require the selected id to occur exactly once. No sorting/deduplication, current-revision lookup by supplied id, or rereading a historical snapshot as current. Malformed required content fails closed.

```rust
// Core decision after validating fragment types/entry syntax:
if reported_observation != *selected.source_observation_id() || matching_entries != 1 {
    return Err(SourceReferenceStale);
}
Ok(())
```

Run GREEN: the same exact test. Regression: `cargo test -p bamep-domain`.

**Review checkpoint:** RF-2/RF-4 matrix and wire parsing; no broader inventory schema or changes to InventoryReport ingestion.

## Task 2 — Structured provenance without changing M1 meaning

**Files:** `crates/domain/src/endpoint_capture.rs`, `transfer.rs`, `lib.rs`; `crates/server/src/ports.rs` and `crates/server/src/adapters/postgres/transfer_repository.rs` for the legacy constructor compatibility guard; `crates/server/tests/transfer_repository.rs` for the guard test; mechanical equality assertion change in `crates/server/tests/transfer_dispatch_commit.rs`. No database atomicity claim in this task.

**Consumes:** Task 1's validated SourceReference.

**Produces:**

```rust
// Immutable value: private field, read-only access, no mutation setter.
pub struct CaptureSourceProvenance(SourceReference);
// from_source_reference(SourceReference) -> Self; source_reference(&self) -> &SourceReference
// Transparent serde: the exact RF-5 object, no extra envelope.
#[serde(untagged)]
pub enum TransferSourceProvenance {
    M1Descriptive(SourceProvenance),
    EndpointCapture(CaptureSourceProvenance),
}
// Transfer.source_provenance now has the above type.
// Existing create_transfer_context(..., SourceProvenance) remains unchanged publicly.
pub fn create_endpoint_capture_context(
    endpoint_id: EndpointId,
    job_id: JobId,
    job_step_id: JobStepId,
    chunk_size: ChunkSize,
    source: CaptureSourceProvenance,
) -> TransferContext;
```

- [ ] **RED.** Add `capture_provenance_is_the_exact_tuple_and_survives_binding` and `m1_provenance_remains_a_string` in `transfer.rs` tests.

```rust
let source = CaptureSourceProvenance::from_source_reference(selected.clone());
let context = create_endpoint_capture_context(endpoint, job, step, size, source);
assert_eq!(serde_json::to_value(&context.transfer.source_provenance).unwrap(),
           serde_json::to_value(&selected).unwrap());
let bound = bind_attempt(&context.transfer, &attempt_for(step)).unwrap();
assert_eq!(bound.source_provenance, context.transfer.source_provenance);
assert_eq!(context.artifact.state, ArtifactState::Incomplete);
assert_eq!(context.transfer.attempt_id, None);
// Existing ctx() uses SourceProvenance::new("disk-0").
assert_eq!(serde_json::to_value(ctx().transfer.source_provenance).unwrap(), "disk-0");
```

Use existing `ctx()`/`attempt_for()` helpers; construct selected with Task 1 constructors. Add a JSON-looking M1 string case to ensure it stays a string variant.

RED: `cargo test -p bamep-domain transfer::tests -- --nocapture`.

- [ ] **RED — legacy entry-point guard.** In `crates/server/tests/transfer_repository.rs`, add `legacy_creation_rejects_m2_context`. Use existing `build_services`, `workflow_context`, and TestDatabase helpers. Construct a valid typed selection with a fresh v4 revision and source id "a"; the legacy operation must reject before attempting to interpret/persist it.

```rust
let db = TestDatabase::setup().await;
let services = build_services(db.pool.clone());
let (endpoint, job, step) = workflow_context(&services, "m2-legacy-guard", Utc::now()).await;
let context = create_endpoint_capture_context(endpoint, job, step,
    ChunkSize::new(4096).unwrap(), CaptureSourceProvenance::from_source_reference(selected));
let repo = PostgresTransferRepository::new(db.pool.clone());
assert!(matches!(repo.create_transfer_context(&context).await,
    Err(CreateTransferError::CaptureRequiresAtomicCreation)));
let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transfers")
    .fetch_one(&db.pool).await.unwrap();
assert_eq!(count, 0);
db.teardown().await;
```

RED: `cargo test -p bamep-server --test transfer_repository legacy_creation_rejects_m2_context -- --exact --nocapture`. Run after introducing type signatures but before the legacy rejection branch. GREEN uses the identical command after the implementation below; the full `transfer_repository` suite is the M1 regression.

- [ ] **GREEN.** Wrap M1 provenance only at the existing constructor's internal assignment. Introduce an internal common context constructor taking the sum type; the new capture constructor fixes `AgentToServer` and `Sha256`. Share Artifact/Manifest creation. Keep source lineage independent from TargetFingerprint. Update the two PostgreSQL Transfer struct initializers mechanically to wrap their existing text as M1, keeping them compilable until Task 3 adds structured loading. For the existing insert binding, match M1Descriptive and use the unchanged inner `SourceProvenance::as_str`; reject EndpointCapture with the new `CreateTransferError::CaptureRequiresAtomicCreation` before any legacy inserts. This keeps the workspace compilable without teaching the legacy entry point to bypass the new atomic seam. No SQL schema change occurs yet; Task 3 tests the guard against PostgreSQL and adds structured mapping.

```rust
// Existing M1 constructor delegates with:
TransferSourceProvenance::M1Descriptive(source_provenance)
// New M2 constructor delegates with:
TransferSourceProvenance::EndpointCapture(source)
```

GREEN: same command. Regressions: `cargo test -p bamep-domain` and `cargo test -p bamep-server --lib`.

**Review checkpoint:** no source replacement API, M1 string serde and `SourceProvenance` APIs intact, no M1 wire parameter added. A typed wrapper changes internal Rust field access; it does not change the closed action contract.

## Task 3 — Atomic accepted-target creation and real PostgreSQL evidence

**Files:** new Server Application/Port/adapter files and module exports in the map; inventory/job/transfer adapters; initial migration; new creation integration test and capture support module.

**Consumes:** Tasks 1–2; existing Domain `create_workflow(endpoint, 1)` and M2 context constructor; existing enrollment eligibility; TestDatabase.

**Produces:** these types in `ports/endpoint_capture.rs`, re-exported by `ports.rs`; service in `application/endpoint_capture.rs`, re-exported by `application/mod.rs`.

```rust
pub struct CaptureCreationLockedFacts {
    pub identity_state: Option<IdentityState>,
    pub current_inventory: Option<InventoryRevision>,
}
pub struct CaptureTargetCreated { pub job: Job, pub context: TransferContext }
pub enum CaptureTargetRejection { EndpointNotFound, EndpointNotEnrolled, SourceReferenceStale }
pub enum CaptureTargetResult { Created(CaptureTargetCreated), Rejected(CaptureTargetRejection) }
pub type CaptureCreationDecision = Box<dyn FnOnce(CaptureCreationLockedFacts)
    -> Result<CaptureTargetCreated, CaptureTargetRejection> + Send>;
#[async_trait]
pub trait EndpointCaptureRepository: Send + Sync {
    async fn create_capture_target(&self, endpoint_id: EndpointId, decide: CaptureCreationDecision)
        -> Result<CaptureTargetResult, RepositoryError>;
}
// EndpointCaptureService<R>::new(Arc<R>) -> Self
// EndpointCaptureService<R>::create_target(EndpointId, SourceReference, ChunkSize)
//     -> Result<CaptureTargetResult, RepositoryError>
// PostgresEndpointCaptureRepository::new(PgPool) -> Self
```

Structural input errors have already been rejected by Task 1's constructors; backend errors are not relabeled as source staleness. Creation never auto-admits or dispatches. Existing workflow eligibility remains Enrolled.

### Fixture implementation coupled to this task

Create `support::endpoint_capture::CaptureFixture` with `db: TestDatabase`, `endpoint: EndpointId`, `selected: SourceReference`, `capture: EndpointCaptureService<PostgresEndpointCaptureRepository>`, `jobs: Arc<PostgresJobRepository>`, and `inventory: Arc<PostgresInventoryRepository>`.

Implement `CaptureFixture::new() -> Self` asynchronously: TestDatabase setup; compose BootOrchestrationService/EnrollmentService exactly as `job_workflow_creation.rs`; issue a fresh BootNonce/credential; redeem; explicitly approve enrollment with a test operator label; record the Task 1 snapshot through `InventoryRepository::record_inventory`; use its returned Server revision id to build selected. No raw Job/Transfer inserts in the fixture.

Implement `create(&self) -> Result<CaptureTargetResult, RepositoryError>` by calling `capture.create_target(endpoint, selected.clone(), ChunkSize::new(4096).unwrap())`. Implement `advance_inventory(&self) -> InventoryRevision` through `record_inventory` with a different observation (32 bytes of value 1 encoded canonically), not a SQL current-pointer update. Implement `counts(&self) -> [i64; 6]` by reading totals in the isolated database for `jobs`, `job_steps`, `transfers`, `artifacts`, `chunk_manifests`, `attempts`; avoid join-only counts that hide orphan rows. Implement `teardown(self)` by `db.teardown().await` after dropping fixture-owned service handles. For missing-inventory/non-Enrolled cases, build the fixture up to the appropriate public enrollment/inventory step, never erase safety state in SQL.

- [ ] **RED — atomic success and rejection matrix.** Add these exact tests to `endpoint_capture_creation.rs`:
  - `valid_source_creates_one_complete_pending_capture`;
  - `invalid_current_sources_create_no_capture_resources` (all Task 1 current-state negatives through real persisted inventories);
  - `missing_or_unenrolled_endpoint_creates_nothing`;
  - `capture_provenance_survives_inventory_change_and_reload`.

```rust
#[tokio::test]
async fn valid_source_creates_one_complete_pending_capture() {
    let f = CaptureFixture::new().await;
    let CaptureTargetResult::Created(created) = f.create().await.unwrap() else {
        panic!("valid current source must create capture");
    };
    assert_eq!(f.counts().await, [1, 1, 1, 1, 1, 0]);
    assert_eq!(created.job.state, JobState::Pending);
    assert_eq!(created.job.steps.len(), 1);
    assert_eq!(created.job.steps[0].state, JobStepState::Pending);
    assert!(created.job.steps[0].destructive_intent.is_none());
    assert_eq!(created.context.transfer.job_step_id, created.job.steps[0].id);
    assert_eq!(created.context.artifact.state, ArtifactState::Incomplete);
    assert_eq!(created.context.transfer.attempt_id, None);
    // Read the actual manifest row: sealed=false, chunk_count/artifact_digest NULL.
    let sealed: bool = sqlx::query_scalar("SELECT sealed FROM chunk_manifests WHERE artifact_id = $1")
        .bind(created.context.artifact.id.0).fetch_one(&f.db.pool).await.unwrap();
    assert!(!sealed);
    f.teardown().await;
}
```

The rejection matrix asserts `Rejected(SourceReferenceStale)` and `[0; 6]` for no inventory, old revision, wrong epoch, unknown source, duplicated selected id, empty source array, and malformed required fragment. Supply syntactically valid typed selections; malformed inventory content is persisted as opaque JSON via the normal Inventory repository. Missing Endpoint/non-Enrolled outcomes retain their own typed rejection, with zero resources. Reload test compares exact raw tuple columns and the reconstructed typed provenance after an inventory update and a newly constructed repository/pool; old lineage must remain readable.

RED: `cargo test -p bamep-server --test endpoint_capture_creation -- --nocapture`.

- [ ] **RED — atomicity and commit failure tests before implementation.** Add `capture_insert_failure_rolls_back_every_resource` and `capture_commit_failure_rolls_back_every_resource`. Install disposable-database triggers, following `job_workflow_creation.rs::failure_partway_through_jobstep_persistence_leaves_no_partial_workflow`.

```sql
CREATE FUNCTION reject_capture_manifest() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'forced capture manifest failure'; END $$;
CREATE TRIGGER reject_capture_manifest BEFORE INSERT ON chunk_manifests
FOR EACH ROW EXECUTE FUNCTION reject_capture_manifest();
```

For the second test use an `AFTER INSERT` constraint trigger on `chunk_manifests`, `DEFERRABLE INITIALLY DEFERRED`, so failure occurs at COMMIT after all insert statements succeeded. Assert RepositoryError, `[0; 6]`, no chunk identities, and original inventory/Endpoint still intact. The late failure catches false success returned before commit. No production fault-injection flag.

```rust
let result = f.create().await;
assert!(matches!(result, Err(RepositoryError::Backend(_))));
assert_eq!(f.counts().await, [0; 6]);
```

Run RED: `cargo test -p bamep-server --test endpoint_capture_creation capture_ -- --nocapture`. Add these alongside the success tests before implementing the capture transaction. The initial RED is the missing creation operation; once signatures exist, verify the failure-path assertions reject success/partial state before completing persistence and commit-result propagation. The GREEN command is the same after the implementation step below.

- [ ] Add `provenance_shape_rejects_partial_or_mixed_columns`; assert database constraint failures leave no extra rows. Retain Task 2's `legacy_creation_rejects_m2_context` regression. Use direct SQL only for constraint-negative data in the disposable database, not to simulate an accepted capture operation. Run RED with `cargo test -p bamep-server --test endpoint_capture_creation -- --nocapture`; the missing representation constraint supplies a database assertion failure. The legacy guard from Task 2 may already pass: retain that regression without manufacturing a failure. Rerun the complete suite for GREEN after the schema/transaction implementation below.

- [ ] **RED — exact M2 source-id persistence.** Add adapter-local PostgreSQL tests `m2_source_agent_id_round_trips_nul_and_unicode` and `m2_source_agent_id_rejects_invalid_persisted_utf8` in `crates/server/src/adapters/postgres/transfer_repository.rs`. Use the existing disposable TestDatabase harness and transaction-local insertion helper to persist a Domain context whose otherwise valid source id is `"source-\u{0000}-α-id"` (Rust string syntax). Reload through both provenance readers and assert string equality and byte equality with the original. Exercise this as a provenance-storage fixture, independently of InventoryReport ingestion; do not claim RF-2 target acceptance from fixture insertion. For the negative read case, inject non-empty invalid UTF-8 bytes such as `[0xff]` into a disposable persisted fixture and require `RepositoryError`, never replacement characters or fallback to M1. Retain the SQL empty-value constraint test.

Run RED before the BYTEA mapping: `cargo test -p bamep-server --lib m2_source_agent_id -- --nocapture`. Rerun the identical command for GREEN after the mapping below, then `cargo test -p bamep-server --test transfer_repository --test endpoint_capture_creation` for regression.

- [ ] **GREEN — schema and transaction implementation.** In `transfers`, make legacy `source_provenance` nullable and add:

```sql
source_inventory_revision_id UUID,
source_observation_id BYTEA,
source_agent_id BYTEA,
CONSTRAINT transfer_provenance_shape CHECK (
  (source_provenance IS NOT NULL AND source_inventory_revision_id IS NULL
    AND source_observation_id IS NULL AND source_agent_id IS NULL)
  OR
  (source_provenance IS NULL AND source_inventory_revision_id IS NOT NULL
    AND source_observation_id IS NOT NULL AND octet_length(source_observation_id) = 32
    AND source_agent_id IS NOT NULL AND octet_length(source_agent_id) > 0)
),
FOREIGN KEY (endpoint_id, source_inventory_revision_id)
  REFERENCES inventory_revisions (endpoint_id, revision_id)
```

No fabricated M1 description for M2, no source-state lifecycle column, no provenance FK to the **current** pointer. Historical inventory FK preserves ownership without making continued provenance validity depend on currency. No backfill/upgrade claim for disposable pre-baseline databases.

Both `load_locked_facts` and `load_locked_transfer_and_artifact` must use one adapter-local provenance mapper: legacy text alone -> M1; complete tuple alone -> validated M2; incomplete/invalid combinations -> RepositoryError. No JSON sniffing or fallback. All binding/manifest/Artifact update SQL leaves provenance untouched.

For M2 `source_agent_id`, write `agent_source_id.as_str().as_bytes()` exactly to BYTEA. On read, load the bytes, require valid UTF-8 with `String::from_utf8`, then require non-empty through `AgentSourceId::new`; reconstruct the exact string without trimming, normalization, or lossy conversion. Invalid UTF-8 or an empty persisted value is persisted-data corruption and returns `RepositoryError`. Domain remains `AgentSourceId(String)` and the wire remains a JSON UTF-8 string, including embedded NUL. `source_observation_id` remains BYTEA containing exactly 32 raw bytes; M1 descriptive TEXT is unchanged.

Extract transaction-local insertion helpers from existing methods without changing M1 behavior:

```rust
// In job_repository.rs:
pub(crate) async fn insert_workflow(tx: &mut Transaction<'_, Postgres>, job: &Job)
    -> Result<(), RepositoryError>;
// In transfer_repository.rs:
pub(crate) async fn insert_transfer_context(tx: &mut Transaction<'_, Postgres>, context: &TransferContext)
    -> Result<(), RepositoryError>;
// In inventory_repository.rs: caller already holds the Endpoint lock.
pub(crate) async fn load_current_inventory(tx: &mut Transaction<'_, Postgres>, endpoint: EndpointId)
    -> Result<Option<InventoryRevision>, RepositoryError>;
// In endpoint_capture_repository.rs: deliberately neither begins nor commits.
pub(crate) async fn create_capture_in_transaction(
    tx: &mut Transaction<'_, Postgres>, endpoint: EndpointId, decide: CaptureCreationDecision,
) -> Result<CaptureTargetResult, RepositoryError>;
```

`insert_workflow` inserts Job then ordered steps; `insert_transfer_context` inserts Artifact, Transfer, then unsealed Manifest. Validate the u32-to-PostgreSQL INTEGER chunk-size conversion with `i32::try_from`, returning a backend representation error instead of wrapping. No retry or second transaction.

The standalone repository begins once, calls `create_capture_in_transaction`, commits once on Created, and rolls back on rejection/error. Inside the helper: lock Endpoint via `SELECT identity_state ... FOR UPDATE`; read its current inventory by `(Endpoint, current pointer)` under that lock; invoke Application's closure. The closure rejects missing/non-Enrolled Endpoint, calls the single Domain RF-2 validator, constructs one fresh pending workflow, and creates a context with exact captured lineage. Only an accepted result reaches insertion helpers. Hold the Endpoint lock throughout insertion and commit so inventory changes/retirement cannot race acceptance.

Retain Task 2's `CreateTransferError::CaptureRequiresAtomicCreation` rejection of M2 contexts at the legacy `PostgresTransferRepository::create_transfer_context` entry point; the capture adapter calls the insertion helper directly inside its encompassing transaction. Preserve all existing M1 error/eligibility behavior. Future submission integration consumes the transaction-local helper, never the standalone committed result.

GREEN: same creation suite. Regression: `cargo test -p bamep-server --test job_workflow_creation --test transfer_repository --test inventory_report_wss`.

**Review checkpoint:** inspect actual BEGIN/COMMIT ownership, lock scope, both provenance readers, raw counts after late failure, and the absence of submission/Attempt creation. This is the gate for the RF-6 atomicity claim.

## Task 4 — Final pre-dispatch currency enforced under the dispatch transaction

**Files:** Domain `transfer_dispatch.rs`; `ports.rs`; Application `mod.rs`; PostgreSQL `job_repository.rs`; new `endpoint_capture_dispatch.rs`; capture support module.

**Consumes:** durable M2 provenance, existing scheduling/arbiter/dispatch Port and Domain decision.

**Produces:** `current_inventory_revision_id: Option<InventoryRevisionId>` on `TransferDispatchLockedFacts` and `TransferDispatchInputs`; `TransferDispatchRejection::SourceReferenceStale`. M1 ignores this field and needs no inventory. All existing test literal initializers explicitly supply None.

- [ ] **RED — pure decision.** Add `capture_dispatch_requires_current_inventory_revision` under `transfer_dispatch::tests`. Start from the existing all-pass inputs, substitute Task 2's structured context for the same Job/step. For None and a different revision, assert SourceReferenceStale, `pending_job_step.state == Pending`, and unchanged unbound input Transfer. Matching revision must succeed; M1 with None must still succeed.

RED: `cargo test -p bamep-domain transfer_dispatch::tests::capture_dispatch_requires_current_inventory_revision -- --exact --nocapture`.

- [ ] **GREEN — final Domain check.** Place the check after structural and workflow checks and before `AttemptId::new`/ActionId minting. Use the existing `deny(...)` mechanism, which owns the Pending result.

```rust
if let TransferSourceProvenance::EndpointCapture(source) = &transfer.source_provenance {
    if inputs.current_inventory_revision_id
        != Some(source.source_reference().inventory_revision_id()) {
        return Err(deny(TransferDispatchRejection::SourceReferenceStale));
    }
}
```

GREEN: same command. Regression: `cargo test -p bamep-domain transfer_dispatch`.

- [ ] **RED — database freshness and reservation behavior.** Add `stale_after_creation_never_commits_an_attempt` and `unchanged_source_commits_once` to `endpoint_capture_dispatch.rs`.

Use CaptureFixture to create a target. Compose existing `JobSchedulingService::new(f.jobs.clone())`; call `admit(job.id)` then `satisfy_current_step_preconditions(job.id, step.id)`. Compose `TechnicalResourceArbiter` with capacity 1 for `ResourceKind::new("network")`, then `TransferDispatchService`. For stale case, call `advance_inventory` after preliminary eligibility and before dispatch. Use the real repository, not a mock currency flag.

```rust
let result = dispatch.commit_transfer_dispatch(job.id, step.id, transfer.id,
    vec![ResourceClaim::new(ResourceKind::new("network"), 1)]).await.unwrap();
assert!(matches!(result,
    TransferDispatchResult::Rejected(TransferDispatchRejection::SourceReferenceStale)));
assert_eq!(f.jobs.find_job(job.id).await.unwrap().unwrap().steps[0].state, JobStepState::Pending);
assert_eq!(f.counts().await, [1, 1, 1, 1, 1, 0]);
// Reacquiring the full capacity proves the failed dispatch released its reservation.
let replacement = arbiter.acquire(vec![ResourceClaim::new(ResourceKind::new("network"), 1)]).unwrap();
arbiter.release(replacement);
```

Also read `transfers.attempt_id IS NULL`; Job remains Running with exclusivity; provenance unchanged. Unchanged case asserts one Dispatched Attempt/action binding, same Transfer/Artifact, and no second Attempt on repeated commitment. No automatic redispatch/retry.

RED: `cargo test -p bamep-server --test endpoint_capture_dispatch -- --nocapture`.

- [ ] **RED/concurrency test — inventory race.** Add `inventory_update_winning_endpoint_lock_blocks_stale_dispatch`. Use two real connections and a test-local gate, not sleeps: an inventory-record transaction holds the Endpoint lock; start dispatch and observe its lock wait through `pg_stat_activity`/`pg_blocking_pids`; allow inventory to commit its new snapshot; assert dispatch re-reads the new revision and rejects with no Attempt. Implement the writer gate with a disposable `inventory_revisions` insert trigger acquiring a test-held advisory lock; the writer is still the production Inventory repository. Poll the actual blocking relationship with a bounded test deadline and `yield_now`, never by assuming elapsed time establishes ordering. Release advisory locks and join both tasks before teardown.

Run RED before the transaction-local freshness-read implementation below, then rerun the same command for GREEN: `cargo test -p bamep-server --test endpoint_capture_dispatch inventory_update_winning_endpoint_lock_blocks_stale_dispatch -- --exact --nocapture`. Read the failure together with the unchanged-source positive: unconditional denial is not an implementation. Do not mutate production code to manufacture a RED; add both tests before wiring the locked read.

- [ ] **GREEN — authoritative read in the same transaction.** After `load_locked_facts` resolves the Transfer, only for M2 execute `SELECT current_inventory_revision_id FROM endpoints WHERE id = $1 FOR UPDATE` using the Job's Endpoint id and the same transaction. Feed the result through the existing decision callback. No `find_current_inventory` call on an independent pool before committing. Retain the Endpoint lock until the existing dispatch transaction commits or rolls back. Existing denial persistence returns Pending; existing Application rejection/error branches release the reservation.

GREEN: same suite. Regression: `cargo test -p bamep-server --test transfer_dispatch_commit --test final_dispatch_authorization --test job_admission_and_scheduling`.

**Review checkpoint:** final check cannot be skipped by the legacy transfer service; locks do not invert Transfer/Endpoint authorization ordering; no Attempt is minted on denial; no destructive evidence introduced; RF-8 does not become a new resume-time current-inventory gate.

## Task 5 — Exact RF-7 construction through existing outbound delivery

**Files:** `application/endpoint_capture.rs`, `application/mod.rs`; unit tests in the capture module; `endpoint_capture_dispatch.rs`.

**Consumes:** successfully committed `TransferDispatchOutcome`, reservation id, structured provenance. No new caller-supplied source/action parameters.

**Produces:**

```rust
pub const M2_ENDPOINT_CAPTURE_ACTION_TYPE: &str = "bamep.m2.endpoint-capture-transfer";
pub const M2_ENDPOINT_CAPTURE_ACTION_VERSION: &str = "1";
// In the new Application submodule, accessible to its parent:
pub(super) fn endpoint_capture_action_parameters(
    transfer: &Transfer, source: &CaptureSourceProvenance,
) -> serde_json::Map<String, serde_json::Value>;
```

- [ ] **RED.** Add `dispatch_capture_sends_exact_rf7_action` and `capture_dispatch_preserves_send_once_and_failure_semantics` to the new Application submodule's tests. Use a recording `AgentDispatchPort` as in existing `mod.rs::tests` outbound tests; this fake proves only message construction/send discipline, not database commitment.

```rust
let sent = transport.last_dispatch().unwrap();
assert_eq!(sent.body.action_type, "bamep.m2.endpoint-capture-transfer");
assert_eq!(sent.body.action_version, "1");
assert_eq!(sent.envelope.correlation_id, Some(sent.body.action_id));
assert_eq!(sent.body.action_id.as_uuid(), attempt.action_id.0);
assert!(sent.body.retry_of.is_none());
assert_eq!(serde_json::Value::Object(sent.body.parameters), serde_json::json!({
    "transfer_id": transfer.id.0.to_string(),
    "artifact_id": transfer.artifact_id.0.to_string(),
    "direction": "agent_to_server",
    "digest_algorithm": "sha256",
    "chunk_size": transfer.chunk_size.get(),
    "source_reference": selected
}));
```

Consume the already-coherent committed Transfer/Attempt context: `evaluate_transfer_dispatch` checks exact Job/JobStep/Endpoint/Transfer correlation before minting the Attempt and binds the Transfer to that Attempt in the committed outcome. Preserve the existing `AttemptState::Dispatched` guard, `AttemptId -> ReservationId` register-before-send handoff, and send-once behavior. Test the existing non-Dispatched guard, duplicate call after success, duplicate after send failure, and reservation retention on uncertain send. Do not introduce an M2-only correlation rejection or reservation-release lifecycle: an early rejection before registration would leave the already-acquired reservation outside the existing ownership path. M1 behavior remains unchanged.

RED: `cargo test -p bamep-server --lib endpoint_capture::tests -- --nocapture`.

- [ ] **RED — committed-to-outbound integration.** Add `committed_capture_reconstructs_rf7_from_durable_provenance` to `endpoint_capture_dispatch.rs`: create via CaptureFixture; schedule; commit using real PostgreSQL; call existing `dispatch_transfer` with committed outcome and a recording transport; assert the exact JSON above and durable Attempt id. Reload Transfer via `find_transfer_context` and verify identical source lineage. For stale result, the consumer match must not call outbound delivery and the recorder remains empty. This establishes the Server seam only, not an Agent accepting/executing M2.

Run RED: `cargo test -p bamep-server --test endpoint_capture_dispatch committed_capture_reconstructs_rf7_from_durable_provenance -- --exact --nocapture`; the existing outbound method still constructs M1. After the implementation below, run the identical command for GREEN and the full `endpoint_capture_dispatch` suite for regression.

- [ ] **GREEN.** In `ActionDispatchService::dispatch_transfer`, match the durable provenance variant. M1 keeps its constants and untouched five-field `transfer_action_parameters`. M2 consumes that committed context and uses M2 constants and a new builder that extends those five shared values with exactly `source_reference` from typed immutable provenance. Both call the existing `dispatch_message`; no second transport, new action identity, arbitrary parameter map, or Server-side resend loop.

```rust
// M2 builder body, reusing the existing parent's five-value mapper:
let mut parameters = super::transfer_action_parameters(transfer);
parameters.insert("source_reference".into(),
    serde_json::to_value(source.source_reference()).expect("validated source reference serializes"));
parameters
```

GREEN: same command. Regressions: `cargo test -p bamep-server --lib` and `cargo test -p bamep-agent-protocol`.

**Review checkpoint:** RF-7 six top-level parameters, exact nested tuple, M1 five parameters unchanged; no Agent/Worker protocol extension and no claim of SOURCE_REFERENCE_STALE Agent handling.

## Task 6 — Regression review and implemented-boundary documentation

This task adds no new behavior; use existing suites rather than manufacturing RED tests.

- [ ] Run the final suite below after Tasks 1–5. Distinguish failures caused by changes, existing failures, and missing prerequisites. Do not change test timeouts/retries or weaken assertions to pass.
- [ ] Review RF-2 matrix, both provenance mappers, SQL constraints, commit-time failure, inventory race, resource release, exact action JSON, and M1 regression evidence.
- [ ] Update only the directly affected Architecture sections: new standalone capture creation operation, transaction helpers, provenance variants, M2 freshness fact alongside the unchanged destructive gate, and M2 construction through existing outbound delivery. Explicitly leave submission persistence and Agent source handling unimplemented. Do not duplicate the normative Specification or fix unrelated stale Architecture text.
- [ ] Revalidate referenced paths and `git diff --check`; report changed paths, actual results, remaining owner review and limitations. Suggest a Conventional Commit message only; do not execute it.

## Exact planned final verification suite

These commands are planned, **not run during planning**. PostgreSQL tests create only disposable databases through the existing harness. The final workspace test covers existing Job/Transfer/Artifact persistence and M1 integration without adding physical dependencies.

```bash
cargo test -p bamep-domain
cargo test -p bamep-server --lib
cargo test -p bamep-server --test endpoint_capture_creation --test endpoint_capture_dispatch
cargo test -p bamep-server --test job_workflow_creation --test inventory_report_wss --test transfer_repository --test transfer_dispatch_commit --test final_dispatch_authorization --test job_admission_and_scheduling
cargo test -p bamep-agent-protocol
cargo test -p bamep-server --test data_plane_transfer_vertical --test data_plane_transfer_failure_matrix --test transfer_terminal_evidence --test job_cancellation --test transfer_authorization_service
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
git diff --stat
git diff --name-only
git status --short
```

Run narrow task commands during TDD; run the broad final suite once after the combined change. Do not rerun successful broad checks without a subsequent edit/failure justifying it. Cargo command/package names derive from current manifests; no checked-in CI workflow or repository-wide command wrapper was found. Formatting/lint baselines may have unrelated failures; report rather than reformat unrelated work.

## Plan self-review and handoff

Coverage: RF-2/RF-4 -> Task 1 and persisted rejection cases in Task 3; RF-5 -> Tasks 2–3; RF-6 atomic creation -> Task 3; RF-6 final freshness -> Task 4; RF-7 construction -> Task 5. Regression and implemented Architecture -> Task 6. RF-3/RF-8 Agent behavior, RF-9 submission equivalence, and full ADR-0019 submission target persistence are explicitly outside this WP's standalone seam.

Owner checkpoints: review this plan before execution; review representation/API boundary after Tasks 1–2; review PostgreSQL creation/rollback evidence after Task 3; review freshness/dispatch evidence after Tasks 4–5; accept final tested delta. A checkpoint does not silently expand the approved WP or authorize publication.

No runtime tests or code were changed/executed while producing this plan. No claim is made about PostgreSQL availability or current test-suite success. Stop here for owner plan review.
