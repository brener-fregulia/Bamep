# M0 — Data-Plane and Storage Contracts

Status: **Approved**

## Purpose and authority

This Specification is the normative contract for Bamep data-plane transfer, Artifact
integrity/lifecycle, transfer-session authorization, source consistency/provenance, and
Storage Target capabilities.

It defines **what implementations must preserve**.

It does not own:

- the architectural rationale for selecting these mechanisms — ADR-0008 owns that rationale;
- Agent Protocol wire encoding outside the transfer-authorization messages referenced here;
- Job/JobStep/Attempt lifecycle and destructive-dispatch base preconditions;
- Endpoint identity/credential/current-boot lifecycle;
- persistence-backend choice or SQL/schema implementation;
- current implementation structure;
- empirical evidence itself.

Reusable resumability evidence belongs to
`docs/reference/transfer-resumability-spike.md`.

## Data-plane boundary

Bulk Artifact bytes use a dedicated HTTPS data-plane channel.

They must not be transported over the Agent Protocol WebSocket control-plane connection.

The control plane may carry:

- transfer-related action/correlation metadata;
- `ActionProgress`;
- transfer-authorization request/grant/denial messages.

Bulk Artifact content remains on the data plane.

The HTTPS Server identity is the same Server TLS identity pinned through the trusted
bootstrap/site trust model. No second independent Server trust relationship is introduced.

The Agent does not authenticate the data plane through a client certificate; sender
constraint is provided by the application-level transfer authorization defined below.

## Chunk manifest

Every Artifact transferred over the data plane has a chunk manifest.

The manifest contains:

- `artifact_id` — identifies the Artifact;
- `digest_algorithm` — explicitly identifies the cryptographic digest algorithm used for
  chunk and Artifact digests;
- `chunk_size` — fixed for this manifest;
- `chunk_count` — fixed once the manifest is sealed;
- for each chunk:
  - `chunk_index`;
  - `size`;
  - `digest`;
- `artifact_digest` — the expected full-Artifact digest using `digest_algorithm`.

For Agent -> Server capture, the producer may compute the expected `artifact_digest`
incrementally while producing the logical Artifact. A second complete source pre-read is not
required merely to know the digest before transfer starts.

The concrete digest algorithm is not selected by this Specification.

The exact chunk size is implementation-time tuning.

Both values must nevertheless be explicit and stable wherever interoperability depends on
them; neither may be inferred implicitly by a contract participant.

Chunk identity metadata is durable domain data according to
`docs/specifications/m0-persistence-observability-and-domain-events.md`.

## Manifest construction and sealing

A manifest may be constructed while capture is in progress.

### Construction

While the Artifact is `Incomplete`:

- `digest_algorithm` is fixed before the first chunk identity using it is committed;
- as each logical chunk is produced, its `chunk_index`, `size`, and `digest` become durable
  manifest metadata;
- the expected `artifact_digest` may be accumulated incrementally.

### Sealing

Once every expected chunk has been identified/produced, the manifest is sealed.

After sealing:

- `digest_algorithm` is immutable;
- `artifact_digest` is immutable;
- `chunk_count` is immutable;
- the complete set of chunk identities is immutable;
- no chunk identity may be added, removed, or rewritten.

An Artifact may reach `PendingVerification` only when:

- the manifest is sealed; and
- every sealed chunk has been durably received and verified against its recorded digest.

### Capture continuation versus resume/retransmission

These are distinct operations.

**Capture continuation** produces a chunk whose identity does not yet exist and extends an
unsealed manifest.

**Resume/retransmission** concerns a chunk whose identity already exists. The same logical
bytes must either:

- be reproduced from a source that can still reproduce them exactly; or
- be obtained from durable staging/materialization.

A previously recorded digest is never rewritten merely because the current source now
produces different bytes.

Verification must compare independently computed content against the recorded expected
identity. The verifier must never define or rewrite the expected value using the value it
is supposed to verify.

## Chunk transfer

Each chunk is an independently addressable transfer unit.

A chunk request/response carries enough identity to bind it to:

- the Artifact;
- the exact chunk;
- the transfer authorization context.

On receipt:

