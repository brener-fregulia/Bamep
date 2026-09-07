# M0 — Minimum Administrative API and Web Read Contract

Status: **Approved**

This Specification is the normative contract for the minimum Administrative API v1 read surface
used by Bamep Web. It owns Server ↔ Web resource representations, versioning, snapshot-read
semantics, and minimum HTTP reads. Domain lifecycle semantics remain authoritative in their
owning Specifications; this contract projects them without redefining them.

This approved contract is retained as minimum Presentation-contract evidence and future product
input. It is not implemented at current HEAD, is no longer required for M1 completion, and is not
a complete future operator-plane API. Later approved Specification work may revise or supersede it
before production implementation without weakening the boundary constraints below.

## Boundary

Bamep has three distinct communication responsibilities:

1. **Agent Control Plane** — Agent ↔ Server through Agent Protocol v1/WSS.
2. **Data Plane** — Agent ↔ Server bulk Artifact transfer.
3. **Administrative / Management Plane** — Bamep Web ↔ Server through this API.

Administrative API v1 is only the third boundary. Web consumes this explicit versioned contract
and never reads the Server database directly. Persistence schema, Rust types, and internal module
boundaries are not the Web API.

## Scope

Administrative API v1 defines:

- minimum current-state reads for a future Presentation implementation;
- a minimum Endpoint collection read for the current small fleet;
- Endpoint and Job representations, including nested JobStep/Attempt state;
- progress/reconciliation and Transfer/Artifact summaries;
- stable cross-boundary identifiers;
- JSON/versioning conventions;
- minimum HTTP routing and 200/404 semantics.

It is intentionally not a future-complete management API.

## Out of scope

This contract does not define Web UI, administrative authentication/users/RBAC, Job creation or
cancellation, enrollment approval, destructive or other Web-originated writes, workflow editing,
browser push/change notifications, collection reads beyond the minimum Endpoint collection,
search/filter/sort queries, pagination/cursors, persistence schema,
Agent Protocol/data-plane mechanics, external ERP APIs, or public third-party compatibility
guarantees.

A non-production implementation may exercise this read contract before production administrative
authentication exists, but must not be represented as production-secure.

## Delivery model

Administrative API v1 is a **request/response snapshot-read contract**. A successful read returns
the authoritative current Server-owned state for the requested resource.

Polling is permitted but is not part of the contract. No push mechanism is required. If SSE,
WebSocket, long polling, or another notification mechanism is introduced later, notifications may
prompt a re-read but must not become the sole source from which Web reconstructs Endpoint or Job
state.

The API is snapshot-oriented, not a domain-event stream.

## Wire format and versioning

- Bodies are UTF-8 JSON.
- Timestamps are RFC 3339 / ISO 8601 UTC strings, never epoch integers.
- Domain identifiers exposed to Web are stable opaque JSON strings; Web must not infer semantics
  from their textual form.
- `action_id`, when present, follows the Agent Protocol v1 identifier contract.
- Optional absent fields are omitted, never serialized as `null`.
- Clients ignore unknown fields inside otherwise known response shapes.
- Administrative API v1 is versioned independently from Agent Protocol and package SemVer.
- `/api/admin/v1/` is the explicit routing/version boundary.

Identifier ownership and persistence correlation remain defined by
`m0-persistence-observability-and-domain-events.md`.

## Minimum HTTP reads

Administrative API v1 defines exactly these minimum reads:

- `GET /api/admin/v1/endpoints`
  - successful collection read → HTTP `200` with the Endpoint collection representation;
  - an empty fleet → HTTP `200` with `{"endpoints": []}`, never HTTP `404`.
- `GET /api/admin/v1/endpoints/{endpoint_id}`
  - existing Endpoint → HTTP `200` with the Endpoint representation;
  - nonexistent Endpoint → HTTP `404`.
- `GET /api/admin/v1/jobs/{job_id}`
  - existing Job → HTTP `200` with the Job representation;
  - nonexistent Job → HTTP `404`.

The Job representation includes ordered JobStep summaries, the relevant Attempt summary, progress
snapshot, and Transfer/Artifact summary when applicable. Separate JobStep/Attempt endpoints are
not part of this minimum contract.

## Resource representations

### Endpoint collection

The response is a JSON object with one required `endpoints` array. Each item is the Endpoint
representation below, with the same fields and semantics as the detail read. No collection
metadata is defined.

An empty collection is:

```json
{
  "endpoints": []
}
```

The collection enumerates existing Endpoints, including their authoritative identity states;
it does not synthesize resources for absent or deleted Endpoints or a separate retired-resource
model. Ordering is semantically unspecified. This small-fleet read has no pagination, cursor,
filter, search, or sort query contract.

The existing v1 unknown-field rule applies to the collection envelope and its Endpoint items.

### Endpoint

Expose:

- `endpoint_id`;
- identity state: `PendingEnrollment` | `Enrolled` | `Retired`;
- credential state:
  `NoActiveCredential` | `CredentialActive` | `CredentialExpired` | `CredentialRevoked`;
- `agent_presence`: `Connected` | `Disconnected`;
- hardware confidence: `Consistent` | `LoweredConfidence` | `Conflict`;
- current inventory revision identifier/reference when one exists;
- whether a current durable inventory revision exists.

`agent_presence` is independent of credential state. A `CredentialActive` Endpoint may be
`Disconnected`; the API must not derive one dimension from the other.

Collection and detail reads preserve `endpoint_id`, identity state, credential state,
`agent_presence`, hardware confidence, and current-inventory presence/reference as separate
facts. Returned `endpoint_id` values are the real opaque Administrative API Endpoint identifiers;
`LAB-*` identifiers are Presentation fixtures only and define no API identity semantics.

