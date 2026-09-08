# Issue #63 — automated physical MiniPC data-plane throughput (Spike, physical Integration Environment)

THROWAWAY Spike tooling for **Issue #63 Phase B**. NOT product architecture, NOT
a production Agent/appliance/service manager. Sibling of — and deliberately
separate from — the closed Issue #61 scaffolding
(`../issue-61-endpoint-capture-data-plane/`, preserved, not redefined here) and
the host-only Phase-A benchmark
(`../../benchmarks/issue-63-data-plane-throughput/`).

The work is delivered in **owner-reviewed stages**. Stage 1 and the Stage-3
36-case chunk-size matrix have PHYSICALLY PASSED (committed). Stage 4 (64 MiB
serial vs prep-ahead depth-2 + Worker PUT decomposition) is BUILT + off-device
validated, awaiting physical arm review.

---

## Stage 1 — prove the risky new automation primitives (NO transfer)

**Goal:** prove the smallest automation chain, end to end, with the owner only
powering the MiniPC on and pressing one key at the existing wimboot prompt:

```
derived Issue-63 PXE/WinPE runtime
  -> WinPE auto-starts the Issue-63 runner after wpeinit   (injected winpeshl.ini)
  -> network becomes ready
  -> runner reaches the Fedora coordinator
  -> Server UTC obtained
  -> WinPE system clock aligned automatically              (SetSystemTime, UTC)
  -> skew re-checked against a strict bound
  -> runner reports READY
```

Stage 1 opens **NO** `\\.\PhysicalDrive*` handle, issues **NO** IOCTL, performs
**NO** Transfer, creates **NO** Artifact and runs **NO** matrix. It carries no
credential, key or token.

### Evidence integrity (execution mode + observed milestones)

- The coordinator is started with an **explicit** `--mode physical | host-smoke`
  (never inferred from a hostname/address). `STAGE1_PHYSICAL_PASS` is *only ever
  printed* from the `Verdict::PhysicalPass` state-machine arm, which is
  structurally unreachable unless `--mode physical` **and** every integrity check
  holds. A host/simulated run reports `STAGE1_HOST_SMOKE_PASS` / `..._FAIL`.
- Physical-mode integrity checks (any failure ⇒ `STAGE1_PHYSICAL_FAIL`, never a
  pass): (1) every ingested event was stamped `mode=physical`; (2) the
  `winpe.runner_start` event reported the real Win32 clock backend
  (`win32-setsystemtime-utc`) — the host binary's `host-stub-noop` backend can
  never satisfy this even if invoked with `--mode physical`; (3) `winpe.booted`
  and `winpe.wpeinit_complete` arrived with `origin=bootstrap-forwarded` — i.e.
  the runner read them from the bootstrap's on-disk `X:\bamep-i63-events.ndjson`
  and forwarded them **verbatim**.
- The runner **never synthesises** `winpe.booted` / `winpe.wpeinit_complete`. If
  the bootstrap's on-disk evidence is absent / incomplete / out-of-order /
  corrupt it emits the distinct `winpe.bootstrap_evidence_missing` failure event,
  `stage1.failed`, and exits 22.

### Coordinator lifecycle

The coordinator **owns** the run lifecycle. On the first terminal verdict it
prints the verdict, then keeps accepting sink connections for `DRAIN_SECS` (5 s)
so the runner's trailing flush (`winpe.runner_end`) still lands in the evidence
file, then writes a one-word marker to `--verdict-file`
(`physical_pass` / `physical_fail` / `host_smoke_pass` / `host_smoke_fail`),
prints `STAGE1_COORDINATOR_TERMINAL marker=…`, and exits (`0` pass / `10` fail).
This is the **only** expected exit path.

The launcher watchdog keys off that verdict file: coordinator gone **with** the
file ⇒ *expected terminal completion* (`>>> coordinator reached its terminal
verdict … and exited (expected)`), normal cleanup. Coordinator gone **without**
the file, or any other child dying ⇒ *unexpected*
(`!!! … EXITED UNEXPECTEDLY — lab is NOT READY`, watchdog-abort marker set), fail
closed. `trap cleanup EXIT` runs on every path.

**Launcher exit status** (`resolve_terminal_exit`):

| terminal state | `run-stage1-lab.sh` `$?` |
|---|---|
| `physical_pass` / `host_smoke_pass` verdict | `0` |
| `physical_fail` / `host_smoke_fail` verdict (expected terminal, but the **experiment failed**) | `10` |
| a lab service died before any verdict | `20` |
| Ctrl-C before any verdict | `130` |
| setup / readiness-gate failure | `1` |

Verify without hardware: `./stage1/run-stage1-lab.sh --selftest-exit <state>`
(writes a synthetic terminal state into a throwaway dir, reports the resolved
exit code; no services, no network).

### Components

