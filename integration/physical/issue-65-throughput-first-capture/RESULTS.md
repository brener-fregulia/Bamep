# Issue #65 — physical results (THROWAWAY Spike evidence)

Durable evidence from two physical runs on the Bamep lab (Integration
Environment). The implementation in this directory is a **throwaway Spike**, not
product architecture. Transient payloads, `target/`, and derived WinPE runtime
trees are intentionally **not** committed — only the numbers below.

Repo HEAD at both runs: `9f574cc8ec0cc1d5168904a64bd50d5365ed2b9d`.
Geometry (every transfer): source `\\.\PhysicalDrive0` on the disposable MiniPC,
`GENERIC_READ` only, fixed **2 GiB** extent (`2147483648` bytes), one continuous
TCP stream, one sequential destination file, incremental SHA‑256, one final
`fsync`. 1 GbE lab link (`enp8s0` ↔ MiniPC `192.168.99.66`).

## 1. Disk / model / serial → destination mapping

| Role in test | Physical device | Model | Serial | ROTA | Destination filesystem | Run |
|---|---|---|---|---|---|---|
| NVMe (original) | `/dev/nvme0n1` (p3→LVM `fedora-root`) | XPG GAMMIX S5 | `2M132928KCH1` | 0 | **XFS on `/` — 79 % full, running‑OS root volume** | `i65-20260908T104933` |
| SATA SSD | `/dev/sda` | Lexar 240GB SSD | `LFG7642019601` | 0 | fresh XFS, `/mnt/i65-sda/i65` | `i65-20260908T114020` |
| HDD 1 | `/dev/sdb` | WDC WD5000LPCX‑24VHAT0 | `WD-WX41A48JXX3J` | 1 | fresh XFS, `/mnt/i65-sdb/i65` | `i65-20260908T114020` |
| HDD 2 | `/dev/sdc` | WDC WD5000LPCX‑21VHAT0 | `WD-WXU1A9932DES` | 1 | fresh XFS, `/mnt/i65-sdc/i65` | `i65-20260908T114020` |

**The original NVMe destination was `fedora-root` (XFS on LVM on `/dev/nvme0n1p3`)
on the XPG GAMMIX S5** — i.e. the near‑full OS root volume, not a dedicated/idle
NVMe filesystem. A dedicated NVMe XFS was **not** tested (NVMe was explicitly kept
out of the destructive lab setup).

## 2. Exact `i65_sink_result` records

### Run `i65-20260908T104933` — destination = NVMe / `fedora-root` (3 transfers)

```json
{"i65_sink_result":true,"label":"warmup","peer":"192.168.99.66:49672","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22184.1,"wall_ms_to_fsync":55362.0,"fsync_ms":33177.9,"mb_s":38.79,"mib_s":36.99,"recv_mb_s":96.80,"recv_mib_s":92.32,"sha256_hex":"2f989e26ffc8848061d26df8bb62050076e2958e6ceea40bfb9fd9a2ed44cf00"}
{"i65_sink_result":true,"label":"measured-1","peer":"192.168.99.66:49673","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22142.0,"wall_ms_to_fsync":44869.6,"fsync_ms":22727.6,"mb_s":47.86,"mib_s":45.64,"recv_mb_s":96.99,"recv_mib_s":92.49,"sha256_hex":"7f5be5fdc5baa7ea3c8bd0f480ea0874b20822c2b626950e1a6041cfebf548c2"}
{"i65_sink_result":true,"label":"measured-2","peer":"192.168.99.66:49674","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22413.0,"wall_ms_to_fsync":40265.6,"fsync_ms":17852.6,"mb_s":53.33,"mib_s":50.86,"recv_mb_s":95.81,"recv_mib_s":91.38,"sha256_hex":"7f5be5fdc5baa7ea3c8bd0f480ea0874b20822c2b626950e1a6041cfebf548c2"}
```

### Run `i65-20260908T114020` — SATA SSD → HDD 1 → HDD 2 (9 transfers, one MiniPC boot)

`conn 1-3` → `/dev/sda`, `conn 4-6` → `/dev/sdb`, `conn 7-9` → `/dev/sdc`. First
transfer of each group (`conn 1/4/7`, label `t1/t4/t7`) is the warmup.

