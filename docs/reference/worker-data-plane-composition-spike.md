# Worker Data-Plane Process Composition — Empirical Spike Evidence

Status: **Completed empirical reference.**

This document preserves empirical evidence from a Technical Spike into how Bamep should
materialize the Worker process boundary already required by ADR-0001 and ADR-0003 for the
M1 data plane. It does not define current Bamep architecture. ADR-0001 owns the accepted
decision that heavy/risky workloads — explicitly including transfer, compression,
verification, and Artifact movement — execute behind a separate Worker process/isolation
boundary; ADR-0003 owns the Worker/Agent language and inter-process contract-explicitness
decision. Neither ADR selects the concrete process/listener/IPC/authorization-ownership
composition; that is what this Spike investigated. The result supports a candidate for
owner architectural approval; it does not itself accept that architecture.

## Question

Which minimum Worker-process/runtime composition should Bamep use for the M1 data plane so
that bulk Artifact transfer executes outside the control-plane process path, transfer
backpressure/failure cannot starve Agent control traffic, the Worker remains part of the
same modular-monolith Server product/release, HTTPS data-plane traffic reuses Bamep's
trusted Server TLS identity, transfer authorization remains Server-authoritative and
fail-closed, durable Transfer/Artifact state stays correctly coordinated with PostgreSQL,
filesystem/storage I/O stays behind the proper Adapter/Worker boundary, and Worker
restart/failure has explicit semantics — without introducing a microservice/distributed
architecture?

This Spike does not reopen *whether* Bamep uses Worker process isolation (ADR-0001,
Accepted) or *whether* the Worker is Rust with an explicit inter-process contract
(ADR-0003, Accepted). A same-process Tokio task was not evaluated as an alternative to
process isolation; it was already rejected by ADR-0001. A Tokio task may exist inside the
Worker process, which is a different question.

## Why existing evidence was insufficient

At the time of this Spike, no Worker crate/binary existed anywhere in the Cargo workspace,
and `crates/server` had no HTTP data-plane implementation. `docs/reference/poc-lessons.md`
documents the FORGE proof-of-concept failure mode that motivated ADR-0001 (shared-runtime
transfer work starving control traffic) but as a retrospective case study, not as evidence
for the concrete Server/Worker process composition Bamep itself should use. ADR-0008
already requires HTTPS data-plane traffic to reuse the Server's trusted TLS identity, but
neither ADR-0008 nor ADR-0001 selects which process owns that listener, how the Worker
obtains the TLS identity, which process remains authoritative for transfer authorization, or
how the two processes coordinate durable PostgreSQL state.

## Environment and toolchain

- Host: Linux (kernel 7.2.0), 12 logical cores.
- `rustc`/`cargo` 1.96.0, built fully offline (`cargo build --offline`) against dependency
  versions already used in production: `tokio` 1.53.1, `rustls` 0.23.43, `tokio-rustls`
  0.26.4, `rcgen` 0.14.9, `ring` 0.17.14.
- All experiment material was created and executed outside the Bamep repository, in a
  disposable scratch directory, and the test processes were terminated after the
  experiment. No repository file was created, edited, or removed by this Spike.
- No real HTTP framework, no PostgreSQL, no production capability cryptography, and no
  physical/destructive testing were used. This Spike is boundary/viability evidence, not a
  throughput benchmark.

## Candidate compositions evaluated

**A — Worker owns the data-plane HTTPS/TLS listener directly**, loading the Server's TLS
identity independently from shared configuration, with an explicit local IPC contract to
`bamepd` for authorization decisions and durable-fact reporting.

**B — `bamepd` terminates the external HTTPS listener and forwards bulk request bodies to
the Worker over local IPC.** Rejected without a new direct experiment: TLS decryption and
byte-copy work for bulk transfer traffic would still compete for the same Tokio runtime/CPU
scheduler as Agent Protocol control traffic in `bamepd`, reproducing the exact failure mode
`docs/reference/poc-lessons.md` already documents as the motivation for ADR-0001. This is
evidence-based interpretation building on already-accepted evidence, not a new measurement
of B.

**C — Kernel-level file-descriptor handoff (`SCM_RIGHTS`) or kTLS to let `bamepd` keep a
single external listener without exposing the long-lived private key to the Worker.**
Considered and rejected for M1: requires an additional dependency and security-review
surface with no concrete Bamep requirement for a single external port. Not implemented or
tested.

Composition **A** was carried forward into the empirical experiment below.

## Experiment structure

Three disposable Rust binaries simulated composition A:

- `control_process` — generates a TLS identity ("Server"), writes cert/key material to a
  shared location, binds a TLS 1.3 "control-plane" listener (same no-client-cert pattern as
  `crates/server/src/adapters/agent_transport.rs`), binds a Unix Domain Socket for IPC as
  the server side, spawns and supervises `worker_process` as a real OS child process
  (`wait()`-based detection and respawn).
- `worker_process` — loads the same cert/key material independently from the shared
  location, binds its own TLS "data-plane" listener, connects to the IPC socket as a
  client, performs a real authorization round-trip over IPC before accepting each simulated
  chunk, and does deliberate CPU-bound work per chunk to saturate its own runtime.
- `probe_client` — measures TLS leaf-certificate fingerprints, control-plane ping latency,
  and generates concurrent simulated-chunk load against the Worker.

## Evidence

### Process isolation

```text
ps -eo pid,ppid,comm
  16255    5056 control_process
  16271   16255 worker_process
```

Two distinct OS PIDs in a real parent/child relationship, not threads or async tasks.

