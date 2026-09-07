# Issue #63 Spike — endpoint-capture data-plane throughput benchmark

**THROWAWAY.** Not production, not a production Agent, not an observability
framework, not a new transport. Detached standalone workspace; it reuses the
real current components verbatim (see `Cargo.toml`).

## Experiment history

- **Phase A1/A2** (approved): reproducible host baseline + per-stage attribution.
- **Case B** (approved): `chunk_size` 8 MiB → 64 MiB; mechanism = amortising the
  ~fixed per-durability-boundary `fsync` cost that dominates small chunks.
- **Knee sweep 8/16/32/64 MiB** (approved, done — `runs/KNEE-FINDINGS.md`):
  throughput rises from 8 MiB toward 32 MiB; tmpfs request-overhead curve
  flattens early (~16 MiB); the 8→16 XFS step is inconclusive (slow-disk
  window). Its 2 GiB / n = 8 reading that "32→64 still materially improves" did
  **not** replicate at 3 GiB / n = 12 — see the upper sweep.
- **Upper sweep 32/64/96/128 MiB** (approved, done — `runs/UPPER-FINDINGS.md`):
  on the noisy XFS host, **no size above 32 MiB demonstrated a defensible
  throughput benefit** — every within-cycle paired step is a 6/12 coin flip.
  32 MiB is the smallest conservative candidate with no demonstrated loss above
  it; the exact knee is **not statistically located**. PUT / per-chunk wall
  becomes approximately byte-proportional in the upper range; an isolated
  diagnostic is consistent with a byte-dependent `fsync(file)` component
  becoming material by the 64 MiB+ regime (mechanism hypothesis, not a located
  transition). Larger chunks do not move the observed throughput wall.
- **Cases C/D/E are NOT implemented.** No keep-alive, no pipelining, no parallel
  uploads, no connection reuse, no production code change — in any build.

## Current default run

`cargo run --release` sweeps `--chunk-mibs 32,64,96,128` at a **3072 MiB**
equal-byte extent (96 / 48 / 32 / 24 whole chunks — no partial final chunk;
startup asserts `extent % chunk_size == 0`) over `--cycles 12` cycles. Each
cycle runs every size once in a **balanced (rotated) order** (a 4×4 Latin square
over the four sizes) so temporal disk-load/cache effects do not systematically
favour one size. One warm-up transfer per size is excluded; every other raw run
is printed and kept. Analysis reports **both** independent medians **and
within-cycle paired ratios** (`32→64`, `64→96`, `96→128`) — the four sizes in
one cycle share a closer disk-load epoch. tmpfs control: `--extent-mib 768`
(24 / 12 / 8 / 6 whole chunks), diagnostic only.

`chunk_size` is the only experiment variable; chunk **count** per transfer is
its inverse and necessarily moves with it.

## What it exercises

The real current data plane, host-side, as much as is faithful without physical
PXE / 1 GbE / MiniPC hardware:

| Layer | Real? | Notes |
|---|---|---|
| Agent-side `DataPlaneClient` (`bamep_simulator::data_plane`) | **real, verbatim** | fresh TCP → pinned TLS 1.3 → HTTP/1.1 → 1 request → teardown, per call |
| Per-request Ed25519 proof (`AgentTransferAuthorization`) | **real, verbatim** | fresh proof + signature per chunk |
| pinned-leaf TLS 1.3 client config | **real, verbatim** | `pinned_tls13_client_config`, exact leaf fingerprint |
| Worker HTTPS `/api/data/v1/` listener (`bamep_worker::data_plane::DataPlane`) | **real, verbatim** | Axum + axum-server, structural parse, streaming body |
| Worker E1 control client (`bamep_worker::ipc::worker_control`) | **real, verbatim** | UDS, correlation, generation, timeouts |
| Worker D1 storage (`FilesystemChunkStore`) | **real, verbatim** | staging write + SHA-256 + `fsync(file)` + `linkat` no-replace + `fsync(dir)` |
| Worker D2 reconstruction (`FullArtifactHasher`) | **real, verbatim** | independent full re-read + SHA-256 at seal, plus a second harness-driven recomputation via the same public API |
| `bamepd` | **FAKE** (UDS auto-responder) | ADR-0018: every host integration harness fakes bamepd. Approves everything with ~0 latency. Real bamepd + PostgreSQL durable-commit cost is a **Phase B / physical-lab** measurement, deliberately NOT modelled. The fake still returns `Verified` only when the Worker's D2 digest matches the Agent's declared digest, so the end-to-end `Verified` assertion is a real check. |
| 1 GbE link | absent | loopback (`127.0.0.1`, ephemeral port) |
| physical source disk | absent | one warm `chunk_size` buffer, `to_vec()` per chunk = "materialization" |

## Baseline configuration (Case A)

- serial: read → per-chunk SHA-256 → rolling Artifact SHA-256 → proof → PUT →
  await response → next;
- connection lifecycle: one TCP + one TLS 1.3 + one HTTP/1.1 handshake per
  request; `chunks + 2` per transfer (resume + N chunk PUTs + seal). The count
  is asserted **by construction** here — the fresh-connection-per-request
  property belongs to the real `DataPlaneClient` and was empirically confirmed
  in Phase A1/A2.
- current Worker durability/integrity semantics, unchanged.