- the receiver verifies the chunk digest;
- a digest mismatch is rejected;
- mismatching bytes must not be persisted as a valid chunk.

Resume behavior is content-aware:

- if the receiver already holds a chunk at the required index whose digest matches the
  manifest, that chunk is retained and skipped;
- only missing or mismatching chunks are transferred again.

The same manifest/integrity pattern applies to:

- Agent -> Server capture/backup;
- Server -> Agent provisioning/restore.

## Transfer-session authentication

Every data-plane transfer requires a **short-lived, transfer-scoped,
sender-constrained capability**.

A capability is Server-issued, bound to an ephemeral Agent-held asymmetric proof key, and
delivered through the already-authenticated Agent Protocol control plane.

The capability alone is insufficient.

Possession of capability bytes without possession of the corresponding ephemeral private
proof key must not authorize a data-plane request.

### Ephemeral proof key

For a transfer-authorization context:

1. the authenticated Agent generates an asymmetric ephemeral keypair;
2. the private key remains Agent-local and non-durable;
3. the key is never an Endpoint identity credential;
4. the key is never persisted as Endpoint trust/identity state;
5. `TransferAuthorizationRequest` supplies the public key or a canonical representation
   from which its cryptographic thumbprint can be derived;
6. the granted capability is bound to that thumbprint.

The proof-key algorithm and representation must be explicit/interoperable when implemented,
but the concrete algorithm is not selected here.

The proof key's lifetime must not outlive the active transfer-authorization context.

### Authorization bindings

One transfer capability is bound to exactly the authorization context it was issued for,
including:

- `endpoint_id`;
- `transfer_id`;
- `artifact_id`;
- direction;
- the `attempt_id` whose transfer JobStep caused issuance;
- bounded expiry;
- the ephemeral proof-key thumbprint;
- a unique capability identity, or equivalent cryptographic identity, to which request
  proofs can bind.

A capability never authorizes:

- another Endpoint;
- another `transfer_id`;
- another Artifact;
- the opposite direction;
- a generic data-plane session.

`job_id`, `jobstep_id`, and `action_id` need not be duplicated into the capability merely
because they can be correlated through `attempt_id`.

### Issuance sequence

The normative sequence is:

```text
1. Agent authenticates over Agent Protocol and has an established session.
2. The owning transfer Attempt is dispatched through the normal Job/Attempt flow.
3. Agent acknowledges the action as required by Agent Protocol.
4. Agent creates or reuses an ephemeral proof key for this transfer.
5. Agent sends TransferAuthorizationRequest{transfer_id, proof_public_key}
   over the authenticated control plane.
6. Server verifies current authorization:
   - the transfer/Attempt exists and is eligible;
   - it belongs to the requesting Endpoint;
   - the Endpoint credential is CredentialActive;
   - other applicable durable authorization state permits continuation.
7. If authorized:
     TransferAuthorizationGrant{transfer_id, token, expires_at}
   otherwise:
     TransferAuthorizationDenied{transfer_id, reason}
8. Agent performs HTTPS chunk requests carrying both the capability and
   fresh proof of possession.
9. Expiry/loss of the authorization material may cause reauthorization
   for the same legitimate transfer; it does not create a new transfer,
   Artifact, or Attempt.
```

`TransferAuthorizationRequest`, `TransferAuthorizationGrant`, and
`TransferAuthorizationDenied` are Agent Protocol v1 messages whose wire contract belongs to
`docs/specifications/m0-agent-protocol-contract.md`.

Their existence does not reopen the established Agent Protocol transport/authentication
contract.

### Per-request proof of possession

Every HTTPS data-plane chunk request carries both:

1. the sender-constrained capability; and
2. a fresh proof signed by the bound ephemeral private key.

The proof is a fixed, domain-separated, versioned structure rather than an arbitrary
caller-selected byte string.

At minimum, the signed proof binds:

- proof-contract discriminator/version;
- capability identity or a cryptographic identity/hash of the exact capability presented;
- HTTP operation/method;
- `transfer_id`;
- `artifact_id`;
- direction;
- `chunk_index` or equivalent exact chunk identity;
- `proof_id` — cryptographically unpredictable and unique for the proof;
- `issued_at`.

