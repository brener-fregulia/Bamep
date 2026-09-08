# Issue #65 — throughput-first endpoint-capture streaming (Spike, physical Integration Environment)

THROWAWAY feasibility tooling for **Issue #65**. NOT product architecture, NOT a
production Agent/Worker/data-plane, NOT a benchmark matrix. Sibling of — and
deliberately separate from — the Issue #63 per-chunk-durability scaffolding
(`../issue-63-data-plane-throughput/`, preserved, not redefined here).

## Question

#63 measured the correctness-first per-chunk-durability capture shape at
~30 MiB/s and concluded it is not product-viable as the capture hot path. #65
resets the optimization order: **prove the fastest useful physical byte path
first; harden it second.**

Product rule: Bamep software must not be the throughput bottleneck. On healthy
1 GbE with capable source/storage, useful capture throughput should approach
`min(source seq-read, useful network, destination seq-write)` — **minimum
acceptable ≥ 100 MB/s decimal (~95.4 MiB/s)**, desired ~105–115 MB/s.

## First candidate — the simplest path that can answer it

```
MiniPC raw source (\\.\PhysicalDrive0, GENERIC_READ, opaque-epoch resolved,
                   Issue-63 source-safety predicate: model / exact length /
                   extent ≤ length; a Reject reads ZERO bulk bytes)
  -> dedicated producer thread: bounded 2 GiB SEQUENTIAL read + incremental
     SHA-256, into a 4-deep bounded queue (≤ 32 MiB in flight)
  -> foreground: one continuous TCP stream (ONE connection) to the Fedora sink
  -> sink: ONE sequential destination file (O_TRUNC) + ONE final fsync
  -> sink replies with a one-line JSON measurement; the probe cross-checks bytes
     + SHA-256 (client digest == sink digest == end-to-end byte correctness)
```

