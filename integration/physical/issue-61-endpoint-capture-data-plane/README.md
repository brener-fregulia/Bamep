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
- The final Issue #61 classification is **Outcome B** (see "CP7A physical
  result — PASSED" and "Issue #61 final classification — Outcome B" below).
  Attempt-by-attempt evidence lives on Issue #61.

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

## CP7A — bounded-prefix physical data-plane pressure (`harness/src/bin/cp7-harness.rs` + `probe7/`)

**Status: physically run and PASSED** — run `cp7a-lab-20260906T222258`, Gate-4a /
Subtest A (`auth_denial`). Owner-approved for the bounded pressure stage only.
CP7B full-device scale characterization is **not** authorized.

CP7A does **not** continue the CP6 Transfer. `probe7` mints a **fresh** source
epoch (fresh random `source_observation_id` + `agent_source_id`s, held only in
this one process) and the harness creates a **fresh** Job / Transfer / Artifact
lineage in `bamep_physint_spike`. The CP6 lineage, its held chunk 0, and
`runtime-cp6/` are never touched. The action remains
`bamep.m1.data-plane-transfer`; **no RF-2 / RF-6 / RF-7 production
source-authority layer is implemented** — its absence is the exact recorded
blocker and the confirmed final Issue #61 classification is **Outcome B**.

Bounded extent: **2,148,532,224 bytes** (`2 GiB + 1 MiB`) at an 8 MiB chunk
size → **257 chunks** = 256 × 8,388,608 B + one final **1,048,576 B** chunk.
This is a **bounded M1 pressure Artifact** — the durable `source_provenance`
carries `capture_extent: "bounded_prefix_pressure"`,
`not_a_complete_source_capture: true`, `prefix_bytes: 2148532224`. It is **NOT**
a complete `\\.\PhysicalDrive0` capture, **NOT** walkthrough D, **NOT** Outcome A.

`probe7` reads the bounded prefix **single-pass** — each logical chunk is read at
most once and enters the rolling full-Artifact SHA-256 exactly once; retries
reuse the same buffered bytes (invariant + host invariant tests in
`probe7/src/stream.rs`, plus mutation-based sensitivity verification). One
controlled fault is auto-armed by the harness per run — either the
**data-plane-only** Worker-listener restart or the Gate-4 Subtest A
authorization denial (see below), never both — once ~`threshold` chunks are
durably held (WSS / PostgreSQL / storage-root / durable chunk files / Agent
process all preserved); the probe re-acquires a fresh grant, re-runs resume
discovery, reconciles held identities from memory, and continues. Then it seals
(`chunk_count = 257`, the rolling digest), the Worker independently reconstructs
and verifies, the Artifact reaches durable `Verified`, and the probe sends
`ActionResult{Succeeded, TRANSFER_VERIFIED}` — which the harness-wired
`TransferTerminalEvidenceService` consumes to drive the Job to a durable terminal
state.

New CP7A pieces vs CP6: a **mandatory `--storage-root <path>`** (fails closed
before the Worker starts — absent / not a directory / not writable / under
`runtime-cp6` / obviously insufficient free space); a narrow lab-only coord
**response** carrying the Server's current UTC so the probe runs an **asymmetric
clock pre-flight gate** (`agent_now − server_utc ∈ [−60 s, +10 s]`, checked
**before** any `CreateFileW`/read/upload; proof freshness is **not** widened);
`cp7-harness issue-credential <signal>` to mint a fresh first-contact credential;
and a read-only fixture SQL final-state readout. Seal uncertain-delivery follows
the **current** M1 behavior (idempotent re-seal while `Incomplete`/
`PendingVerification`; a fresh-but-still-`401` data plane after a lost seal
response means the terminal verdict is **not observable** via the protocol —
recorded as finding **N1**, never fabricated as `TRANSFER_VERIFIED` or
`TRANSFER_ABANDONED`).