Binding the proof to the exact capability prevents a proof created for one valid capability
from being paired with another.

Binding operation and chunk identity prevents a captured proof from being replayed for a
different operation or chunk.

### Per-request verification

For every data-plane chunk request, authorization succeeds only if **all** applicable checks
succeed.

The Server verifies:

- capability signature/integrity;
- capability expiry;
- exact capability scope;
- proof signature validity;
- proof-key match with the capability's bound thumbprint;
- proof-to-capability identity binding;
- operation/chunk binding;
- proof freshness;
- proof replay status;
- current durable transfer authorization;
- owning Attempt state;
- Artifact binding/state as applicable;
- Endpoint credential remains `CredentialActive`.

Cryptographic capability/proof verification may be stateless.

The complete authorization decision is **not** stateless:

- replay detection uses bounded transient runtime state;
- authorization additionally depends on current durable domain/security state.

### Fail-closed, non-enumerable denial

Any failed authorization check denies the request.

The requester receives one generic denial outcome that does not reveal which internal check
failed.

At minimum, denial covers:

- missing/malformed authorization;
- invalid capability signature;
- expired capability;
- wrong Endpoint;
- wrong `transfer_id`;
- wrong Artifact;
- wrong direction;
- wrong Server;
- missing proof;
- invalid proof signature;
- wrong proof key;
- proof bound to another capability;
- proof for another operation/chunk;
- stale proof;
- replayed `proof_id`;
- terminal transfer;
- owning Attempt closed `Indeterminate`;
- Endpoint credential no longer `CredentialActive`.

Internal diagnostic/audit information may record a more specific reason according to the
persistence/observability contract, but the data-plane denial remains non-enumerable.

Explicit `CredentialRevoked` invalidates further use of outstanding transfer capabilities
for that Endpoint even when their embedded expiry has not yet elapsed.

### Authorization does not define Artifact integrity

Proof of possession authorizes a request.

It does not verify Artifact bytes.

Capability renewal, capability expiry, or proof-key replacement must not:

- create a new Artifact;
- reset the manifest;
- invalidate a valid chunk;
- cause a valid chunk to be transferred again merely because authorization changed.

Chunk/Artifact digests remain the sole integrity contract for Artifact bytes.

### Replay and freshness

Each request proof carries a unique `proof_id`.

Requirements:

- proofs are accepted only within a bounded freshness window measured from `issued_at`;
- exact window duration is implementation-time;
- accepted `proof_id` values are retained in a bounded transient replay cache for at least
  the applicable acceptance window;
- reuse of an accepted `proof_id` fails closed.

The replay cache is runtime security state, not durable domain history.

Loss of that runtime state on restart must never cause old proofs/capabilities to regain
validity without reauthorization.

### Lifetime and scope

A capability is:

- single-Endpoint;
- single-`transfer_id`;
- single-Artifact;
- single-direction;
- bound to one proof-key thumbprint;
- short-lived;
- renewable/reissuable while the same durable transfer remains authorized.

The capability may cover multiple chunk requests within its validity window, but each
request requires a fresh proof.

Further use and renewal are denied when:

- the transfer is terminal; or
- the owning Attempt is terminal/closed in a state that does not permit continuation,
  including `Indeterminate` without a further authorized Attempt.

Exact capability TTL is implementation-time.

### Reconnect and restart behavior

#### WSS disconnect during an active HTTPS transfer

A transient control-plane disconnect alone:

- does not revoke `CredentialActive`;
- does not terminate the durable transfer;
- does not automatically invalidate an otherwise-valid capability;
- does not itself authorize continuation or renewal.

A still-valid capability plus matching proof key may continue only while every per-request
durable authorization check continues to pass.

If the capability expires, renewal requires the authenticated control-plane context again.

#### Agent reconnect

Normal Agent Protocol reconnect and Attempt reconciliation remain authoritative.

Data-plane authorization never substitutes for Attempt reconciliation.

#### Agent process restart

The ephemeral private proof key is intentionally lost.

Therefore the old capability is unusable by the restarted Agent.

After Agent Protocol reauthentication/reconciliation, a still-authorized transfer may:

