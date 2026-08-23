# M0 — Simulator Contract and Validation Strategy

Status: **Approved**

This Specification defines the normative Bamep Simulator fidelity boundary, Simulated Endpoint behavior, required scenarios, concurrency target, and empirical validation obligations.

General test-layer policy belongs to `docs/development/testing.md`. The Simulator consumes the Endpoint, Agent Protocol, Job lifecycle, persistence, data-plane, and trusted-bootstrap Specifications without redefining their contracts.

## Fidelity boundary

The Simulator combines two fidelity levels:

- **Simulated Endpoints/Agents** exercise real Bamep Server-side behavior: Endpoint identity/enrollment, Agent Protocol v1, Job/JobStep/Attempt orchestration, scheduler/resource leases, persistence, reconciliation, and applicable data-plane behavior.
- **Faked hardware/OS boundaries** replace PXE/DHCP/UEFI/Secure Boot/bootloader execution, hardware discovery/probing, physical disks, and similar device-specific behavior with deterministic fixtures or temporary local storage.

At **Simulator level**, the Agent-side participant MUST use the real Agent Protocol v1 transport path end to end:
- real WSS connection to the real Server-side Agent Control Gateway;
- real Agent Protocol v1 UTF-8 JSON serialization;
- real TLS pin verification and Agent Protocol handshake;
- real credential authentication;
- real disconnect/reconnect behavior.

An in-process fake Agent transport does not satisfy a Simulator-level scenario. Narrower Unit/Component/Integration tests may still fake Agent connections according to `docs/development/testing.md`.

Fault injection may control timing, duplication, delay, disconnect, restart, and other simulated conditions, but the resulting Simulator-level Agent scenario must still cross the real WSS boundary.

## Trusted-bootstrap fixture boundary

The Simulator does not execute or validate the production Secure-Boot-backed boot chain. It may receive deterministic fixture material representing the trusted-bootstrap inputs that a physical boot path would provide, as defined by `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md`.

Fixture use MUST NOT be reported as evidence that production Secure Boot, PXE, firmware, bootloader, or physical Agent integrity was validated.

After that fixture boundary, the Simulator still exercises real Agent Protocol behavior, including:
- valid pinned Server fingerprint;
- fingerprint mismatch failing closed before Agent Protocol authentication;
- valid and rejected Agent credentials;
- `BootstrapEvidence` acceptance/rejection through the real Server verification path;
- disconnect/reconnect and fresh authentication;
- uncertain delivery and status reconciliation where applicable.

The fixture's concrete configuration/file representation is an implementation detail; its security semantics remain owned by the trusted-bootstrap Specification.

## Simulated Endpoint contract

A Simulated Endpoint must support configuration sufficient to model at least connection/disconnection/reconnection, latency, throughput, CPU/resource constraints, storage characteristics/pressure, operation duration, injected failures/interruptions, workflow-driven retries, inventory changes, and simulated Agent restart/loss of local action state.

Its behavioral surface includes:
- **Identity/enrollment** — configurable inventory/correlation evidence and normal Endpoint enrollment/reconnect paths.
- **Agent session** — real TLS pin verification, `AuthRequest`, `SessionEstablished`, `AuthError`, runtime-credential renewal, and reconnect behavior.
- **Inventory** — configurable snapshots sent as post-session `InventoryReport{inventory}` through
  the real WSS/Agent Protocol path, exercising first report, unchanged re-report, changed report,
  and phase-invalid rejection at the persistence contract's write-on-change boundary.
- **Actions** — typed `ActionDispatch` handling with configurable timing/failure and the full Agent-local action-state vocabulary required by Agent Protocol.
- **Data plane** — simulated source/target bytes in temporary storage, chunk manifests, interruption/resume, corruption, and source mutation as required by the data-plane Specification.

The Simulator must never expose generic arbitrary command execution merely for test convenience.

## Required scenarios

The Simulator must be capable of exercising at least:

