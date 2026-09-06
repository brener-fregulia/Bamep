# Issue #61 — Endpoint-Capture Data-Plane Spike Harness

Authored, **throwaway** Spike scaffolding for Issue #61. Preserved as a sibling
to the closed Issue #60 harness (`../issue-60-winpe-agent-slice/`), which is
**not** repurposed.

## Authoritative status (read this first)

- **Throwaway Spike scaffolding.** Not production architecture, not
  `crates/agent`, not the `bamepd` composition root, not a second transfer
  protocol, not a new Artifact/chunk model.
- The **CP2** driver (`harness/`) is **off-device only**. It proves the
  Worker / harness / data-plane side of the M1 vertical independently from WinPE
  and from any physical device.
- The action exercised is the **existing `bamep.m1.data-plane-transfer`**
  reference path (`bamep_simulator::DataPlaneTransferAgent` / `DataPlaneClient`).
  It is **not** `bamep.m2.endpoint-capture-transfer` and must not be reported
  as the M2 product action.
- Attempt-by-attempt evidence and the A/B/C/D classification live on Issue #61.

## CP2 — what `harness/` proves

One off-device loopback run, every boundary real:

```text
real Endpoint / Job / JobStep / Attempt   (Application authority, real PostgreSQL)
  -> real Transfer + Artifact{Incomplete} + ChunkManifest
  -> real ActionDispatch / ActionAck{Accepted}      (real Agent Protocol v1 WSS, pinned TLS 1.3)
  -> real TransferAuthorizationRequest -> TransferAuthorizationGrant
  -> real sender-constrained Ed25519 per-request proof
  -> real GET resume discovery / PUT chunks / POST seal   (real Worker HTTPS listener)
  -> real Server<->Worker UDS chunk acceptance / seal / verification
  -> real Worker full-Artifact streaming SHA-256 reconstruction
  -> durable Artifact::Verified   (independently-computed source SHA-256 == verified Artifact digest)
```

Plus one negative (a mutated chunk body with an honest declared digest is
rejected `409 DIGEST_MISMATCH` and never durably held) and one idempotency
check (re-`PUT` of an already-held chunk returns `200 already_held` without
rewriting the recorded identity), then the transfer still completes `Verified`.