```json
{"i65_sink_result":true,"conn":1,"label":"t1","dest_dir":"/mnt/i65-sda/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22279.6,"wall_ms_to_fsync":24585.8,"fsync_ms":2306.2,"mb_s":87.35,"mib_s":83.30,"recv_mb_s":96.39,"recv_mib_s":91.92,"sha256_hex":"4ef21739e0aeae23152a3b775cf6cd1bf0673b00895c69d589ad4e05db9b9827"}
{"i65_sink_result":true,"conn":2,"label":"t2","dest_dir":"/mnt/i65-sda/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22119.8,"wall_ms_to_fsync":24539.9,"fsync_ms":2420.2,"mb_s":87.51,"mib_s":83.46,"recv_mb_s":97.08,"recv_mib_s":92.59,"sha256_hex":"0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4"}
{"i65_sink_result":true,"conn":3,"label":"t3","dest_dir":"/mnt/i65-sda/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22218.7,"wall_ms_to_fsync":27386.0,"fsync_ms":5167.3,"mb_s":78.42,"mib_s":74.78,"recv_mb_s":96.65,"recv_mib_s":92.17,"sha256_hex":"0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4"}
{"i65_sink_result":true,"conn":4,"label":"t4","dest_dir":"/mnt/i65-sdb/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":24003.6,"wall_ms_to_fsync":34007.2,"fsync_ms":10003.6,"mb_s":63.15,"mib_s":60.22,"recv_mb_s":89.47,"recv_mib_s":85.32,"sha256_hex":"0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4"}
{"i65_sink_result":true,"conn":5,"label":"t5","dest_dir":"/mnt/i65-sdb/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22564.8,"wall_ms_to_fsync":32632.3,"fsync_ms":10067.5,"mb_s":65.81,"mib_s":62.76,"recv_mb_s":95.17,"recv_mib_s":90.76,"sha256_hex":"0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4"}
{"i65_sink_result":true,"conn":6,"label":"t6","dest_dir":"/mnt/i65-sdb/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22586.7,"wall_ms_to_fsync":32052.5,"fsync_ms":9465.7,"mb_s":67.00,"mib_s":63.90,"recv_mb_s":95.08,"recv_mib_s":90.67,"sha256_hex":"0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4"}
{"i65_sink_result":true,"conn":7,"label":"t7","dest_dir":"/mnt/i65-sdc/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":24053.2,"wall_ms_to_fsync":35103.3,"fsync_ms":11050.1,"mb_s":61.18,"mib_s":58.34,"recv_mb_s":89.28,"recv_mib_s":85.14,"sha256_hex":"0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4"}
{"i65_sink_result":true,"conn":8,"label":"t8","dest_dir":"/mnt/i65-sdc/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22657.7,"wall_ms_to_fsync":33237.4,"fsync_ms":10579.8,"mb_s":64.61,"mib_s":61.62,"recv_mb_s":94.78,"recv_mib_s":90.39,"sha256_hex":"0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4"}
{"i65_sink_result":true,"conn":9,"label":"t9","dest_dir":"/mnt/i65-sdc/i65","bytes":2147483648,"expected_bytes":2147483648,"complete":true,"wall_ms_recv":22651.9,"wall_ms_to_fsync":33004.2,"fsync_ms":10352.3,"mb_s":65.07,"mib_s":62.05,"recv_mb_s":94.80,"recv_mib_s":90.41,"sha256_hex":"0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4"}
```

## 3. Results table (measured runs only — the warmup per group excluded)

| Destination | recv MB/s | recv MiB/s | fsync (s) | recv+fsync (s) | **durable MB/s** | **durable MiB/s** |
|---|---|---|---|---|---|---|
| SATA SSD `/dev/sda` (XFS) | 96.9–97.1 | 92.2–92.6 | 2.4 – 5.2 | 24.5 – 27.4 | **78.4 – 87.5** | **74.8 – 83.5** |
| HDD 1 `/dev/sdb` (XFS) | 95.1 | 90.7–90.8 | 9.5 – 10.1 | 32.1 – 32.6 | **65.8 – 67.0** | **62.8 – 63.9** |
| HDD 2 `/dev/sdc` (XFS) | 94.8 | 90.4 | 10.4 – 10.6 | 33.0 – 33.2 | **64.6 – 65.1** | **61.6 – 62.1** |
| NVMe `fedora-root` / `/` (busy, 79 % full) | 95.8–97.0 | 91.4–92.5 | 17.9 – 22.7 | 40.3 – 44.9 | **47.9 – 53.3** | **45.6 – 50.9** |

Warmup transfers (for reference): SATA SSD 87.35 MB/s durable / fsync 2.31 s;
HDD 1 63.15 / 10.00 s; HDD 2 61.18 / 11.05 s; NVMe root 38.79 / 33.18 s.

## 4. Analysis

**Fastest receive rate:** `97.08` MB/s (`92.59` MiB/s) — SATA SSD run, `conn 2`.
Across all 12 transfers the receive rate is `89.3 – 97.1` MB/s (`85.1 – 92.6`
MiB/s); the four low values (`~89` MB/s, `~24.0` s) are each the *first* transfer
sent to a freshly‑mounted disk. **The live network stream never reached the #65
`>= 100` MB/s decimal minimum on this 1 GbE lab (~96–97 MB/s best).**