| Path | What it is | LOC (authored) |
|---|---|---|
| `coordinator/` | Fedora-side lab coordinator: `coord` TCP endpoint (hands the runner the Server's current UTC) + `sink` TCP endpoint (ingests the runner's cumulative NDJSON) + a pure, unit-tested **mode-aware** Stage-1 state machine that prints a deterministic `STAGE1_PHYSICAL_PASS` / `STAGE1_PHYSICAL_FAIL` (or `STAGE1_HOST_SMOKE_PASS` / `..._FAIL`). No TLS, no auth, no DB, no Worker. | ~490 |
| `winpe-runner/` | WinPE-native runner (`x86_64-pc-windows-msvc`, static CRT). Forwards the bootstrap's boot milestones **verbatim** from `X:\`, waits (bounded) for the network, round-trips the coordinator for UTC, aligns the WinPE **SYSTEM clock in UTC with `SetSystemTime`**, reads it back with `GetSystemTime`, re-checks the residual skew, then emits `stage1.ready`. Every event is stamped with its `--mode`. If `SetSystemTime` returns `FALSE` it records the **exact Win32 error** and fails closed with **no** registry/timezone fallback. | ~770 |
| `stage1/derive-stage1-runtime.sh` | Builds a derived Issue-63 PXE/WinPE runtime from the preserved Phase-9d assets **without modifying them**: a derived `autoexec.ipxe` with three extra `initrd` lines that make wimboot overlay `winpeshl.ini` + a bootstrap `.cmd` + the runner `.exe` into `X:\Windows\System32`; the Phase-9d `wimboot`/`BCD`/`boot.sdi`/`boot.wim` are HTTP-served through symlinks and re-hashed before **and** after. | ~270 |
| `stage1/run-stage1-lab.sh` | One-command foreground lab supervisor: preflight (read-only), derive, bring up dnsmasq + WinPE HTTP + coordinator (`--mode physical`), verify every readiness boundary, print `READY_FOR_MINIPC_POWER_ON`, then stream the coordinator verdict. Reverts only the lab state it created. | ~390 |
| `stage1/winpeshl.ini`, `stage1/bamep-i63-bootstrap.cmd.template` | The injected WinPE auto-start payload. `winpeshl.ini` launches `cmd /k bamep-i63-bootstrap.cmd`; the bootstrap writes `winpe.booted` **before** `wpeinit` and `winpe.wpeinit_complete` **after** to `X:\bamep-i63-events.ndjson` (local, no network), then runs the runner `--mode physical`, then leaves the CMD open. | ~35 |

### Why this doesn't modify the Phase-9d boot.wim

The dev host has **no WIM tooling** (`wimlib-imagex`, `dism`, `wimmountrw`,
`mkwinpeimg`, `cabextract` are all absent), so the `boot.wim` cannot be
rebuilt — and must not be. Instead, wimboot's documented file-overlay is used:
files passed as extra `initrd <uri> <name>` lines appear inside the booted image
at `X:\Windows\System32\<name>`. The derived `autoexec.ipxe` adds exactly three
such lines and `boot.wim` stays the last `initrd`, byte-identical to Phase-9d.

**Injection-name evidence (verified, no syntax change made):**

- **wimboot official docs** (`https://ipxe.org/wimboot`): *"You can provide
  additional files to wimboot. These files will appear within the
  `X:\Windows\System32` directory."* Documented `initrd` form:
  `initrd winpeshl.ini winpeshl.ini` — the **two-argument alias form**, flat
  name, no subdirectory components.
- **Phase-9d physical proof (this exact UEFI Secure Boot lineage):**
  `issue53-phase9d-setup-run.sh` boots with
  `initrd http://.../BCD BCD` · `initrd http://.../boot.sdi boot.sdi` ·
  `initrd http://.../boot.wim boot.wim` and the resulting WinPE ran the #60/#61
  probes — so the `initrd <uri> <name>` alias form works with wimboot under UEFI
  + Secure Boot on this hardware. The Phase-9d script itself quotes the wimboot
  architecture doc: *"placed in a CPIO archive under the literal flat virtual
  filename `<name>` (no subdirectory components)."*
- The generated Issue-63 `autoexec.ipxe` uses **exactly** that proven alias form
  for all three new files, with the exact intended flat names
  `winpeshl.ini`, `bamep-i63-bootstrap.cmd`, `bamep-i63-runner.exe`.

