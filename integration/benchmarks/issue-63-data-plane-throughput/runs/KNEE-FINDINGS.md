# Issue #63 — chunk-size knee experiment (8 / 16 / 32 / 64 MiB)

**Status:** findings only. No production change, no commit, no GitHub mutation.
Cases C/D/E not started. Connection reuse / pipelining not implemented.

> **Superseded in part by `UPPER-FINDINGS.md` (32/64/96/128 MiB, 3 GiB extent,
> n = 12, within-cycle paired analysis).** This document's reading that the XFS
> curve "had not flattened through 64 MiB" and that "32→64 still materially
> improves" rested on the 2 GiB / n = 8 median, where 64 MiB drew two lucky
> ~12 s runs. That did **not** replicate: at 3 GiB with n = 12 and paired
> ratios, 32→64 is a 6/12 coin flip (median paired ratio 0.957). Current
> defensible reading: throughput improves from the small-chunk regime up toward
> 32 MiB; no size above 32 MiB shows a defensible benefit on this noisy host;
> 32 MiB is the smallest conservative candidate with no demonstrated loss above
> it; the exact knee is not statistically located. The tmpfs, fsync-
> decomposition, RSS, correctness and connection-count data below stand.

**Method.** Same throwaway harness as Case A/B (real `DataPlaneClient` → fresh
TCP + fresh pinned TLS 1.3 + fresh HTTP/1.1 per request → real Worker
`DataPlane` → real E1 `worker_control` → real D1 `FilesystemChunkStore`
staging + per-chunk SHA-256 + `fsync(file)` + no-replace `linkat` +
`fsync(chunks dir)` → real D2 `FullArtifactHasher` full reread at seal;
`bamepd` faked per ADR-0018). **`chunk_size` is the only variable.**

Ordering: 8 cycles, each cycle runs all four sizes once in a balanced rotated
order (`8,16,32,64` / `64,32,16,8` / `16,64,8,32` / `32,8,64,16`), so
temporal disk-load / cache effects do not systematically favour one size.
One warm-up transfer per size excluded. n = 8 measured transfers per size.
Medians are primary. Raw logs: `knee-xfs-2gib-*.log`, `knee-tmpfs-512mib-*.log`.

Host: Fedora, Ryzen 5 3350G, 8 vCPU, 15 GiB RAM. Primary FS = XFS (`/`), 2 GiB
equal-byte extent (even 64 MiB → 32 durability boundaries). Control = tmpfs
(`/tmp`), 512 MiB extent.

---

## PRIMARY MATRIX — XFS, 2 GiB extent (median of 8)

| chunk | chunks/xfer | conns/xfer | median MiB/s | median MB/s | vs 8 MiB | incremental | median per-chunk wall | full put_chunk | seal+D2 | peak RSS (med / max) |
|------:|------:|------:|------:|------:|------:|------:|------:|------:|------:|------:|
|  8 MiB | 256 | 258 |  61.32 |  64.30 | 1.00× | —             | 125.4 ms | 104.4 ms | 1331 ms | 45 / 45 MiB |
| 16 MiB | 128 | 130 |  65.63 |  68.82 | 1.07× | 8→16: **+7.0 %**  | 233.7 ms | 204.9 ms | 1327 ms | 61 / 61 MiB |
| 32 MiB |  64 |  66 |  79.31 |  83.17 | 1.29× | 16→32: **+20.8 %** | 382.2 ms | 336.6 ms | 1366 ms | 101 / 109 MiB |
| 64 MiB |  32 |  34 | 103.35 | 108.37 | 1.69× | 32→64: **+30.3 %** | 581.5 ms | 428.9 ms | 1335 ms | 157 / 173 MiB |