**Durable MB/s per disk (measured):** SATA SSD ~78–88 · HDD 1 ~66–67 · HDD 2
~65 · NVMe‑root ~48–53.

**What limited each case:**

- *Receive / live‑stream phase* — **network path (or MiniPC source read), not the
  destination and not Bamep software.** The receive rate is ~95–97 MB/s
  regardless of whether the destination is an SSD or a 5400‑RPM HDD, because the
  sink drains the socket into the page cache; the two‑thread producer/consumer
  probe keeps the single TCP stream continuously fed. The ~3–15 % gap to the
  100–115 MB/s target is the 1 GbE link / TCP / source read+hash, not the sink.
- *Durable completion* — **destination `fsync`, in every case.** The final
  `fsync` of the 2 GiB artifact adds ~2–5 s on the SATA SSD, ~10 s on each HDD,
  and a pathological 18–33 s on the near‑full running‑OS root NVMe volume.

## 5. Source / digest correctness (what is actually available)

- All **12** transfers: sink recorded `complete:true` with
  `bytes == expected_bytes == 2147483648`.
- Run `i65-20260908T114020`: `conn 2` through `conn 9` — **8 consecutive 2 GiB
  transfers across three different destination disks** — produced the identical
  sink‑computed SHA‑256 `0fd6deb0f41b9ab47bdce2c416640fd25269854fc6c828ab77f5d338ef5861c4`.
  Run `i65-20260908T104933`: `measured-1 == measured-2` (`7f5be5fd…`). ⇒ the
  streaming + write + hash path is byte‑exact and deterministic within a boot.
- **Anomaly (not investigated — flagged for later):** the *first* transfer of
  each boot differs from the rest (`2f989e26…` in run 1, `4ef21739…` in run 2),
  and the stable value differs between the two boots (`7f5be5fd…` vs
  `0fd6deb0…`). The MiniPC source region `[0, 2 GiB)` is evidently **not
  write‑stable across boots** (something on the endpoint touches it between
  power cycles). This does not affect the throughput measurement.
- The probe's own client↔sink `I65_DIGEST_MATCH` line and per‑transfer exit
  codes print to the WinPE console only and were **not captured** (WinPE runs
  from RAM; local NDJSON is lost at power‑off).

## 6. Cleanup confirmation

- Run `i65-20260908T114020`: every transfer logged
  `I65_SINK_CLEANUP conn=N ... remove_ok=true gone=true`; end‑of‑run reported
  `OK: no leftover Issue-65 capture payloads on any destination`.
- Post‑run free space (fresh XFS, no payloads): `/mnt/i65-sda` `235310141440` B
  (~219 GiB), `/mnt/i65-sdb` `490257551360` B (~456 GiB), `/mnt/i65-sdc`
  `490257551360` B (~456 GiB).
- Run `i65-20260908T104933`: the pre‑selector sink removed `capture.bin` at end;
  verified — no `*.bin` remains under `evidence/`.
- The three lab disks (`/dev/sda`, `/dev/sdb`, `/dev/sdc`) keep their GPT + single
  XFS partition for lab reuse; they were unmounted by the operator after the run.

## 7. Recommendation — Bamep hot / cold storage

- **The capture receive rate is destination‑independent** on this 1 GbE lab
  (~96 MB/s everywhere). Destination choice does not change how fast bytes come
  off the wire; it changes only the **durable‑completion latency** (the fsync).
- **Hot tier / capture landing zone:** a **dedicated SSD filesystem** — the SATA
  SSD gave ~87 MB/s durable with a ~2–5 s fsync per 2 GiB. A dedicated (idle,
  not‑near‑full, not OS‑root) NVMe should beat that and is the preferred hot
  tier. **Do not land captures on the OS root volume:** run 1 shows that path
  collapses to ~48–53 MB/s durable with 18–33 s stalls, purely from fsync
  contention on a busy 79 %‑full LVM root.
- **Cold / bulk‑retention tier:** the 5400‑RPM HDDs (~65 MB/s durable, ~10 s
  fsync per 2 GiB) are adequate for archival copies where completion latency is
  off the critical path. The two HDD units tracked within ~4 % of each other —
  no bad drive.
- **For 2.5 GbE later:** the SATA SSD's ~87 MB/s and the HDDs' ~65 MB/s durable
  rates would themselves become the ceiling below a 2.5 GbE wire. A hot tier will
  need NVMe (or striping), and the per‑artifact `fsync` cost becomes the primary
  thing to engineer around — batched / deferred durability, already flagged by
  #63 and by this Spike.
- **Open item for #65 proper (separate work):** the live stream tops out at
  ~96–97 MB/s, ~3 % under the 100 MB/s decimal floor. Whether the residual
  limiter is the MiniPC NIC, the switch/TCP path, or the source read+hash is
  **not** resolved here and must be measured before declaring the throughput
  gate met.
