# M1 — Worker Data-Plane Control Contract

Status: **Approved**

This Specification is the authoritative, implementation-language-independent wire contract
for the local Unix Domain Socket (UDS) boundary between `bamepd` and the isolated Worker
process required by ADR-0018. ADR-0001 owns the Worker process-isolation rationale; ADR-0003
owns the Worker language and contract-independence requirement; ADR-0018 owns the accepted
topology (`bamepd` is the UDS server, Worker is the reconnecting UDS client) and the
durable-authority boundary this contract must preserve. This Specification owns only the
normative IPC message shapes, framing, versioning, and failure semantics; it does not
reproduce ADR-0018's WHY/topology rationale.

A participant must remain implementable from this document alone, without reading Worker or
`bamepd` Rust source, per ADR-0003.

## Scope

This contract defines the minimum IPC surface required by
`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005 and by
`m0-data-plane-and-storage-contracts.md`'s "HTTPS data-plane v1 contract":

- handshake / version compatibility;
- per-request authorization for `chunk_upload`;
- durable verified-chunk acceptance coordination;
- resume-discovery durable-state retrieval, including pagination within the frame limit;
- `seal_manifest` first durable commit (`Incomplete -> PendingVerification`);
- full-Artifact verification-result coordination and the authoritative
  `PendingVerification -> Verified | Failed` transition.

It does not define a general-purpose RPC framework, and it does not define every message a
future non-M1 Worker responsibility (for example, compression) may eventually require.

## Protocol version

The current wire `protocol_version` for this contract is **`"1"`**.

The complete message catalog documented here is **Worker Protocol v1** — the first supported
Worker IPC baseline for the MVP. There is no released or supported Worker Protocol below it:
no deployed Worker consumes an earlier version, no compatibility promise covers one, and no
independently deployed old Worker must stay interoperable. An earlier development-time catalog
(handshake/`ProtocolError` plus a single `AuthorizationQuery`/`AuthorizationDecision` pair
partially materialized under #37/#38) was an incomplete in-progress rendering of this same
first protocol, not a separate released generation; it is development history, not a
compatibility baseline, and this Specification carries no artificial `v1 -> v2` history for
it.

`bamepd` MAY accept more than one `protocol_version` once a later version exists (see
"Compatibility and unknown fields"), but the MVP defines and requires only `"1"`. A peer
speaking an incompatible `protocol_version` is rejected at handshake like any other
version mismatch. This is a contract-catalog revision, not an architectural change —
ADR-0018 explicitly leaves "the concrete IPC message catalog" to this Specification — so no
ADR changes.

## Transport, framing, and versioning

- Transport is a local Unix Domain Socket (`AF_UNIX`, stream socket). `bamepd` binds and
  listens; Worker connects as a client and reconnects after disconnect or restart, per
  ADR-0018.
- Trust boundary: this socket is host-local and relies on filesystem/socket permissions, not
  an application-layer credential; it is not exposed over any network transport, and this
  Specification defines no additional authentication for it.
- Framing is explicit and independent of any Rust in-memory representation, per ADR-0003 — no
  Rust enum layout, `bincode`, or other native-serialization framing:

  ```text
  frame := u32be(byte_length_of_json_payload) || utf8_json_payload
  ```

  `byte_length_of_json_payload` is the exact UTF-8 byte length of `utf8_json_payload`, which is
  a single UTF-8-encoded JSON object.
- Maximum frame size is **1 MiB**. Only metadata/control fields cross this boundary; bulk
  chunk bytes never do, and neither do reconstructed full-Artifact bytes
  (`m0-data-plane-and-storage-contracts.md` "HTTPS data-plane v1 contract" — Worker
  stages/verifies bytes locally and reports only digests/sizes/outcomes over UDS). A frame
  declaring a length above the maximum is a protocol violation: the receiver closes the
  connection without attempting to read the oversized payload.
- The one response whose payload can grow with Artifact size — the resume-discovery
  held-chunk set — is **paginated** so every individual frame stays within the 1 MiB limit
  (see "Resume-manifest pagination"). The 1 MiB limit is never raised to fit a resume
  payload.
- Every message is a JSON object carrying at least:

  ```text
  {
    "protocol_version": "1",
    "message_id": "<uuid v4>",
    "type": "<MessageType>",
    ...type-specific fields...
  }
  ```

  A response additionally carries `"in_reply_to": "<message_id of the request>"`.
- `protocol_version` is `"1"` for this contract version. An incompatible version is rejected
  explicitly at handshake (below), never best-effort interpreted.
- Unknown top-level `type` (after a compatible handshake): rejected with `ProtocolError`
  (below). Unknown fields inside an otherwise valid known message type are ignored, for
  forward-compatible minor additions — identical in spirit to `m0-agent-protocol-contract.md`'s
  wire-compatibility rule, so that adding one optional field does not require a synchronized
  Worker/`bamepd` release merely due to serialization rigidity, per ADR-0003.
- Absent optional fields are omitted, never sent as `null`, matching the convention already
  used by `m0-agent-protocol-contract.md`. Where the HTTPS contract requires a JSON `null`
  (for example `expected_chunk_count` before sealing), the IPC message omits the field and
  the Worker renders `null` in the HTTP response.

## Handshake

On every new connection (initial connect and every reconnect), before any other message type
is valid:

```text
Worker -> bamepd: WorkerHello{worker_protocol_version, worker_instance_id}
bamepd -> Worker: ServerHello{server_protocol_version, compatible: bool}
                   | HandshakeRejected{reason}
