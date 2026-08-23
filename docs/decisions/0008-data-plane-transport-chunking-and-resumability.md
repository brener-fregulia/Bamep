# ADR-0008: Data-plane transport, chunking, and resumability strategy

Status: Accepted

## Context

Bamep needed architectural decisions for large Artifact transfer before implementation.

Issue #6 defined the data-plane/storage problem; Issue #9 supplied empirical resumability
evidence; Issue #15 later resolved transfer-session authentication.

The reusable Spike conclusion is retained in
`docs/reference/transfer-resumability-spike.md`: independently identified chunks allow
verification and selective retransmission, but cannot make changed source bytes reproduce a
previous capture.

Normative transfer, Artifact, authorization, restart, provenance, and storage behavior is
defined in `docs/specifications/m0-data-plane-and-storage-contracts.md`.

## Decision

### Separate control and data planes

Bulk Artifact bytes use HTTPS rather than the Agent Protocol WebSocket.

This keeps high-throughput transfer/backpressure/failure behavior independent from
safety-relevant control traffic such as action status, cancellation, and reconciliation.

HTTPS reuses the Server TLS identity already trusted by the Agent; no separate data-plane
Server trust anchor is introduced.

### Chunk-oriented transfer

Artifacts are transferred as independently verifiable chunks.

General raw byte-offset resume is rejected because it is only safe when the producer can
reproduce exactly the same bytes at the requested offset. Chunk identities provide explicit
integrity and retransmission boundaries.

The design applies in both directions:

- Agent -> Server capture;
- Server -> Agent restore/provisioning.

Chunk size and digest algorithm are not selected by this ADR. The Spike's 4 MiB/SHA-256
parameters were experiment choices, not architectural conclusions.

### Resumability depends on source reproducibility

Chunking detects changed/missing content; it does not solve source mutation.

If a previously identified chunk can no longer be reproduced exactly, it must fail rather
than receive a new identity under the same Artifact.

### V1 capture is offline

V1 backup/capture occurs in the maintenance environment while installed Windows is not
running and the relevant source is treated read-only.

This removes the normal concurrent writer and avoids requiring VSS/live snapshot machinery
for V1.

Offline capture establishes source stability during capture, not semantic filesystem or
application health.

Live-Windows capture remains outside V1.

### Integrity and capture consistency are separate

Cryptographic verification proves stored bytes match the Artifact identity.

Capture consistency proves the accepted source-stability conditions were established.

Neither implies the other. The normative states and destructive-use gates belong to the
data-plane Specification.

### Artifact completion is explicit

Incomplete or partially verified Artifact content must not be exposed as usable complete
content.

The exact Artifact lifecycle and manifest sealing rules belong to the Specification.

### Storage is capability-based

Storage Targets expose logical capabilities rather than physical layout.

Baseline roles are `SYSTEM`, `CACHE`, and `ARCHIVE`, and one target may expose multiple
roles.

Domain/Application logic must not depend on RAID layout, filesystem, or raw device names.

Verification and retention remain separate concerns.

### Volume/Image and Selective backup are distinct

Volume/Image is a linear byte-range capture.

Selective backup is file-granular in the baseline direction and may use chunking for large
files.

Per-file Selective behavior was not empirically validated by the transfer Spike and must
not be presented as a measured finding.

### Transfer authorization is sender-constrained

A data-plane request uses a short-lived Server-issued capability scoped to one transfer and
bound to an ephemeral Agent-held proof key.

A plain bearer token is rejected because possession alone would allow misuse if token bytes
were stolen.

The accepted capability:

- is issued through the authenticated Agent Protocol control plane;
- is scoped to the Endpoint/transfer/Artifact/direction/Attempt context;
- requires proof of possession;
- is revalidated against current durable authorization;
- uses bounded anti-replay state.

It is not the Agent runtime credential, an Endpoint identity credential, a client
certificate, or a persistent Endpoint key.

Detailed claims, proof fields, replay rules, TTLs, renewal, and failure behavior belong to
the Specification.

### Reuse existing trust; no mTLS/OAuth stack

The data plane reuses the pinned Server TLS identity.

Application-level proof of possession is preferred over introducing:

- a second client-certificate PKI;
- mTLS Agent identity;
- OAuth/OIDC;
- DPoP as a protocol dependency.

Those systems solve broader problems than Bamep's internal transfer authorization requires.

### Authorization lifetime is not transfer identity

Authorization may expire or be replaced while the same logical transfer continues.

Renewal/restart must not by itself create a new:

- `transfer_id`;
- Artifact;
- Attempt;
- destructive retry.

`transfer_id` identifies the durable logical transfer, not one HTTP request, connection,
capability, `action_id`, or `attempt_id`.

### Source provenance and target identity are separate

An Artifact records where its bytes came from.

A destructive operation separately authorizes the currently installed target disk.

These identities must not be forced equal: a valid workflow may capture an old disk,
replace it, revalidate a new disk, provision it, and restore retained data.

This does not weaken target-disk revalidation.

## Alternatives considered

### Plain byte-offset resume

Rejected as the general mechanism because a changed or regenerated stream may not reproduce
the original bytes at the same offset.

### One continuous unchunked stream

Rejected because it lacks independent integrity and selective retransmission boundaries.

### Continuous compression with arbitrary offset resume

Rejected as a general pattern. The Spike demonstrated the failure mode with gzip. A
seekable/framed or per-chunk representation may be used later.

### Live-source snapshot mechanism for V1

Rejected as unnecessary for the accepted offline maintenance workflow.

### Combined `Verified` + capture-consistency state

Rejected because byte integrity and source consistency are different facts.

### Bulk transfer over Agent Protocol WebSocket

Rejected because transfer backpressure/failure would be coupled to the control plane.

### Long-lived Agent credential for transfer authorization

Rejected as excessive authority and blast radius for a single transfer.

### Plain bearer transfer capability

Rejected because token possession alone does not prove the authorized sender.

### mTLS/client certificates

Rejected because it introduces a second persistent credential/PKI lifecycle for a narrower
problem.

### OAuth/OIDC/DPoP stack

Rejected because Bamep does not need a general delegated-authorization ecosystem.

### Source/target disk fingerprint equality

Rejected because it would break legitimate disk replacement and migration workflows.

## Consequences

- Data-plane bytes are separate from Agent Protocol.
- Chunk identities are the integrity/resume boundary.
- Resume is valid only while exact content is reproducible or staged.
- V1 capture is offline; live capture requires future design.
- Artifact integrity and capture consistency remain independent.
- Storage policy reasons about capabilities, not topology.
- Transfer authorization is least-authority, short-lived, sender-constrained, and
  fail-closed.
- Transfer authorization/session changes do not redefine logical transfer or Artifact
  identity.
- Source provenance does not authorize the destructive target.
- Detailed normative behavior is centralized in
  `docs/specifications/m0-data-plane-and-storage-contracts.md`.

## Related

- `docs/specifications/m0-data-plane-and-storage-contracts.md` — normative contract.
- `docs/specifications/m0-agent-protocol-contract.md` — transfer-authorization control
  messages.
- `docs/reference/transfer-resumability-spike.md` — empirical evidence.
- ADR-0005 — Agent control-plane transport.
- ADR-0006 — Attempt/reconciliation model.
- ADR-0010 / ADR-0011 — trusted Server bootstrap/site trust.
- Issue #19 — current M1 data-plane implementation Work Package.