`SourceProvenance` is carried as **descriptive-only text** (a labelled JSON blob
with the #59-tuple *shape* and synthetic values). No `SourceReference`
validation is claimed or exercised.

Synthetic deterministic source: **35 MiB** at an **8 MiB** chunk size -> 5 chunks
(4 full + a short 3 MiB final). No performance claim is made from it.

### Composition (all real current implementations)

`bamep_server` Application: `JobService`, `JobSchedulingService`, `TransferService`,
`TransferDispatchService`, `TransferAuthorizationService`, `ChunkAcceptanceService`,
`ManifestSealService`, `ArtifactVerificationService`, `ActionEvidenceService`,
`TransferTerminalEvidenceService`, `AgentControlGateway` + `AgentTransportAcceptor`,
PostgreSQL-backed repositories.
`bamep_worker`: `WorkerControlPlane` over AF_UNIX, `data_plane::DataPlane` HTTPS
listener, `FilesystemChunkStore`, upload digest verification, `FullArtifactHasher`.
Agent side: `bamep_simulator::{DataPlaneTransferAgent, DataPlaneClient}` and the
proof-key / proof-transcript machinery.

`harness/src/vertical.rs` is adapted **verbatim** (bar 4 noted edits) from
`crates/server/tests/support/transfer_vertical.rs` at HEAD, so CP2 exercises the
exact real vertical the workspace integration tests already cover — with a
physical-representative chunk size and a runnable off-device form.

### What CP2 does NOT prove

- `bamep.m2.endpoint-capture-transfer`;
- Server-side `SourceReference` freshness validation (RF-2 / RF-6);
- Agent-side `SOURCE_REFERENCE_STALE`;
- RF-6 atomic M2 target creation;
- structured `SourceProvenance` authority;
- physical source reading;
- WinPE compatibility of the async M1 transfer participant.

These remain later Issue #61 checkpoints / production seams.

## Run (off-device, lab server or any dev host with PostgreSQL)

```bash
cd integration/physical/issue-61-endpoint-capture-data-plane/harness
cargo run --release
```

PostgreSQL: `BAMEP_ISSUE61_CP2_ADMIN_URL`, else `BAMEP_TEST_PG_ADMIN_URL`, else a
peer-authenticated Unix-socket DSN for the current OS user targeting the
`postgres` maintenance database (the role must be able to `CREATE`/`DROP`
databases). The harness creates one `bamep_issue61_cp2_<uuid>` database, migrates
it through the real Adapter, and `DROP ... WITH (FORCE)`s it on exit.

Disposable filesystem chunk store + UDS + TLS material: `harness/runtime/`
(git-ignored, on the ordinary root filesystem — never `sda`/`sdb`/`sdc`, never
the system temp dir). Each per-vertical directory is removed on drop; the empty
`runtime/` parent is removed on a clean exit.

## CP3 — physical selected-source M1-shaped context (`src/bin/cp3-context.rs`)

Establishes ONE exact selected-source `bamep.m1.data-plane-transfer` context
for the **real physical WinPE Endpoint** (from CP1), against the **existing**
`bamep_physint_spike` database (never created or dropped here), through
existing Domain/Application authority only:

```text
physical Endpoint (CP1, physically authenticated)
  -> current InventoryRevisionId + capture_source_observation_id + capturable_sources[]
  -> select exactly ONE agent_source_id  ->  SourceReference { inventory_revision_id, source_observation_id, agent_source_id }
  -> EnrollmentService::approve_enrollment      (PendingEnrollment -> Enrolled; CP1 left trusted-bootstrap out of scope)
  -> JobService::create_workflow (2 steps)  ->  admit  ->  satisfy_current_step_preconditions(step0)
  -> TransferService::create_transfer_context(step0, descriptive SourceProvenance = the exact tuple)
  -> TransferDispatchService::commit_transfer_dispatch(step0)  ->  Attempt{Dispatched}
  -> PRESSURE CHECK: create_transfer_context(step1, descriptive SourceProvenance = a STALE tuple
       — the Endpoint's PREVIOUS inventory revision / source-observation epoch)  ->  accepted with NO validation
```

`SourceProvenance` is descriptive-only labelled JSON (`descriptive_only:true`,
`not_a_validated_source_reference:true`). The action is
`bamep.m1.data-plane-transfer`. **Zero source bytes are read** (`cp3-context.rs`
has no disk/block-device/file/endpoint I/O; the MiniPC is never contacted).

The pressure check demonstrates that the current M1-shaped creation path does
**not** validate `current InventoryRevisionId` / `capture_source_observation_id`
/ `agent_source_id` — the absent RF-2/RF-6 Server-side freshness seam. This is
recorded, not treated as an M1 bug, and not fixed.

```bash
cargo run --release --bin cp3-context -- \
  --endpoint-id <physical-endpoint-uuid> --select <agent_source_id> --approve-endpoint
# re-read an already-created context without any mutation:
cargo run --release --bin cp3-context -- --dump-only <job-id>
```

## CP4 + CP5 — WinPE-native source-authority pressure + read-only physical bytes (`probe/`)

A **fresh** WinPE-native probe (`x86_64-pc-windows-msvc`, static CRT), rebuilt
from current HEAD — **not** the Issue #60 binary. Two independent logical
checkpoints, one process, one WinPE session:

- **CP4** — mint THIS boot's source-observation epoch by read-only enumeration;
  an operator predicate over **local hardware evidence** (model substring)
  picks one `agent_source_id`; the **pure `resolver`** resolves the tuple
  `(source_observation_id, agent_source_id)` — and only that tuple — to exactly
  the mapped SSD. Structurally-valid but **stale/unrecognised** references (the
  CP3 tuples, both now superseded; plus an unknown id in the current epoch) are
  **rejected fail-closed before any `GENERIC_READ` device handle is opened**,
  with **no fallback** to `PhysicalDrive0`/first/ordinal. Instrumented:
  `resolution_attempt_count`, `resolution_success_count`,
  `data_device_open_count`, `data_read_count` — the last two must be `0`
  throughout CP4. Prints `CP4_EXITCODE=`.
- **CP5** — only if CP4 PASSED: open the **CP4-resolved** locator with
  `GENERIC_READ` (never `GENERIC_WRITE`), obtain the exact device byte length
  via three read-only IOCTLs (`IOCTL_DISK_GET_LENGTH_INFO`,
  `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`, `IOCTL_STORAGE_READ_CAPACITY`) and require
  they agree, then perform small bounded raw reads at begin / middle / end plus
  a repeat of begin and middle — each SHA-256'd; the repeats must hash
  identically; no read may cross the device length. No filesystem mount, no
  volume lock/dismount, no write API, no state-mutating IOCTL. Prints
  `CP5_EXITCODE=`.

CP4/CP5 are **probe-local Spike evidence**. They do NOT prove
`bamep.m2.endpoint-capture-transfer` or a product `SOURCE_REFERENCE_STALE` —
no product component resolves an authoritative `SourceReference`. No
authenticated WSS session is used; evidence goes to stderr + a local NDJSON
file + one line to the lab sink.

Because a #60/#61 probe invocation mints a **fresh** epoch and never persists an
old one (RF-3: no cross-process/cross-boot physical identity), the CP3
`SourceReference` cannot be CP4's "current" tuple — it is CP4's genuine
**stale** input. CP4's current tuple is the one this run mints.

```bash
# host: pure resolver logic (red -> green)
cd probe && cargo test

# WinPE cross-build (owner-approved #60 toolchain: cargo-xwin + static CRT)
export PATH="$HOME/.local/bin:$PATH" XWIN_ACCEPT_LICENSE=1
RUSTFLAGS="-C target-feature=+crt-static" cargo xwin build --release --target x86_64-pc-windows-msvc
```

## CP6 — one real 8 MiB physical chunk across the real Worker data plane

`harness/src/bin/cp6-harness.rs` (Server-side, network-exposed on the lab
interface) + `probe6/` (WinPE-native, reuses `bamep-simulator`'s M1 reference
components — `connect_pinned_wss`, `authenticate`, `send_inventory_report`,
`DataPlaneClient`, `AgentProofKey`, `AgentTransferAuthorization`, proof
transcript; **no second transfer protocol, no sync re-implementation**).

`probe6/` cross-builds the `bamep-simulator` async dependency graph (the
tokio / hyper / rustls-related path, static CRT) for `x86_64-pc-windows-msvc`;
its PE imports exactly the #60-proven stock-WinPE DLL set
(`kernel32 ntdll ws2_32 ADVAPI32 bcrypt bcryptprimitives
api-ms-win-core-synch-l1-2-0`), no VCRUNTIME/UCRT. **Physical CP6 confirmed
async TCP / pinned WSS operation on stock WinPE** (`cp6.wss.established`).

**Fresh coherent lineage, one probe process** (RF-3 — the CP3/CP4/CP5 epochs are
all superseded and CP6 mints its own): enumerate → mint one fresh
`source_observation_id` + opaque `agent_source_id`s → operator-local SSD
selection (model substring, **local evidence only**) → lab-only coord TCP line
`{cp6_coord, source_observation_id, selected_agent_source_id}` (no
PhysicalDriveN/model/serial) → pinned TLS 1.3 WSS → real Agent auth →
`InventoryReport` carrying that epoch → the harness waits for the
`InventoryRevision` carrying that exact fresh source-observation epoch to
persist, correlates it through fixture-local read-only SQL, verifies the
selected `agent_source_id` is present, builds the exact descriptive
`SourceProvenance` tuple, `create_workflow` → `admit` →
`satisfy` → `create_transfer_context(8 MiB, that tuple)` → `commit_transfer_dispatch`
→ `ActionDispatch` (`bamep.m1.data-plane-transfer`) over the live session →
`ActionAck{Accepted}` → `TransferAuthorizationRequest`/`Grant` → resolver
`(obs, id)` → SSD locator → `CreateFileW(GENERIC_READ)` → raw chunk 0 = exactly
8 MiB → local SHA-256 → `PUT /api/data/v1/transfers/{id}/chunks/0` (real Worker
HTTPS) → digest-checked, durably accepted via UDS → **idempotent retry with a
FRESH request proof → `AlreadyHeld`** → **STOP**.

### CP6 physical result — PASSED

CP6 ran on the physical MiniPC and **passed**:

- one real **8,388,608-byte** chunk was read from `\\.\PhysicalDrive0`
  (the disposable 256 GB-class SSD), `CreateFileW` requesting **`GENERIC_READ`
  only** — **no source write**, no mount, no repair, no state-mutating IOCTL;
- the chunk crossed the **real sender-constrained Worker HTTPS** path
  (`https://192.168.99.1:<data-plane-port>`); the Server↔Worker authority
  decision used the **real UDS** path;
- the **first upload returned `Accepted`**; the durable chunk identity and the
  Worker chunk file were both committed before the success response;
- a **Fedora-independent SHA-256** of the durable chunk file equals the WinPE
  probe's physical-read digest:
  `cc0f11959a9dfe84de412a17db4b5c348db6ffd091b54ed7eb6ffaa8c611a649`;
- the **retry** used a **fresh request proof** (fresh `proof_id`) for the same
  transfer / chunk / digest / body and returned **`AlreadyHeld`**;
- **no duplicate durable chunk identity** was created.

The durable CP6 state intentionally **stopped** at: Job `Running`, JobStep
`Dispatching`, Attempt `InProgress`, Artifact `Incomplete`, ChunkManifest
unsealed, **held chunks = {0}**. CP7 is expected to continue exactly this
Transfer / Artifact.

### What CP6 does NOT prove

- production M2 `SourceReference` validation;
- `SOURCE_REFERENCE_STALE` product behavior;
- full-device capture;
- filesystem-/allocation-aware capture;
- manifest sealing;
- full Artifact reconstruction;
- `Artifact::Verified`;
- transfer completion;
- `ActionResult{Succeeded}`;
- Job success;
- production clock synchronization (see below).

The action is `bamep.m1.data-plane-transfer` — **not**
`bamep.m2.endpoint-capture-transfer`. No product backup policy is implied.
Filesystem-/allocation-aware capture, whole-device RAW fallback, and other
capture modes remain subjects for later Discovery. That is not CP6.

### Clock-skew Spike finding

During CP6 the physical bare-metal WinPE environment showed an **untrustworthy
UTC interpretation**:

- Fedora: local ≈ 00:20 -03, i.e. **UTC ≈ 03:20**.
- WinPE: the displayed local wall clock ≈ matched Fedora, **but**
  `HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation` `Bias = 0x1e0`
  = 480 minutes = **UTC-8**.
- Consequence: Rust `SystemTime` / Agent proof `issued_at` values were
  **≈ +5 hours ahead** of the Server.
- The M1 proof verifier **correctly failed closed with `AUTHORIZATION_DENIED`**
  because the bounded future-skew window was exceeded.
- The owner performed a **lab-only** manual clock alignment and repeated CP6,
  which then succeeded.

This is valuable physical Spike evidence. It is **not** a product bug to patch
inside Issue #61, production time synchronization is **not** solved, and
trustworthy time in the bare-metal maintenance environment is a **future
Discovery / architecture** requirement (the sender-constrained proof contract
depends on it).

```bash
# host: build the CP6 harness + resolver tests + host loopback smoke
cargo build --release --bin cp6-harness            # in harness/  (uses a disposable DB via BAMEP_PHYSINT_DB_URL + loopback ports)
cd probe6 && cargo test && cargo build --release    # host stub source = deterministic 8 MiB

# WinPE cross-build (owner-approved #60 toolchain: cargo-xwin + static CRT)
export PATH="$HOME/.local/bin:$PATH" XWIN_ACCEPT_LICENSE=1
RUSTFLAGS="-C target-feature=+crt-static" cargo xwin build --release --target x86_64-pc-windows-msvc
```

## Layout (21 authored files)

```text
issue-61-endpoint-capture-data-plane/
├── README.md
├── .gitignore
├── harness/                       CP2 + CP3 + CP6 (Server-side; one crate, three binaries)
│   ├── Cargo.toml
│   ├── Cargo.lock
│   └── src/
│       ├── main.rs               CP2 driver (off-device synthetic vertical)
│       ├── vertical.rs           adapted from crates/server/tests/support/transfer_vertical.rs
│       ├── testdb.rs             disposable PostgreSQL helper (CP2)
│       └── bin/
│           ├── cp3-context.rs    CP3 driver (physical selected-source M1-shaped context; + --dump-only)
│           └── cp6-harness.rs    CP6 harness: WSS gateway + network Worker HTTPS + UDS +
│                                 coord listener + orchestrator, against bamep_physint_spike
├── probe/                         CP4 + CP5 (WinPE-native, sync, no bamep-simulator)
│   ├── Cargo.toml
│   ├── Cargo.lock
│   ├── build.rs
│   └── src/
│       ├── main.rs               CLI + CP4 runner + CP5 runner + exit codes
│       ├── resolver.rs           pure (obs, id) tuple resolver + host red->green tests
│       └── sources.rs            read-only enumeration + GENERIC_READ raw reads + 3-IOCTL device length
└── probe6/                        CP6 (WinPE-native, async; links bamep-simulator M1 client)
    ├── Cargo.toml
    ├── Cargo.lock
    ├── build.rs
    └── src/
        ├── main.rs               CLI + async Agent session + one chunk PUT + idempotent retry
        ├── resolver.rs           byte-identical copy of probe/src/resolver.rs
        └── sources.rs            ~copy of probe/src/sources.rs (+ read_bytes_at; non-Windows stub = 8 MiB pattern)
```

`probe/src/resolver.rs`/`sources.rs` and `probe6/src/resolver.rs`/`sources.rs`
are deliberately duplicated so each WinPE probe is an independently
reproducible standalone physical participant (`probe/` is sync with zero
`bamep-simulator`; `probe6/` necessarily links it). This is intentional and is
**not** being refactored in this Spike, matching the #60 self-contained-probe
convention.

Generated binaries, `runtime/`, `runtime-cp6/`, evidence/NDJSON/logs,
credentials, and TLS key material stay ignored/untracked.