```bash
# host: invariant tests + host-loopback vertical + WinPE cross-build
cd probe7 && cargo test                              # host invariant + Gate-4 checkpoint tests; 2 heavy tests are #[ignore]
cargo test --release -- --ignored e_full_bounded     # the exact 2,148,532,224-byte digest + exactly-once
cd ../harness && cargo build --release --bin cp7-harness
export PATH="$HOME/.local/bin:$PATH" XWIN_ACCEPT_LICENSE=1
cd ../probe7 && RUSTFLAGS="-C target-feature=+crt-static" cargo xwin build --release --target x86_64-pc-windows-msvc
```

### CP7A physical result — PASSED

CP7A ran on the physical MiniPC (`cp7a-lab-20260906T222258`, Gate-4a / Subtest A
`auth_denial`) and **passed**:

- bounded extent **2,148,532,224 B** = **257 chunks** (256 × 8,388,608 B + one
  final 1,048,576 B), read from the CP4-resolved disposable SSD with
  `CreateFileW` requesting **`GENERIC_READ` only** — no source write, no
  format/partition/repair/restore, no mount, no CP7B, no full-device claim;
- real physical bytes crossed the real sender-constrained Worker HTTPS data
  plane; chunks became durably held; the manifest sealed at `chunk_count = 257`;
- **Gate-4a checkpoint** at `held = 8`: the probe read + buffered the pending
  chunk (`pending_chunk_index = 8`) and issued exactly one bodyless
  `discover_resume`; the harness fault decorator produced exactly **one**
  authorization denial (`held_chunks = 8`, `threshold = 8`, `deny_count = 1`,
  `denied_total = 1`);
- the probe **suspended once** (`suspension_count = 1`,
  `reason = authorization_denied`), requested a **fresh grant**, re-entered the
  stream with the same `StreamState`, and resume discovery reconciled
  `held = 8` / `pending = 8` / `pending_already_held = false` — the pending
  chunk was uploaded with **no second physical source read**;
- stream completed `held_chunks = 257`, `device_read_count = 257`; zero
  `cp7a.stream.put_transient`, zero `cp7a.stream.put_auth_denied`, zero
  `gate4_checkpoint.unexpected`;
- rolling full-Artifact SHA-256
  `g3G5p7YUFRfjTicWcEa_U9gkA22uWB_OQfiaqeVlNmA`
  (`total_bytes_hashed = 2148532224`, `chunk_count = 257`);
- the Worker independently reconstructed and verified; durable **Artifact
  `Verified`** (`artifact_id 785f0891-7584-46a0-ac3f-f2cf8179432f`); the probe
  sent `ActionResult{Succeeded, TRANSFER_VERIFIED}`; the harness drove **Job /
  JobStep / Attempt = `Succeeded`** with `manifest_sealed = true`; WinPE wrapper
  `CP7A_EXITCODE = 0`; final probe verdict `cp7a_pass = true`.

This PASS is a **bounded-prefix M1 pressure Artifact** exercised through
`bamep.m1.data-plane-transfer`. It is **NOT** a complete `\\.\PhysicalDrive0`
capture, **NOT** walkthrough D, **NOT** Outcome A, and **NOT** production
`bamep.m2.endpoint-capture-transfer`. Gate-4a proves the Spike
suspend → fresh-grant → resume state machine under a deterministic authorization
denial; it does **not** prove generic real-world network-outage recovery.

Raw run logs stay under `evidence/cp7a-lab-20260906T222258/` (git-ignored lab
evidence, never committed); only the concise durable facts above belong here.

### Issue #61 final classification — Outcome B