- generate a new ephemeral keypair;
- receive a new capability;
- continue the same `transfer_id`;
- retain the same Artifact/manifest;
- retain already verified chunks.

The proof private key must not be persisted merely to avoid this flow.

#### Server restart

Outstanding capabilities whose replay-protection continuity cannot be guaranteed are
invalid after restart.

Before further use, the Agent must:

- re-establish the authenticated control-plane context;
- reconcile durable transfer/Attempt state;
- obtain new authorization if the durable state permits continuation.

The implementation must explicitly ensure pre-restart authorization cannot silently regain
validity after the transient replay cache is empty.

The concrete mechanism may be an authorization epoch, fresh signing context, or another
equivalent design; that mechanism is not selected here.

Reauthorization after restart does not create a new transfer, Artifact, or Attempt and does
not imply destructive retry.

#### `AwaitingReconciliation`

An owning Attempt in `AwaitingReconciliation` is not automatically terminal.

An existing/renewed transfer authorization may remain usable while the Attempt remains
eligible under the Job lifecycle contract.

Once the Attempt is closed `Indeterminate` or otherwise terminal without continuation
authorization, further transfer authorization is denied.

### Renewal

Renewal uses the same `transfer_id`.

It may:

- reuse a still-held ephemeral proof key; or
- bind a new ephemeral proof key.

Renewal must not:

- create a new `transfer_id`;
- create a new Artifact;
- discard verified chunks;
- reset the manifest;
- imply a new Attempt;
- imply destructive retry.

The Server re-evaluates current durable authorization state before every renewal.

### Relationship with Agent presence/session

The following are separate facts:

- authenticated Endpoint/credential state;
- current WebSocket presence;
- transfer authorization;
- durable transfer state.

A WSS disconnect is not credential revocation.

A transfer capability may remain usable during a transient disconnect only within its own
bounded lifetime and only while the complete per-request authorization remains valid.

Authorization never outlives:

- terminal transfer state;
- a disqualifying Attempt state;
- explicit credential revocation.

### Durable versus transient transfer-authorization state

Durable state includes the transfer authorization/correlation facts required for recovery
and authorization, including the applicable:

- `endpoint_id`;
- `transfer_id`;
- `artifact_id`;
- direction;
- `attempt_id`.

Durable persistence follows
`docs/specifications/m0-persistence-observability-and-domain-events.md`.

The Server capability-signing secret is operational security configuration whose concrete
storage mechanism is outside this Specification.

Where transfer authorization itself requires an audit record because it participates in a
safety-relevant destructive workflow, that audit behavior composes with the persistence
contract rather than defining a separate audit subsystem here.

Transient state includes:

- the issued short-lived capability itself;
- the Agent ephemeral proof keypair;
- request proofs;
- the bounded replay cache.

These must not be persisted as reusable durable domain credentials merely for convenience.

### Threat-model boundary

The mechanism protects against:

- passive provisioning-LAN capture through HTTPS;
- use of capability bytes without possession of the bound private proof key;
- cross-Endpoint substitution;
- cross-transfer/cross-Artifact/cross-direction use;
- replay of an already accepted proof;
- stale/revoked/terminal authorization use;
- confused-deputy mistakes covered by the explicit bindings.

It does not claim protection when an attacker compromises the authenticated Agent deeply
enough to obtain **both**:

- a valid capability; and
- its matching ephemeral private proof key.

This does not extend the assurance boundary beyond
`docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md`.

## Source reproducibility: V1 offline maintenance capture

If a previously identified chunk cannot be reproduced to match its recorded digest, the
chunk transfer fails.

The recorded digest is never rewritten to accept the changed source.

For V1, capture consistency is established through offline maintenance capture:

- the endpoint boots the Linux maintenance environment;
- the installed Windows OS is not running during capture;
- Volume/Image source disks/volumes are treated as non-destructive read sources;
- Selective source filesystems are accessed read-only;
- Bamep does not make the original source writable merely to perform the safety capture.

Snapshot/VSS/live-quiescing technology is not required for the normal V1 workflow.

Live backup while installed Windows remains running is outside V1 scope.