### TLS identity reuse

Independently obtained leaf-certificate fingerprints from real TLS handshakes against both
listeners were identical:

```text
127.0.0.1:8801 (control) leaf sha256=bf13db7434069f5a198532106e3aa1f7499a5f71510b293380bf2598fe42067e
127.0.0.1:8802 (data-plane) leaf sha256=bf13db7434069f5a198532106e3aa1f7499a5f71510b293380bf2598fe42067e
```

### Backpressure/failure isolation

Control-plane ping latency, no Worker load (baseline, n=813): min 0.427ms, avg 0.849ms, p95
1.006ms, max 1.245ms.

Control-plane ping latency, during concurrent Worker saturation (8 connections × 24 MiB with
artificial per-block CPU cost, n=819): min 0.370ms, avg 0.698ms, p95 0.961ms, max 1.271ms —
statistically equivalent, not degraded.

CPU sampling during saturation: `worker_process` at 42–50% cumulative CPU (its own two
runtime threads saturated); `control_process` remained at 0.2–0.9% CPU throughout —
consistent with the Worker's load not competing for the control process's scheduler/CPU.

### Crash/respawn

```text
[control pid=16255] sent deliberate CRASH command to worker over IPC
[worker  pid=16271] received deliberate CRASH command over IPC; exiting immediately
[control pid=16255] worker IPC connection closed (EOF)
[control pid=16255] worker pid=16271 exited: Ok(ExitStatus(unix_wait_status(256)))
[control pid=16255] respawning worker (attempt 1)
[control pid=16255] spawned worker process pid=16943 (generation 1)
[worker  pid=16943] loaded shared Server TLS identity independently, leaf sha256=bf13...
[worker  pid=16943] IPC handshake acknowledged by control process
```

`control_process` (PID 16255) never restarted; the same PID ran for the full 141s
experiment. The Worker respawned under a new PID roughly 2ms after its exit was detected.
Exactly 2 of 8 in-flight simulated transfers failed with `UnexpectedEof` at the moment of
the crash — consistent with the already-accepted chunk-resume contract
(`m0-data-plane-and-storage-contracts.md`), not a new terminal-state requirement. After
respawn, a fresh fingerprint check and a new 4 MiB load both succeeded immediately against
the new Worker generation.

## Negative/open findings

- When `control_process` was terminated first, `worker_process` did not self-terminate: it
  kept accepting data-plane connections, but any request depending on the IPC authorization
  round-trip would hang indefinitely (the pending request is never resolved). Whether the
  Worker should self-terminate on IPC loss or fail closed after a bounded authorization
  timeout is **not decided by this Spike** and must be an explicit choice in the follow-up
  ADR/Work Package, not an implicit default.
- Composition B was rejected by analysis referencing already-accepted `poc-lessons.md`
  evidence, not by a new direct measurement in this Spike.
- Shared-TLS-material provisioning (shared file vs. inherited file descriptor at spawn time)
  was only exercised via a shared file; a spawn-time fd-inheritance alternative is plausible
  but untested.
- Only a clean IPC-triggered exit was tested as "Worker crash," not a real `SIGSEGV`/abort or
  a partially written/corrupted IPC message.
- No real HTTP framework (Axum/Tower or otherwise), no real PostgreSQL adapter, no real
  capability/proof cryptography, and no 20–24 endpoint scale were exercised. Whether the
  Worker's HTTPS listener should use Axum/Tower (as ADR-0017 selected for the Administrative
  surface) or a more direct rustls composition suited to large streamed chunk bodies was not
  evaluated here.
- Throughput was not benchmarked; this Spike is boundary/viability evidence only.

## Conclusion

The experiment supports composition A — the Worker owning and terminating the data-plane
HTTPS/TLS listener directly, loading the Server's TLS identity independently without the
private key crossing the IPC channel, communicating with `bamepd` over a local Unix Domain
Socket for per-chunk authorization round-trips and durable-fact reporting, with `bamepd`
remaining the sole authority for capability issuance, proof verification, replay-cache
mutation, durable authorization/state checks, and all PostgreSQL access — as the candidate
for owner architectural approval to satisfy ADR-0001/ADR-0003/ADR-0008's already-accepted
process-isolation, language, and TLS-reuse requirements for the M1 data plane. Process
isolation, TLS identity reuse, backpressure/failure isolation, and independent Worker
crash/restart were each empirically demonstrated. Worker behavior on IPC loss remains an
open design point for the follow-up decision. This remains empirical evidence for a future
architectural decision; it does not itself constitute owner-accepted architecture.

## Related

- `docs/decisions/0001-runtime-topology-modular-monolith.md` — the accepted Worker
  process-isolation decision this Spike's evidence informs.
- `docs/decisions/0003-worker-and-agent-language-strategy.md` — the accepted Worker language
  and inter-process contract-explicitness decision this Spike's evidence informs.
- `docs/decisions/0008-data-plane-transport-chunking-and-resumability.md` — the accepted
  HTTPS/TLS-identity-reuse and chunk/resume contract this composition must satisfy.
- `docs/decisions/0013-postgresql-persistence-backend.md` — the accepted persistence
  backend and Domain/Application isolation boundary this composition preserves.
- `docs/reference/poc-lessons.md` — the FORGE case-study evidence for the shared-runtime
  starvation failure mode motivating ADR-0001, referenced by this Spike's rejection of
  composition B.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — the normative chunk,
  manifest, Artifact, resume, and authorization contract this composition must serve.
- Issue #19 — the Work Package this evidence makes architecture-ready, pending a
  corresponding ADR.