**Outcome B — physical source read works; the product-action / Application seam
blocks honest transfer.** Every lower layer is now physically proven: stock
WinPE source resolution and `GENERIC_READ` reads, real bytes across the
sender-constrained Worker HTTPS data plane, durable chunks, manifest seal,
Worker full-Artifact reconstruction/verification, durable `Artifact::Verified`,
terminal `TRANSFER_VERIFIED`, and one suspension → fresh authorization → resume
cycle. All complete-Artifact pressure still runs through
`bamep.m1.data-plane-transfer`, because the repository has **no** honest
production M2 seam: RF-2 `SourceReference` authority/freshness enforcement, RF-5
structured immutable `SourceProvenance` binding, RF-6 authoritative atomic
endpoint-capture target creation with server-side freshness checks + final
pre-dispatch revalidation, and RF-7 `bamep.m2.endpoint-capture-transfer`
dispatch/action handling (with the matching Agent-side `SOURCE_REFERENCE_STALE`
check before source read) are all absent. CP3 showed today's M1-shaped creation
path mechanically accepts a stale tuple as opaque provenance — evidence of the
missing M2 seam, not an M1 bug. The Issue's conditional full-source walkthrough D
is therefore not honestly reachable inside this Spike, and a larger fixture
capture would only add scale through the already-proven lower layer without
answering the outstanding M2 authority question.

### CP7A Gate-4 subtests — proving the interruption-recovery state machine

The single `data-plane-only` interruption in physical run 2 proved end-to-end
verification but did **not** exercise the Agent-side
`suspend → fresh grant → resume discovery → continue` recovery path: its PCAP
showed the ~400 ms Worker-listener restart is absorbed by the WinPE TCP stack
(one SYN retransmit) before it reaches the probe's application layer. The
recovery machinery is proven separately, in two subtests split by the launcher's
fault mode (exactly one per run; the harness fails closed if both are armed).
**Subtest A is the mode physically run and PASSED at Gate-4a**
(`cp7a-lab-20260906T222258`, above); Subtest B remains a later separate
validation:

- **Subtest A — `auth_denial` (launcher default; physically run + PASSED at
  Gate-4a, `cp7a-lab-20260906T222258`).** A Spike-local decorator over
  the real `bamep_server::ports::TransferAuthorizationRepository` Port produces
  **one** deterministic authorization denial for the run's own Transfer: the
  first eligible `load_authorization_state` call at/above
  `CP7_AUTH_DENY_AFTER_HELD` held chunks returns the Port's existing contractual
  `Ok(None)`, flowing through the unchanged real
  `authorize_request → WorkerAuthorizationOutcome::Denied →
  AuthorizationDecision::denied → HTTP 401 {"error":{"code":"AUTHORIZATION_DENIED"}}`
  chain. To make that denial land **deterministically** — with no ~8 MiB
  request body in flight and therefore no transport race — the probe is staged
  with a matching `--gate4-auth-denial-checkpoint-after-held N`: once `N` chunks
  are durably held it reads + hashes the next chunk into its `pending` buffer
  and then, **before that chunk's PUT**, performs exactly **one bodyless
  `discover_resume`** checkpoint. A `GET`-shaped resume carries no body, so the
  single real `401` is delivered cleanly → `ResumeStatus::AuthDenied` →
  **`SuspendedNeedsAuthorization`**. The probe requests a fresh grant, re-enters
  `run_stream_pass` with the **same `StreamState`**, entry resume discovery
  reconciles the `N` held chunks, the buffered `pending` chunk is `PUT` with
  **no source re-read and no rolling-hash double-update**, and the transfer
  verifies normally with `suspensions == 1`. The checkpoint fires exactly once
  and does not re-fire after reauthorization; a checkpoint resume that is *not*
  `AuthDenied` fails closed (the pending chunk is never uploaded). This is
  **LAB-ONLY Spike fault injection** — **NOT** production Agent recovery
  behaviour, **NOT** a recommendation for normal capture-protocol sequencing,
  and **NOT** a transport-outage simulation. No `crates/**` change; Worker
  authorization ordering is untouched.