**Not here** (deliberately, per #65): per-chunk files / per-chunk fsync /
per-chunk DB commit / per-chunk ACK barrier / resume / retry / TLS / auth /
protocol framing beyond a single JSON preamble line / Artifact lifecycle /
window or chunk-size matrix.

The **one** non-trivial choice is the producer/consumer split: a single serial
`read → hash → write` loop would leave the NIC idle during every read + hash
(the MiniPC hashes SHA-256 at only ~340 MiB/s per #63 evidence), imposing an
artificial software ceiling — exactly what #65 says Bamep must not do. Two
threads + a small bounded queue keep the single TCP stream continuously fed;
`--no-digest` isolates whether hashing is the limiter.

## Components

| Path | What it is |
|---|---|
| `capture-probe/` | WinPE-native probe (`x86_64-pc-windows-msvc`, static CRT). `resolver.rs` / `sources.rs` copied from the Issue-63 stage2-probe (themselves closed-#61 copies); the source-safety predicate is reused from `bamep-i63-stage2-engine::safety` (no re-implementation). No tokio: std TCP + `sha2` + `getrandom` + `windows-sys` file/IOCTL APIs. |
| `sink/` | Fedora-side plain-TCP capture sink: one JSON preamble line, stream exactly `extent_bytes` into one `O_TRUNC` file, one `fsync`, reply with a one-line JSON measurement, drop the file (+ verify it is gone), next connection. `serde_json` + `sha2` only. `--dest-dir DIR` (repeatable) runs `--count` connections against each directory in order — the per-disk destination comparison; each result line carries `dest_dir`. |
| `derive-issue65-runtime.sh` | Derives the Issue-65 PXE/WinPE runtime from the pinned Issue #53 Phase-9d assets **without modifying them** (identical lineage to the Issue-63 Stage-1 derive; re-hashed before/after). Three `initrd` overlay lines: `winpeshl.ini` + the bootstrap `.cmd` + the capture probe `.exe`; `boot.wim` stays last, byte-identical. `--total-transfers N` controls how many back-to-back captures the bootstrap runs. Carries **no** credential/key/token. |
| `winpeshl.ini`, `bamep-i65-bootstrap.cmd.template` | The injected WinPE auto-start payload: `winpeshl.ini` → `cmd /k bootstrap.cmd`; the bootstrap runs `wpeinit` then a `for /L` loop of `@TOTAL_TRANSFERS@` back-to-back 2 GiB captures (`t1..tN`). The operator types nothing after the one wimboot keypress. |
| `run-issue65-lab.sh` | One-command foreground supervisor: preflight (read-only) → derive → dnsmasq + WinPE HTTP + capture sink → readiness gate → `READY_FOR_MINIPC_POWER_ON` → stream the sink result lines. Reverts only the lab network state it created. Never powers the MiniPC. |

## Measurement

Sink-side, single clock, authoritative:
- `t0` first payload byte received, `t1` last byte written to page cache, `t2`
  fsync returned;
- `mb_s` / `mib_s` over `(t2 - t0)` — the durable capture wall;
- `recv_mb_s` over `(t1 - t0)` and `fsync_ms` reported separately so the fsync
  cost is visible.

The probe additionally reports its own client-side wall + throughput as a
cross-check, and `I65_DIGEST_MATCH` on a client↔sink SHA-256 match.

## Run it

```bash
# build
( cd sink && cargo build --release )
( cd capture-probe && export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" XWIN_ACCEPT_LICENSE=1
  RUSTFLAGS="-C target-feature=+crt-static" cargo xwin build --release --target x86_64-pc-windows-msvc )

# host wiring check (safety Accept + Reject; add --loopback for one streamed transfer)
( cd sink && ./target/release/bamep-i65-capture-sink --listen 127.0.0.1:9265 --dest /tmp/i65.bin --count 1 --extent-bytes 268435456 & )
./capture-probe/target/release/bamep-i65-capture-probe --self-check --loopback --sink 127.0.0.1:9265 --extent-bytes 268435456

# read-only lab gate
./run-issue65-lab.sh --preflight

# bring the lab up (needs sudo for dnsmasq); prints READY_FOR_MINIPC_POWER_ON and waits
./run-issue65-lab.sh
```

Then power on the disposable MiniPC, press a key once at wimboot's prompt, and
watch the launcher terminal for the `i65_sink_result` lines.
Evidence: `evidence/<run-id>/` (git-ignored).

### Per-disk destination comparison (one MiniPC boot)

Set `I65_DEST_DIRS` to a comma-separated list of writable directories, each on
the filesystem under test. The sink runs `I65_TRANSFERS` (default 3 = 1 warmup +
2 measured) connections against each directory **in order**; the bootstrap runs
`len(dirs) * I65_TRANSFERS` back-to-back captures in a single boot. Every result
line carries `dest_dir` and `conn` so warmup (first of each group) vs measured
is unambiguous. The launcher preflight refuses any directory that resolves to
the root filesystem, and both the sink (per transfer) and the launcher (on exit)
delete `capture.bin` and verify it is gone.

```bash
I65_DEST_DIRS=/mnt/i65-sda/i65,/mnt/i65-sdb/i65,/mnt/i65-sdc/i65 ./run-issue65-lab.sh --preflight
I65_DEST_DIRS=/mnt/i65-sda/i65,/mnt/i65-sdb/i65,/mnt/i65-sdc/i65 ./run-issue65-lab.sh
```

The mountpoints and filesystems are the operator's responsibility (create,
mount, unmount); the launcher only reads and writes inside the given directories.

## Status

Built + host-validated + **physically run**. Two lab runs on 2026-09-08
(`i65-20260908T104933` NVMe/root, `i65-20260908T114020` SATA SSD + 2× HDD) —
compact results in [`RESULTS.md`](RESULTS.md). Headline: the live 1 GbE stream
tops out at ~96–97 MB/s (destination-independent); durable completion is
`fsync`-bound per destination (SATA SSD ~2–5 s, HDD ~10 s, busy root NVMe
18–33 s). The ~3 % gap to the 100 MB/s decimal floor is unresolved (network /
source, not the sink) and is follow-up work for #65 proper. This remains a
throwaway Spike — no hardening was added.