Offline capture establishes that no concurrent installed-OS writer changed the source
during capture.

It does **not** establish that:

- the filesystem was clean;
- the filesystem was not hibernated;
- application state was semantically healthy;
- the prior shutdown was clean.

## Capture/source-consistency fact

`Verified` means cryptographic integrity only.

Capture consistency is a separate durable fact:

`capture_consistency: NotApplicable | NotEstablished | Established`

- `NotApplicable` — the Artifact is not a mutable client-state capture for which source
  consistency is meaningful.
- `NotEstablished` — required offline/read-only source conditions were not positively
  established for the capture.
- `Established` — the maintenance workflow positively established those conditions for the
  capture duration.

`Established` means the bytes belong to a stable capture under the declared capture
conditions.

It does not mean the filesystem/application state was semantically healthy.

For an Artifact type whose destructive use requires capture consistency:

- `Verified` is required; and
- `capture_consistency == Established` is required.

A `Verified` Artifact with `NotEstablished` capture consistency cannot authorize the
dependent destructive operation.

The concrete component/mechanism that positively establishes this fact remains outside this
Specification.

## Artifact lifecycle

Artifact states are:

- `Incomplete`;
- `PendingVerification`;
- `Verified`;
- `Failed`.

Transitions:

- `(created) -> Incomplete`
- `Incomplete -> PendingVerification`
- `PendingVerification -> Verified`
- `PendingVerification -> Failed`
- `Incomplete -> Failed`

`Incomplete -> PendingVerification` requires:

- sealed manifest;
- every expected chunk present;
- every expected chunk individually verified.

`PendingVerification -> Verified` requires:

- independent full-Artifact digest computation;
- equality with the sealed expected `artifact_digest`.

The transition to `Verified` is atomic and is the point at which the Artifact becomes
usable/visible as complete.

A partially written or unverified Artifact must never be observed as complete.

`PendingVerification -> Failed` covers full-Artifact verification failure.

`Incomplete -> Failed` covers cases including:

- required chunk cannot be reproduced/verified;
- capture/transfer abandonment or cancellation according to the owning workflow.

`Verified` and `Failed` are terminal for that Artifact.

A `Failed` Artifact is not repaired/retried in place into a different Artifact identity.
A later authorized workflow attempt creates a new Artifact.

### Artifact atomicity

One Artifact is one atomic integrity/completeness unit.

Failure of any required chunk prevents the Artifact from becoming `Verified`.

There is no partial success inside one Artifact.

If Selective backup is later represented using multiple independent Artifacts, those
Artifacts may succeed/fail independently. Whether a workflow can accept partial success
across multiple independent Artifacts is a separate workflow-policy question.

## Artifact safety gates for destructive use

A destructive JobStep that depends on an Artifact must revalidate applicable Artifact facts
at final dispatch time.

At minimum:

- the Artifact is `Verified`;
- where that Artifact type requires capture consistency,
  `capture_consistency == Established`.

These are **additional** gates.

They never replace or narrow the complete destructive-operation safety gate owned by:

- `docs/specifications/m0-endpoint-identity-lifecycle.md`; and
- `docs/specifications/m0-job-lifecycle-and-scheduling.md`.

A valid Artifact alone never authorizes destructive execution.

## Artifact source provenance and multi-disk endpoints

An Endpoint is not modeled as having one implicit disk.

Bamep must represent endpoints with multiple disks/volumes/filesystems.

Artifact source provenance identifies the concrete source from which bytes were captured,
not merely the Endpoint.

The contract must be able to preserve the applicable source disk/volume/filesystem identity
distinct from Endpoint identity.

The exact provenance schema is not selected here.

## Source identity versus target-disk identity

Artifact source identity and destructive target-disk identity are independent facts.

A valid workflow may:

1. capture from an old disk;
2. replace the physical disk;
3. revalidate inventory;
4. authorize the new installed disk as the destructive target;
5. provision it;
6. restore retained data.

Restoring data must not require the destination fingerprint to equal the source Artifact's
source-disk fingerprint.

Source provenance answers:

> Where did these bytes come from?

Target-disk identity answers:

> Which currently installed disk is this destructive Job authorized to modify?

