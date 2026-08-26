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
`m0-data-plane-and-storage-contracts.md`'s "HTTPS data-plane v1 contract": handshake/version
compatibility, per-request authorization decisions, durable chunk-acceptance coordination, and
Artifact-verification-result coordination. It does not define a general-purpose RPC framework,
and it does not define every message a future non-M1 Worker responsibility (for example,
compression) may eventually require.

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
  chunk bytes never do (`m0-data-plane-and-storage-contracts.md` "HTTPS data-plane v1
  contract" — Worker stages/verifies bytes locally and reports only digests/sizes/outcomes
  over UDS). A frame declaring a length above the maximum is a protocol violation: the receiver
  closes the connection without attempting to read the oversized payload.
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
- Unknown top-level `type`: rejected with `ProtocolError` (below). Unknown fields inside an
  otherwise valid known message type are ignored, for forward-compatible minor additions —
  identical in spirit to `m0-agent-protocol-contract.md`'s wire-compatibility rule, so that
  adding one optional field does not require a synchronized Worker/`bamepd` release merely due
  to serialization rigidity, per ADR-0003.
- Absent optional fields are omitted, never sent as `null`, matching the convention already
  used by `m0-agent-protocol-contract.md`.

## Handshake

On every new connection (initial connect and every reconnect), before any other message type
is valid:

```text
Worker -> bamepd: WorkerHello{worker_protocol_version, worker_instance_id}
bamepd -> Worker: ServerHello{server_protocol_version, compatible: bool}
                   | HandshakeRejected{reason}
```

- `worker_instance_id` is a UUID v4 the Worker generates fresh at process start, identifying
  one Worker process lifetime; it changes across every Worker restart and lets `bamepd`
  recognize a new connection generation.
- `compatible` is `true` only when `bamepd` supports `worker_protocol_version`. `bamepd` MAY
  support more than one version for a bounded compatibility window; this contract does not
  define that window.
- `HandshakeRejected{reason}` uses one closed generic value (`"incompatible_version"`); no
  other message is valid on that connection afterward, and `bamepd` closes it.
- Every message sent before a successful handshake, other than `WorkerHello` itself, is a
  protocol violation (`ProtocolError`, below).

## Connection generations and correlation

Handshake success starts a new **connection generation**. `bamepd` and Worker each track
outstanding requests only for the current connection generation. A response whose
`in_reply_to` does not match any request outstanding on the current generation — including a
response that arrives after reconnect for a request sent on a prior generation — is discarded
without being applied to any state; this is "stale response" handling under "Failure
semantics" below.

## Minimum messages

Every request/response pair below correlates via `in_reply_to`. Field names are illustrative;
implementations must use these exact wire names.

### 1. Authorization query / decision

```text
Worker -> bamepd: AuthorizationQuery{
    token,                     // opaque capability, forwarded exactly as received
    operation,                 // "chunk_upload" | "resume_discovery" | "seal_manifest"
    transfer_id,
    artifact_id,
    direction,
    chunk_index?,              // present only for chunk_upload
    proof_id,
    issued_at,
    signature
}
bamepd -> Worker: AuthorizationDecision{
    decision: "approved" | "denied",
    expected_chunk_digest?     // present only when decision=approved, operation=chunk_upload,
                                // and chunk_index is already durable; the already-recorded
                                // expected digest for that chunk_index
}
```

`bamepd` reconstructs the exact canonical proof transcript from `token` and the request fields
above per `m0-data-plane-and-storage-contracts.md` "Per-request proof", and performs the
complete authoritative check: signature validity, capability validity/expiry/scope, replay,
and current durable authorization. Worker MAY perform a local mechanical pre-check (for
example, rejecting an obviously malformed signature encoding before spending a round trip) as
a performance optimization, but such a local check is never sufficient authorization by
itself and never substitutes for this query, per ADR-0018 — every operation requiring current
durable authorization obtains an authoritative decision from `bamepd` first. `decision: denied`
carries no further reason field; the non-enumerable-denial requirement from
`m0-data-plane-and-storage-contracts.md` applies identically across this boundary, so Worker
cannot leak a more specific HTTP response than the generic `401` even if it wanted to.

### 2. Verified-chunk durable acceptance

```text
Worker -> bamepd: ChunkAcceptanceRequest{
    transfer_id, artifact_id, chunk_index, digest, size
}
bamepd -> Worker: ChunkAcceptanceDecision{
    outcome: "committed" | "already_committed" | "rejected",
    reason?    // present only when outcome=rejected
}
```

Sent only after Worker has itself verified the received bytes hash to `digest`
(`m0-data-plane-and-storage-contracts.md` "Durable chunk acceptance ordering"). `bamepd`
durably commits the chunk identity (first-writer for a new `chunk_index`) or recognizes an
identical already-committed identity and returns `already_committed`; either outcome is a
Worker-visible success. `rejected` covers a durable state conflict — for example, the Transfer
became terminal, or a *different* digest already exists for that `chunk_index` — and Worker
maps this deterministically to the HTTP `409` shapes in `m0-data-plane-and-storage-contracts.md`.