```

- `worker_protocol_version` and `server_protocol_version` are `"1"` for this contract version.
- `worker_instance_id` is a UUID v4 the Worker generates fresh at process start, identifying
  one Worker process lifetime; it changes across every Worker restart and lets `bamepd`
  recognize a new connection generation.
- `compatible` is `true` only when `bamepd` supports `worker_protocol_version`. `bamepd` MAY
  support more than one version once a later version exists; this contract does not define
  that window, and the MVP requires only `"1"`. For the MVP, a `"1"` Worker against a `"1"`
  `bamepd` is compatible. If a future `"2"` is introduced, a `"1"` Worker against a
  `"2"`-only `bamepd` (one that has dropped `"1"` support) is rejected with
  `HandshakeRejected{ reason: "incompatible_version" }`; there is no such incompatibility to
  handle today because `"2"` does not yet exist.
- `HandshakeRejected{reason}` uses one closed generic value (`"incompatible_version"`); no
  other message is valid on that connection afterward, and `bamepd` closes it. A peer
  speaking a `protocol_version` the other side does not support reaches exactly this
  outcome — a mismatched peer never silently interprets a message type from a version it does
  not support, because the version mismatch is caught first. No such incompatibility exists
  within the MVP, where every participant speaks `"1"`.
- Every message sent before a successful handshake, other than `WorkerHello` itself, is a
  protocol violation (`ProtocolError`, below).
- There is no per-message feature negotiation: a compatible handshake establishes the full
  `protocol_version "1"` catalog for that connection generation.

## Connection generations and correlation

Handshake success starts a new **connection generation**. `bamepd` and Worker each track
outstanding requests only for the current connection generation. A response whose
`in_reply_to` does not match any request outstanding on the current generation — including a
response that arrives after reconnect for a request sent on a prior generation — is discarded
without being applied to any state; this is "stale response" handling under "Failure
semantics" below.

Transient operation handles (below) are also generation-scoped: a handle minted on one
generation is invalid on every later one.

## Operations, HTTP mapping, and transcript inputs

Each of the three `m0-data-plane-and-storage-contracts.md` "HTTPS data-plane v1 contract"
operations maps to one **authorizing request** plus zero or more generation-scoped
**follow-up** messages:

| HTTP operation | authorizing request | follow-up(s) |
| --- | --- | --- |
| `PUT .../chunks/{chunk_index}` (chunk upload) | `AuthorizationQuery` | `ChunkAcceptanceRequest` |
| `GET .../chunks` (resume discovery) | `ResumeDiscoveryQuery` | `ResumeDiscoveryContinue` (per page) |
| `POST .../seal` (seal manifest) | `ManifestSealRequest` | `ArtifactVerificationReport` |

The `operation` value bound into the canonical proof transcript
(`m0-data-plane-and-storage-contracts.md` "Per-request proof": `1 = chunk_upload`,
`2 = resume_discovery`, `3 = seal_manifest`) is **implied by the authorizing request's
`type`**, not carried as a separate wire field — `AuthorizationQuery` is always
`chunk_upload`, `ResumeDiscoveryQuery` is always `resume_discovery`, `ManifestSealRequest` is
always `seal_manifest`.

`bamepd` reconstructs the exact 137-byte proof transcript
(`m0-data-plane-and-storage-contracts.md` "Per-request proof" — the transcript layout is
unchanged) from these input sources:

| Transcript field | Source when `bamepd` reconstructs |
| --- | --- |
| `capability_id` | `SHA-256` of the exact UTF-8 `token` bytes the Worker forwarded |
| `operation` | the authorizing request message `type` (table above) |
| `transfer_id` | the `transfer_id` the Worker forwarded from the HTTP route |
| `artifact_id` | the `artifact_id` **bound to `token`** in the capability `bamepd` itself issued — never a wire value |
| `direction` | the `direction` **bound to `token`** in the capability `bamepd` itself issued — never a wire value |
| `chunk_index` | `AuthorizationQuery` only: the `chunk_index` the Worker forwarded from the HTTP route; absent (`chunk_index_present = 0`) for the other two |
| `proof_id`, `issued_at` | the wire values from the Worker (originally the `X-Bamep-Transfer-Proof` carrier) |

Worker never sends `artifact_id` or `direction` on this boundary and never needs them: they
are not present in the HTTP route or common headers, and the `token` is opaque to Worker
(`m0-data-plane-and-storage-contracts.md` "Capability opacity"). The Agent still signs
`artifact_id` and `direction` into its transcript exactly as before; because `bamepd`
reconstructs those two fields from the capability binding rather than from anything the
request could assert, a proof that signed a different `artifact_id`/`direction` than the
`token` is bound to simply fails signature verification and is denied with the generic
non-enumerable denial — cross-Artifact / cross-direction substitution stays fail-closed, and
proof binding is not weakened.

## Minimum messages

Every request/response pair below correlates via `in_reply_to`. Implementations must use these
exact wire `type` names and field names.

### 1. Chunk-upload authorization (`AuthorizationQuery` / `AuthorizationDecision`)

The pre-body authorization for `PUT .../chunks/{chunk_index}`.

```text
Worker -> bamepd: AuthorizationQuery{
    token,          // opaque capability, forwarded exactly as received
    transfer_id,    // from the HTTP route
    chunk_index,    // from the HTTP route (non-negative integer)
    proof_id,       // from the X-Bamep-Transfer-Proof carrier
    issued_at,      // from the X-Bamep-Transfer-Proof carrier
    signature       // from the X-Bamep-Transfer-Proof carrier
}
bamepd -> Worker: AuthorizationDecision{
    decision: "approved" | "denied",
    // all of the following are present only when decision = "approved":
    digest_algorithm,          // e.g. "sha256" — the Transfer manifest's fixed algorithm
    chunk_size,                // positive integer bytes — the Transfer manifest's fixed chunk size
    acceptance_handle,         // transient, generation-scoped; see "Transient operation handles"
    expected_chunk_digest?     // additionally present only when chunk_index is already durable:
                               // the already-recorded expected digest for that chunk_index,
                               // canonical base64url-no-pad
}
```

`bamepd` performs the complete authoritative check for `operation = chunk_upload`: signature
validity over the reconstructed transcript, capability validity/expiry/scope, proof freshness,
replay, and current durable authorization (owning Attempt still `InProgress`, credential
`CredentialActive`, Artifact/manifest state permits this chunk). Worker MAY perform a local
mechanical pre-check (for example, rejecting an obviously malformed signature encoding before
spending a round trip), but such a local check is never sufficient authorization by itself and
never substitutes for this query, per ADR-0018.

On `approved`, `digest_algorithm` and `chunk_size` are the authoritative durable manifest
facts the Worker MUST use — to enforce the `413 CHUNK_TOO_LARGE` body bound before reading
the body, and to compute the chunk digest with the correct algorithm — never inferred from a
Worker constant. On `denied`, `AuthorizationDecision` carries `decision` only: no
`digest_algorithm`, `chunk_size`, `acceptance_handle`, or `expected_chunk_digest`, so it
leaks nothing beyond the generic non-enumerable denial. The Worker maps `denied` to the
fixed `401` body and, for a declared-vs-`expected_chunk_digest` mismatch, to
`409 CHUNK_IDENTITY_CONFLICT` before reading the body
(`m0-data-plane-and-storage-contracts.md` "Durable chunk acceptance ordering").

### 2. Verified-chunk durable acceptance (`ChunkAcceptanceRequest` / `ChunkAcceptanceDecision`)

Sent only after Worker has itself received the body and verified the bytes hash to `digest`
with the `digest_algorithm` from the `AuthorizationDecision`
(`m0-data-plane-and-storage-contracts.md` "Durable chunk acceptance ordering" step 6).

```text
Worker -> bamepd: ChunkAcceptanceRequest{
    acceptance_handle,   // exactly the value from the approving AuthorizationDecision
    transfer_id,
    chunk_index,
    digest,              // canonical base64url-no-pad; the Worker-verified actual chunk digest
    size                 // exact count of raw verified chunk bytes (see below)
}
bamepd -> Worker: ChunkAcceptanceDecision{
    outcome: "committed" | "already_committed" | "rejected",
    reason?              // present only when outcome = "rejected"; closed vocabulary below
}
```

`acceptance_handle` binds this durable-commit request to the specific just-authorized
`chunk_upload`: `bamepd` rejects a `ChunkAcceptanceRequest` whose handle it did not mint on
the current generation, or whose `transfer_id`/`chunk_index` do not match the handle's
authorized operation. `bamepd` still independently re-validates current durable state at
commit time (Transfer/Artifact/Attempt eligibility, manifest compatibility, no conflicting
existing identity) — the handle does not replace that check.

`size` is the exact number of raw verified chunk bytes the Worker received and hashed. It has
already been bounded by the `AuthorizationDecision`'s `chunk_size` (plus any bounded Worker
margin) before the body was accepted; `digest` is over exactly those `size` bytes. The Worker
MUST NOT use a client-declared `Content-Length` that disagrees with the bytes actually
received as `size`. `bamepd` durably records/revalidates `size` against current manifest
constraints as part of the commit.

Outcomes:

- `committed` — `bamepd` durably committed the chunk identity `(transfer_id, chunk_index,
  digest)` as first-writer for a new `chunk_index`. HTTP `201 { "chunk_index": N, "status":
  "accepted" }`.
- `already_committed` — `bamepd` recognized an identical already-durable
  `(transfer_id, chunk_index, digest)` and did not re-commit. HTTP `200 { "chunk_index": N,
  "status": "already_held" }`. Only `bamepd` durable authority establishes this; a
  Worker-local staged file never does.
- `rejected` with `reason`, a closed vocabulary the Worker maps deterministically to one HTTP
  `409` shape, with no Worker inference from `bamepd` internals:

  | `reason` | HTTP response |
  | --- | --- |
  | `chunk_identity_conflict` | `409 { "error": { "code": "CHUNK_IDENTITY_CONFLICT" } }` — a *different* digest is already durable for that `chunk_index` (for example a race that committed another digest after this request was authorized) |
  | `transfer_not_continuable` | `409 { "error": { "code": "TRANSFER_NOT_CONTINUABLE" } }` — the owning Transfer/Artifact/Attempt is already terminal, or the manifest is already sealed and `chunk_index` was never part of the sealed set |

  `DIGEST_MISMATCH` and `CHUNK_TOO_LARGE` are never `ChunkAcceptanceDecision` outcomes: the
  Worker detects both locally (mismatch after hashing the body; oversize from `chunk_size`
  before the body) and never reaches this message.

### 3. Resume-discovery authorization and first page (`ResumeDiscoveryQuery` / `ResumeDiscoveryPage`)

The authorizing request for `GET .../chunks`. It authorizes `operation = resume_discovery`
and returns the first page of durable resume state in one round trip.

```text
Worker -> bamepd: ResumeDiscoveryQuery{
    token,          // opaque capability, forwarded exactly as received
    transfer_id,    // from the HTTP route
    proof_id,       // from the X-Bamep-Transfer-Proof carrier
    issued_at,      // from the X-Bamep-Transfer-Proof carrier
    signature       // from the X-Bamep-Transfer-Proof carrier
}
bamepd -> Worker: ResumeDiscoveryPage{
    decision: "approved" | "denied",
    // all of the following are present only when decision = "approved":
    transfer_id,
    sealed: bool,
    digest_algorithm,          // e.g. "sha256"
    chunk_size,                // positive integer bytes
    expected_chunk_count?,     // present only when sealed; omitted before sealing
                               //   (Worker renders JSON null in the HTTP response)
    held_chunks: [ { chunk_index, digest }, ... ],   // this page's slice, ascending chunk_index
    resume_cursor?             // present iff more held-chunk pages remain
}
```

`bamepd` performs the complete authoritative check for `operation = resume_discovery`
(identical discipline to `AuthorizationQuery`). On `denied`, `ResumeDiscoveryPage` carries
`decision` only.

`held_chunks` reflects **only** chunk identities `bamepd` durably holds and has individually
verified — never Worker-local staged-but-uncommitted bytes — so it stays correct after HTTP
connection loss, Worker restart, authorization renewal, Agent reconnect, or `bamepd` restart
(`m0-data-plane-and-storage-contracts.md` "Resume discovery"). Each page's `held_chunks` is
ordered by ascending `chunk_index` and the page set contains no omitted or duplicated durable
chunk identity for chunks durable at authorization time; a chunk accepted *after* that instant
MAY be absent, which is safe because re-submitting an already-held chunk is idempotent
(`200 already_held`).

### 4. Resume-discovery pagination (`ResumeDiscoveryContinue` / `ResumeDiscoveryPage`)

```text
Worker -> bamepd: ResumeDiscoveryContinue{
    resume_cursor   // exactly the value from the previous ResumeDiscoveryPage
}
bamepd -> Worker: ResumeDiscoveryPage{
    decision: "approved" | "denied",
    held_chunks?: [ { chunk_index, digest }, ... ],   // next slice, ascending
    resume_cursor?                                     // present iff still more pages remain
}
```

Continuation pages carry `held_chunks` and `resume_cursor` only; the manifest-level fields
(`sealed`, `digest_algorithm`, `chunk_size`, `expected_chunk_count`) appear on the first page
and the Worker reuses them. `bamepd` serves every page of one resume query from a consistent
durable snapshot taken at authorization time. A `resume_cursor` `bamepd` does not recognize on
the current generation (stale, wrong generation, already consumed) returns
`ResumeDiscoveryPage{ decision: "denied" }`; the Worker then fails the HTTP request closed
(below) and discards any partial `held_chunks` it had aggregated.

See "Resume-manifest pagination" for the frame-limit reasoning and cursor semantics.

### 5. Seal-manifest first durable commit (`ManifestSealRequest` / `ManifestSealDecision`)

The authorizing request for `POST .../seal`. It authorizes `operation = seal_manifest` and,
on success, performs the first durable commit (`Incomplete -> PendingVerification`) as one
transaction (`m0-data-plane-and-storage-contracts.md` "Durable chunk acceptance ordering").

```text
Worker -> bamepd: ManifestSealRequest{
    token,             // opaque capability, forwarded exactly as received
    transfer_id,       // from the HTTP route
    proof_id,          // from the X-Bamep-Transfer-Proof carrier
    issued_at,         // from the X-Bamep-Transfer-Proof carrier
    signature,         // from the X-Bamep-Transfer-Proof carrier
    chunk_count,       // Agent-declared, from the HTTP request body (non-negative integer)
    artifact_digest    // Agent-declared, from the HTTP request body (canonical base64url-no-pad)
}
bamepd -> Worker: ManifestSealDecision{
    outcome: "sealed" | "already_pending_verification" | "rejected" | "denied",
    reason?,                    // present only when outcome = "rejected"; closed vocabulary below
    // the following are present only when outcome ∈ {"sealed", "already_pending_verification"}:
    verification_handle,        // transient, generation-scoped; see "Transient operation handles"
    artifact_id,               // the durable Artifact owned by transfer_id; opaque response
                               //   data the Worker needs for the HTTP seal response, not
                               //   independent authority
    digest_algorithm,          // e.g. "sha256"
    chunk_size,                // positive integer bytes
    chunk_count,               // the authoritative durable sealed chunk_count
    expected_artifact_digest   // the authoritative durable sealed full-Artifact digest,
                               //   canonical base64url-no-pad
}
```

Outcomes:

- `sealed` — first valid seal: `bamepd` verified every chunk index `0..chunk_count-1` is
  durably held and individually verified, sealed the manifest with
  `(chunk_count, artifact_digest)`, and committed `Incomplete -> PendingVerification` in one
  transaction. The Worker proceeds to full-Artifact verification.
- `already_pending_verification` — idempotent retry: the manifest is already sealed with an
  identical `(transfer_id, chunk_count, artifact_digest)` and the Artifact is already
  `PendingVerification`. No second seal transaction. The Worker re-drives full-Artifact
  verification (this is how the Agent safely retries after a Worker crash between sealing and
  verification completing).
- `rejected` with `reason`, a closed vocabulary the Worker maps deterministically:

  | `reason` | HTTP response |
  | --- | --- |
  | `incomplete_manifest` | `409 { "error": { "code": "INCOMPLETE_MANIFEST" } }` — `bamepd` does not durably hold every chunk index `0..chunk_count-1` individually verified |
  | `manifest_already_sealed` | `409 { "error": { "code": "MANIFEST_ALREADY_SEALED" } }` — the manifest is already sealed with a *different* `chunk_count` or `artifact_digest` |

- `denied` — the authorization check failed; the message carries `outcome` only and nothing
  else. HTTP `401`, generic non-enumerable. An owning Transfer/Artifact/Attempt that is
  already terminal is an authorization denial here (`denied`), not a `409`, matching
  `m0-data-plane-and-storage-contracts.md`, which lists no `TRANSFER_NOT_CONTINUABLE` shape
  for `seal_manifest`.

On `sealed`/`already_pending_verification`, `expected_artifact_digest` and `chunk_count` are
the **authoritative durable sealed values**. The Worker MUST verify against these, never
against the values it sent in the HTTP body — which matters especially for an idempotent
retry, where the durable sealed values are authoritative and the request body is only an
idempotency assertion.

`artifact_id` is the exact durable Artifact `bamepd` bound to `transfer_id`; it is not on the
HTTP route or in any header, and the `token` is opaque, so this decision is the Worker's only
source for the `artifact_id` field of the HTTP seal response
(`m0-data-plane-and-storage-contracts.md` "HTTPS data-plane v1 contract" operation 3). For
`already_pending_verification`, `bamepd` MUST return the same authoritative durable
`artifact_id` as the original `sealed` decision, so a Worker restart followed by an idempotent
seal retry can reconstruct the exact final HTTP response with no pre-restart transient state.
The Worker holds this `artifact_id` for the lifetime of that one HTTP operation and treats it
as opaque response data, never as independent authority (`ArtifactVerificationAck` does not
repeat it).

### 6. Full-Artifact verification result (`ArtifactVerificationReport` / `ArtifactVerificationAck`)

Sent once, after `bamepd` has already durably committed `Incomplete -> PendingVerification`
(outcome `sealed` or `already_pending_verification` above) and the Worker has reconstructed
the full Artifact byte stream and computed its digest
(`m0-data-plane-and-storage-contracts.md` "Full-Artifact byte reconstruction").

```text
Worker -> bamepd: ArtifactVerificationReport{
    verification_handle,        // exactly the value from the ManifestSealDecision
    computed_artifact_digest    // canonical base64url-no-pad; the digest the Worker computed
                                //   over the reconstructed full-Artifact byte stream
}
bamepd -> Worker: ArtifactVerificationAck{
    outcome: "committed",
    artifact_status: "Verified" | "Failed"
}
```

`verification_handle` binds this report to the specific just-committed
`Incomplete -> PendingVerification`. The Worker reports only the **mechanical fact** it
observed — the digest it computed. `bamepd` **independently** compares
`computed_artifact_digest` against its own durable `expected_artifact_digest` and commits
`PendingVerification -> Verified` on a match or `PendingVerification -> Failed` on a mismatch.
The Worker carries no verdict field and cannot establish `Verified` by assertion — only
`bamepd`'s own comparison against its durable expected digest decides.
`ArtifactVerificationAck` returns the authoritative committed `artifact_status`, which the
Worker renders in the HTTP
seal response `{ "transfer_id", "artifact_id", "sealed": true, "artifact_status": "Verified" |
"Failed" }`. `Failed` is an HTTP `200` (a completed operation that produced a `Failed`
Artifact), not a `409` (`m0-data-plane-and-storage-contracts.md` "HTTPS data-plane v1
contract" operation 3).

Once `bamepd` commits, it is responsible for the owning Agent Protocol `ActionResult` on the
separate WSS control connection (`m1-simulated-vertical-slice-and-baseline-validation.md`
RF-005) — this message never itself produces an Agent Protocol effect, preserving that the
Agent remains the sole Agent Protocol participant and Worker never emits `ActionResult`
directly.

If the Worker cannot complete verification within the HTTP request — it cannot access the
accepted chunk bytes it staged (for example after a restart that lost staging), or UDS is
lost before the `Ack` — it does not send a fabricated `Ack`: the HTTP request fails closed
with the generic `401`-shaped response, the durable Artifact remains `PendingVerification`,
and a later idempotent seal retry re-drives verification.

### 7. Generic protocol error (`ProtocolError`)

```text
{ "type": "ProtocolError", "code": string, "message"?: string, "in_reply_to"?: string }
```

Used for a malformed frame/message, an unknown `type` (after a compatible handshake), a
pre-handshake violation, or an oversized-frame violation observed before the connection is
closed. `code` is a stable non-empty diagnostic string; `message`, when present, must not
expose secrets (see "Security and logging"). Whether the connection is closed after sending
`ProtocolError` is implementation policy for this contract, unless a future safety requirement
states otherwise — mirroring `m0-agent-protocol-contract.md`'s equivalent rule for its own
`ProtocolError`.

## Transient operation handles

`acceptance_handle`, `resume_cursor`, and `verification_handle` share one definition:

- opaque to the Worker; the Worker echoes the exact value back and never parses it;
- minted by `bamepd` in the response to an authorizing request, on one connection generation;
- bound to exactly one authorized data-plane operation instance (one `transfer_id`, one
  authorized `proof_id`, and — for `acceptance_handle` — one `chunk_index`);
- valid only on the generation that minted it; discarded on disconnect and never honoured on
  a later generation;
- consumed by its follow-up message(s): `acceptance_handle` and `verification_handle` are
  single-use; `resume_cursor` advances one page per `ResumeDiscoveryContinue` and each value
  is accepted once;
- **never a durable business identity.** A handle is not persisted, is not an idempotency key,
  and never authorizes anything on its own — it correlates a follow-up message to an
  operation `bamepd` already authorized this generation, and `bamepd` still independently
  validates current durable state on every durable commit.

A retried logical operation (fresh `proof_id`) obtains a fresh authorizing decision and fresh
handles; it never reuses handles from a prior attempt or generation.

## Resume-manifest pagination

`ResumeDiscoveryPage.held_chunks` is the only response payload that grows with Artifact size.
Each entry is `{ "chunk_index": <integer>, "digest": "<43-char base64url>" }` ≈ 60–70 UTF-8
bytes. A realistic M1 Volume/Image capture (a disk image of tens to hundreds of GiB at, for
example, the 4 MiB experimental chunk size from
`docs/reference/transfer-resumability-spike.md`) produces tens of thousands to hundreds of
thousands of held-chunk entries — well over the 1 MiB frame limit for a single frame. The M1
contract does not bound one Transfer's resume payload below 1 MiB, so this response is
paginated rather than raising the universal frame limit.

- `bamepd` chooses a bounded page size such that every `ResumeDiscoveryPage` frame — including
  its non-`held_chunks` fields on the first page — stays safely within 1 MiB.
- Pages are strictly ordered by ascending `chunk_index`. Semantically the cursor represents
  "the highest `chunk_index` already returned"; `bamepd` MAY encode it opaquely, but the next
  page always begins at the next higher held `chunk_index` with no gap and no repeat.
- `resume_cursor` is present on a `ResumeDiscoveryPage` iff at least one more held chunk
  remains; its absence means the aggregate is complete.
- Every page is correlated to the current connection generation like any other response.
- The Worker aggregates pages in order into the single HTTP `200` response body. If any page
  cannot be obtained — disconnect, stale generation, `bamepd` returns
  `ResumeDiscoveryPage{ decision: "denied" }` for the cursor — the Worker abandons the
  partial aggregate and returns the generic `401`-shaped fail-closed response. It never
  returns a partial `held_chunks` list as if it were complete.

If a future M1 decision bounds one Transfer's held-chunk count such that the whole set
provably fits one frame, this pagination degenerates to a single page (`resume_cursor` never
present) with no contract change.

## Authority

Every message above preserves ADR-0018's authority split: Worker only reports observed
mechanical facts (bytes verified, digest computed, size received) or requests a decision;
`bamepd` is the only participant that decides authorization, durable chunk acceptance,
manifest sealing, and Artifact lifecycle transitions. The transient handles do not give the
Worker authority — `bamepd` independently re-validates current durable state on every commit,
and a handle only ties a follow-up message to an operation `bamepd` already authorized this
generation. No message in this catalog lets Worker submit a general database mutation,
arbitrary durable-state query, or independent business decision: `ResumeDiscoveryQuery` is not
a generic PostgreSQL read — it is one authorization-bound retrieval of one authorized
Transfer's durable resume state. Worker holds no PostgreSQL repository Adapter and this
contract gives it no path to one.

## Failure semantics

- **UDS unavailable (no connection):** fail-closed. Worker must not authorize new work from
  stale local assumptions, must not fabricate any `AuthorizationDecision`,
  `ChunkAcceptanceDecision`, `ResumeDiscoveryPage`, `ManifestSealDecision`, or
  `ArtifactVerificationAck` outcome locally, and must not independently advance any Domain
  state. Every HTTP request that would otherwise require an authorizing request instead
  receives the same generic `401` denial defined by `m0-data-plane-and-storage-contracts.md`,
  identical in shape to an ordinary authorization denial — a distinguishable "try again later"
  response is deliberately not defined, since it would leak Worker/`bamepd` operational state
  to an unauthenticated observer without helping a legitimate Agent, whose
  retry/reconnection behavior is already governed by the Job lifecycle contract rather than
  by HTTP error text.
- **Disconnect with a request in flight:** the outstanding request is treated as
  failed/uncertain, never as success. Worker must not report HTTP success for an operation
  whose authorizing decision, `ChunkAcceptanceDecision`, `ResumeDiscoveryPage`,
  `ManifestSealDecision`, or `ArtifactVerificationAck` it never received. Worker reconnects
  and, if the operation is retried, sends a fresh authorizing request (fresh `message_id`,
  fresh `proof_id`); durable idempotency is governed by the identities in "Idempotency
  identities", never by UDS `message_id` or by a transient handle.
- **Disconnect during resume pagination:** the HTTP `GET .../chunks` fails closed with the
  generic `401`-shaped response; any partial `held_chunks` aggregate is discarded. A fresh
  request with a fresh proof restarts pagination from the first page.
- **Disconnect after `ManifestSealDecision{sealed}` but before `ArtifactVerificationAck`:**
  the Artifact durably remains `PendingVerification` — never falsely `Verified`, never lost
  back to `Incomplete`. The HTTP `POST .../seal` fails closed; an idempotent seal retry
  (`already_pending_verification`) re-drives verification.
- **Duplicate authorizing request after reconnect:**
  - `ChunkAcceptanceRequest` — `bamepd` recognizes an already-committed identical
    `(transfer_id, chunk_index, digest)` and returns `already_committed`, never
    double-committing.
  - `ManifestSealRequest` — `bamepd` recognizes an identical already-sealed
    `(transfer_id, chunk_count, artifact_digest)` and returns
    `already_pending_verification`, never double-sealing.
  - `ResumeDiscoveryQuery` — idempotent by nature (read-only); a fresh proof simply
    re-authorizes and re-reads current durable state.
- **Stale response / unknown correlation:** a response whose `in_reply_to` does not match an
  outstanding request on the current connection generation, or a follow-up carrying a handle
  or cursor from a prior generation, is discarded and never applied to any state or surfaced
  to an HTTP client, per "Connection generations and correlation".
- **Incompatible protocol version:** rejected explicitly at handshake (`HandshakeRejected`);
  no further message is processed on that connection. Worker retries reconnect per its own
  backoff policy, which is implementation-time.
- **Malformed frame/message:** the receiver sends `ProtocolError` where the frame was at
  least parseable enough to identify a correlation target, and otherwise simply closes the
  connection; the peer reconnects and re-handshakes. Given this boundary is fully internal
  and host-local, this Specification does not require the more elaborate degraded-operation
  behavior Agent Protocol defines for its externally-facing connection.
- Whether Worker stays alive and keeps reconnecting, or self-terminates and relies on
  `bamepd` supervision (ADR-0018) to respawn it, after prolonged UDS loss remains
  implementation-time; the invariant that must hold regardless of that choice is fail-closed
  loss of authority, per ADR-0018.

## Idempotency identities

Kept distinct, per `m0-data-plane-and-storage-contracts.md`'s equivalent rule for its own
identities:

| Identity | Scope | Idempotency meaning |
| --- | --- | --- |
| UDS `message_id` | one message transmission on one connection generation | none; fresh per send, never a durable idempotency key |
| UDS connection generation (`worker_instance_id` + handshake) | one Worker process lifetime/connection | scopes which outstanding requests/responses and transient handles are still valid |
| `acceptance_handle` / `resume_cursor` / `verification_handle` | one authorized operation instance on one connection generation | correlates a follow-up message to an already-authorized operation; **never** a durable identity, never persisted, discarded on disconnect |
| `transfer_id` | durable logical Transfer | correlates every message about one Transfer; not itself sufficient for chunk-level idempotency |
| `artifact_id` | durable logical Artifact | correlation identity `bamepd` resolves from the Transfer / capability binding; not carried on this boundary |
| capability identity (`SHA-256(token)`) | one issued capability instance | identifies which capability a proof is bound to; changes on renewal, is not a business idempotency key |
| `proof_id` | one HTTP request attempt | single-use anti-replay only; a retried operation mints a fresh `proof_id`, per `m0-data-plane-and-storage-contracts.md` "Idempotent retry is not proof reuse" |
| `(transfer_id, chunk_index, digest)` | one durable chunk identity | **the** idempotency key for `ChunkAcceptanceRequest`/durable chunk acceptance; a repeated request with this identical triple is always safe |
| `(transfer_id, chunk_count, artifact_digest)` | one sealed manifest | **the** idempotency key for a repeated `ManifestSealRequest` once already sealed with identical declared values |

Collapsing any of these — for example, treating a fresh `proof_id` as proof that a chunk must
be new, treating UDS `message_id` as a durable acceptance key, or treating a transient handle
as a durable idempotency key — would either falsely reject a legitimate idempotent retry or
falsely admit a replay; neither is acceptable.

## Compatibility and unknown fields

### Version lifecycle

- Before a `protocol_version` has become an implemented supported baseline, corrections needed
  to complete its first functional contract — including materially new message types, new
  required fields, or a change to which participant authoritatively supplies a required
  value — may be incorporated without incrementing `protocol_version`. The contract is still
  being brought to its first complete form.
- Once a `protocol_version` has become an implemented supported baseline, any incompatible
  wire change requires a `protocol_version` increment. After that freeze point, all of the
  following require a new protocol version: materially new message types; removing existing
  fields; changing the meaning of a required field; adding a new required field; changing
  which participant authoritatively supplies a required value; any other incompatible wire
  semantics.
- A compatible addition — an optional field the unaware peer can safely ignore — may be made
  within the same `protocol_version` at any time, per the forward-compatibility rule below.
  This is not an elaborate SemVer scheme: there is one `protocol_version` string, incremented
  only on an incompatible change to a frozen baseline.

### Freeze point for v1

- The complete contract documented here is the **Worker Protocol v1 MVP baseline**. Its first
  production implementation is being delivered by #39.
- Until #39 establishes this complete catalog as the implemented baseline, `protocol_version`
  stays `"1"` even as the contract is corrected to its first complete functional form — the
  earlier partial #37/#38 rendering never constituted a supported baseline that a later
  change would break.
- Once #39 establishes this v1 protocol as the implemented supported baseline, any future
  incompatible Worker IPC change requires `protocol_version = "2"`. "Implemented supported
  baseline" — not any customer-release milestone — is the boundary that freezes v1.

### Forward compatibility

- Adding an optional field to a message in this catalog, where the unaware peer can safely
  ignore it, is a forward-compatible minor change within the current `protocol_version`.
- A materially new message *type*, a new required field, or a change to which participant
  authoritatively derives a value is — once the baseline is frozen — a `protocol_version`
  change, handled explicitly at handshake rather than silently negotiated per-message.
- The optional-field allowance must not be used to overload `AuthorizationQuery`/
  `AuthorizationDecision` (or any authorizing request) with unrelated durable-mutation or
  generic-query semantics merely to avoid a version bump: authorization stays authorization,
  durable seal stays an explicit `ManifestSealRequest`, and resume-state retrieval stays an
  explicit authorization-bound `ResumeDiscoveryQuery`.
- Worker ships with and is released alongside `bamepd` (ADR-0018), but this contract remains
  independently specified so a participant does not need synchronized Rust releases merely to
  add an optional field, and so a future non-Rust participant remains implementable from this
  document alone, per ADR-0003.

## Security and logging

- The opaque `token` value, `proof_id`/`issued_at`/`signature` proof material, the Ed25519
  private proof key (which never leaves the Agent and never crosses this boundary at all), and
  the Server TLS private key (which never crosses this boundary either, per ADR-0018) MUST be
  redacted from logs and debug output wherever this contract's messages are logged.
- Transient handles (`acceptance_handle`, `resume_cursor`, `verification_handle`) are not
  long-lived secrets, but carry no diagnostic value and SHOULD be redacted or abbreviated in
  logs.
- Chunk digests, the full-Artifact digest, and chunk sizes are integrity identities, not
  secrets, and may appear in diagnostics. `proof_public_key` and its thumbprint are not
  secret and may appear in diagnostics. Raw chunk bytes and reconstructed Artifact bytes
  never cross this boundary and are never logged.
- A `denied` outcome on any authorizing request (`AuthorizationDecision`,
  `ResumeDiscoveryPage`, `ManifestSealDecision`) carries no reason field precisely so that no
  log/diagnostic derived from this contract can casually leak a more specific internal
  authorization reason (unknown transfer, wrong Endpoint, terminal Attempt, revoked
  credential, replay, wrong proof key, or another internal cause) into a Worker-side
  observable surface. The `rejected` reasons that *do* exist
  (`chunk_identity_conflict`, `transfer_not_continuable`, `incomplete_manifest`,
  `manifest_already_sealed`) are post-authorization semantic conflicts that
  `m0-data-plane-and-storage-contracts.md` already exposes as distinct HTTP `409` codes, not
  authorization-enumeration reasons. Precise internal authorization diagnostics may still
  exist inside `bamepd` per its own observability policy
  (`m0-persistence-observability-and-domain-events.md`); this contract does not carry them to
  Worker.

## Out of scope

- a general-purpose RPC/service framework beyond the messages above;
- compression, retention, or future Worker responsibilities not required by M1 RF-005;
- the concrete host-local mechanism by which Worker obtains Server TLS key material (ADR-0018
  requires it not cross this IPC protocol, but the concrete provisioning mechanism is
  implementation-time);
- the concrete opaque encoding of transient handles (`bamepd` implementation choice, subject
  only to the properties in "Transient operation handles");
- process supervision/respawn mechanics (ADR-0018 topology, not this wire contract);
- PostgreSQL schema and query mechanics;
- the HTTPS data-plane request/response contract itself (owned by
  `m0-data-plane-and-storage-contracts.md`).

## Validation

At minimum:

- handshake success on matching `protocol_version "1"`, incompatible-version rejection for a
  peer offering any other `protocol_version`, and the pre-handshake protocol violation for any
  other message type;
- frame length-prefix parsing, including the oversized-frame rejection boundary;
- unknown `type` rejection (after a compatible handshake) and unknown-field forward
  compatibility;
- `AuthorizationQuery`/`AuthorizationDecision` for approved and denied cases, including that
  `approved` carries `digest_algorithm`/`chunk_size`/`acceptance_handle` and `denied` carries
  none of them and no reason;
- proof-transcript reconstruction using `artifact_id`/`direction` from the capability binding,
  including that a proof signed over a different `artifact_id`/`direction` fails closed;
- `ChunkAcceptanceRequest` for new-identity commit, idempotent `already_committed` replay,
  and each `rejected` reason mapping to its HTTP `409` code; a `ChunkAcceptanceRequest`
  carrying a foreign/stale `acceptance_handle` rejected;
- `ResumeDiscoveryQuery`/`ResumeDiscoveryPage` returning only durable held chunks, never
  Worker-local staged bytes; approved vs denied; `expected_chunk_count` omitted before
  sealing and present after;
- resume pagination: multi-page aggregation, ascending order with no gap/repeat, every frame
  within 1 MiB, stale-cursor denial, and disconnect-during-pagination failing the HTTP
  request closed with no partial list;
- `ManifestSealRequest`/`ManifestSealDecision` for first `sealed`, idempotent
  `already_pending_verification`, `incomplete_manifest`, `manifest_already_sealed`, and
  `denied` for a terminal owning Attempt; authoritative `artifact_id`/`expected_artifact_digest`/
  `chunk_count` returned on success, with `already_pending_verification` returning the same
  `artifact_id` as the original `sealed` decision so a restart + retry rebuilds the exact
  HTTP seal response;
- `ArtifactVerificationReport`/`ArtifactVerificationAck`: `bamepd` compares
  `computed_artifact_digest` to its own durable expected value and returns the authoritative
  `artifact_status`; a report cannot drive `Verified` by assertion;
- UDS disconnect with a request in flight never producing a fabricated success, for every
  authorizing request and follow-up;
- disconnect after the seal first-commit leaving the Artifact durably `PendingVerification`;
- stale response / stale handle / stale cursor discarded without effect;
- reconnect after Worker restart completing a fresh handshake and connection generation, and
  every prior-generation handle/cursor rejected;
- fail-closed HTTP behavior (generic `401`) while UDS is unavailable, for every operation.

Contract tests exercise the real framing/message shapes; Simulator-level scenarios exercising
this boundary must not bypass it through in-process shortcuts, consistent with
`m0-simulator-contract-and-validation-strategy.md`'s general Agent Protocol fidelity
requirement applied to this analogous internal boundary.

## Related

- ADR-0001 — Worker process isolation.
- ADR-0003 — Worker/Agent language and contract-independence requirement.
- ADR-0008 — data-plane transport/chunking/resumability rationale.
- ADR-0018 — isolated Worker data-plane process boundary; UDS direction and durable-authority
  split this contract implements, and which explicitly leaves the concrete IPC message
  catalog (including its versioning) to this Specification.
- `m0-data-plane-and-storage-contracts.md` — HTTPS data-plane v1 contract, proof
  canonicalization, full-Artifact byte reconstruction, and durable chunk-acceptance ordering
  this IPC contract serves.
- `m0-agent-protocol-contract.md` — the separate Agent Protocol boundary `bamepd` alone
  remains responsible for; Worker never emits Agent Protocol messages.
- `m0-persistence-observability-and-domain-events.md` — durable state/audit authority
  remaining exclusively in `bamepd`.
- `m1-simulated-vertical-slice-and-baseline-validation.md` — RF-005, the concrete M1 action
  this contract's messages ultimately serve.
- `docs/reference/worker-data-plane-composition-spike.md` — empirical evidence for the Worker
  process/listener/IPC composition this contract's framing choices build on.
- `docs/reference/transfer-resumability-spike.md` — empirical evidence for chunked
  reconstruction and the held-chunk-set scale that motivates resume pagination.