**What is NOT yet proven on this lineage:** Phase-9d injected only recognised
boot-manager files (BCD/boot.sdi); the routing of *non-boot-manager* extras into
`X:\Windows\System32\` is documented but unexercised here. The bootstrap fails
safe: on any error it prints a clear message and leaves an interactive CMD, so
one manual fallback is always possible. The first physical boot is also the proof
that the overlay places the three files in `X:\Windows\System32\`.

### Clock alignment detail

`SetSystemTime` sets `SYSTEMTIME` **in UTC**, so the stock-WinPE timezone `Bias`
that made Issue #61's manual `date`/`time` entry unreliable does not apply. The
call requires `SeSystemtimePrivilege` in the caller's token; WinPE normally runs
with it, and `SetSystemTime` enables it for the call — but this is not assumed.
The runner checks the `BOOL` result, records `GetLastError` on failure, reads the
clock back, re-checks skew against `[I63_SKEW_FLOOR_MS, I63_SKEW_CEIL_MS]`
(default `[-2000, +2000]` ms), and fails closed (`stage1.failed`, exit 31) if the
residual skew is still out of bound. **If `SetSystemTime` fails, that is a
STOP-and-report condition — the exact Win32 error is in the evidence.**

### Non-physical validation performed

- Stage-1 physical run **`stage1-20260907T155230` PASSED** on the disposable MiniPC
  (`evidence/stage1-20260907T155230/`): all 8 milestones, `SetSystemTime` `set_ok:true`
  `win32_error:0`, skew `-14856 ms → -6 ms`, `STAGE1_PHYSICAL_PASS`. A post-verdict
  supervisor-lifecycle bug (below) was found, root-caused, and fixed **without** a
  physical rerun.
- `cargo test` — coordinator state machine (15 tests: 5 physical-vs-host PASS-guard +
  3 lifecycle/verdict-marker) + runner pure helpers (16 tests, incl. observed-vs-missing
  bootstrap-evidence + generated-format fidelity). All green. RED→GREEN captured for
  the two evidence-integrity guards **and** the lifecycle verdict marker.
- `cargo build --release` — coordinator (Linux); `cargo xwin build --release --target x86_64-pc-windows-msvc` with `-C target-feature=+crt-static` — runner (WinPE). Clean.
- Runner PE imports: `api-ms-win-core-synch-l1-2-0.dll kernel32.dll ntdll.dll ws2_32.dll` — a strict subset of the #60/#61-proven stock-WinPE DLL set; no VCRUNTIME/UCRT; no new dependency.
- `bash -n` on both scripts (ShellCheck is not installed on this host).
- `derive-stage1-runtime.sh` dry-run: Phase-9d originals byte-identical before/after; derived tree serves the exact pinned Phase-9d bytes; no secret/key material; `autoexec.ipxe` structurally correct; `bootstrap.cmd` launches `--mode physical` and records `bootstrap_local_ts`.
- End-to-end host smoke (`--mode host-smoke`): derived runtime served over `python3 -m http.server` (symlink following verified) + coordinator + runner (host-stub clock) → `STAGE1_HOST_SMOKE_PASS`. Asserted: the literal `STAGE1_PHYSICAL_PASS` never appears in host-smoke coordinator output or evidence.
- Guard cases: host binary invoked `--mode physical` against a `--mode physical` coordinator → `STAGE1_PHYSICAL_FAIL` (clock backend integrity); missing bootstrap-evidence file → `winpe.bootstrap_evidence_missing` + runner exit 22 + `STAGE1_HOST_SMOKE_FAIL`; bad args / no `--mode` → exit 2; coordinator unreachable → `stage1.failed` exit 20.
- `run-stage1-lab.sh --preflight` — green.

The real `SetSystemTime` path and the wimboot System32 overlay are Windows/PXE
only and are proven **only** by the physical MiniPC boot.

### Run it

```bash
# build (once)
( cd coordinator && cargo build --release )
( cd winpe-runner && export PATH="$HOME/.local/bin:$PATH" XWIN_ACCEPT_LICENSE=1
  RUSTFLAGS="-C target-feature=+crt-static" cargo xwin build --release --target x86_64-pc-windows-msvc )

# read-only gate
./stage1/run-stage1-lab.sh --preflight

# bring the lab up (needs sudo for dnsmasq); prints READY_FOR_MINIPC_POWER_ON and waits
./stage1/run-stage1-lab.sh
```

Then: power on the MiniPC, press a key once at wimboot's
`Press any key to continue booting...` prompt, and watch the launcher terminal
for `STAGE1_PHYSICAL_PASS`. Ctrl-C tears the lab down (only what the launcher
created is reverted). Evidence: `evidence/<run-id>/` (git-ignored).

---

---

## Stage 2 — build + validate the safe physical matrix engine OFF-DEVICE (NO transfer matrix)

Owner-approved. Stage 2 builds the throwaway measurement machinery Stage 3 will
run on the disposable MiniPC, and proves as much as possible without the MiniPC.
**PHYSICAL MATRIX NOT ARMED** — there is deliberately no command that starts the
36 physical transfers.

| Path | What it is | LOC (authored / adapted) |
|---|---|---|
| `stage2-engine/` | Pure deterministic authority: the exact **36-case matrix plan** (2048 MiB extent, 8/16/32/64 MiB, 4 warm-ups + 32 measured, 4×4 Latin square ×2), exact chunk arithmetic + runtime agreement gate, the **physical source-safety predicate** (fail-closed; a rejection performs ZERO bulk reads), the Stage-3 **disk-budget preflight** (72 GiB payload / 90 GiB gate), the typed **per-case lifecycle** + **matrix sequencer** (stop on first FAILED/CONTAMINATED), the per-case NDJSON **result schema** (both measurement boundaries), and **measured-only aggregation + within-cycle paired ratios**. No I/O. | ~1340 / ~30 (Phase-A stat helpers, re-derived) |
| `coordinator/src/matrix.rs` | Typed Issue-63 lab ops (`next_case` / `case_ready` / `case_started` / `case_completed` / `matrix_completed`) over the engine; deterministic sequencing; stop-on-failure; result aggregation. `coordinator --matrix-selftest` walks the full 36-case plan in-memory with stub results; `coordinator --matrix` prints `PHYSICAL MATRIX NOT ARMED`. Stage-1 behaviour + regression unchanged. | ~430 / 0 |
| `stage2-harness/` | Linux one-transfer harness, **Phase-A backend (option (a))**: real Worker HTTPS `DataPlane`, real `FilesystemChunkStore` staging/fsync/linkat, real D2 `FullArtifactHasher`, real `bamep_simulator::DataPlaneClient` + per-request proof; `bamepd` faked over UDS (ADR-0018). `--smoke` = the **8/16/32/64 MiB host synthetic vertical** at a 128 MiB extent (one transfer/size → `Artifact::Verified` + a structured `CaseResult` each). `--case-file <Case.json>` runs one synthetic transfer. | ~560 / ~120 (Phase-A `run_one_transfer` + fake bamepd) |
| `stage2-probe/` | WinPE-native transfer probe (`x86_64-pc-windows-msvc`, static CRT). `resolver.rs` byte-identical copy from `#61` probe7; `sources.rs` minimally-adapted copy (`GENERIC_READ` only); `stream.rs` adapted from probe7 with the **CP7A Gate-4 fault-injection checkpoint removed** (Issue-63 clean fast path = ZERO deliberate fault injection); new `safety.rs` glue to the engine predicate; new smaller `main.rs`. One process = one transfer case: enumerate → mint epoch → operator selection → coord/Server-UTC → clock pre-flight → WSS/auth → dispatch → grant → resolver → GENERIC_READ open + 3-IOCTL length → **source-safety predicate** → **chunk-size agreement gate** → single-pass stream → seal → `Artifact::Verified` → `ActionResult`. `--self-check` proves the Accept + Reject paths and `bulk_read_count == 0` on the host. | ~700 / ~890 (resolver+sources copied, stream adapted) |
| `winpe-runner/src/matrix.rs` | LAB-ONLY `--matrix` subcommand on the committed Stage-1 runner, **NOT ARMED**: pure `parse_case` / `probe_argv` helpers (propagate chunk size + extent verbatim) for the Stage-3 loop, plus a not-armed banner. Intercepted before `parse_args` so Stage-1 invocation is byte-for-byte unchanged. | ~160 / 0 |
| `stage2/run-stage2-checks.sh` | Runs every Stage-2 off-device check in order. NO physical boot, NO device read, NO coordinator TCP, NO matrix. | ~55 |