| Scenario | Required behavior |
| --- | --- |
| Duplicate/delayed messages | Agent Protocol duplicate/idempotency behavior without duplicate execution while authoritative Agent state exists |
| Stale inventory | destructive dispatch fails when the current inventory revision no longer satisfies authorization |
| Endpoint disappearance | active/uncertain execution enters the Job lifecycle reconciliation path |
| Agent restart | lost local action state is represented as `Unknown`, never as proof of non-execution |
| Server restart | persisted uncertain Attempts reconcile instead of being blindly redispatched |
| Scheduler contention | Job-scoped Endpoint exclusivity and Attempt-scoped resource leases contend correctly |
| Resource exhaustion | resource/capacity failure is represented without bypassing scheduling or storage constraints |
| Partial failure | JobStep/Artifact failure remains failure; partial Artifact completion is never reported as success |
| Cancellation | real `CancelAction`/`CancelAck` behavior composes with Job `Cancelling` semantics |
| Recovery after interruption | chunk resume and Attempt reconciliation occur without assuming prior execution outcome |

Data-plane Simulator obligations from `docs/specifications/m0-data-plane-and-storage-contracts.md` remain additive, including interrupted/corrupted chunk transfer; source mutation/reproducibility failure; destructive rejection when `capture_consistency` is `NotEstablished` even if the Artifact is `Verified`; disk-replacement/provenance scenarios where source Artifact disk identity is not incorrectly required to equal the later destructive target disk identity; and authenticated transfer-session/sender-constrained capability behavior where exercised.

## Trusted-bootstrap independence scenario

The Simulator MUST include the fail-closed case where destructive-operation preconditions 1–6 all hold and only trusted current bootstrap is absent.

In that case:
- no destructive Attempt may be created;
- no durable destructive dispatch commitment may be persisted;
- no `ActionDispatch` may be sent.

This proves that valid Endpoint identity, current credential, authorized workflow, fresh inventory, target-disk revalidation, and `Consistent` hardware confidence do not imply trusted bootstrap. The authoritative seven-precondition gate remains `docs/specifications/m0-endpoint-identity-lifecycle.md`.

## Concurrency target

The Simulator must support **20–24 concurrent Simulated Endpoints** in the representative high-density scenario. Smaller scenarios remain valid for faster development, but they do not demonstrate the required concurrency ceiling.

The 20–24 target applies especially to scheduler/resource contention, representative concurrent Job/JobStep/Attempt activity, persistence-load validation, and data-plane transfer scenarios intended to validate behavior at target concurrency. It is a validation workload, not a commercial licensing limit.

## Persistence-load validation

At the 20–24 Endpoint target, the Simulator must generate representative durable activity and measure/record at least:
- durable write volume;
- contention;
- persistence latency;
- backpressure.

The workload must include representative state transitions, domain events, audit records, inventory-on-change, and Artifact/manifest metadata according to the persistence Specification.

No numeric pass/fail threshold is defined here without empirical evidence. If observed behavior is unacceptable for the adopted PostgreSQL baseline, ADR-0013 must be revisited rather than silently weakening this scenario or inventing a different persistence contract.

## Validation boundary

`docs/development/testing.md` owns the general test-layer model.

For this Specification specifically:
- Simulator claims require the real Simulator fidelity boundary defined above;
- persistence behavior claimed by Simulator scenarios uses the real selected persistence backend where the scenario depends on persistence semantics/load;
- hardware/firmware claims require the Integration Environment or explicit owner manual validation;
- passing Simulator scenarios does not prove physical provisioning compatibility.

## Not represented by the Simulator

The Simulator does not validate real PXE/DHCP behavior, UEFI firmware, Secure Boot execution, GRUB/WinPE/Alpine physical boot behavior, physical NIC/MikroTik behavior, real disk tooling/destructive disk operations, Windows deployment, physical driver injection, hardware-specific compatibility, or destructive end-to-end physical provisioning.

Those belong to the Integration Environment and/or explicit manual validation.

The Simulator also does not own the Administrative API/Web contract; Web observation semantics remain defined by `docs/specifications/m0-administrative-api-web-read-contract.md`.

## Out of scope

- Simulator process/thread/crate/module architecture and concrete configuration format;
- concrete persistence-load acceptance thresholds before empirical evidence exists;
- production boot/provisioning implementation;
- production storage/destructive tooling;
- Administrative API design;
- replacement of physical Integration Environment validation.

## Related

- `docs/development/testing.md` — general test layers, fakes, Simulator, and Integration Environment policy.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — Endpoint state and destructive gate.
- `docs/specifications/m0-agent-protocol-contract.md` — real Simulator-level Agent wire path.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — scheduling, reconciliation, retry, cancellation, and dispatch semantics.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — persistence/load obligations.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — transfer/Artifact Simulator scenarios.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — trusted-bootstrap fixture semantics.
- ADR-0013 — PostgreSQL persistence baseline evaluated by the persistence-load scenario.