## Run

```
cargo run --release -- [--chunk-mibs a,b,c,d] [--extent-mib N] [--cycles C] [--storage-dir DIR]
```

- `--chunk-mibs` sizes to sweep (default `32,64,96,128`).
- `--extent-mib` equal byte extent per transfer (default `3072`; must be a whole
  multiple of every chunk size, else startup panics). tmpfs control: `768`.
- `--cycles` balanced cycles; measured transfers per size == cycles (default `12`).
- `--storage-dir` real-disk parent for the Worker chunk store. **Important:**
  the default is `$TMPDIR`, which on the dev host is **tmpfs (RAM)** and hides
  the per-chunk `fsync` cost. Point this at a real filesystem (xfs) for the
  primary result.

Unknown flags are ignored; an unparseable value falls back to the default.

## Reproducing the published findings

The two `runs/*-FINDINGS.md` documents are derived entirely from the four
`runs/*.log` files, which contain: host line, resolved config, every warm-up and
every measured transfer wall + MiB/s + MB/s + peak RSS, the per-cycle order, the
full KNEE ANALYSIS block (medians, per-chunk decomposition, staging split, raw
fsync decomposition, independent ceilings) and the WITHIN-CYCLE PAIRED ANALYSIS
block (raw per-cycle ratios, median paired ratio, IQR, MAD, overall-median
ratio). Re-running `cargo run --release` with the same `--storage-dir`
filesystem type reproduces the analysis structure; absolute numbers depend on
the disk and its concurrent load.

## Raw logs: what was and was not changed

All four committed `runs/*.log` files were generated *before* the owner-review
wording corrections. **Their raw measurements are unchanged.** During review,
only *derived interpretation and statistics* were corrected — in the
`*-FINDINGS.md` documents and (for future runs) in the harness — never in the
logs:

1. **Line 3 (`storage_dir: …`)** of each of the four logs was edited to replace
   a host-local absolute scratch path (which exposed a username and a random
   throwaway directory UUID, no evidentiary value) by a neutral placeholder that
   keeps the meaningful part — the detected filesystem kind (`(fs ~= xfs)` /
   `(fs ~= tmpfs)`). Newer builds print only the filesystem kind, never the path.
   This is the **only** edit made to any log file.
2. **All four logs** (`knee-*.log` and `upper-*.log`) carry one generated
   interpretive caveat near their end ("*Case B + this sweep indicate the
   durability cost in THIS environment is strongly fixed-durability-boundary
   dominated*"). That sentence is **superseded by `runs/UPPER-FINDINGS.md`**: the
   fixed-per-boundary regime is established only for *small* chunks (≤ 32 MiB);
   an isolated diagnostic is consistent with a byte-dependent `fsync(file)`
   component becoming material by the 64 MiB+ regime, so the tested 32–128 MiB
   range is not uniformly fixed-boundary dominated. The harness's current build
   prints the corrected caveat.
3. **RSS "median" values** displayed in the logs' `KNEE ANALYSIS` blocks were
   computed by a helper that returned the upper-middle value, not a true median
   (fixed in the harness — see the RSS-median notes in both `*-FINDINGS.md`).
   The affected derived cells (KNEE XFS 32 MiB `109→101`, KNEE tmpfs 32 MiB
   `108→100`, UPPER XFS 96 MiB `205→204.5`) were corrected **in the FINDINGS
   docs only**; the logs' `peak process RSS` lines and every raw per-transfer
   `peakRSS` value are unchanged.

**No timing, throughput, ordering, RSS, correctness or statistical measurement
in any log was altered** — only the line-3 path field was redacted.
Loopback addresses (`127.0.0.1:<ephemeral port>`) are retained as non-sensitive.

## Known harness limitations

- `bamepd` faked (see table) — cannot measure real durable-commit cost; Phase B.
- No 1 GbE, no physical firmware / NIC / storage-controller path.
- The UDS control socket and the generated TLS identity live under `$TMPDIR`
  (tmpfs) even for the XFS run — they are control-plane only, not the measured
  chunk store, and the fake bamepd is ~0 latency.
- `fsync` latency on a shared dev disk is high-variance (±30–40 %, occasionally
  up to 2×); the *mechanism* and the *within-cycle paired* comparison are the
  findings, not any single millisecond value.
- The `worker_staging_split` probe uses a size-dependent sample
  (`n` ≈ 512 MiB / chunk_size, clamped 4–24), so its large-chunk rows are
  noisier than its small-chunk rows. `raw_fsync_decomposition` uses a fixed
  `n = 12` for every size.
- `per-chunk stall` in the report is `(median wall − median resume − median
  seal) ÷ chunks`, an estimate of the serialised per-chunk interval, not a
  directly measured quantity.
- This crate is a single `main.rs` binary with no `#[test]` functions —
  `cargo test` compiles it and runs zero tests (it is a build check).
- The process leaves its chunk store behind if it panics mid-run; clean
  `$TMPDIR/bamep-issue63-*` and the `--storage-dir` between large runs.

## Committed contents

`src/main.rs`, `Cargo.toml`, `Cargo.lock`, `README.md`, `.gitignore`, and
`runs/` (both `*-FINDINGS.md` and all four raw `*.log` files — small, safe,
and the sole evidence for the findings). `target/` is git-ignored.
