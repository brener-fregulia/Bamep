# Issue #63 — upper chunk-size experiment (32 / 64 / 96 / 128 MiB)

**Status:** findings only. No production change, no commit, no GitHub mutation.
No keep-alive, no pipelining, no connection reuse. Cases C/D/E not started.

**Method.** Same throwaway harness as Case A/B and the 8/16/32/64 sweep. Real
`DataPlaneClient` → fresh TCP + fresh pinned TLS 1.3 + fresh HTTP/1.1 per
request → real Worker `DataPlane` → real E1 `worker_control` → real D1
`FilesystemChunkStore` staging + per-chunk SHA-256 + `fsync(file)` + no-replace
`linkat` + `fsync(chunks dir)` → real D2 `FullArtifactHasher` full reread at
seal; `bamepd` faked (ADR-0018). **`chunk_size` is the only experiment
variable** (chunk count per transfer is its inverse and moves with it by
necessity).

- Primary FS = XFS (`/`), **3072 MiB** equal-byte extent → 96 / 48 / 32 / 24
  whole chunks, **no partial final chunk in any cell**, identical byte total
  (3 221 225 472 B) every cell (asserted at startup: `extent % chunk_size == 0`).
- Control = tmpfs (`/tmp`), 768 MiB extent → 24 / 12 / 8 / 6 whole chunks.
  Diagnostic only — **not** production throughput.
- **12 balanced cycles** per filesystem. Each cycle runs all four sizes once in
  a rotated order; the four cycle patterns are `32,64,96,128` / `128,96,64,32` /
  `64,128,32,96` / `96,32,128,64` (a Latin square — each size lands in each
  temporal slot exactly 3 times over 12 cycles). One warm-up transfer per size
  is run first and excluded from all analysis. n = 12 measured transfers/size.
- Analysis reports **both** independent medians **and within-cycle paired
  ratios** — the four sizes in one cycle share a closer disk-load epoch.

Raw logs: `upper-xfs-3gib-20260907T115957.log`,
`upper-tmpfs-768mib-20260907T115957.log`. Host: Fedora, Ryzen 5 3350G, 8 vCPU,
15 GiB RAM.