Web composes operator-facing presentation from these facts. The API defines no aggregate
`ready`, `situation`, `health`, or `availability` state and no durable Endpoint naming/alias
concept such as `display_name`, `asset_tag`, or hostname authority.

Endpoint semantics are owned by `m0-endpoint-identity-lifecycle.md`; inventory durability is owned
by `m0-persistence-observability-and-domain-events.md`. Richer inventory content is deferred until
that content model has its own approved contract.

### Job

Expose:

- `job_id`, `endpoint_id`;
- state: `Pending` | `Running` | `Cancelling` | `Succeeded` | `Failed` | `Cancelled`;
- ordered JobStep summaries;
- terminal outcome;
- when failed, operator-relevant information from the failing JobStep.

### JobStep

Expose:

- `jobstep_id`, `job_id`;
- state:
  `Pending` | `PreconditionsSatisfied` | `Dispatching` | `Succeeded` | `Failed` | `Cancelled`;
- when failed, `failure_reason`:
  `PreconditionNotMet` | `DispatchRejected` | `ExecutionFailed` |
  `ReconciliationIndeterminate`;
- current or most recent Attempt summary when one exists.

### Attempt

Expose:

- `attempt_id`, `jobstep_id`;
- optional `action_id` for Agent-executed Attempts;
- state:
  `Dispatched` | `InProgress` | `AwaitingReconciliation` | `Succeeded` | `Failed` |
  `Cancelled` | `Rejected` | `Indeterminate`;
- optional progress snapshot: `percent?`, `bytes_processed?`, `eta?`.

`attempt_id` and `action_id` remain distinct. Progress is latest-value/transient state. Absence of
progress is an omitted field, never a fabricated `0` or other default.

Job/JobStep/Attempt semantics are owned by `m0-job-lifecycle-and-scheduling.md`; `ActionProgress`
semantics are owned by `m0-agent-protocol-contract.md`.

### Transfer / Artifact summary

For a transfer JobStep expose:

- `artifact_id`;
- Artifact state: `Incomplete` | `PendingVerification` | `Verified` | `Failed`;
- `capture_consistency` when applicable:
  `NotApplicable` | `NotEstablished` | `Established`;
- `transfer_id`.

Chunk-manifest details are outside this minimum Web read contract. Artifact, transfer, and
capture-consistency semantics are owned by `m0-data-plane-and-storage-contracts.md`.

## Representation semantics

- **Absent detail resource:** HTTP `404`; no synthetic empty Domain state.
- **Empty Endpoint collection:** HTTP `200` with an empty `endpoints` array.
- **Optional absent field:** omitted from JSON, never `null`.
- **Pending:** use the owning Domain state; no parallel Web-only "not started" state.
- **Failed/rejected:** expose the owning state and applicable `failure_reason`; do not collapse them
  into a generic boolean/error state.
- **Terminal Job:** expose `Succeeded`, `Failed`, or `Cancelled`; no parallel `done` flag.
- **Reconciliation:** expose `AwaitingReconciliation` and `Indeterminate` honestly. Neither may be
  mapped to success, failure, or "not executed".

Web-specific lifecycle vocabulary must not diverge from the authoritative Domain Specifications.

## Deferred decisions

Future work not defined by this minimum read contract:

- production administrative authentication/authorization;
- Web-originated command/write semantics;
- richer inventory reads;
- collection reads beyond the minimum Endpoint collection, search/filter/sort queries,
  pagination/cursors, and broader HTTP conventions;
- browser update-notification mechanism;
- generation format for opaque Domain identifiers;
- mechanism used by the Server to derive `agent_presence`.

## Validation

Implementation requires at least:

- serialization coverage for every representation/state above;
- Endpoint collection envelope/item serialization, including an empty fleet returning `200`
  with an empty `endpoints` array;
- multiple Endpoints preserving independent identity, credential, presence, hardware-confidence,
  and current-inventory presence/reference facts;
- collection `endpoint_id` values remaining opaque strings, without Presentation fixture-id
  semantics or a Server-authoritative situation/readiness summary;
- RFC 3339 timestamps and opaque identifiers;
- omitted-vs-`null` optional-field behavior;
- unknown-field forward compatibility, including the collection envelope and Endpoint items;
- credential state and `agent_presence` proven independent;
- absent progress represented as omitted;
- unchanged Endpoint/Job detail reads returning `200` for existing and `404` for nonexistent
  resources, with Endpoint detail fields/semantics consistent with collection items;
- correct Job nesting of JobStep, Attempt, progress, and Transfer/Artifact summaries;
- `AwaitingReconciliation`, `Indeterminate`, and every `failure_reason` represented without
  collapsing/substitution;
- verification that an implementation of this contract introduces no Web-originated write route.

General test-layer policy is owned by `docs/development/testing.md`.

## Related

- `m1-simulated-vertical-slice-and-baseline-validation.md` — headless M1 scope, which does not
  require this contract's implementation.
- `m0-endpoint-identity-lifecycle.md` — Endpoint state vocabulary.
- `m0-job-lifecycle-and-scheduling.md` — Job/JobStep/Attempt state vocabulary.
- `m0-agent-protocol-contract.md` — `action_id` and progress semantics.
- `m0-persistence-observability-and-domain-events.md` — identifiers/correlation and transient
  progress boundary.
- `m0-data-plane-and-storage-contracts.md` — Transfer/Artifact state.
- `m0-stack-and-boundaries-baseline.md` — Presentation/API dependency boundary.