> **RSS-median correction (owner review).** The `peak RSS (med / max)` column
> for **32 MiB** originally read `109 / 109` — the harness's old median helper
> returned the upper-middle value, not a true median. From the unchanged raw
> per-transfer RSS `[93, 93, 93, 93, 109, 109, 109, 109]` MiB the true median is
> **101** MiB (max 109). Only this derived text was corrected; the raw logs and
> every timing/throughput/statistic are unchanged. Same fix below for the
> tmpfs table (`108 → 100`) and the memory-envelope table. Other rows
> (8/16/64 MiB) are unaffected.

Raw rep walls (s), balanced order, XFS:

```
 8 MiB : [41.5, 32.4, 37.2, 40.7, 26.5, 26.4, 34.5, 31.0]
16 MiB : [21.5, 41.9, 23.0, 30.1, 38.8, 28.3, 32.4, 39.7]
32 MiB : [25.2, 21.3, 31.3, 25.3, 26.3, 23.1, 30.0, 31.3]
64 MiB : [17.6, 12.1, 21.5, 12.8, 22.7, 18.3, 23.6, 22.5]
```

Per-transfer spread is roughly ±30–40 % on this shared disk. The balanced
ordering did its job (each size hit every slot twice), but with n = 8 a median
can still sit 10–20 % off — treat the *per-step* percentages as directional,
not precise.

**Owner-review corrections (2026-09-07):**

1. The **8→16 MiB incremental result is inconclusive** — four of the eight
   16-MiB transfers fell in a slow-disk window (38–42 s). No estimate of the
   "true" 8→16 gain is made.
2. This document originally read the load-bearing evidence as "(a) tmpfs
   request-overhead curve flattens early; (b) the XFS durability curve did not
   flatten through 64 MiB; (c) 32→64 still materially improves". **Points (b)
   and (c) did not survive the larger upper sweep** (see the banner at the top
   of this file and `UPPER-FINDINGS.md`): at 3 GiB / n = 12 with within-cycle
   paired ratios, 32→64 is a 6/12 coin flip. Only **(a)** — the tmpfs
   request-overhead curve flattening early — still stands. The current
   defensible reading is that throughput improves from the small-chunk regime
   toward 32 MiB, no size above 32 MiB shows a defensible benefit on this noisy
   host, and the exact knee is not statistically located.

### Durability-boundary decomposition (XFS, end-of-run, single-threaded, isolated)

| chunk | write+flush | fsync(file) | hard_link | fsync(dir) | D1 finalize (real API) |
|------:|------:|------:|------:|------:|------:|
|  8 MiB |  2.5 ms | 182.8 ms | 0.05 ms | 190.6 ms | 283.9 ms |
| 16 MiB |  4.9 ms | 166.2 ms | 0.05 ms | 195.6 ms | 359.7 ms |
| 32 MiB |  9.1 ms | 181.7 ms | 0.05 ms | 180.2 ms | 347.5 ms |
| 64 MiB | 18.1 ms | 188.9 ms | 0.04 ms | 198.5 ms | 445.3 ms |

- **`fsync(file)` + `fsync(dir)` ≈ 360–390 ms per chunk and does NOT grow with
  chunk size** (8×the bytes → fsync ≈ flat, +4 %). This is a per-durability-
  boundary fixed cost in this environment.
- **`write + flush` is byte-proportional** (2.5 → 18 ms, ~linear) but an order
  of magnitude smaller than the fsync pair.
- Caveat: these isolated numbers are an **upper bound** on the amortized
  per-chunk cost inside a real back-to-back transfer — XFS group-commit lets a
  following `fsync` piggyback on an in-flight log commit, so the streamed path
  finalizes faster per chunk than this end-of-run probe. The *shape* (fixed
  fsync ≫ byte-proportional write) is the finding, not the millisecond value.
- SHA-256 ≈ 1.9 GB/s, memcpy ≈ 1.5 GB/s, bare TCP+TLS1.3+HTTP/1.1 handshake
  ≈ 0.63 ms/connection — all unchanged, all negligible next to fsync.
