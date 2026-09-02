# Headless operational-core scale validation

## Purpose

This document records the reusable empirical evidence produced by Issue #21 for the final
hardware-independent M1 validation boundary. Normative requirements remain in
`docs/specifications/m1-simulated-vertical-slice-and-baseline-validation.md` and the Simulator,
persistence, scheduling, and data-plane Specifications it references.

## Environment

- Date: 2026-09-02.
- Repository commit: `17daeca0ce7041c5583af4ff4c6367f8c0fafe36` plus the uncommitted Issue #21
  test/evidence changes.
- OS: Fedora Linux, kernel `7.1.10-200.fc44.x86_64`, x86-64.
- Available CPU concurrency: 8 logical processors.
- Memory: approximately 15.4 GiB (`MemTotal` 16,178,472 KiB).
- PostgreSQL: 18.4, Fedora x86-64 build; `max_connections = 100`.
- Bamep PostgreSQL pool: the production Adapter configuration, bounded at 10 connections.
- Rust test profile: unoptimized with debug information.

The harness created one migrated disposable PostgreSQL database per test through
`support::TestDatabase` and removed it after success. All transfer bytes used temporary local
filesystem storage.

## Method

The automated harness is
`crates/server/tests/headless_scale_validation.rs`. It separates contractual assertions from
timing observations and uses explicit barriers to make all 24 participants concurrently relevant.

The scheduler/persistence scenario starts 24 tasks together. Each task establishes and approves a
distinct Endpoint, records a changed inventory revision, creates and admits a one-step Job,
satisfies its step preconditions, and creates Transfer/Artifact/manifest metadata through the real
Application services and PostgreSQL Adapters. A second barrier then makes all 24 prepared
workflows compete for 8 units of the existing transient network resource. A non-intrusive observer
records peak checked-out PostgreSQL connections while the workload runs.

The data-plane scenario prepares 24 distinct integrated verticals, each with a real Agent Protocol
WSS session, pinned TLS, Worker HTTPS endpoint, Worker Protocol over AF_UNIX, temporary filesystem
chunk storage, and shared real PostgreSQL persistence. A barrier starts all 24 three-chunk uploads
together. Each transfer uploads and seals 10,000 bytes, verifies its Artifact, and commits terminal
Attempt/JobStep/Job evidence.

The recorded latencies are observations, not pass/fail thresholds.

## Correctness evidence

The scheduler/persistence scenario passed with:

- 24 distinct durable Endpoints and concurrent workflows;
- exactly 8 committed dispatches at capacity 8;
- exactly 16 explicit `ResourceUnavailable` outcomes, without bypassing capacity;
- every committed JobStep durably `Dispatching`;
- every successful reservation released, after which the full capacity was acquirable again;
- 24 durable changed-inventory revisions.

The data-plane scenario passed with:

- 24 concurrent authenticated transfers;
- 240,000 bytes transferred in total;
- 72 individually held chunk identities (three per Artifact);
- 24 `Verified` Artifacts;
- all 24 Attempts and Jobs durably `Succeeded` after terminal evidence;
- the expected progress sequence `0, 4096, 8192, 10000` for every transfer.

## Empirical observations

One fresh serial execution of the two tests produced the following values:

| Observation | Scheduler/persistence | Data plane |
| --- | ---: | ---: |
| Concurrent Endpoint tasks/transfers | 24 | 24 |
| Total scenario workload elapsed | 277 ms | 986 ms |
| Per-Endpoint setup p95 / maximum | 248 / 251 ms | not measured separately |
| Dispatch p95 / maximum | 11 / 11 ms | not applicable |
| Concurrent transfer p95 / maximum | not applicable | 507 / 513 ms |
| PostgreSQL peak checked-out connections | 10 of 10 | not separately sampled |
| Explicit resource backpressure | 16 of 24 | none observed |

The scheduler/persistence workload left 320 durable rows before teardown:

| Durable relation | Rows |
| --- | ---: |
| `endpoints` | 24 |
| `inventory_revisions` | 24 |
| `audit_records` | 24 |
| `domain_events` | 120 |
| `jobs` | 24 |
| `job_steps` | 24 |
| `attempts` | 8 |
| `artifacts` | 24 |
| `transfers` | 24 |
| `chunk_manifests` | 24 |
| `chunk_identities` | 0 |

The separate completed data-plane workload left 408 durable rows before teardown: 24 Endpoints,
48 audit records, 120 domain events, 24 each of Jobs, JobSteps, Attempts, Artifacts, Transfers, and
chunk manifests, plus 72 held chunk identities. Inventory-on-change was exercised in the
scheduler/persistence workload rather than duplicated in this transfer workload.

No PostgreSQL error, timeout, transaction failure, transfer failure, or unexpected rejection was
observed. The pool reached its configured 10 checked-out connections under the 24-task durable
workload, so work was bounded by the existing pool rather than creating unbounded database
connections. The latency figures above include any resulting pool/database wait; the harness does
not claim to separate those components precisely.

## ADR-0013 assessment

This evidence supports retaining ADR-0013. PostgreSQL preserved the tested durable invariants and
completed the representative 24-Endpoint workloads without errors or timeouts while the bounded
pool saturated. Nothing observed justifies an ADR revisit. This conclusion is limited to the
software baseline and environment described above; it is not a commercial sizing claim.

## Limitations

- One run is a baseline observation, not a statistically rigorous benchmark or capacity limit.
- Debug-profile timings include test instrumentation and local scheduling noise.
- Durable row counts are logical persisted records, not physical WAL bytes, filesystem writes, or
  storage-device write amplification.
- Peak pool occupancy shows saturation, but does not attribute latency precisely among pool wait,
  PostgreSQL locks, CPU scheduling, TLS, IPC, or filesystem work.
- The integrated Worker runs in-process as already documented by the shared transfer harness;
  separate M1 tests prove process supervision/isolation.
- Loopback networking and temporary local storage do not prove physical network, disk, firmware,
  PXE, Secure Boot, WinPE, commercial hardware throughput, retention, or BOM capacity.
- The 24-Endpoint target is a validation workload, not a product/licensing limit.

## Reproduction

With the repository-documented disposable PostgreSQL administrator connection available:

```bash
BAMEP_TEST_PG_ADMIN_URL='<admin PostgreSQL URL>' \
  cargo test -p bamep-server --test headless_scale_validation -- \
  --nocapture --test-threads=1
```

The `M1_SCALE` lines contain the observed values for the current run. They are diagnostic evidence;
the correctness assertions do not fail merely because another valid environment is slower.