### 3. Artifact verification result

```text
Worker -> bamepd: ArtifactVerificationReport{
    transfer_id, artifact_id, computed_artifact_digest, matches_expected: bool
}
bamepd -> Worker: ArtifactVerificationAck{
    outcome: "committed"
}
```

Sent once, after `bamepd` has already durably committed `Incomplete -> PendingVerification`
for the `seal_manifest` operation (`m0-data-plane-and-storage-contracts.md` "Durable chunk
acceptance ordering"). `bamepd` commits the resulting `PendingVerification -> Verified |
Failed` transition and, once committed, is responsible for the owning Agent Protocol
`ActionResult` on the separate WSS control connection
(`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005) — this message never itself
produces an Agent Protocol effect, preserving that the Agent remains the sole Agent Protocol
participant and Worker never emits `ActionResult` directly.

### 4. Generic protocol error

```text
{ "type": "ProtocolError", "code": string, "message"?: string, "in_reply_to"?: string }
```

Used for a malformed frame/message, an unknown `type`, a pre-handshake violation, or an
oversized-frame violation observed before the connection is closed. `code` is a stable
non-empty diagnostic string; `message`, when present, must not expose secrets (see "Security
and logging" below). Whether the connection is closed after sending `ProtocolError` is
implementation policy for this contract, unless a future safety requirement states otherwise —
mirroring `m0-agent-protocol-contract.md`'s equivalent rule for its own `ProtocolError`.

## Authority

Every message above preserves ADR-0018's authority split: Worker only reports observed
mechanical facts (bytes verified, digest computed) or requests a decision; `bamepd` is the
only participant that decides authorization, durable chunk acceptance, and Artifact lifecycle
transitions. No message in this catalog lets Worker submit a general database mutation,
arbitrary query, or independent business decision. Worker holds no PostgreSQL repository
Adapter and this contract gives it no path to one.

## Failure semantics

- **UDS unavailable (no connection):** fail-closed. Worker must not authorize new work from
  stale local assumptions, must not fabricate a `ChunkAcceptanceDecision`/
  `ArtifactVerificationAck` outcome locally, and must not independently advance any Domain
  state. Every HTTP request that would otherwise require an `AuthorizationQuery` instead
  receives the same generic `401` denial defined by
  `m0-data-plane-and-storage-contracts.md`, identical in shape to an ordinary authorization
  denial — a distinguishable "try again later" response is deliberately not defined, since it
  would leak Worker/`bamepd` operational state to an unauthenticated observer without helping a
  legitimate Agent, whose retry/reconnection behavior is already governed by the Job lifecycle
  contract rather than by HTTP error text.
- **Disconnect with a request in flight:** the outstanding request is treated as failed/
  uncertain, never as success. Worker must not report HTTP success for an operation whose
  `AuthorizationDecision`, `ChunkAcceptanceDecision`, or `ArtifactVerificationAck` it never
  received. Worker reconnects and, if the operation is retried, sends a fresh request (fresh
  `message_id`); durable idempotency is governed by the identities in "Idempotency identities"
  below, never by UDS `message_id`.
- **Duplicate request after reconnect:** for `ChunkAcceptanceRequest`, `bamepd` recognizes an
  already-committed identical `(transfer_id, chunk_index, digest)` and returns
  `already_committed` rather than erroring or double-committing. The equivalent applies to a
  repeated `seal_manifest` flow per `m0-data-plane-and-storage-contracts.md`.
- **Stale response/unknown correlation:** a response whose `in_reply_to` does not match an
  outstanding request on the current connection generation is discarded and never applied to
  any state or surfaced to an HTTP client, per "Connection generations and correlation" above.
- **Incompatible protocol version:** rejected explicitly at handshake (`HandshakeRejected`);
  no further message is processed on that connection. Worker retries reconnect per its own
  backoff policy, which is implementation-time.
- **Malformed frame/message:** the receiver sends `ProtocolError` where the frame was at least
  parseable enough to identify a correlation target, and otherwise simply closes the
  connection; the peer reconnects and re-handshakes. Given this boundary is fully internal and
  host-local, this Specification does not require the more elaborate degraded-operation
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
| UDS connection generation (`worker_instance_id` + handshake) | one Worker process lifetime/connection | scopes which outstanding requests/responses are still valid for correlation |
| `transfer_id` | durable logical Transfer | correlates every message about one Transfer; not itself sufficient for chunk-level idempotency |
| `artifact_id` | durable logical Artifact | correlates Artifact-verification messages |
| capability identity (`SHA-256(token)`) | one issued capability instance | identifies which capability a proof is bound to; changes on renewal, is not a business idempotency key |
| `proof_id` | one HTTP request attempt | single-use anti-replay only; a retried operation mints a fresh `proof_id`, per `m0-data-plane-and-storage-contracts.md` "Idempotent retry is not proof reuse" |
| `(transfer_id, chunk_index, digest)` | one durable chunk identity | **the** idempotency key for `ChunkAcceptanceRequest`/durable chunk acceptance; a repeated request with this identical triple is always safe |
| `(transfer_id, chunk_count, artifact_digest)` | one sealed manifest | the idempotency key for a repeated `seal_manifest` request once already sealed with identical declared values |

Collapsing any of these — for example, treating a fresh `proof_id` as proof that a chunk must
be new, or treating UDS `message_id` as a durable acceptance key — would either falsely reject
a legitimate idempotent retry or falsely admit a replay; neither is acceptable.

## Compatibility and unknown fields

- Adding an optional field to any message in this catalog is a forward-compatible minor
  change: the unaware peer ignores it.
- A materially new message *type* or a new required field is a `protocol_version` change,
  handled explicitly at handshake rather than silently negotiated per-message.
- Worker ships with and is released alongside `bamepd` (ADR-0018), but this contract remains
  independently specified so a participant does not need synchronized Rust releases merely to
  add an optional field, and so a future non-Rust participant remains implementable from this
  document alone, per ADR-0003.

## Security and logging

- The opaque `token` value, `proof_id`/`issued_at`/`signature` proof material, the Ed25519
  private proof key (which never leaves the Agent and never crosses this boundary at all), and
  the Server TLS private key (which never crosses this boundary either, per ADR-0018) MUST be
  redacted from logs and debug output wherever this contract's messages are logged.
- `proof_public_key` and its thumbprint are not themselves secret and may appear in
  diagnostics.
- `AuthorizationDecision{decision: "denied"}` carries no reason field precisely so that no
  log/diagnostic derived from this contract can casually leak a more specific internal
  authorization reason (unknown transfer, wrong Endpoint, terminal Attempt, revoked
  credential, replay, wrong proof key, or another internal cause) into a Worker-side
  observable surface. Precise internal diagnostics may still exist inside `bamepd` according
  to its own observability policy (`m0-persistence-observability-and-domain-events.md`); this
  contract does not carry them to Worker.

## Out of scope

- a general-purpose RPC/service framework beyond the messages above;
- compression, retention, or future Worker responsibilities not required by M1 RF-005;
- the concrete host-local mechanism by which Worker obtains Server TLS key material (ADR-0018
  requires it not cross this IPC protocol, but the concrete provisioning mechanism is
  implementation-time);
- process supervision/respawn mechanics (ADR-0018 topology, not this wire contract);
- PostgreSQL schema and query mechanics;
- the HTTPS data-plane request/response contract itself (owned by
  `m0-data-plane-and-storage-contracts.md`).

## Validation

At minimum:

- handshake success/incompatible-version rejection, including the pre-handshake protocol
  violation for any other message type;
- frame length-prefix parsing, including the oversized-frame rejection boundary;
- unknown `type` rejection and unknown-field forward compatibility;
- `AuthorizationQuery`/`AuthorizationDecision` for approved and denied cases, including that
  `denied` never carries a reason;
- `ChunkAcceptanceRequest` for new-identity commit, idempotent already-committed replay, and
  rejected/conflicting identity;
- `ArtifactVerificationReport`/`ArtifactVerificationAck` committing the correct
  `PendingVerification -> Verified | Failed` transition;
- UDS disconnect with a request in flight never producing a fabricated success;
- stale response/unknown correlation discarded without effect;
- reconnect after Worker restart completing a fresh handshake and connection generation;
- fail-closed HTTP behavior (generic `401`) while UDS is unavailable.

Contract tests exercise the real framing/message shapes; Simulator-level scenarios exercising
this boundary must not bypass it through in-process shortcuts, consistent with
`m0-simulator-contract-and-validation-strategy.md`'s general Agent Protocol fidelity
requirement applied to this analogous internal boundary.

## Related

- ADR-0001 — Worker process isolation.
- ADR-0003 — Worker/Agent language and contract-independence requirement.
- ADR-0008 — data-plane transport/chunking/resumability rationale.
- ADR-0018 — isolated Worker data-plane process boundary; UDS direction and durable-authority
  split this contract implements.
- `m0-data-plane-and-storage-contracts.md` — HTTPS data-plane v1 contract, proof
  canonicalization, and durable chunk-acceptance ordering this IPC contract serves.
- `m0-agent-protocol-contract.md` — the separate Agent Protocol boundary `bamepd` alone
  remains responsible for; Worker never emits Agent Protocol messages.
- `m0-persistence-observability-and-domain-events.md` — durable state/audit authority
  remaining exclusively in `bamepd`.
- `m1-simulated-vertical-slice-and-baseline-validation.md` — RF-005, the concrete M1 action
  this contract's messages ultimately serve.
- `docs/reference/worker-data-plane-composition-spike.md` — empirical evidence for the Worker
  process/listener/IPC composition this contract's framing choices build on.