> **Note on the raw upper logs.** They were generated *before* the final
> owner-review wording correction. Two consequences:
> 1. Their `storage_dir:` header line (line 3) was afterwards sanitised to drop a
>    host-local absolute path — see README § "Raw log sanitisation".
> 2. Near their end they carry one **generated interpretive caveat**
>    ("*Case B + this sweep indicate the durability cost in THIS environment is
>    strongly fixed-durability-boundary dominated*"). That sentence is
>    **superseded by this document** — the correct, more precise reading is that
>    small chunks (≤ 32 MiB) show a fixed-per-boundary regime while `fsync(file)`
>    becomes materially byte-dependent by ~32–64 MiB, so the tested 32–128 MiB
>    range is *not* uniformly fixed-boundary dominated (§ "XFS durability-boundary
>    decomposition", § Interpretation).
>
> **No timing, throughput, ordering, RSS, correctness or statistical data in the
> raw logs was changed** — only the one host-local path field was redacted.

---

## XFS 3 GiB — independent medians (n = 12)

| chunk | chunks/xfer | conns | median MiB/s | median MB/s | per-chunk stall | full put_chunk | seal+D2 | peak RSS (med / max) |
|------:|------:|------:|------:|------:|------:|------:|------:|------:|
|  32 MiB | 96 | 98 | **83.59** | 87.65 |  362 ms |  312 ms | 1985 ms |  77 / 78 MiB |
|  64 MiB | 48 | 50 | 72.84 | 76.37 |  837 ms |  675 ms | 1988 ms | 141 / 142 MiB |
|  96 MiB | 32 | 34 | 73.47 | 77.04 | 1245 ms | 1014 ms | 1994 ms | 204.5 / 205 MiB |
| 128 MiB | 24 | 26 | 75.01 | 78.65 | 1623 ms | 1360 ms | 1996 ms | 268 / 270 MiB |

- `conns` = `chunks + 2` **by construction** (resume + N chunk PUTs + seal, one
  fresh TCP+TLS1.3+HTTP/1.1 each). The fresh-connection-per-request property is
  a property of the real `DataPlaneClient`; the count was empirically confirmed
  in Phase A1/A2 and is asserted-by-construction here, not re-counted.
- **RSS-median correction (owner review):** the `peak RSS (med / max)` **96 MiB**
  cell originally read `205 / 205` — the harness's old median helper returned the
  upper-middle value. From the unchanged raw per-transfer RSS (six 204 MiB, six
  205 MiB) the true median is **204.5** MiB. This is the only RSS cell in this
  document that changed; only the derived text was corrected, the raw logs and
  all timing/throughput/statistics are unchanged.
- `per-chunk stall` = (median wall − median resume − median seal) ÷ chunks — an
  estimate of the per-chunk serialised interval, not a directly measured value.
- Marginal throughput gain (independent medians): 32→64 **−12.9 %**, 64→96
  **+0.9 %**, 96→128 **+2.1 %**. Durability boundaries/GiB: 32 / 16 / 10.7 / 8.

Raw transfer walls (s), balanced order, XFS:

```
 32 MiB : [35.6, 37.7, 36.1, 37.4, 45.3, 40.8, 33.0, 34.4, 34.1, 33.7, 43.4, 41.7]   spread 33-45
 64 MiB : [43.8, 47.7, 34.8, 41.5, 33.0, 34.4, 29.4, 49.7, 31.9, 44.9, 42.9, 49.2]   spread 29-50
 96 MiB : [31.4, 45.6, 40.6, 49.4, 43.2, 45.2, 40.8, 48.9, 42.9, 38.5, 26.0, 29.1]   spread 26-49
128 MiB : [29.5, 53.5, 52.6, 40.7, 54.6, 48.1, 26.7, 27.5, 41.2, 43.5, 37.3, 26.9]   spread 27-55
```

32 MiB has both the **fastest median and the tightest distribution**. 128 MiB
spans nearly 2× best-to-worst.

## XFS 3 GiB — WITHIN-CYCLE PAIRED RATIOS (primary signal, n = 12)

`ratio = throughput(bigger) / throughput(smaller)` in the *same* cycle
(equivalently `wall_smaller / wall_bigger`, since the byte extent is identical).
`> 1.0` ⇒ bigger was faster that cycle.

| pair | raw per-cycle ratios | median paired ratio | cycles favouring bigger | spread (IQR width / MAD) | overall-median ratio (secondary) |
|---|---|---:|:---:|---:|---:|
| 32→64  | 0.81, 0.79, 1.04, 0.90, 1.37, 1.19, 1.12, 0.69, 1.07, 0.75, 1.01, 0.85 | **0.957 (−4.3 %)** | **6 / 12** | 0.275 / 0.155 | 0.871 (−12.9 %) |
| 64→96  | 1.40, 1.05, 0.86, 0.84, 0.77, 0.76, 0.72, 1.02, 0.75, 1.17, 1.65, 1.69 | **0.936 (−6.4 %)** | **6 / 12** | 0.460 / 0.182 | 1.009 (+0.9 %) |
| 96→128 | 1.06, 0.85, 0.77, 1.21, 0.79, 0.94, 1.53, 1.78, 1.04, 0.88, 0.70, 1.08 | **0.989 (−1.1 %)** | **6 / 12** | 0.278 / 0.167 | 1.021 (+2.1 %) |

**Every adjacent step is a coin flip: exactly 6/12 cycles each way, median
paired ratio at or below 1.0, IQR straddling 1.0 with width 0.27–0.46.** The
±30–40 % (here up to 2×) shared-disk variance completely swamps any chunk-size
signal in the 32 → 128 MiB range. **The upper sweep is therefore itself
inconclusive** — it does not demonstrate a benefit *or* a loss for any step; it
only shows that if a difference exists it is smaller than this host's noise.

## XFS durability-boundary decomposition (isolated diagnostic — hypothesis, not a located transition)

Isolated syscall probe, `n = 12` fresh files per size, run **once, after** the
transfer matrix, in **increasing size order** (not balanced), on the same
high-variance shared XFS device. It is a like-for-like probe of `finalize`'s
internal steps (the real `finalize_inner` is private). **It supports a mechanism
hypothesis; it does not precisely locate a transition point.** It also
upper-bounds the amortised in-transfer cost (XFS group-commit lets a following
`fsync` piggyback on an in-flight log commit). The `worker_staging_split`
"finalize" column below uses a smaller, size-dependent sample
(`n` = 16 / 8 / 5 / 4 for 32 / 64 / 96 / 128 MiB), so its large-chunk rows are
noisier still.

| chunk | write+flush | fsync(file) | fsync(dir) | D1 finalize (real API, n=16/8/5/4) |
|------:|------:|------:|------:|------:|
|  32 MiB |  9.1 ms | 162.2 ms | 136.2 ms | 377.8 ms |
|  64 MiB | 18.1 ms | 288.5 ms | 130.9 ms | 451.5 ms |
|  96 MiB | 27.5 ms | 310.6 ms | 149.5 ms | 417.5 ms |
| 128 MiB | 36.2 ms | 477.7 ms | 164.8 ms | 505.1 ms |

**`fsync(file)` is not flat across this range.** In the earlier 8/16/32/64
sweep it held ~166–189 ms across an 8× size range; here it reads
162 → 289 → 311 → 478 ms, while `fsync(dir)` stays ~flat (130–165 ms). Given the
caveats above, the isolated diagnostic is **consistent with a byte-dependent
component of `fsync(file)` becoming material by the 64 MiB+ regime in this
observed XFS environment** — it does not pin the transition to a specific size.

The **robust, end-to-end** finding does not depend on that: per-chunk stall
grows roughly linearly with size (362 → 837 → 1245 → 1623 ms), so PUT / per-chunk
wall is approximately byte-proportional in the upper range, halving the chunk
count no longer halves the work, and **larger chunks do not move the observed
throughput wall** (§ paired ratios: every 32→128 step is 6/12).

- Seal D2 full reread ≈ 1.99 s at 3 GiB, **chunk-size-independent**
  (byte-proportional; scales cleanly from the 1.33 s seen at 2 GiB).
- SHA-256 ≈ 1.9 GB/s, memcpy ≈ 1.46 GB/s, handshake ≈ 0.7 ms/conn — unchanged,
  all negligible.

---

## TMPFS 768 MiB CONTROL (fsync ≈ free; diagnostic only, n = 12)

| chunk | median MiB/s | vs 32 MiB | incremental | paired median ratio | cycles favouring bigger | IQR width / MAD |
|------:|------:|------:|------:|------:|:---:|---:|
|  32 MiB | 208.61 | 1.00× | —              | —              | —      | — |
|  64 MiB | 217.12 | 1.04× | 32→64 +4.1 %   | 1.044 (+4.4 %) | 11 / 12 | 0.023 / 0.011 |
|  96 MiB | 230.61 | 1.11× | 64→96 +6.2 %   | 1.070 (+7.0 %) | 11 / 12 | 0.063 / 0.035 |
| 128 MiB | 233.49 | 1.12× | 96→128 +1.2 %  | 1.017 (+1.7 %) | 10 / 12 | 0.030 / 0.016 |

tmpfs distributions are **tight** (32 MiB walls 3.50–3.82 s; 128 MiB
3.05–3.35 s) and **consistent** (10–11 of 12 cycles favour the bigger chunk on
every step). The non-durable path shows a real but shallow improvement 32 → 96
(+11 % total) that then rolls off 96 → 128 (+1.7 %). This is amortisation of the
**aggregate per-request cost** — proof creation, request/HTTP handling, the E1
authorize/commit round trips, staging setup *and* connection setup — spread over
fewer, larger chunks.

**Connection reuse alone would recover only a small part of this.** The bare
fresh TCP + TLS 1.3 + HTTP/1.1 handshake was measured here at ≈ 0.656 ms/conn.
Going 32 → 96 MiB on the 768 MiB tmpfs control drops the request count from 26
to 10, so reuse could remove ≈ 16 × 0.656 ≈ **10.5 ms** — about **0.3 %** of a
~3.3–3.7 s transfer, a small fraction of the observed ~11 % 32→96 gap.
Pipelining *might* hide a larger share of the serial request/Worker latency, but
that is a distinct experiment and has not been validated. **No production
recommendation follows from this.**

**Contrast is the finding:** with fsync removed the signal is clean and small,
rolling off near ~96 MiB; with real fsync the signal is gone entirely across
32 → 128 MiB. The durable path is storage-serialisation-bound, and that wall
does not move with chunk size in this range.

---

## MEMORY / RETRY ENVELOPE

Measured harness RSS (A) grows ≈ 2 × chunk_size (one avoidable
`source_chunk.to_vec()` copy per chunk — **not** optimised in this Spike).
B and C are design estimates, **not** measured production RSS:

| chunk | A: measured harness peak RSS | B: ownership-based SERIAL Agent (~1 buffer) | C: two-buffer PIPELINED Agent (~2 buffers) | max retransmission unit | one-chunk stall (XFS) |
|------:|------:|------:|------:|------:|------:|
|  32 MiB |  77 MiB | ~32 MiB | ~64 MiB  | 32 MiB |  362 ms |
|  64 MiB | 141 MiB | ~64 MiB | ~128 MiB | 64 MiB |  837 ms |
|  96 MiB | 204.5 MiB | ~96 MiB | ~192 MiB | 96 MiB | 1245 ms |
| 128 MiB | 268 MiB | ~128 MiB | ~256 MiB | 128 MiB | 1623 ms |

"+ small transport overhead" on B and C. RAM, retransmission unit and
one-chunk stall all grow **strictly linearly** with chunk_size.

---

## INTERPRETATION

The question was *"where does marginal throughput stop justifying the linear
increase in RAM, retry granularity, and single-chunk stall?"*

- **Throughput improves materially from the small-chunk regime up toward
  32 MiB.** The earlier 8/16/32/64 sweep: 8 MiB ≈ 61 MiB/s, 16 MiB ≈ 66, 32 MiB
  ≈ 80–84.
- **In this 32/64/96/128 sweep, no size above 32 MiB demonstrated a defensible
  throughput benefit on this noisy XFS host.** Every within-cycle paired step is
  6/12 each way with a median ratio ≤ 1.0 and an IQR straddling 1.0. Meanwhile
  RAM (77→268 MiB), retransmission unit (32→128 MiB) and one-chunk stall
  (362→1623 ms) all grow ~4×. **The linear cost is real and paid; a throughput
  return above 32 MiB was not demonstrated.**
- **Therefore 32 MiB is the current smallest / most conservative candidate with
  no demonstrated throughput loss above it.** The exact location of the knee
  between the small-chunk regime and this flat region is **not statistically
  located or proven** by this data — the host variance is too large. It is
  *somewhere at or below 32 MiB*; where exactly is unresolved.
- **Mechanism (hypothesis).** The fixed per-durability-boundary fsync cost that
  made 8 → 32 MiB pay off appears essentially amortised by ~32 MiB. The isolated
  diagnostic is *consistent with* a byte-dependent component of `fsync(file)`
  becoming material by the 64 MiB+ regime in this observed XFS environment
  (162 → 478 ms across 32 → 128 MiB) — plausibly a proportionally larger dirty
  range to flush — so trading many small fixed costs for few large proportional
  ones is roughly break-even. This is a mechanism hypothesis, not a located
  transition (see § decomposition caveats).
- **Wording guard.** This does **not** say fsync latency is universally fixed or
  byte-independent, nor that it "stops being fixed" at a precise size. Case B +
  the 8/16/32/64 sweep established the fixed-per-boundary regime for *small*
  chunks; this sweep is consistent with a byte-proportional component present in
  the 64 MiB+ regime.

### A. Useful default for a single ~1 GbE Endpoint

A practical ~1 GbE payload target ≈ 112 MiB/s (raw 1 Gbit/s line rate is
~119.2 MiB/s / 125 MB/s; usable payload is lower). The host XFS path delivers
73–84 MiB/s at every size 32–128 MiB — storage-bound, below that target,
chunk-size-insensitive within noise.
On this evidence **32 MiB is the better single-endpoint candidate:** best
median, tightest distribution, and 2–4× lower RAM / retransmission unit / stall
than 64–128 MiB, with no demonstrated throughput loss from choosing it. (Not a
production decision — owner's call.)

### B. Underlying data-plane / storage optimum (faster links, multi-Endpoint, aggregate Worker throughput)

**This Spike cannot answer it.** Single stream, single disk, faked bamepd,
loopback. The tmpfs control suggests the non-durable per-request path keeps
improving slightly to ≈96 MiB, but that is aggregate request-overhead
amortisation, not storage — and only the handshake portion of it (≈ 0.3 % of
transfer wall here) is recoverable by connection reuse; the rest is proof /
HTTP / E1 / staging setup, some of which pipelining might hide (unvalidated).
Aggregate and multi-Endpoint behaviour is a concurrency experiment (a Case),
not a chunk-size sweep.

---

## DECISION OUTPUT (owner's classification)

**Option 4 — INCONCLUSIVE for the 32 → 128 MiB range**, in the direction of
"the curve has already flattened at or below 32 MiB". XFS variance prevents a
defensible distinction between 32, 64, 96 and 128 MiB: every within-cycle
paired step is a 6/12 coin flip with a median ratio ≤ 1.0 and wide IQR. What
*is* defensible: no size above 32 MiB showed any throughput benefit, the point
estimates favour the *smaller* size at every step, and the linear RAM / retry /
stall costs are incurred for no measured return. Combined with the earlier
sweep (throughput rises 8 → 32), **32 MiB is the smallest conservative
candidate with no demonstrated loss above it; the exact knee is not
statistically located.**

This is **not** "still rising through 128" — so, per the brief, no automatic
request for larger sizes.

---

## CORRECTNESS (every transfer, both filesystems)

Run completed exit 0, no panic, across **4 + 48 XFS** and **4 + 48 tmpfs**
transfers. `run_one_transfer` runs these assertions on **every** transfer
(warm-up and measured alike); process exit 0 ⇒ none fired:

- resume discovery `Approved`;
- **every** expected chunk `Accepted` (96 / 48 / 32 / 24 on XFS) — panics on the
  first non-`Accepted` outcome;
- manifest sealed, `SealArtifactStatus::Verified` — and the fake bamepd returns
  `Verified` only if the Worker's own D2 full-reread digest equals the Agent's
  declared rolling digest, so this is a real end-to-end D2 check, not a stamp;
- a **second, harness-driven** independent D2 recomputation
  (`FullArtifactHasher::for_store(store).compute_blocking(...)`, the same public
  API the Worker seal path uses, invoked separately against the on-disk store)
  with `digest == Agent rolling SHA-256`;
- D2 reconstructed `total_size == exact extent` (3 221 225 472 / 805 306 368 B);
- D2 `chunk_count == expected`;
- no integrity / durability mechanism disabled — `fsync(file)`, no-replace
  `linkat`, `fsync(chunks dir)`, durable ordering and D2 full reread are all the
  real production code paths and are exercised on every chunk.

Per-transfer store cleanup and the RSS-max reset both happen **outside** the
timed wall interval; `make_source` buffer generation happens before the wall
clock starts. The staging-split and fsync-decomposition probes run **after** all
measured cycles.

---

## RECOMMENDED NEXT EXPERIMENT (one; NOT run) — needs owner review

Further chunk-size sweeping in the 32 MiB+ range has reached diminishing returns
against this host's ±35 % variance: the upper sweep could not distinguish 32,
64, 96 or 128 MiB.

**Recommendation: stop chunk-size exploration and run Case D — a minimal
depth-2 *preparation* pipeline at 32 MiB chunks.** Precisely: while chunk *N*'s
PUT is in flight and the Worker is doing its durability work for chunk *N*, the
Agent already reads + hashes + builds the per-request proof for chunk *N+1*, so
that CPU/IO work is finished before ACK *N* arrives. **PUT *N+1* still starts
only after ACK *N*** — there is still exactly one PUT in flight, no keep-alive,
no extra connection. This removes the Agent-side `read + per-chunk SHA + proof`
interval (measured here at ~60 ms/chunk of `materialize + chunkSHA + rollSHA` at
32 MiB) from the serial critical path. Its buffer envelope is ≈ 2 × 32 = 64 MiB
(one chunk being uploaded, one being prepared).

**Overlapping the upload/network of chunk *N+1* with the durability of chunk
*N* is a different and larger change** — it requires ≥ 2 requests / chunks in
flight simultaneously (true pipelining), and is a distinct later experiment, not
this one.

If the owner prefers to stay strictly within chunk-size: the only remaining
question is where between the known-worse 16 MiB and the flat-within-noise
32 MiB the transition sits — a tight **16 / 24 / 32 / 48 MiB** sweep at the
3 GiB extent (192 / 128 / 96 / 64 whole chunks), ≥ 12 balanced cycles, same
paired analysis. Lower value: the noise floor may not resolve 24 vs 32.

**STOP for owner review.**
