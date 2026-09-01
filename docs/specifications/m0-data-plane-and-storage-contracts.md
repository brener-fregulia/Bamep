# M0 — Data-Plane and Storage Contracts

Status: **Approved**

This Specification is the normative contract for Bamep data-plane transfer, Artifact
integrity/lifecycle, transfer authorization, capture consistency/provenance, and Storage
Target capabilities. ADR-0008 owns rationale; `docs/reference/transfer-resumability-spike.md`
owns empirical resumability evidence.

## Data-plane boundary

- Bulk Artifact bytes use HTTPS, never the Agent Protocol WebSocket.
- Agent Protocol may carry transfer control/correlation, progress, and transfer-authorization
  messages.
- HTTPS reuses the Server TLS identity established by Bamep trusted bootstrap; no second
  Server trust anchor or client-certificate PKI is introduced.

## Chunk manifest

Every transferred Artifact has a manifest containing:

- `artifact_id`;
- explicit `digest_algorithm`;
- fixed `chunk_size`;
- `chunk_count` once sealed;
- per chunk: `chunk_index`, `size`, `digest`;
- expected full-Artifact `artifact_digest`.

`digest_algorithm` and `chunk_size` are not selected here, but must be explicit and stable
for a manifest; how they are communicated to the Agent for a given milestone's concrete
transfer action is owned by the Specification that introduces that action (for M1, see
`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005).

`chunk_index` values are 0-based and assigned sequentially as the producing participant
identifies chunks; for Agent -> Server capture the Agent assigns the next sequential index as
it produces each chunk from the reproducible source, consistent with "continuation may add a
new chunk identity to an unsealed manifest" below.

Every digest value on the wire — a chunk `digest` or the full-Artifact `artifact_digest` — is
the raw digest-algorithm output (32 bytes for SHA-256), encoded as canonical RFC 4648
base64url without padding (43 ASCII characters for a 32-byte SHA-256 digest), under the same
strict canonicalization/parsing rule used throughout this Specification. This is a wire
encoding rule, not a digest-algorithm selection; it applies uniformly regardless of which
`digest_algorithm` a manifest declares.

For Agent -> Server capture, `artifact_digest` may be computed incrementally; a second
complete source pre-read is not required.

Chunk identity metadata is durable state per
`m0-persistence-observability-and-domain-events.md`.

### Construction and sealing

While the Artifact is `Incomplete`:

- `digest_algorithm` is fixed before the first chunk identity is committed;
- each produced chunk identity becomes durable as it is produced;
- `artifact_digest` may be accumulated incrementally.

When all expected chunks are identified, the manifest is sealed. After sealing,
`digest_algorithm`, `artifact_digest`, `chunk_count`, and the complete chunk-identity set
are immutable.

`Incomplete -> PendingVerification` requires a sealed manifest and every expected chunk
durably present and individually verified.

Capture continuation and transfer resume are distinct:

- **continuation** may add a new chunk identity to an unsealed manifest;
- **resume/retransmission** must satisfy an already recorded chunk identity from
  reproducible source bytes or durable staging.

A recorded digest is never rewritten to accept different bytes. Verification compares an
independently computed value with the recorded expected value.

## Chunk transfer and resumability

- Chunks are independently addressable transfer units.
- A received chunk is accepted only if its digest matches the manifest.
- Invalid bytes are not persisted as a valid chunk.
- Resume skips already-held chunks whose digest matches.
- Only missing or invalid chunks are transferred again.
- The same pattern applies Agent -> Server and Server -> Agent.

Resumability is valid only while each required logical chunk remains reproducible or
durably staged. Source mutation that makes a recorded chunk unreproducible fails the
transfer; it never causes manifest identity to be rewritten.

## Transfer authorization

Every data-plane request requires a **short-lived, transfer-scoped, sender-constrained
capability** issued through the authenticated Agent Protocol session.

The capability alone is insufficient: the requester must also prove possession of the
ephemeral private key to which the capability is bound.

### Ephemeral proof key

For one authorization context:

1. the authenticated Agent creates an asymmetric ephemeral keypair;
2. the private key remains Agent-local and non-durable;
3. it is not an Endpoint identity credential or trusted Endpoint state;
4. `TransferAuthorizationRequest` supplies the public key or canonical equivalent;
5. the Server binds the granted capability to that key's thumbprint.

**Algorithm:** ordinary Ed25519, with no prehash mode and no optional context mode — the same
algorithm and verification discipline already selected by
`m0-trusted-bootstrap-and-server-fingerprint-contract.md` "Site signing key and SiteKeyId" for
the site bootstrap signing key, and already an accepted repository dependency
(`ed25519-dalek`). This is a distinct key from the site signing key, the Agent Protocol TLS
Server key, and every Endpoint identity/runtime credential; it exists only for one transfer
authorization context and is never persisted as Endpoint state. Verification must reject
non-canonical/problematic signature and public-key representations and weak-key cases where
the verifying implementation supports doing so, mirroring the site-key verification discipline.

**Wire encoding:** `proof_public_key` is the raw 32-byte Ed25519 public-key value, encoded as
canonical RFC 4648 base64url without padding: exactly 43 ASCII characters. This is the same
canonicalization rule already used for `BootNonce` and `bootstrap_assertion`
(`m0-trusted-bootstrap-and-server-fingerprint-contract.md`); parsing is strict under the
identical rule — reject padding, the standard-base64 `+`/`/` alphabet, whitespace, wrong
length, non-canonical trailing bits, or any value that does not round-trip byte-for-byte
through the canonical encoder.

**Thumbprint:** the proof-key thumbprint bound to a capability is exactly
`SHA-256(raw 32-byte Ed25519 public-key value)`, mirroring `SiteKeyId`'s definition in the
trusted-bootstrap contract. SPKI, PEM, DER wrappers, and human-readable text are never hashed.
The thumbprint is Server-internal correlation state; it is not independently carried on the
wire by the Agent on every request, because the Server already durably associates it with the
issued capability (`token`) at grant time — see "Per-request proof" below.

This algorithm/encoding selection is the M1 data-plane interoperability choice materialized by
this Work Package; it is not claimed as a universal cryptographic policy for every future
Bamep proof-key use.

### Capability bindings

A capability binds at least:

- `endpoint_id`;
- `transfer_id`;
- `artifact_id`;
- direction;
- owning `attempt_id`;
- bounded expiry;
- proof-key thumbprint;
- unique capability identity suitable for proof binding.

It authorizes only that tuple and is never a generic data-plane credential.

### Issuance

```text
authenticated Agent session
  -> owning transfer Attempt exists and is authorized
  -> TransferAuthorizationRequest{transfer_id, proof_public_key}
  -> Server revalidates current durable authorization
  -> TransferAuthorizationGrant | TransferAuthorizationDenied
  -> HTTPS chunk requests with capability + fresh proof
```

`TransferAuthorizationRequest`, `TransferAuthorizationGrant`, and
`TransferAuthorizationDenied` are Agent Protocol v1 messages; their wire contract belongs
to `m0-agent-protocol-contract.md`.

### Capability opacity

`token` is opaque to the Agent and to the Worker. Neither participant parses its internal
serialization/signing format; only `bamepd` issues and interprets it. This Specification
normatively defines only:

- its externally observable carrier: an opaque UTF-8 string, transported as a JSON string
  value in `TransferAuthorizationGrant` (`m0-agent-protocol-contract.md`) and as an HTTP
  header value on the data-plane surface below; concrete maximum length is implementation-time
  but must remain comfortably within ordinary HTTP header size limits;
- its required semantic bindings, listed under "Capability bindings" above;
- the derived, externally computable **capability identity** used for proof binding: exactly
  `SHA-256(UTF-8 bytes of the exact token string as currently held)`. Both the Agent (when
  signing a proof) and `bamepd` (when verifying one, using the exact token bytes the Worker
  forwarded from the request) compute this identity independently from the same token bytes
  that are already on the wire; it is never itself transmitted as a separate field, and the
  Worker never needs to compute or understand it.

The Worker passes the opaque `token` to the authoritative `bamepd` decision path unmodified;
it never treats token structure as business authority, per ADR-0018.

### Per-request proof

Every HTTPS chunk request carries:

1. the capability (`token`); and
2. a fresh proof signed by its bound ephemeral private key.

**Canonical signed bytes.** The proof is a fixed-width binary transcript, following the same
domain-separated, explicit-offset discipline already used for the trusted-bootstrap assertion
(`m0-trusted-bootstrap-and-server-fingerprint-contract.md` "Exact signed representation"),
chosen specifically to avoid ambiguous JSON canonicalization across independent
implementations. The exact signed payload is:

```text
u16be(34)
|| ASCII("bamep.m1.data-plane-transfer.proof")
|| u16be(1)
|| capability_id[32]
|| operation[1]
|| transfer_id[16]
|| artifact_id[16]
|| direction[1]
|| chunk_index_present[1]
|| chunk_index[8]
|| proof_id[16]
|| issued_at[8]
```

| Field | Exact content |
| --- | --- |
| domain length + discriminator | `u16be(34)` then the exact 34-byte ASCII string `bamep.m1.data-plane-transfer.proof` |
| schema version | `u16be(1)` |
| `capability_id` | 32 raw bytes; `SHA-256(UTF-8 bytes of token)`, per "Capability opacity" above |
| `operation` | 1 byte, closed enum: `1 = chunk_upload`, `2 = resume_discovery`, `3 = seal_manifest`; other values are reserved and unassigned in V1 |
| `transfer_id` | 16 raw bytes; the exact UUID v4 byte representation |
| `artifact_id` | 16 raw bytes; the exact UUID v4 byte representation |
| `direction` | 1 byte, closed enum: `1 = agent_to_server`; value `2` is reserved for a future Server -> Agent milestone and is unassigned/rejected in V1 |
| `chunk_index_present` | 1 byte, `0` or `1`; `0` for `resume_discovery` and `seal_manifest`, which are transfer-scoped, not chunk-scoped |
| `chunk_index` | `u64be`; the chunk index for `chunk_upload`; exactly `0` when `chunk_index_present == 0` |
| `proof_id` | 16 raw bytes; see "Freshness and replay representation" below |
| `issued_at` | `u64be`; Unix epoch milliseconds |

The signed payload is exactly 137 bytes; the Ed25519 signature is computed over exactly these
bytes and is never itself part of the signed payload. This transcript binds proof
contract/version (the domain string + schema version), exact capability identity, the exact
HTTP operation, `transfer_id`, `artifact_id`, direction, exact chunk identity when applicable,
and the unique `proof_id`/`issued_at` freshness pair — the complete list this Specification's
"Per-request proof" contract requires, expressed as exact bytes rather than a URL string that
independent HTTP libraries could normalize differently.

Both the Agent (constructing and signing this transcript locally before each request) and
`bamepd` (reconstructing the identical transcript) MUST arrive at byte-identical transcripts
for signature verification to succeed. `bamepd` sources each transcript field as follows:

| Transcript field | `bamepd` reconstruction source |
| --- | --- |
| `capability_id` | `SHA-256` of the exact UTF-8 `token` bytes received on the request |
| `operation` | the HTTP method + route of the request (`chunk_upload` / `resume_discovery` / `seal_manifest`) |
| `transfer_id` | the `{transfer_id}` path segment |
| `chunk_index` | the `{chunk_index}` path segment for `chunk_upload`; absent (`chunk_index_present = 0`) otherwise |
| `artifact_id` | the `artifact_id` `bamepd` durably bound to this `token` when it issued the capability — **not** any value on the data-plane request |
| `direction` | the `direction` `bamepd` durably bound to this `token` when it issued the capability — **not** any value on the data-plane request |
| `proof_id`, `issued_at` | the wire values from the `X-Bamep-Transfer-Proof` carrier |

`artifact_id` and `direction` are deliberately **not** carried on the data-plane request:
they are absent from the `/api/data/v1/transfers/{transfer_id}/...` route and from the common
headers, and the `token` is opaque to the requester and to the Worker (see "Capability
opacity"). The Agent still signs `artifact_id` and `direction` into its transcript. Because
`bamepd` reconstructs those two fields from the capability binding it controls — never from
anything the request asserts — a proof that signed a different `artifact_id`/`direction` than
the `token` is bound to fails signature verification and is denied with the single generic
non-enumerable denial: cross-Artifact and cross-direction substitution stay fail-closed, and
`capability_id` being in the signed transcript already transitively commits the signer to the
one `(endpoint, transfer, artifact, direction, attempt, proof-key)` tuple that `token` binds.
No party transmits the assembled transcript itself; only `proof_id`, `issued_at`, and the
signature travel explicitly on the wire, in the per-request proof carrier defined under
"HTTPS data-plane v1 contract" below. The Server↔Worker relay of these inputs is
`m1-worker-data-plane-control-contract.md` "Operations, HTTP mapping, and transcript inputs".

**Signature wire encoding:** the raw 64-byte Ed25519 signature, encoded as canonical RFC 4648
base64url without padding: exactly 86 ASCII characters, under the identical strict-parsing
rule used throughout this Specification and `m0-trusted-bootstrap-and-server-fingerprint-contract.md`.

### Freshness and replay representation

- `proof_id` is 16 bytes generated fresh per request from the operating-system CSPRNG,
  unpredictable, and encoded as canonical RFC 4648 base64url without padding: exactly 22 ASCII
  characters under the identical strict-parsing rule used elsewhere in this Specification.
- `issued_at` is the Unix epoch millisecond timestamp at which the Agent constructed the proof,
  carried both inside the signed transcript (`u64be`, per the table above) and on the wire as
  the decimal ASCII string of that exact same integer — never an RFC 3339 string for this
  field — so no string/integer conversion ambiguity can exist between the signed bytes and the
  transported value.
- Proofs outside a configured bounded freshness window (measured against `issued_at`) fail
  closed. A previously accepted `proof_id` fails closed on reuse (replay). Accepted `proof_id`
  values remain in a bounded transient replay cache for at least the accepted freshness window.
  Loss of replay-cache continuity — for example across a `bamepd` restart — must never make a
  previously accepted or previously expired proof valid again; this composes with the "Server
  restart" invalidation rule below.
- Exact freshness-window duration and replay-cache capacity remain implementation-time, per
  this Specification's existing "Replay and freshness" contract; only their interoperable wire
  representation is materialized here.

**Idempotent retry is not proof reuse.** A `proof_id` is single-use by construction. A
legitimate retry of the same logical operation (for example, resending a chunk whose durable
acceptance response was lost) MUST mint a fresh `proof_id`/`issued_at`/signature; it never
resubmits a previously used proof. Idempotency for a retried `chunk_upload` is established at
the chunk-identity layer (`transfer_id` + `chunk_index` + digest), not at the proof/replay
layer — see "Idempotency identities" in `m1-worker-data-plane-control-contract.md`. This
reconciles "response loss after durable commit is idempotent" with "replay fails closed": the
two guarantees operate at different identity layers and are not in tension.

### Per-request verification

Every request fails closed unless all applicable checks pass:

- capability signature/integrity and expiry;
- exact Endpoint/transfer/Artifact/direction scope;
- proof signature and bound-key match;
- proof-to-capability match;
- operation/chunk match;
- proof freshness;
- `proof_id` not replayed;
- current durable transfer/Artifact authorization;
- owning Attempt still permits continuation;
- Endpoint credential remains `CredentialActive`.

Cryptographic verification may be stateless; the complete authorization decision is not.
It also depends on transient replay state and current durable state.

All authorization failures return one generic non-enumerable denial. Internal diagnostics
may record the precise reason.

`CredentialRevoked` invalidates outstanding capabilities for that Endpoint even before
their embedded expiry.

### Replay and freshness

- Each proof has a unique unpredictable `proof_id`.
- Proofs are accepted only inside a bounded freshness window.
- Accepted `proof_id` values are kept in a bounded transient replay cache for at least that
  window.
- Reuse fails closed.
- Exact freshness duration and capability TTL are implementation-time.

Loss of replay-cache continuity must never make old authorization valid again.

### Authorization lifetime versus transfer identity

Authorization material is shorter-lived than the logical transfer.

Renewal or proof-key replacement for an otherwise authorized transfer:

- keeps the same `transfer_id`;
- keeps the same Artifact and manifest;
- keeps already verified chunks;
- does not create a new Attempt;
- does not imply destructive retry.

`transfer_id` is distinct from HTTP request/connection identity, capability/proof identity,
`attempt_id`, and `action_id`.

### Disconnect and restart

**WSS disconnect:** does not by itself grant or revoke data-plane authorization. A
still-valid capability may continue only while all per-request checks pass.

**Agent reconnect:** uses normal Agent Protocol authentication/reconciliation; data-plane
authorization never replaces Attempt reconciliation.

**Agent restart:** the ephemeral private key is lost. If durable state still authorizes the
transfer, the Agent reauthenticates, creates a new key, obtains a new capability, and
continues the same transfer/Artifact/chunks.

**Server restart:** any pre-restart capability whose replay-protection continuity cannot be
guaranteed is invalid. Legitimate continuation requires control-plane reauthentication,
durable-state reconciliation, and new authorization. The concrete invalidation mechanism
(e.g. epoch/fresh signing context) is implementation-time.

An Attempt in `AwaitingReconciliation` may continue only when the Job lifecycle contract
still permits it. `Indeterminate` or another disqualifying terminal outcome denies further
authorization unless a later workflow explicitly authorizes new work.

### Durable versus transient authorization state

Durable transfer authorization/correlation includes the applicable
`endpoint_id`, `transfer_id`, `artifact_id`, direction, and `attempt_id`.

Transient state includes issued capabilities, ephemeral proof keys, request proofs, and the
replay cache. They are not persisted as reusable Endpoint credentials.

Persistence/audit semantics follow `m0-persistence-observability-and-domain-events.md`.

### Threat-model boundary

The mechanism protects against passive LAN capture, bearer-token theft without the bound
private key, cross-Endpoint/transfer/Artifact/direction substitution, replay, stale/revoked
authorization, and confused-deputy mistakes covered by the bindings above.

It does not claim protection when an attacker obtains both a valid capability and its
matching ephemeral private key from a compromised authenticated Agent. This does not extend
Bamep's trusted-bootstrap assurance boundary.

## HTTPS data-plane v1 contract

This is the minimum versioned HTTPS surface the isolated Worker (ADR-0018) exposes for the M1
Agent -> Server chunk path. It is a distinct, explicit namespace from the Administrative API
(`/api/admin/v1/`, owned by `m0-administrative-api-web-read-contract.md`): every route below is
rooted at `/api/data/v1/`, served only by the Worker-owned listener reached through
`data_plane_base_url` (`m0-agent-protocol-contract.md` "Transfer authorization"). It defines no
Administrative operation and no SPA fallback.

### Common request elements

Every request below carries:

- `X-Bamep-Capability: <token>` — the opaque capability from `TransferAuthorizationGrant`.
- `X-Bamep-Transfer-Proof: <proof_id>.<issued_at>.<signature>` — the per-request proof, as
  three dot-separated segments: `proof_id` (22-character base64url-no-pad), `issued_at` (the
  decimal ASCII string of the exact Unix-millisecond integer signed), and `signature`
  (86-character base64url-no-pad), each exactly as canonicalized under "Per-request proof" and
  "Freshness and replay representation" above. This single-header compact form carries the
  fields the recipient cannot otherwise derive; `capability_id`, `operation`, `transfer_id`,
  and `chunk_index` are reconstructed by `bamepd` from the token and the request's own
  method/route, and `artifact_id`/`direction` from the capability binding `bamepd` recorded at
  grant time — see the transcript-source table under "Per-request proof". No data-plane
  request carries `artifact_id` or `direction`.

`chunk_upload` additionally carries `X-Bamep-Chunk-Digest: <digest>` — the Agent-declared
chunk digest, encoded per "Chunk manifest" above, for the exact bytes in the request body.

Every response body below is UTF-8 JSON. An error response uses the shape
`{ "error": { "code": string, "message"?: string } }`, mirroring the `ActionAck.error` shape
already established by `m0-agent-protocol-contract.md`, except the generic-denial response
(`401` below), which is always exactly `{ "error": { "code": "AUTHORIZATION_DENIED" } }` with
no variation and no `message`, to guarantee the required non-enumerable shape.

### Operations

**1. `PUT /api/data/v1/transfers/{transfer_id}/chunks/{chunk_index}` — chunk upload**

- Method/path: `PUT`; `chunk_index` is a non-negative decimal integer path segment.
- Headers: the common headers above, `X-Bamep-Chunk-Digest` (required), `Content-Type:
  application/octet-stream`, `Content-Length`.
- Body: the exact raw chunk bytes.
- Success: `201 Created` with `{ "chunk_index": N, "status": "accepted" }` for a chunk not
  previously durably held; `200 OK` with `{ "chunk_index": N, "status": "already_held" }` when
  an identical already-durable chunk is idempotently resubmitted (see "Durable chunk
  acceptance ordering" below).
- Generic authorization failure: `401`, the fixed generic denial body above — covers every
  case in "Per-request verification", including a `transfer_id` that does not exist at all;
  a nonexistent transfer is never distinguished from any other denial reason (see "Route
  addressing an unknown resource" below).
- Malformed request: `400` `{ "error": { "code": "MALFORMED_REQUEST" } }` — missing/invalid
  required headers, non-canonical base64url in any header, `chunk_index` not a well-formed
  non-negative integer, or a missing/invalid `Content-Type`. Checked only after route/method
  matching and before authorization, since a structurally malformed request cannot be verified.
- Semantic conflict: `409`, `{ "error": { "code": "CHUNK_IDENTITY_CONFLICT" } }` when
  `chunk_index` is already durable with a *different* declared digest than
  `X-Bamep-Chunk-Digest`; `409` `{ "error": { "code": "DIGEST_MISMATCH" } }` when the bytes
  actually received hash to a value different from the declared/expected digest (new or
  existing chunk); `409` `{ "error": { "code": "TRANSFER_NOT_CONTINUABLE" } }` when the owning
  Transfer/Artifact/Attempt is already terminal or the manifest is already sealed and
  `chunk_index` was never part of the sealed set.
- Oversized body: `413` `{ "error": { "code": "CHUNK_TOO_LARGE" } }` when the body exceeds the
  Transfer's `chunk_size` (plus any bounded implementation margin). The Worker enforces this
  bound before reading the body, using the authoritative `chunk_size` the approved
  authorization decision returns (`m1-worker-data-plane-control-contract.md`
  "Chunk-upload authorization"), never a Worker-side constant.
- Already-held valid chunk: idempotent `200`, above; never re-verified against a *different*
  declared digest, and never re-triggers a durable acceptance transition.
- Digest mismatch: `409` `DIGEST_MISMATCH`, above; invalid bytes are never persisted as a valid
  chunk and never rewrite an already-recorded expected identity.
- Unsupported method/resource: `405` `{ "error": { "code": "METHOD_NOT_ALLOWED" } }` for a
  recognized path with an unsupported method; `404` `{ "error": { "code": "UNKNOWN_ROUTE" } }`
  only for a path that matches no route *shape* at all, independent of any identifier's value.

**2. `GET /api/data/v1/transfers/{transfer_id}/chunks` — resume discovery**

- Method/path: `GET`.
- Headers: the common headers above (no `X-Bamep-Chunk-Digest`; the signed `operation` is
  `resume_discovery` and `chunk_index_present` is `0`).
- Body: none.
- Success: `200` with
  `{ "transfer_id", "sealed": bool, "digest_algorithm", "chunk_size", "expected_chunk_count":
  integer|null, "held_chunks": [ { "chunk_index", "digest" }, ... ] }`. `held_chunks` reflects
  only chunks `bamepd` durably holds and has individually verified — never Worker-local
  transient memory or staged-but-uncommitted bytes — so it remains correct after HTTP
  connection loss, Worker restart, authorization renewal, Agent reconnect, or `bamepd`
  restart, per "Resume discovery" below. `expected_chunk_count` is `null` before the manifest
  is sealed. The Worker obtains this entire payload from `bamepd` durable state through the
  authorization-bound `ResumeDiscoveryQuery` control path
  (`m1-worker-data-plane-control-contract.md` "Resume-discovery authorization and first
  page"), which is paginated so no single control frame exceeds its 1 MiB limit for a large
  held-chunk set; the Worker aggregates the pages into this one response and fails the request
  closed (generic `401`) if any page cannot be obtained.
- Generic authorization failure / Malformed / Unsupported method/resource: identical shapes to
  operation 1.
- This operation has no chunk-identity or oversized-body failure mode of its own.

**3. `POST /api/data/v1/transfers/{transfer_id}/seal` — seal manifest and verify Artifact**

- Method/path: `POST`.
- Headers: the common headers above (no `X-Bamep-Chunk-Digest`; signed `operation` is
  `seal_manifest`, `chunk_index_present` is `0`).
- Body: `{ "chunk_count": integer, "artifact_digest": string }` — the Agent's declared final
  chunk count and its incrementally computed full-Artifact digest
  (`m0-data-plane-and-storage-contracts.md` "Chunk manifest" already permits incremental
  computation).
- Success: `200` with `{ "transfer_id", "artifact_id", "sealed": true, "artifact_status":
  "Verified" | "Failed" }`. This request is synchronous for M1: it drives sealing, full-Artifact
  verification, and the authoritative `PendingVerification -> Verified | Failed` transition
  before responding; see "Durable chunk acceptance ordering" below for the two-step commit that
  keeps this safe across a Worker crash mid-verification. `artifact_status: "Failed"` is a
  `200` response (a completed operation reporting a `Failed` Artifact), not a `409`. The Worker
  authorizes and triggers the first commit through the `ManifestSealRequest` control message
  and verifies against the authoritative sealed `chunk_count`/`artifact_digest` `bamepd`
  returns, never the values in this request body; the response `artifact_id` also comes from
  that `ManifestSealDecision` (it is not on the route or in a header)
  (`m1-worker-data-plane-control-contract.md` "Seal-manifest first durable commit"). `bamepd`
  durably commits this authoritative `PendingVerification -> Verified | Failed` outcome before
  the HTTP success response exposes it; the Agent, after observing that committed
  `artifact_status`, sends the terminal `ActionResult` over its existing Agent Protocol WSS
  session — `ActionResult` is Agent -> Server (`m0-agent-protocol-contract.md`) — and `bamepd`
  consumes that inbound message through its normal Agent Protocol action-evidence path. The
  HTTP response never substitutes for that `ActionResult`.
- Generic authorization failure / Malformed: identical shapes to operation 1.
- Semantic conflict: `409` `{ "error": { "code": "INCOMPLETE_MANIFEST" } }` when `bamepd` does
  not already durably hold every chunk index `0..chunk_count-1` individually verified; `409`
  `{ "error": { "code": "MANIFEST_ALREADY_SEALED" } }` when the manifest is already sealed with
  a *different* `chunk_count` or `artifact_digest` than previously declared. A retry with the
  *same* already-sealed `chunk_count`/`artifact_digest` is idempotent success (`200`, above),
  re-driving/confirming full-Artifact verification if it had not yet completed — this is how
  the Agent safely retries after a Worker crash between sealing and verification completing.
- Unsupported method/resource: identical shapes to operation 1.

### Route addressing an unknown resource

A `transfer_id` that does not durably exist, or that exists but is not bound to the requesting
Endpoint/proof-key/capability, is never distinguished from any other authorization denial: it
returns the same generic `401`, never `404`. `404` is reserved exclusively for a path that
matches no defined route *shape* at all (for example, an unrelated path), independent of any
identifier value in it — this prevents transfer existence/ownership from being enumerated
through HTTP status alone, extending the same non-enumerable-denial principle
`m0-agent-protocol-contract.md` already applies to `TransferAuthorizationDenied.reason`.

### Durable chunk acceptance ordering

For `chunk_upload`, the required ordering (per this Specification's existing "Per-request
verification" and ADR-0018's "Worker executes mechanism, `bamepd` owns durable authority") is:

```text
1. Worker receives request metadata (headers, capability, proof, chunk_index, declared digest)
2. Worker sends AuthorizationQuery over the UDS contract; bamepd returns the authoritative
   AuthorizationDecision: capability/proof validity, plus (when approved) the durable
   digest_algorithm and chunk_size, an acceptance_handle, and — if chunk_index is already
   durable — its already-recorded expected digest
3. Worker rejects immediately (401 or 409 CHUNK_IDENTITY_CONFLICT) if that decision denies,
   or if a declared digest conflicts with an already-recorded expected digest; and rejects
   413 CHUNK_TOO_LARGE if the announced/received body exceeds chunk_size
4. Worker receives/buffers the body and independently computes the actual digest with
   digest_algorithm
5. Worker rejects (409 DIGEST_MISMATCH) if the actual digest does not match the declared/
   expected digest; invalid bytes are never staged as a durable chunk
6. only once bytes are verified does Worker send ChunkAcceptanceRequest (carrying the
   acceptance_handle, verified digest, and exact received size) asking bamepd to durably
   commit acceptance (new expected identity if chunk_index was not yet durable; idempotent
   confirmation if it already exactly matches)
7. bamepd's durable commit (ChunkAcceptanceDecision outcome committed or already_committed)
   is the authoritative acceptance; a rejected outcome maps deterministically to
   409 CHUNK_IDENTITY_CONFLICT or 409 TRANSFER_NOT_CONTINUABLE
8. only after that commit is confirmed does Worker return the HTTP success response
```

A Worker-local verified buffer/file is never itself durable Artifact state (ADR-0018); if the
durable commit at step 7 succeeds but the HTTP response at step 8 is lost (Worker crash,
connection reset), a retried identical request (same `chunk_index`, same digest, fresh proof
per "Idempotent retry is not proof reuse" above) safely reaches the same durable outcome and
returns `200 already_held` without a second commit or any rewritten identity.

For `resume_discovery`, the Worker authorizes and retrieves durable resume state in one
authorization-bound control exchange (`ResumeDiscoveryQuery` and, for a large held-chunk set,
paginated `ResumeDiscoveryContinue`), never an independent durable-state query
(`m1-worker-data-plane-control-contract.md`).

For `seal_manifest`, the equivalent two-step commit is: the Worker sends `ManifestSealRequest`
(authorizing the operation and carrying the Agent-declared `chunk_count`/`artifact_digest`);
`bamepd` first durably commits `Incomplete -> PendingVerification` (sealing plus confirming
every declared chunk is already durable and individually verified) as one transaction and
returns the authoritative durable `artifact_id`, sealed `chunk_count`/`artifact_digest`, and a
`verification_handle`;
only afterward does the Worker reconstruct and verify the full Artifact bytes and send
`ArtifactVerificationReport{computed_artifact_digest}`, and `bamepd` — comparing that reported
digest against its own durable expected value, never trusting a Worker verdict — commits the
second, independent `PendingVerification -> Verified | Failed` transition and returns the
authoritative `artifact_status`. If the Worker crashes after the first commit but before the
report, the Artifact durably remains `PendingVerification` — never falsely `Verified` and
never lost back to `Incomplete` — and the idempotent seal retry above
(`already_pending_verification`) resumes exactly at the verification step.

### Full-Artifact byte reconstruction

`PendingVerification -> Verified` requires an independently computed full-Artifact digest that
matches the sealed `artifact_digest` ("Artifact lifecycle"). For the M1 linear Agent -> Server
capture model, the exact byte representation over which that digest is computed is:

```text
full_artifact_bytes :=
    raw bytes of chunk 0
 || raw bytes of chunk 1
 || ...
 || raw bytes of chunk (chunk_count - 1)
```

- chunks are concatenated in strictly ascending numeric `chunk_index` order;
- no framing bytes, length prefixes, separators, padding, JSON, or encoded digest text are
  inserted — only the raw chunk payload bytes, each exactly the `size` recorded for that
  chunk identity;
- every chunk except the last has `size == chunk_size`; the final chunk MAY be shorter
  (`1..=chunk_size` bytes);
- the manifest's `digest_algorithm` (M1: `sha256`) is applied to exactly that concatenated
  stream, producing the value compared against the sealed `artifact_digest`.

This is byte-equivalent to hashing the reproducible source stream sequentially, so the
Agent's incremental `artifact_digest` computation ("Chunk manifest": "`artifact_digest` may
be computed incrementally") and this reconstruction converge on the same value; it is also
consistent with chunked resume, since each chunk contributes its bytes at one fixed position
regardless of transfer order. `docs/reference/transfer-resumability-spike.md` Experiment C
demonstrated this concatenation reproducing a source digest exactly. This representation is
M1-scoped, like the chunk size and digest algorithm themselves; a future compressed or
non-linear model would define its own reconstruction rule.

### Resume discovery

Operation 2 above is the mechanism by which the Agent learns which already-recorded/verified
chunks it need not resend. Because it reads only `bamepd`-durable state, it produces correct
answers after HTTP connection loss, Worker restart, transfer-authorization renewal, Agent
reconnect, and `bamepd` restart followed by fresh authorization — none of these events erase
durable resume knowledge, and none of them alone extends how long the *authorization* granting
access to that knowledge remains valid: transfer-authorization lifetime is intentionally
shorter-lived than transfer lifetime (see "Authorization lifetime versus transfer identity"
above), and a lapsed authorization is renewed independently of the durable resume state it will
then be used to read.

The Worker is the party that reads this durable state on the Agent's behalf, and it does so
only through the authorization-bound control path in
`m1-worker-data-plane-control-contract.md`: a `ResumeDiscoveryQuery` that carries the same
per-request proof the HTTP request presented and that `bamepd` authorizes exactly as it
authorizes a `chunk_upload`, followed — only for a held-chunk set too large for one control
frame — by paginated `ResumeDiscoveryContinue` requests bound to that one authorized query
for the current Worker connection generation. The Worker has no PostgreSQL access and no
message that would let it read arbitrary durable Transfer state (ADR-0018); this path returns
one authorized Transfer's resume payload and nothing else.

## V1 capture consistency

V1 capture is an offline maintenance operation:

- the endpoint boots the Linux maintenance environment;
- installed Windows is not running;
- Volume/Image sources are read-only/non-destructive read sources;
- Selective source filesystems are accessed read-only.

VSS/live-Windows snapshotting is not required for V1. Live backup is outside V1.

Offline capture establishes source stability during capture; it does **not** prove that the
filesystem/application state was clean or semantically healthy.

## Capture-consistency fact

Cryptographic integrity and capture consistency are independent.

`capture_consistency` is:

- `NotApplicable`;
- `NotEstablished`;
- `Established`.

`Established` requires positive confirmation that the applicable offline/read-only capture
conditions held for the capture duration; it is never the default.

For Artifact types whose destructive use requires capture consistency, both must hold:

- Artifact is `Verified`;
- `capture_consistency == Established`.

The concrete component/mechanism that establishes this fact is outside this Specification.

## Artifact lifecycle

States:

`Incomplete -> PendingVerification -> Verified | Failed`

Additionally, `Incomplete -> Failed`.

Rules:

- `Incomplete -> PendingVerification`: manifest sealed and every expected chunk present and
  individually verified.
- `PendingVerification -> Verified`: independently computed full-Artifact digest matches
  sealed `artifact_digest`.
- `PendingVerification -> Failed`: full-Artifact verification fails.
- `Incomplete -> Failed`: required chunk cannot be reproduced/verified, or capture/transfer
  is abandoned/cancelled.
- `Verified` and `Failed` are terminal for that Artifact.
- A failed Artifact is not repaired into a different content identity in place; later
  authorized work creates a new Artifact.

An Artifact is one atomic integrity/completeness unit. No subset of a failed Artifact is
exposed as partial success.

If a future Selective workflow uses multiple independent Artifacts, those Artifacts may
succeed/fail independently; workflow acceptance of partial success is a separate policy.

### `Incomplete -> Failed` ownership and ordering (M1 Agent -> Server transfer)

For the M1 Agent -> Server transfer, `Incomplete -> PendingVerification -> Verified | Failed`
is driven durably by `bamepd` from the Worker seal/verification path
(`m1-worker-data-plane-control-contract.md` "Seal-manifest first durable commit" and
"Full-Artifact verification result"). Those commits precede the Agent's terminal
`ActionResult`, which the Agent sends only after observing the committed `artifact_status` in
the seal HTTP response (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005).

`Incomplete -> Failed` has no such preceding operation: no HTTPS data-plane operation and no
Worker control message performs it, and interruption/restart alone never does (a transfer
merely interrupted keeps its Artifact `Incomplete`). For this transfer, `bamepd` drives
`Incomplete -> Failed` when it consumes the owning action's authoritative terminal Agent
Protocol evidence that the capture cannot complete, while its Artifact is still `Incomplete`:

- the Agent's `ActionResult{Failed}` carrying `CHUNK_VERIFICATION_FAILED` or
  `TRANSFER_ABANDONED`;
- an authoritative terminal `Cancelled` outcome for the owning Attempt — the existing Issue
  #27 `CancelAck{Cancelled}` path, or an authoritative `Cancelled` reconciliation outcome;
- an authoritative terminal `Failed` outcome for the owning Attempt learned through
  `m0-job-lifecycle-and-scheduling.md` `#28` reconciliation — `StatusReport{Failed}` applied
  from `AwaitingReconciliation` — when the terminal `ActionResult{Failed}` was lost and the
  reconnect reconciliation carries the outcome instead. This is the reconciliation equivalent
  of the direct `ActionResult{Failed}` case above; the workflow decision remains unchanged
  `#28` reconciliation.

`bamepd` first validates action/Transfer/Artifact correlation — for a direct `ActionResult`
the evidence's `artifact_id` must be the Artifact durably bound to that Transfer/Attempt; for
a reconciliation outcome the resolved `action_id` must own the durable `Transfer` bound to
that Attempt. A mismatch fails closed and commits nothing. Only then does it commit, as
**one atomic persistence transaction**:

- `Incomplete -> Failed` for that Artifact;
- the terminal Attempt/JobStep/Job workflow transition the same evidence produces
  (`m0-job-lifecycle-and-scheduling.md` "Attempt lifecycle" — for a direct `ActionResult` the
  matching terminal transition, for a cancellation outcome the existing Issue #27 cancellation
  terminal transition, for a `#28` reconciliation `Failed`/`Cancelled` outcome the existing
  reconciliation terminal transition, all unchanged);
- the domain events and audit records those transitions already require
  (`m0-persistence-observability-and-domain-events.md` "Atomic persistence" and "Required M1
  normal-terminal Job/JobStep events"). This composition adds no event or audit record
  beyond those, and no new event type.

A reconciliation `StatusReport` carries only `known_state` — never an RF-005 failure `code`
or `artifact_id`. The durable Artifact state is the authoritative disambiguator, and it is
sufficient here: an `Incomplete` Artifact has not entered the seal-verification path
(`Incomplete -> PendingVerification` has not committed), so for a terminally-`Failed` or
terminally-`Cancelled` owning Attempt the capture can no longer complete and `Incomplete ->
Failed` is the only safe lifecycle transition, whatever failure `code` the lost `ActionResult`
would have carried. `ARTIFACT_VERIFICATION_FAILED` cannot apply — it requires
`PendingVerification`. If the bound Artifact is already `Verified` or `Failed`, the
reconciliation outcome still transitions the workflow per `#28` but the terminal Artifact is
never rewritten (`Verified` is never driven to `Failed` merely because reconciliation reports
`Failed`). If the bound Artifact is `PendingVerification`, `PendingVerification -> Verified |
Failed` remains owned by the Worker seal/verification path and reconciliation never drives it.

If that transaction cannot commit, no partial Artifact or workflow state is durable; the
Agent's idempotent re-send of the same `ActionResult`, or a repeated reconciliation
`StatusQuery`/`StatusReport` exchange, recovers it. Matching duplicate terminal evidence is a
no-op against the already-committed outcome, and conflicting later evidence never overwrites
the first committed terminal Artifact/Attempt outcome
(`m0-job-lifecycle-and-scheduling.md` "Duplicate and delayed evidence"). The terminal-Artifact
immutability rules earlier in this section still hold: conflicting later terminal evidence — a
direct `ActionResult` or a `#28` reconciliation outcome — against an already-terminal Artifact
never rewrites it: `Verified` is never rewritten to `Failed` to satisfy later evidence,
`Failed` is never rewritten to `Verified`, `CHUNK_VERIFICATION_FAILED` / `TRANSFER_ABANDONED`
and a reconciliation `Failed`/`Cancelled` outcome may drive `Incomplete -> Failed` only, and
recorded chunk identities, manifest identity, `transfer_id`, and `artifact_id` are never
rewritten.

## Destructive-use composition

Artifact checks are additional safety gates.

A dependent destructive JobStep revalidates at final dispatch:

- Artifact is `Verified`;
- required `capture_consistency` is `Established`.

These never replace or narrow the complete destructive-operation precondition set owned by
`m0-endpoint-identity-lifecycle.md` and composed by
`m0-job-lifecycle-and-scheduling.md`.

A valid Artifact alone never authorizes destructive execution.

## Artifact provenance and target identity

Endpoints may contain multiple disks/volumes/filesystems.

Artifact source provenance must identify the concrete capture source, not only the Endpoint.
The exact provenance schema is not selected here.

Source identity and destructive target identity are independent. A valid workflow may back
up an old disk, replace it, revalidate the new disk, provision it, and restore retained
data. The destination fingerprint therefore need not equal the Artifact source fingerprint.

This does not weaken target-disk revalidation: the current target must still satisfy the
Endpoint/Job destructive-dispatch contract immediately before execution.

A future planned-hardware-change authorization mechanism must preserve this legitimate
disk-replacement case.

**The fail-closed provenance case, clarified.** Source/destination fingerprint *inequality*
is, by itself, never a provenance failure — it is the expected shape of the legitimate
capture/replace/restore workflow above. The provenance fact this Specification requires to
fail closed is *inconsistency within the same Transfer/Artifact's own source context*: source
bytes previously associated with the same logical source/Transfer can no longer reproduce a
chunk identity already durably recorded for that Transfer's manifest. That is an
internal-consistency failure of one Artifact's own capture, detected through the immutable
chunk-identity and resume rules ("Chunk transfer and resumability" above), not a comparison
against a later, unrelated destructive target. Expected chunk identity, manifest identity,
`transfer_id`, and `artifact_id` are never rewritten to accommodate the changed source; the
Artifact fails closed instead. Destructive-use composition (above) already independently
revalidates the current target immediately before execution regardless of any Artifact's
recorded source; provenance consistency and target revalidation remain two independent
checks, and neither substitutes for the other.

### M1 scope of `SourceProvenance`

For M1, `SourceProvenance` is **immutable descriptive provenance bound to the Transfer**: it
records what source the capture is understood to represent, is fixed when the Transfer is
created, and is never rewritten. It is **not**, in M1, an independently re-observed
hardware-identity credential.

The operational same-Transfer consistency guarantee M1 implements is exactly:

- immutable expected chunk identity;
- authoritative durable resume state;
- source reproducibility.

If bytes previously associated with the same logical source/Transfer can no longer reproduce
the durably recorded chunk identity, the transfer **fails closed**; the recorded expected
identity is never rewritten to accept the changed source.

M1 does **not** define, require, or exercise:

- disk WWN comparison;
- disk/controller serial-number comparison;
- GPT/partition-table fingerprint as source identity;
- a composite hardware `SourceIdentity`;
- repeated hardware-source re-observation during a transfer;
- a provenance token, field, or message in Agent Protocol or Worker Protocol.

A concrete independently re-observed physical source identity — should one be required — is
deferred to the future physical-disk / hardware-integration milestone, which must define its
schema and authority explicitly before implementation. Nothing in M1 detects a physical
source-hardware substitution that still reproduces every durably recorded chunk identity.

## Storage capability model

A Storage Target exposes:

- `roles`: a set drawn from `SYSTEM`, `CACHE`, `ARCHIVE`;
- available capacity;
- read/write characteristics relevant to scheduling.

One target may expose multiple roles. Domain/Application code must not assume RAID layout,
filesystem, or raw device names; storage is accessed through the `storage` Port.

Role semantics:

| Role | Meaning |
|---|---|
| `SYSTEM` | Bamep operational durable state/configuration; not implicitly bulk Artifact storage |
| `CACHE` | optional working/staging/performance storage; may hold incomplete or extra copies |
| `ARCHIVE` | optional retained storage for completed/verified Artifacts |

Verification and retention are independent. `Verified` does not imply archived, and
`ARCHIVE` placement does not imply verification.

Migration between roles, multi-copy consistency, and retention duration are outside this
Specification.

## Backup strategy boundary

- **Volume/Image:** linear disk/volume capture using the chunk/Artifact contract above.
- **Selective:** file-granular baseline direction; large files may use the same chunking
  internally.

Per-file Selective behavior was not empirically validated by the resumability Spike and
must not be presented as such. It remains subject to the same source reproducibility and
capture-consistency rules.

## Out of scope

- a universal, permanently fixed Bamep chunk size or digest algorithm (M1's concrete values
  are an M1-scoped interoperability choice, materialized by
  `m1-simulated-vertical-slice-and-baseline-validation.md` RF-005 and this Specification's
  wire-encoding rules, not a forever-fixed architectural constant);
- live-Windows/VSS capture;
- concrete mechanism establishing `capture_consistency = Established`;
- exact numeric capability TTL, proof-freshness-window duration, and replay-cache capacity
  (their required properties and interoperable wire representation are materialized above;
  the numbers remain implementation-time);
- capability token internal serialization/signing format (kept opaque; see "Capability
  opacity");
- planned hardware-change authorization workflow;
- exact Artifact provenance schema, and any independently re-observed physical source
  identity — deferred to the future physical-disk / hardware-integration milestone (only the
  fail-closed/legitimate-inequality distinction and M1's descriptive-provenance scope are
  clarified above);
- final production backup/snapshot format;
- RAID/filesystem/device layout;
- database schema/index layout;
- retention-duration policy;
- Server↔Worker UDS wire contract (owned by `m1-worker-data-plane-control-contract.md`).

## Validation

At minimum:

**Artifact/transfer**
- lifecycle transition positives/negatives;
- manifest sealing and immutable expected identities;
- chunk and full-Artifact digest failures;
- interrupted transfer and selective resume;
- duplicate valid chunk idempotence;
- source mutation cannot rewrite chunk identity;
- restart preserves `Incomplete` as incomplete;
- atomic `PendingVerification -> Verified`.

**Transfer authorization**
- valid capability + matching proof succeeds only when durable state authorizes;
- missing/wrong-key/wrong-capability/wrong-operation/wrong-chunk proof fails;
- wrong Endpoint/transfer/Artifact/direction fails;
- stale or replayed proof fails;
- expired or revoked authorization fails;
- denials remain non-enumerable;
- renewal/reconnect/restart preserve transfer identity and verified chunks;
- pre-restart authorization is rejected when replay continuity was lost.

**Safety composition**
- `Verified` + `NotEstablished` capture consistency cannot authorize destructive use;
- any failed independent base destructive precondition still blocks destructive use;
- legitimate source-disk/destination-disk replacement does not require fingerprint equality.

**HTTPS data-plane v1**
- proof canonical-byte reconstruction is byte-identical between an independent signer and
  `bamepd`'s verifier for every operation/chunk-identity combination, with `bamepd` sourcing
  `artifact_id`/`direction` from the capability binding, not the request;
- a proof signed over a different `artifact_id`/`direction` than the capability is bound to
  fails closed with the generic denial;
- chunk upload: new-identity accept, resume-with-matching-digest idempotent accept,
  resume-with-mismatched-digest conflict, corrupted-bytes rejection, oversized-body rejection
  against the authoritative `chunk_size`;
- resume discovery reflects only durable state (never Worker-staged-but-uncommitted bytes)
  and survives Worker restart and reconnect; a held-chunk set larger than one Worker control
  frame is retrieved and reassembled across pages without omission, duplication, or a partial
  result on interruption;
- full-Artifact byte reconstruction (ascending `chunk_index`, raw concatenation, short final
  chunk) yields the digest compared against the sealed `artifact_digest`;
- seal: incomplete-manifest rejection, already-sealed idempotent retry, already-sealed
  conflicting-values rejection, and the two-step `PendingVerification` commit surviving a
  Worker crash between sealing and verification, with `bamepd` — not the Worker — comparing
  the reported full-Artifact digest and committing `Verified`/`Failed`;
- an unknown/unauthorized `transfer_id` returns the identical generic `401` used for every
  other denial reason, never a distinguishing status;
- durable acceptance/verification commits before the corresponding HTTP success response in
  every case, and a lost response after commit is idempotent on retry.

Transfer-authorization messages use the real Agent Protocol contract; Simulator paths must
not bypass the real authorization mechanism.

Issue #19 owns deterministic M1 implementation/fail-closed validation. Issue #21 owns
20–24 concurrent Simulated Endpoint scale validation, including chunked transfer.

## Related

- ADR-0008 — data-plane/storage design rationale.
- ADR-0018 — isolated Worker data-plane process boundary.
- `m0-agent-protocol-contract.md` — control-plane transfer-authorization messages and
  Worker-endpoint discovery.
- `m0-job-lifecycle-and-scheduling.md` — Attempt/reconciliation/destructive dispatch.
- `m0-endpoint-identity-lifecycle.md` — credential and target-disk safety semantics.
- `m0-persistence-observability-and-domain-events.md` — durability/correlation/audit.
- `m0-trusted-bootstrap-and-server-fingerprint-contract.md` — trust boundary, Server identity,
  and the canonical base64url encoding convention this Specification reuses.
- `m0-stack-and-boundaries-baseline.md` — `storage` Port.
- `m0-simulator-contract-and-validation-strategy.md` — Simulator fidelity.
- `m1-simulated-vertical-slice-and-baseline-validation.md` — RF-005 concrete M1 action and
  digest-algorithm/chunk-size communication.
- `m1-worker-data-plane-control-contract.md` — Server↔Worker UDS contract (`protocol_version
  "1"`) that authorizes each HTTP operation, relays the proof-transcript inputs, retrieves
  paginated durable resume state, and coordinates the seal and full-Artifact verification
  commits referenced above.
- `docs/reference/transfer-resumability-spike.md` — empirical resumability evidence.