This contract does not weaken target-disk revalidation.

The target disk must still satisfy the Endpoint/Job destructive-dispatch contract
immediately before execution.

A future planned-hardware-change authorization mechanism must preserve this legitimate disk
replacement use case.

## `transfer_id`

`transfer_id` is the durable identity of one logical data-plane transfer.

It is distinct from:

- HTTP request identity;
- network connection identity;
- capability/proof identity;
- `attempt_id`;
- `action_id`.

One logical transfer may span:

- multiple HTTP requests;
- interrupted connections;
- authorization renewal;
- Agent reconnect;
- Server/Agent restart recovery when continuation remains authorized.

Those events do not inherently create a new `transfer_id`.

A new logical movement of an Artifact receives a new `transfer_id`, even when it moves the
same Artifact bytes.

Durable correlation with Endpoint/Attempt/Artifact state follows the persistence contract.

## Storage capability model

A Storage Target exposes logical capabilities rather than physical-layout assumptions.

At minimum it exposes:

- `roles` — a set drawn from `SYSTEM`, `CACHE`, `ARCHIVE`;
- available capacity;
- read/write characteristics relevant to scheduling.

One physical target may expose multiple roles.

Domain/Application code must not assume:

- RAID layout;
- filesystem;
- raw Linux device name.

Storage is reached through the `storage` Port defined by
`docs/specifications/m0-stack-and-boundaries-baseline.md`.

### Role semantics

**`SYSTEM`**

Storage required for Bamep's own operational durable state/configuration.

It is not implicitly the preferred bulk-Artifact target unless it also exposes an
appropriate Artifact role/capability.

**`CACHE`**

Optional working/staging/performance-oriented Artifact storage.

It may hold:

- `Incomplete` Artifacts;
- additional copies of completed Artifacts.

It must not be assumed to be the sole retained copy when an Artifact retention requirement
requires durable preservation.

**`ARCHIVE`**

Optional storage eligible for retained completed/`Verified` Artifacts.

### Verification and retention are independent

`Verified` is an Artifact content/integrity property.

Placement in `ARCHIVE` is a retention/placement property.

Neither implies the other.

Migration between roles, multi-copy consistency, and retention-duration policy are outside
this Specification.

## Volume/Image and Selective backup

### Volume/Image

Volume/Image capture is modeled as a linear disk/volume byte-range Artifact using the
chunking and Artifact lifecycle defined here.

### Selective

The current baseline direction is file-granular.

A selected file may be its own Artifact or an independently identified unit within a
selective-backup representation; a large file may itself use the chunk mechanism.

Per-file Selective backup was **not** empirically exercised by the resumability Spike.
This direction must not be cited as an experimental finding.

Selective capture remains subject to the same source-reproducibility and capture-consistency
rules as Volume/Image.

## Out of scope

This Specification does not select or define:

- exact chunk size;
- concrete `digest_algorithm`;
- live-Windows capture/snapshot/VSS mechanism;
- exact implementation that establishes `capture_consistency = Established`;
- exact transfer-capability TTL;
- exact proof-freshness window;
- concrete capability/proof signing algorithms;
- concrete proof/capability serialization beyond the contract fields/properties defined
  here;
- HTTP header names/status codes/framing beyond the chunk-addressable request model;
- planned hardware-change authorization workflow;
- exact Artifact provenance schema;
- final production backup/snapshot format;
- RAID/filesystem/device layout;
- database schema/index layout;
- retention-duration policy;
- telemetry/domain-event extensions not otherwise owned here.

## Validation expectations

Validation must exercise the normative behavior in this Specification.

### Unit/domain validation

At minimum:

- valid and rejected Artifact lifecycle transitions;
- manifest sealing invariants;
- chunk digest verification;
- full-Artifact digest verification;
- source mutation cannot rewrite a prior chunk identity;
- `Verified` and capture consistency remain independent facts.

### Data-transfer/component validation

At minimum:

- interrupted transfer with a partial chunk set;
- resume skips already valid chunks;
- corrupt chunk is detected and retransferred/rejected as applicable;
- duplicate valid chunk request is idempotent;
- storage exhaustion during chunk write fails safely;
- producer/consumer disconnect mid-chunk fails safely;
- `Incomplete` Artifact survives restart as `Incomplete`;
- atomic `PendingVerification -> Verified`;
- failed verification prevents destructive consumption.

