# ADR-0004: Endpoint identity and enrollment/trust bootstrap model

Status: Accepted

## Context

Bamep needs a durable Endpoint identity that survives normal hardware changes without
treating MAC addresses or other inventory signals as authentication.

The provisioning LAN is controlled operationally but is not a trust anchor.

Normative identity, credential, hardware-confidence, current-boot, and destructive-safety
behavior belongs to `docs/specifications/m0-endpoint-identity-lifecycle.md`.

## Decision

### Server-assigned durable identity

Endpoint identity is a Server-assigned identifier independent from MAC address, NIC, disk
fingerprint, DMI/SMBIOS data, IP address, or current connection.

Hardware/network observations are evidence attached to that identity. They may support
inventory, continuity assessment, confidence, and target-disk revalidation, but never
replace authentication.

A legitimate component replacement therefore need not create a new Endpoint identity.

### Boot-scoped enrollment bootstrap

A new boot receives a short-lived enrollment credential/context through the
boot-orchestration boundary.

This avoids requiring a permanent per-device secret to be installed manually before first
network boot.

Credential representation, lookup, BootContext correlation, rotation, recovery, and expiry
were refined later by ADR-0012 and ADR-0014 and are not redefined here.

### Authenticate the Server before presenting credentials

The Agent must establish the expected Server identity before sending enrollment/runtime
credentials.

Provisioning-network reachability alone is insufficient Server authentication. The concrete
trusted-bootstrap and Agent Protocol mechanisms are defined by their own decisions/contracts.

### Operator-gated first enrollment

Successful first authentication does not automatically create a trusted Endpoint.

A first-seen Endpoint enters the pending-enrollment path and requires an explicit durable
operator approval before becoming `Enrolled`.

PXE reachability, a Bamep boot artifact, plausible inventory, or a valid boot-scoped
credential do not themselves authorize durable trust.

### Enrollment approval is durable identity trust, not recurring session trust

Once explicitly enrolled, an Endpoint does not require repeated operator approval on every
reconnect/reboot when the current continuity rules permit the same identity.

Previous approval does not bypass current credential, hardware-confidence,
trusted-bootstrap, or other safety checks.

### Hardware change affects confidence, not identity silently

Hardware divergence must not silently rewrite trusted evidence.

Legitimate replacement may preserve Endpoint identity while lowering/breaking confidence
until the applicable revalidation/operator process resolves it.

The exact confidence states and transitions belong to the Endpoint identity Specification.

### Future pre-authorization remains explicit operator authorization

A future workflow may allow an operator to authorize enrollment before first contact.

That moves the explicit decision earlier; it does not create unrestricted automatic
enrollment and is not a separate Endpoint identity state.

### Endpoint identity is only one destructive-safety fact

`Enrolled` identity is necessary where required, but never sufficient on its own for a
destructive operation.

The complete current precondition set belongs to
`m0-endpoint-identity-lifecycle.md` and is composed at dispatch by
`m0-job-lifecycle-and-scheduling.md`.

This ADR intentionally does not copy that list.

## Alternatives considered

### MAC or hardware fingerprint as identity

Rejected. MAC addresses and hardware components can change legitimately and can be
duplicated/spoofed. Hardware fingerprints remain continuity evidence, not permanent
identity.

### Automatic trust from provisioning-network access

Rejected. The provisioning LAN is not a trust anchor.

### Automatic re-trust from matching hardware

Rejected. Matching evidence supports continuity but does not authenticate the Agent or
current boot.

### Pre-shared per-device secret

Rejected as the V1 default because it restores a manual per-device provisioning step. It
may still suit a future higher-assurance deployment profile.

### Per-Agent mTLS/client-certificate PKI

Rejected as the V1 baseline because it introduces a second certificate lifecycle for
issuance, rotation, revocation, recovery, and operator management without a current
requirement.

### TPM/hardware-rooted attestation

Rejected as a V1 requirement because suitable hardware cannot be assumed across the target
endpoint population.

## Consequences

- Endpoint identity survives legitimate NIC/MAC and other component replacement.
- Hardware fingerprints remain evidence, never authentication.
- First enrollment requires explicit operator approval.
- An Agent cannot approve its own enrollment.
- Boot orchestration participates in the security boundary by issuing boot-scoped enrollment
  context.
- Existing enrollment approval does not waive later authentication or destructive-safety
  checks.
- Hardware discrepancies are surfaced through confidence/revalidation instead of silently
  rewriting trust.
- Later credential, BootContext, trusted-bootstrap, and protocol decisions refine this model
  without replacing its durable identity decision.

## Related

- `docs/specifications/m0-endpoint-identity-lifecycle.md` — normative identity lifecycle and
  safety contract.
- `docs/specifications/m0-agent-protocol-contract.md` — Agent authentication/session wire
  contract.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — Server
  trust/bootstrap contract.
- ADR-0010 / ADR-0011 — trusted bootstrap and site trust anchor.
- ADR-0012 — runtime credential lifecycle.
- ADR-0014 — credential lookup and BootContext correlation.
- Issue #17 — completed M1 trust/enrollment/session slice.
