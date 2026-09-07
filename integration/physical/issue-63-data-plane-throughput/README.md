# Issue #63 — automated physical MiniPC data-plane throughput (Spike, physical Integration Environment)

THROWAWAY Spike tooling for **Issue #63 Phase B**. NOT product architecture, NOT
a production Agent/appliance/service manager. Sibling of — and deliberately
separate from — the closed Issue #61 scaffolding
(`../issue-61-endpoint-capture-data-plane/`, preserved, not redefined here) and
the host-only Phase-A benchmark
(`../../benchmarks/issue-63-data-plane-throughput/`).

The work is delivered in **owner-reviewed stages**. Only Stage 1 exists so far.

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

## Stages 2 and 3 (not built yet)

- **Stage 2** — Issue-63 probe adaptation (exact source-length/safety gate,
  `CP7_CHUNK_SIZE`), fresh harness/transfer lineage per case, coordinator matrix
  state machine, balanced 8/16/32/64 MiB ordering, paired-ratio analysis. Proven
  off-device first. No physical matrix run.
- **Stage 3** — one-command supervisor + the actual clean-fast-path physical
  matrix (8/16/32/64 MiB, 2 GiB extent, 1 warm-up/size, 8 balanced cycles).

Each stage stops for owner review. No commit / push / GitHub mutation.