### Security-negative validation

At minimum:

- valid capability + valid matching proof accepted when durable state authorizes;
- capability without proof rejected;
- wrong proof key rejected;
- stolen capability without the bound private key rejected;
- proof bound to another capability rejected;
- wrong operation/chunk proof rejected;
- replayed `proof_id` rejected;
- stale proof rejected;
- wrong Endpoint rejected;
- wrong `transfer_id` rejected;
- wrong Artifact rejected;
- wrong direction rejected;
- expired capability rejected;
- capability denied after `CredentialRevoked`;
- terminal/disallowed transfer state rejected;
- non-enumerable denial preserved.

### Renewal/restart validation

At minimum:

- legitimate renewal continues the same `transfer_id`;
- renewal retains already verified chunks;
- WSS reconnect alone neither grants nor revokes transfer authorization;
- Server restart invalidates pre-restart authorization when replay continuity cannot be
  guaranteed, then legitimate reauthorization continues the same transfer;
- Agent restart loses the old proof key/capability usability, then legitimate
  reauthorization continues the same transfer;
- none of those flows imply a new Artifact, Attempt, or destructive retry.

### Safety composition validation

At minimum:

- `Verified` + `capture_consistency = NotEstablished` cannot authorize the dependent
  destructive operation;
- a valid Artifact still cannot authorize destructive use when any independent base
  destructive precondition fails;
- source provenance from an old disk does not require equality with a legitimately
  revalidated replacement target disk.

### Agent Protocol contract validation

`TransferAuthorizationRequest`, `TransferAuthorizationGrant`, and
`TransferAuthorizationDenied` serialization/compatibility belongs to
`docs/specifications/m0-agent-protocol-contract.md`.

The integrated data-plane validation must use those real messages rather than a
Simulator-only bypass.

### M1 execution

Issue #19 (`[WP] Execute authenticated resumable simulated data-plane transfer`) owns
deterministic small-scale implementation/validation of the transfer contract and required
fail-closed scenarios.

Issue #21 (`[WP] Validate Simulator concurrency and M1 persistence baseline`) owns genuine
20–24 concurrent Simulated Endpoint validation of chunked data-plane transfer at scale.

The broader Simulator fidelity requirements belong to
`docs/specifications/m0-simulator-contract-and-validation-strategy.md`.

## Acceptance mapping

This Specification satisfies the durable M0 data-plane/storage contract by defining:

- chunk manifest/construction/sealing;
- resumable chunk transfer;
- transfer-session authentication;
- proof/replay/restart behavior;
- V1 source-consistency rules;
- capture-consistency semantics;
- Artifact lifecycle and destructive-use gates;
- `transfer_id`;
- source provenance / target identity separation;
- Storage Target capabilities;
- validation obligations.

Issues #6 and #15 are the historical M0 work that produced this contract.

## Related decisions, specifications, and evidence

- ADR-0008 — architectural rationale for data-plane transport, chunking, offline capture,
  storage capability modeling, transfer authorization, and identity separation.
- `docs/reference/transfer-resumability-spike.md` — empirical resumability/integrity
  evidence.
- `docs/specifications/m0-agent-protocol-contract.md` — transfer-authorization control
  messages.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — Attempt/reconciliation and
  destructive-dispatch semantics.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — credential state and
  target-disk safety semantics.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — durable/transient
  state and transfer correlation.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — trust
  boundary and Server identity semantics.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — `storage` Port and component
  boundaries.
- `docs/specifications/m0-simulator-contract-and-validation-strategy.md` — Simulator
  fidelity/validation contract.

## Related work

- Issue #6 — historical M0 data-plane/storage contract Work Package.
- Issue #9 — completed resumability Technical Spike.
- Issue #15 — completed transfer-session authentication Work Package.
- Issue #19 — current M1 implementation Work Package for authenticated resumable simulated
  transfer.
- Issue #21 — current M1 scale-validation Work Package.

Status: Approved.