- Seal D2 full reread ≈ 1.33 s at 2 GiB, **chunk-size-independent** (matches
  physical CP7A seal 1414–1430 ms).

---

## TMPFS CONTROL — 512 MiB extent, fsync ≈ free (median of 8)

| chunk | chunks/xfer | median MiB/s | vs 8 MiB | incremental | peak RSS (med / max) |
|------:|------:|------:|------:|------:|------:|
|  8 MiB | 64 | 155.43 | 1.00× | —              | 44 / 45 MiB |
| 16 MiB | 32 | 203.25 | 1.31× | 8→16: **+30.8 %** | 60 / 61 MiB |
| 32 MiB | 16 | 210.91 | 1.36× | 16→32: **+3.8 %**  | 100 / 108 MiB |
| 64 MiB |  8 | 226.02 | 1.45× | 32→64: **+7.2 %**  | 156 / 173 MiB |

tmpfs is low-variance (64 MiB: 2.21–2.31 s). **On tmpfs the curve has a clear
knee at 16 MiB**: once the per-request fixed cost (TCP/TLS/proof/parse/E1
round-trips) is halved from 64 to 32 requests it is essentially gone; 32 and
64 MiB add only 3.8 % and 7.2 %. Do **not** read tmpfs MiB/s as production
throughput — it only isolates "what is left once durable-media fsync is
removed."

---

## MEMORY / RETRY ENVELOPE

Observed peak RSS grows ≈ 2 × chunk_size (the harness keeps one avoidable
`source_chunk.to_vec()` copy). Design envelopes (estimates, **not** measured
production RSS):

| chunk | A: observed peak RSS (this harness) | B: ownership-based SERIAL Agent (~1 buffer) | C: two-buffer PIPELINED Agent (~2 buffers) | retry granularity | one-chunk stall (XFS median) |
|------:|------:|------:|------:|------:|------:|
| 16 MiB |  61 MiB | ~16 MiB | ~32 MiB  | 16 MiB | 234 ms |
| 32 MiB | 101 MiB | ~32 MiB | ~64 MiB  | 32 MiB | 382 ms |
| 64 MiB | 157 MiB | ~64 MiB | ~128 MiB | 64 MiB | 582 ms |

(32 MiB RSS median corrected 109 → 101 — see the RSS-median correction note
above; raw evidence unchanged.)

"+ small transport overhead" on B and C. A lost / uncertain PUT re-sends **up
to one whole chunk**. RAM, retry bytes, and one-chunk stall all grow **linearly**
with chunk_size.

---

## INTERPRETATION