- **Subtest B — `listener_restart`** (`CP7A_AUTH_DENY_AFTER_HELD=0
  CP7A_INTERRUPT_AFTER_HELD=8`). The original ~400 ms data-plane listener
  restart. A later separate validation will exercise a real transport/link
  failure that the WinPE stack cannot mask.

## Layout (29 authored files)

```text
issue-61-endpoint-capture-data-plane/
├── README.md
├── .gitignore
├── harness/                       CP2 + CP3 + CP6 + CP7A (Server-side; one crate, four binaries)
│   ├── Cargo.toml
│   ├── Cargo.lock
│   └── src/
│       ├── main.rs               CP2 driver (off-device synthetic vertical)
│       ├── vertical.rs           adapted from crates/server/tests/support/transfer_vertical.rs
│       ├── testdb.rs             disposable PostgreSQL helper (CP2)
│       └── bin/
│           ├── cp3-context.rs    CP3 driver (physical selected-source M1-shaped context; + --dump-only)
│           ├── cp6-harness.rs    CP6 harness (FROZEN) — one chunk PUT + idempotent retry
│           └── cp7-harness.rs    CP7A harness: CP6 boundaries + TransferTerminalEvidenceService +
│                                 mandatory --storage-root + Server-UTC coord response +
│                                 one of two mutually-exclusive data-plane fault modes
│                                 (auth_denial default / listener_restart) + issue-credential + final-state SQL
├── probe/                         CP4 + CP5 (WinPE-native, sync, no bamep-simulator)
│   ├── Cargo.toml
│   ├── Cargo.lock
│   ├── build.rs
│   └── src/
│       ├── main.rs               CLI + CP4 runner + CP5 runner + exit codes
│       ├── resolver.rs           pure (obs, id) tuple resolver + host red->green tests
│       └── sources.rs            read-only enumeration + GENERIC_READ raw reads + 3-IOCTL device length
├── probe6/                        CP6 (FROZEN; WinPE-native, async; links bamep-simulator M1 client)
│   ├── Cargo.toml
│   ├── Cargo.lock
│   ├── build.rs
│   └── src/
│       ├── main.rs               CLI + async Agent session + one chunk PUT + idempotent retry
│       ├── resolver.rs           byte-identical copy of probe/src/resolver.rs
│       └── sources.rs            ~copy of probe/src/sources.rs (+ read_bytes_at; non-Windows stub = 8 MiB pattern)
└── probe7/                        CP7A (WinPE-native, async; links bamep-simulator M1 client)
    ├── Cargo.toml
    ├── Cargo.lock
    ├── build.rs
    └── src/
        ├── main.rs               CLI + clock pre-flight + async Agent session + single-pass stream +
        │                         interruption/resume + Gate-4 LAB checkpoint + seal (A/B/C/D) + ActionResult
        ├── resolver.rs           byte-identical copy of probe6/src/resolver.rs
        ├── sources.rs            ~copy of probe6/src/sources.rs (non-Windows stub = position-keyed pattern)
        └── stream.rs             single-pass streaming INVARIANT + retry-safe loop + Gate-4 pre-PUT checkpoint + host tests
```

`probe/`, `probe6/`, and `probe7/` each keep a standalone `resolver.rs` /
`sources.rs` so every WinPE probe is an independently reproducible physical
participant (`probe/` is sync with zero `bamep-simulator`; `probe6/`/`probe7/`
necessarily link it). This duplication is intentional and is **not** being
refactored in this Spike; cross-checkpoint refactoring is deferred until after
Issue #61. `probe7/src/stream.rs` additionally exists (vs `probe6/`) because the
single-pass streaming/retry/hash invariant is host-test-covered there, with
mutation-based RED/sensitivity verification demonstrating the tests detect a
physical reread, a duplicate rolling-hash update, an uncertain-PUT re-send, and
a held-digest mismatch.

Generated binaries, `runtime/`, `runtime-cp6/`, `runtime-cp7a/`,
evidence/NDJSON/logs, credentials, and TLS key material stay ignored/untracked.