### Run it (off-device)

```bash
./stage2/run-stage2-checks.sh          # engine + coordinator + probe + host smoke + runner regression

# WinPE cross-build + PE import inspection (owner-approved #60 toolchain)
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" XWIN_ACCEPT_LICENSE=1
export RUSTFLAGS="-C target-feature=+crt-static"
( cd stage2-probe && cargo xwin build --release --target x86_64-pc-windows-msvc )
llvm-readobj --coff-imports stage2-probe/target/x86_64-pc-windows-msvc/release/bamep-i63-stage2-probe.exe | grep 'DLL'
```

Probe PE imports (verified): `ADVAPI32 api-ms-win-core-synch-l1-2-0 bcrypt bcryptprimitives
kernel32 ntdll ws2_32` — a strict subset of the #60/#61-proven stock-WinPE DLL set;
no VCRUNTIME/UCRT, no new dependency.

---

## Stage 3 — ARM the minimum physical matrix path (PHYSICALLY RUN — `matrix_pass`)

Owner-approved. Stage 3 composes the already-proven pieces into ONE foreground
supervisor for the exact clean-fast-path matrix — **4 warm-ups + 8 balanced
cycles × 4 = 36 transfers**, **2,147,483,648 bytes (2048 MiB) per case**, chunk
sizes **8 / 16 / 32 / 64 MiB** (→ 256 / 128 / 64 / 32 chunks, no partial final
chunk), **one WinPE boot for the whole matrix, NO reboot between cases**, and
**ZERO deliberate fault injection** (no auth-denial, no listener restart, no
retry/resume fault). The only intended experiment variable is `chunk_size`.

**PHYSICAL MATRIX ARMED ONLY BEHIND `--arm`.** `./stage3/run-stage3-lab.sh`
with no `--arm` prints `PHYSICAL MATRIX NOT ARMED` and exits 0. Even with
`--arm` the supervisor runs every host-side preflight and only then prints
`READY_FOR_MINIPC_POWER_ON`. It never powers the MiniPC, never starts a physical
source read, never runs a transfer. **NO physical transfer has been executed.**