> **Superseded below this line (points 4–5, the "Wording guard", and "Candidate
> knee").** They reflect the 2 GiB / n = 8 reading. The larger 3 GiB / n = 12
> paired upper sweep (`UPPER-FINDINGS.md`) did not reproduce a 32→64 benefit; an
> isolated diagnostic there is consistent with a byte-dependent `fsync(file)`
> component in the 64 MiB+ regime (mechanism hypothesis, not a located
> transition); and no size above 32 MiB showed a defensible benefit. The
> per-step medians and the tmpfs result (points 1–3, 4-tmpfs) are unaffected raw
> evidence and stand.

1. **8 → 16 MiB:** XFS median +7.0 %, but **inconclusive** — four of eight
   16-MiB transfers landed in a slow-disk window. No estimate is made. (tmpfs
   analogue, for contrast only: +30.8 %.)
2. **16 → 32 MiB:** XFS median +20.8 %.
3. **32 → 64 MiB:** XFS median +30.3 % (79.3 → 103.4 MiB/s).
4. **Where does fixed-boundary amortization flatten?**
   - On **tmpfs** (per-request fixed cost only): **at 16 MiB.** Beyond 16 MiB
     there is almost nothing left to amortize (+3.8 %, +7.2 %).
   - On **XFS** (real per-durability-boundary fsync cost): **not within the
     tested range.** 32 → 64 still buys +30 %. Concretely, **XFS at 64 MiB
     (103 MiB/s) is still slower than tmpfs at 8 MiB (155 MiB/s)** — at 64 MiB
     / 2 GiB you still pay the ~fixed fsync boundary 32 times and it is still
     the dominant term. The knee is **above 64 MiB and not yet located.**
5. **Does the remaining gain justify the linear cost?** Through 64 MiB, yes:
   32 → 64 trades +30 % throughput for ~+56 MiB RSS (157 − 101), +32 MiB retry granularity,
   +200 ms one-chunk stall. Against a practical ~1 GbE payload target
   (~112 MiB/s) the 79 → 103 MiB/s difference is "does not saturate the link" vs
   "nearly saturates it," so it is material. Above 64 MiB the trade is unknown
   and is exactly what the follow-up must answer.

**Wording guard (as read at the time; see the superseded banner above):** this
sweep's small-chunk range (8–32 MiB) showed fsync ≈ flat vs bytes with
write+flush byte-proportional but ~10× smaller — a fixed-per-boundary regime for
*small* chunks. It was **never** a claim that fsync latency is universally fixed
or byte-independent, and the isolated fsync figures only upper-bound the
amortized in-transfer cost. The upper sweep later found the flatness does not
extend into the 64 MiB+ range.

### Candidate knee (NOT a production decision)

- **Not 32 MiB.** 64 MiB is +30 % faster than 32 MiB on comparable XFS medians
  — far outside "a few percent."
- **16 MiB is the knee only for the non-durable (per-request) fixed cost**, as
  the tmpfs control shows; it is not the knee for the durability path.
- On XFS the marginal-return curve **has not flattened through 64 MiB.**

---

## CORRECTNESS (every transfer, both filesystems)

The harness asserts, and the run completed with exit 0 and no panic across all
4 + 32 XFS and 4 + 32 tmpfs transfers:

- resume discovery Approved;
- **every** expected chunk `Accepted` (256 / 128 / 64 / 32 on XFS);
- manifest sealed, `SealArtifactStatus::Verified`;
- independent D2 `FullArtifactHasher` digest **==** the Agent rolling
  SHA-256 digest;
- D2 reconstructed `total_size` **==** exact extent bytes
  (2 147 483 648 / 536 870 912);
- D2 `chunk_count` **==** expected chunk count;
- no integrity or durability mechanism disabled — `fsync(file)`, no-replace
  `linkat`, `fsync(chunks dir)`, durable ordering, and D2 full reread all real
  and exercised on every chunk.

---

## RECOMMENDED NEXT EXPERIMENT (one; NOT run)

> **Done.** This recommendation was executed as the 32/64/96/128 MiB upper sweep
> (owner-approved; 3 GiB extent, 12 cycles, n = 12) — results in
> `UPPER-FINDINGS.md`. The "knee known to be above 64 MiB" premise did not hold.

**32 / 64 / 96 / 128 MiB knee sweep**, identical harness and identical
balanced-cycle method, XFS 2 GiB primary + tmpfs 512 MiB control, but **≥ 12
cycles** (n ≥ 12 per size) to beat down the ±30–40 % per-transfer variance that
made the 8→16 XFS step unreadable at n = 8. Keep 32 MiB as the low anchor so
the new sweep overlaps this one. Purpose: locate the XFS knee that is known to
be above 64 MiB, and measure whether 64 → 96 → 128 keeps paying enough to
justify the linear RAM / retry / one-chunk-stall growth (128 MiB ⇒ ~128 MiB
serial-ownership buffer, ~256 MiB two-buffer pipeline, 128 MiB retransmit
granularity, ~1 s one-chunk stall).

This is justified by the owner's own contingency: 64 MiB remained materially
faster than 32 MiB, so 96 / 128 MiB are now warranted.

**STOP for owner review.**
