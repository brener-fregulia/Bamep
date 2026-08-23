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
for a manifest.

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

The concrete proof-key algorithm/encoding is implementation-time, but must be explicit and
interoperable.

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

### Per-request proof

Every HTTPS chunk request carries:

1. the capability; and
2. a fresh proof signed by its bound ephemeral private key.

The proof is domain-separated/versioned and binds at least:

- proof contract/version;
- exact capability identity/hash;
- HTTP operation/method;
- `transfer_id`;
- `artifact_id`;
- direction;
- exact chunk identity;
- unpredictable unique `proof_id`;
- `issued_at`.

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

- exact chunk size and digest algorithm;
- live-Windows/VSS capture;
- concrete mechanism establishing `capture_consistency = Established`;
- capability TTL and proof-freshness duration;
- concrete proof/capability algorithms and serialization beyond required bindings;
- HTTP header/status/framing details;
- planned hardware-change authorization workflow;
- exact Artifact provenance schema;
- final production backup/snapshot format;
- RAID/filesystem/device layout;
- database schema/index layout;
- retention-duration policy.

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

Transfer-authorization messages use the real Agent Protocol contract; Simulator paths must
not bypass the real authorization mechanism.

Issue #19 owns deterministic M1 implementation/fail-closed validation. Issue #21 owns
20–24 concurrent Simulated Endpoint scale validation, including chunked transfer.

## Related

- ADR-0008 — data-plane/storage design rationale.
- `m0-agent-protocol-contract.md` — control-plane transfer-authorization messages.
- `m0-job-lifecycle-and-scheduling.md` — Attempt/reconciliation/destructive dispatch.
- `m0-endpoint-identity-lifecycle.md` — credential and target-disk safety semantics.
- `m0-persistence-observability-and-domain-events.md` — durability/correlation/audit.
- `m0-trusted-bootstrap-and-server-fingerprint-contract.md` — trust boundary and Server identity.
- `m0-stack-and-boundaries-baseline.md` — `storage` Port.
- `m0-simulator-contract-and-validation-strategy.md` — Simulator fidelity.
- `docs/reference/transfer-resumability-spike.md` — empirical resumability evidence.