| Path | What it is | LOC (authored / adapted) |
|---|---|---|
| `coordinator/src/matrix_net.rs` | The ARMED networked wiring around the pure Stage-2 `MatrixCoordinator`. `coordinator --matrix --arm` opens a matrix TCP listener (one JSON object per line, one request per connection: `server_utc` / `next_case` / `case_ready` / `case_started` / `case_completed` / `case_failed`) + a probe-evidence sink. On the first terminal outcome (36 completed, or any halt) it writes `analysis.json` + the one-word `matrix.verdict` (`matrix_pass` / `matrix_fail`), prints `STAGE3_MATRIX_TERMINAL`, drains, exits (0 / 10). `--matrix` WITHOUT `--arm` still prints `PHYSICAL MATRIX NOT ARMED`. No device, no transfer, no DB, no Agent/Worker protocols. | ~430 / 0 |
| `winpe-runner/src/matrix.rs` | The `bamep-i63-runner --matrix --arm` loop (the committed `--matrix` alone stays inert). Per case: `next_case` from the matrix coordinator → re-check/re-align the WinPE UTC clock OUTSIDE the measured wall (the PROVEN Stage-1 `SetSystemTime` path) → `case_ready` → launch ONE Issue-63 transfer probe with explicit typed argv → observe its exit + `probe.case_result` line → on a verified Artifact `case_started` + `case_completed{result}`, on ANY other outcome `case_failed` and STOP (no retry). Opens no device, issues no IOCTL. | ~430 / 0 |
| `stage2-probe/` (delta) | `--runtime-credential-out <path>`: persists the rotated `runtime_credential` from `SessionEstablished` so the NEXT per-case probe process authenticates without re-redeeming the single-use first-contact credential (ADR-0012 rotation). The coord `source_selection` line now also carries `chunk_size` + `case_id`. `stream.rs` accumulates `read_ms` / `chunk_sha_ms` / `rolling_sha_ms` and `main.rs` `proof_ms` / `put_ack_ms` into the existing `CaseResult` schema fields (fill only — no new metric, no new boundary). | ~90 / 0 |
| `stage3-harness/` | The #61/CP7-shaped real Server/Postgres/WSS/Worker harness, **adapted from the closed Issue #61 CP7A harness** (`../issue-61-endpoint-capture-data-plane/harness/src/bin/cp7-harness.rs`). Removed: the Gate-4 auth-denial episode decorator, the `FaultMode` selection, the listener-restart supervisor. Added: a PER-CASE orchestration loop — one fresh Job / Transfer / Artifact lineage per matrix case (chunk size + `case_id` from the coord message), over ONE long-lived enrolled endpoint, runtime-credential rotation between per-case probe processes. Real boundaries unchanged: PostgreSQL adapter, `AgentControlGateway`/WSS, Worker control plane, Worker HTTPS `DataPlane`, `FilesystemChunkStore`, `TransferTerminalEvidenceService`. Mandatory `--storage-root` (≥ 90 GiB, fail closed). `bamep_physint_spike` is used, never created/dropped; #61 is not modified. | ~180 / ~600 |
| `stage3/derive-stage3-runtime.sh` | Derives the Issue-63 Stage-3 PXE/WinPE runtime from the pinned Phase-9d assets **without modifying them** (identical lineage to Stage 1; re-hashed before/after). Adds FIVE `initrd` overlay lines: `winpeshl.ini` + the Stage-3 bootstrap `.cmd` + the Stage-1 runner `.exe` + the Stage-2 probe `.exe` + the single first-contact enrollment credential; `boot.wim` stays last, byte-identical. The credential is the ONLY secret-shaped file permitted in the derived tree (mode 600, isolated-link HTTP; a deliberate Spike simplification vs #61's SMB mode-600). | ~270 / 0 |
| `stage3/run-stage3-lab.sh` | The one-command foreground supervisor. `--arm` required; `--preflight` and `--arm --preflight` run every host-side check and stop. Composes: derive → stage3-harness (real PG/WSS/Worker; fingerprint source) → ONE first-contact credential → matrix coordinator (`--matrix --arm`) → WinPE HTTP → dnsmasq → readiness gate → `READY_FOR_MINIPC_POWER_ON` → stream the coordinator verdict. Reverts only the lab network state it created. | ~470 / 0 |
| `stage3/{winpeshl.ini, bamep-i63-stage3-bootstrap.cmd.template}` | The injected WinPE auto-start payload: `winpeshl.ini` → `cmd /k bootstrap.cmd`; the bootstrap runs `wpeinit` then `bamep-i63-runner.exe --matrix --arm` with the full typed argv. The operator types NOTHING in WinPE after the one wimboot keypress. | ~35 / 0 |
| `stage3/run-stage3-checks.sh` | Every Stage-3 off-device check in order. NO physical boot, NO device read, NO 36-case matrix. | ~50 / 0 |

### From `./stage3/run-stage3-lab.sh --arm` to `READY_FOR_MINIPC_POWER_ON`

1. read-only host preflight (fails BEFORE any mutation): binaries built; runner
   **and** probe PE imports a subset of the stock-WinPE set; Phase-9d 7/7
   pinned; `coordinator --matrix-selftest` (36-case plan + chunk arithmetic +
   72 GiB payload + 90 GiB budget gate); Worker storage root known, writable,
   git-ignored, ≥ 90 GiB free; runtime/evidence dirs writable + git-ignored;
   PostgreSQL reachable + `bamep_physint_spike` present (read-only; never
   created/migrated here); lab interface; all six lab ports free + no stale
   Issue-63 process; scratch space; a note that these ports are lab-only (no
   production Bamep service is replaced);
2. evidence dir + `trap cleanup EXIT` installed;
3. lab network runtime (adds `192.168.99.1/24` + firewalld zone only if missing;
   reverted on exit);
4. start the stage3-harness; wait for `worker.https_listening` + WSS/coord/DP
   listeners; capture the 64-hex server leaf fingerprint;
5. mint exactly ONE fresh first-contact enrollment credential (umask 077, mode
   600, never printed);
6. derive the Stage-3 runtime with the fingerprint baked into the WinPE
   bootstrap + the credential injected; assert Phase-9d byte-identical
   before/after;
7. start WinPE HTTP + dnsmasq (derived conf) + the matrix coordinator
   (`--matrix --arm`, `--observed-free-bytes` from `df`);
8. readiness gate — every port, HTTP serves the pinned Phase-9d bytes, all five
   injections present + `boot.wim` last, bootstrap carries `--matrix --arm` +
   this fingerprint, `matrix-plan.json` written, every child alive;
9. health watchdog + print `READY_FOR_MINIPC_POWER_ON`.

After power-on + the single wimboot keypress **no WinPE typing is needed**:
`winpeshl.ini` auto-runs `bootstrap.cmd` → `wpeinit` → `bamep-i63-runner.exe
--matrix --arm …`, which drives all 36 cases and reports each to the coordinator.

### Per-case execution + failure/stop behaviour

Per case: `next_case` (typed `Case`) → clock re-check/re-align outside the
measured wall → fresh per-case Server/Transfer/Artifact lineage (harness) →
fresh runtime credential as needed (rotated + persisted by the previous probe)
→ source observation + Issue-63 source-safety predicate (fail-closed, ZERO bulk
read on reject) → exact chunk-size agreement across plan / dispatch / Server
Transfer / probe / manifest → single serial pass → seal → Worker D2 → require
`Artifact::Verified` → complete `CaseResult` → only then `next_case`.

Any source-safety failure, unexpected retry/resume/auth suspension, transport /
read / digest / proof / Worker / seal failure, `Artifact != Verified`, malformed
result, or per-case process death ⇒ the case is `FAILED` or `CONTAMINATED`, the
coordinator **stops handing out cases**, all completed evidence is preserved,
`analysis.json` + `matrix.verdict=matrix_fail` are written, and the matrix
terminates NON-ZERO. **No "retry until green", no silent repeat of a measured
case.**

### Measurements (Stage-2 definitions, unchanged)

`bulk_stream_wall` (first bounded source read → final expected chunk durably
accepted) and `verified_transfer_wall` (before resume/stream → `Artifact
Verified` after seal + D2) are separate. `resume_ms`, `seal_d2_ms`, `read_ms`,
`chunk_sha_ms`, `rolling_sha_ms`, `proof_ms`, `put_ack_ms`, exact bytes/chunks,
and the by-construction connection count are all preserved.

### Evidence — one run directory `evidence/<run-id>/`

`launcher.log` · `harness.log` · `http.log` · `dnsmasq.log` · `coordinator.log`
· `derive.log` · `fingerprint.txt` · `matrix/{matrix-plan.json,
case-results.ndjson, coordinator-events.ndjson, probe-evidence.ndjson,
analysis.json}` · `derived-runtime/{phase9d-hashes-{before,after}.txt,
derived-manifest.txt}` · `matrix.verdict`. No raw disk bytes, no credentials, no
keys, no secrets. All runtime evidence is git-ignored.

### Source safety (Spike level — unchanged from Stage 2)

current source observation / `agent_source_id` → resolver → expected model
(`NGFF 2280 256GB SSD`) → exact device length (`256,060,514,304` bytes) →
bounded extent ≤ length → no ordinal/path-only authority → zero bulk read until
PASS. `\\.\PhysicalDrive0` is NOT authority. `GENERIC_READ` only; no
`GENERIC_WRITE`, no destructive IOCTL, no format/repartition/mount/repair.

### Run it (off-device only — nothing here is armed)

```bash
./stage3/run-stage3-checks.sh                    # engine + coordinator + probe + runner + harness + scripts
./stage3/run-stage3-lab.sh --preflight           # read-only host checks
./stage3/run-stage3-lab.sh --arm --preflight     # armed interface, host checks only, still no boot

# WinPE cross-build (owner-approved #60 toolchain) + PE import inspection
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" XWIN_ACCEPT_LICENSE=1
export RUSTFLAGS="-C target-feature=+crt-static"
( cd winpe-runner && cargo xwin build --release --target x86_64-pc-windows-msvc )
( cd stage2-probe && cargo xwin build --release --target x86_64-pc-windows-msvc )
```

### Non-physical validation performed

- **Physical attempt 1 aborted at the bootstrap → runner argv contract** (before
  any network wait, `next_case`, probe launch, source access or Transfer): the
  WinPE bootstrap passed `--run-id` to the runner, which `parse_matrix_args`
  does not accept ⇒ `BAD_ARGS` exit 2; and a literal `(no retry)` inside the
  bootstrap's `if/else` block broke cmd.exe parsing (`. was unexpected at this
  time.`). Both fixed in `bamep-i63-stage3-bootstrap.cmd.template`; a new
  `run-stage3-checks.sh` step now renders the bootstrap and runs its exact
  generated runner argv against the host binary.
- `stage3/run-stage3-checks.sh` — **STAGE3_CHECKS_PASS**: stage2-engine (53
  tests) + coordinator (25 tests incl. 3 `matrix_net` line-protocol tests) +
  stage2-probe (14) + winpe-runner (23 incl. 7 matrix-loop tests) + **the
  generated-bootstrap → runner argv contract** (renders the template with the
  same `@TOKEN@` set, extracts the `bamep-i63-runner.exe …` invocation, runs
  that exact 28-arg argv against the host runner → `STAGE3_MATRIX_RUNNER_ARMED`,
  NOT `BAD_ARGS`, reaches the deliberately unreachable network boundary and exits
  `20`) + stage3-harness release build + `issue-credential` arg guard + `bash -n`
  all scripts + the launcher `PHYSICAL MATRIX NOT ARMED` banner.
- `stage3-harness` composes against **real PostgreSQL** (`db.connected_and_migrated`),
  real WSS (`wss.listening`), real Worker control plane (`worker.ipc_available`),
  real Worker HTTPS (`worker.https_listening`); stable 64-hex leaf fingerprint.
- **End-to-end one-case host smoke** (127.0.0.1, real PG, synthetic STUB source,
  full 2048 MiB / 256-chunk transfer): runner `next_case` → harness fresh
  lineage `chunk_size=8388608` → WSS auth → endpoint enrolled → dispatch → 256
  chunks streamed (each read once, `device_read_count=256`) → **seal →
  `Artifact::Verified` via real Worker D2 of 2 GiB** → `probe.case_result` with
  every field incl. the new sub-timings → the rotated `runtime_credential`
  persisted for the next process. The loaded host loopback induced one transient
  PUT ⇒ the case came back `CONTAMINATED` ⇒ the coordinator halted, wrote
  `matrix_fail` + `analysis.json`, handed out no more cases (**fail-closed, no
  retry — exactly the required behaviour**).
- `run-stage3-lab.sh --preflight` and `--arm --preflight` — green on the wired
  physical lab host (Phase-9d 7/7, PG reachable, 164 GiB free under the storage
  root, all ports free).
- WinPE cross-build clean; PE imports (`llvm-readobj --coff-imports`):
  runner `api-ms-win-core-synch-l1-2-0 kernel32 ntdll ws2_32`; probe
  `ADVAPI32 api-ms-win-core-synch-l1-2-0 bcrypt bcryptprimitives kernel32 ntdll
  ws2_32` — both a strict subset of the #60/#61-proven stock-WinPE set; the
  Stage-3 deltas added NO new import.

### Stage 3 — physical result (authoritative)

- **run_id `i63s3-20260907T185027`** — `matrix_pass`. 36/36 `CaseResult`, 0
  `excluded_unverified`, every Artifact `Artifact::Verified`. One WinPE boot, no
  reboot between cases, zero deliberate fault injection. Runtime evidence is
  git-ignored and not committed.
- Median throughput (MiB/s), `bulk` / `verified`:

  | chunk | bulk | verified |
  |---|---|---|
  | 8 MiB | 31.3437 | 30.4337 |
  | 16 MiB | 32.1515 | 30.8837 |
  | 32 MiB | 34.0892 | 30.9899 |
  | 64 MiB | 37.3213 | 33.6851 |

- 32 → 64 MiB paired physical `bulk`: median ratio **1.10066**, 64 MiB faster in
  **7/8** cycles.
- Timing diagnosis (64 MiB cases): `read_ms` ~5.5–5.9 s, `chunk_sha_ms`
  ~5.36–5.48 s, `rolling_sha_ms` ~6.06–6.18 s, `proof_ms` ~6 ms, `put_ack_ms`
  commonly ~31–36 s (slower outliers), `bulk` wall commonly ~50–55 s. **`put_ack`
  dominates**; serial local preparation **plus** PUT/ACK waiting explains nearly
  the whole `bulk` wall, because the probe prepares chunk N+1 only after
  `put_chunk(N)` returns.
- Conclusion: **64 MiB is the best of the tested serial chunk sizes**, but chunk
  size is not the dominant remaining lever — the serial prep↔PUT dependency and
  the Worker-observed PUT boundary are. This is the Stage-4 question.

---

## Stage 4 — 64 MiB serial vs prep-ahead pipeline depth 2 + Worker PUT decomposition (micro-Spike)

Owner-approved throwaway micro-Spike. Answers exactly two questions:

- **Q1** — how much physical `bulk` throughput is recovered when local
  preparation of chunk N+1 overlaps the in-flight PUT/ACK of chunk N?
- **Q2** — inside the Worker-observed PUT boundary, where is the remaining time
  spent, enough to choose the next optimization?

**Pipeline depth 2 here = PREP-AHEAD ONLY.** At most: chunk N is being
PUT/awaiting ACK while chunk N+1 is read + hashed concurrently. Still exactly
**one network PUT in flight**, **ascending PUT order**, **no out-of-order durable
acceptance**, **no two concurrent Worker PUT requests**. Multi-PUT concurrency is
explicitly out of scope for this stage.

### Status — BUILT + OFF-DEVICE VALIDATED; physical 10-case matrix NOT yet run

| Path | What it is (delta only) | authored |
|---|---|---|
| `stage2-probe/src/stream.rs` | `run_stream_pass_prep_ahead` + the dedicated **producer thread** (owns its own `GENERIC_READ` handle — a 2nd open of the already-safety-PASSED locator — and the rolling `Sha256` for the whole pass; `sync_channel(1)`; NO `unsafe`). Foreground: one `current` buffer, one `put_chunk().await`, one outstanding `Prepare`. `StreamState::{prepared_peak,producer_read_log,finalized_digest}`. Serial `run_stream_pass` **byte-identical**. | ~330 |
| `stage2-probe/src/main.rs` | `--mode serial \| prep_ahead_2` (default `serial` ⇒ every Stage-1/2/3 invocation unchanged); prep-ahead builds a `Send + 'static` reader factory per `'outer` pass; `mode` + `prepared_buffer_peak` on `probe.plan` / `probe.case_result`; `--pipeline-check` host synthetic serial-vs-prep-ahead digest-parity smoke. | ~140 |
| `crates/worker/src/data_plane/i63_timing.rs` (+ `http.rs` / `upload.rs` deltas) | env-gated (`BAMEP_I63_WORKER_PUT_TIMING`) best-effort NDJSON per chunk PUT keyed `transfer_id`+`chunk_index`: `authorize` / `stage_call` / **overlapping** `body_pump` + `staging_worker` / `begin_stage` / `write_sum` / `digest` / `finalize` (fsync/placement) / `commit_chunk` / `handler_total`. Absent ⇒ **zero** clock reads, zero I/O, zero behaviour change. No fsync of the sink, no secrets, write failure never affects auth/durability/integrity/status/outcome. | ~180 |
| `stage2-engine/src/stage4.rs` | the deterministic **10-case plan** (2 warm-up `S,P` + 4 cycles `(S,P)(P,S)(S,P)(P,S)`, 64 MiB only, 32 chunks/2048 MiB), `S4CaseResult`, `analyse_s4` (per-mode medians + within-cycle paired **P/S** ratios, `n=4`, **no significance claim**, `overlap_saved_ms` derived diagnostic), `WorkerPutRecord` + `analyse_worker_decomp` (per-interval medians/p10-p90, overlapping intervals kept labelled). | ~430 |
| `coordinator/src/stage4_net.rs` (+ `--stage4` / `--stage4-selftest`) | ARMED networked 10-case authority: `server_utc` / `next_case` / `case_ready` / `case_started` / `case_completed{S4CaseResult}` / `case_failed`; strict linear per-case order; halt on any non-`Verified` / failure; at terminal writes `analysis.json` (S-vs-P + Worker decomp joined by `transfer_id`→mode) + `matrix.verdict` = `stage4_pass` / `stage4_fail` / **`stage4_invalid`** (10/10 done but Worker timing missing/short ⇒ Q2 unanswerable). `--stage4` without `--arm` ⇒ `STAGE4 NOT ARMED`. | ~430 |
| `winpe-runner/src/matrix.rs` (`--stage4` / `--stage4 --arm`) | the Stage-4 loop: `parse_s4_case` (carries `mode`), `s4_probe_argv` (= Stage-3 argv + `--mode <wire>`), `build_s4_case_result`, per-case clock re-align OUTSIDE the measured wall, one probe process per case, halt-on-non-verified. Committed `--matrix` path untouched. | ~330 |
| `stage3-harness` (`--stage4`) | same #61-shaped real PG/WSS/Worker composition; `--stage4` only swaps `runtime-stage4/`, the 30 GiB budget floor, and **requires `--worker-timing-file <path>`** (fails closed early; must be OUTSIDE the chunk-store tree) → exports `BAMEP_I63_WORKER_PUT_TIMING` before the in-process Worker spawns. Per-case orchestration / coord protocol / action (`bamep.m1.data-plane-transfer`) unchanged. | ~70 |
| `stage3/derive-stage3-runtime.sh --stage 4` | same Phase-9d lineage / 5 injections / secret sweep; `--stage 4` swaps the injected bootstrap `.cmd` (`--stage4 --arm`) + `winpeshl-stage4.ini`. Phase-9d re-hashed before/after; unchanged. | ~20 |
| `stage4/run-stage4-lab.sh` | one-command foreground supervisor; `--arm` required; `--preflight` / `--arm --preflight` stop after host checks. `READY_FOR_STAGE4_MINIPC_POWER_ON`. Reverts only the lab network state it created. | ~570 |
| `stage4/run-stage4-checks.sh` | every Stage-4 off-device check in order. | ~110 |

### Off-device validation performed

- `stage4/run-stage4-checks.sh` → **STAGE4_CHECKS_PASS**: stage2-engine (67 tests
  incl. 15 `stage4`) + `clippy -D warnings`; coordinator (30 tests incl. 5
  `stage4_net`) + `--stage4-selftest` (`STAGE4_SELFTEST_PASS`) + `--stage4` NOT
  ARMED banner; stage2-probe (19 tests) + `--self-check` + **`--pipeline-check`**
  (`PROBE_PIPELINE_CHECK_PASS`: prep-ahead full-Artifact digest **bit-identical**
  to serial over the same bounded stub source, `prepared_buffer_peak == 2`, every
  chunk read once ascending); winpe-runner (28 tests incl. 5 stage4-loop);
  `bamep-worker` (108 tests incl. the env-gated timing hook) + inert-with-var-unset
  check; **generated Stage-4 bootstrap → runner argv contract** (renders the
  template, runs the exact `--stage4 --arm …` argv → `STAGE4_RUNNER_ARMED`, NOT
  `BAD_ARGS`, reaches the deliberately unreachable network boundary, exits `20`);
  stage3-harness release build + `--stage4` requires `--worker-timing-file`;
  `bash -n` all scripts.
- **prep-ahead pipeline RED→GREEN** (`stage2-probe cargo test`): RED = the serial
  pass reads no chunk N+1 while PUT(N) is in flight (reads at PUT exit == entry ==
  `N+1`); GREEN = prep-ahead reads+hashes chunk N+1 during PUT(N) (reads at PUT
  exit `≥ N+2`), `max_in_flight == 1`, PUT order `0..n`, `finish_digest ==`
  serial reference digest, `prepared_peak() == 2`, producer opened its source
  once; plus contamination-stops (401 on a PUT → `SuspendedNeedsAuthorization` +
  `PutAuthDenied`, no retry-to-green) and producer-open-failure → `Fatal`.
- **Stage-3 regression**: `stage3/run-stage3-checks.sh` → **STAGE3_CHECKS_PASS**
  (serial path + committed `--matrix` runner + Stage-3 bootstrap→argv contract all
  unchanged).
- WinPE cross-build (`cargo xwin`, `-C target-feature=+crt-static`) clean. PE
  imports (`llvm-objdump -p`): runner `api-ms-win-core-synch-l1-2-0 kernel32 ntdll
  ws2_32`; probe `ADVAPI32 api-ms-win-core-synch-l1-2-0 bcrypt bcryptprimitives
  kernel32 ntdll ws2_32` — **identical to the committed Stage-3 executables**; the
  `--mode` / prep-ahead producer thread (`std::thread` + `std::sync::mpsc`) and
  the Stage-4 runner loop added **no new import**.
- `run-stage4-lab.sh --preflight` / `--arm --preflight` run every host-side check
  and stop before any service, derive, or boot.

The physical MiniPC boot, the physical SSD `GENERIC_READ` prep-ahead path, the
env-gated Worker PUT timing under real load, and the 10-case matrix have **NOT**
been run. Stops here for owner blocker/safety review + physical arm authorization.
