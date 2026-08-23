# ADR-0011: Site trust-anchor establishment — operator-verified first-key pairing

Status: Accepted

## Context

Bamep trusted bootstrap requires a previously unprepared Endpoint to learn which public key
legitimately represents its Bamep installation before it can trust signed bootstrap material.

The provisioning network cannot establish that trust by itself. ADR-0010 also rules out TOFU.

The site-trust-anchor spike validated two firmware-backed pre-provisioning mechanisms:

- **shim/MOK enrollment** works end to end, but requires a per-Endpoint interactive MokManager
  ceremony with physical/console-equivalent presence and two reboots for enrollment or
  revocation;
- **direct UEFI `db`/PK enrollment** also works end to end and has a better authenticated
  post-enrollment update path, but first enrollment requires legitimate access to UEFI Setup
  Mode; the spike found no generic unattended way to establish that prerequisite on an
  arbitrary previously-unprepared OEM Endpoint.

Neither mechanism demonstrated unattended first-site trust for the product's arbitrary-OEM
baseline. The empirical details remain in
`docs/reference/site-trust-anchor-provisioning-spike.md`.

## Decision

For Bamep V1, the default first-site trust mechanism for a previously unprepared Endpoint is
**operator-verified first-site-key pairing**.

A candidate site public key obtained through the provisioning path is not trusted merely
because it is the first key observed. The Endpoint and the legitimate Bamep installation
derive a human-verifiable representation of the same key, and the operator independently
compares and explicitly approves that representation before the key becomes trusted.

The comparison must provide meaningful collision resistance against the active-network-
attacker threat model. A bare unauthenticated "accept this key?" prompt is insufficient.

The exact representation, transport, encoding, UI, durable storage format, and recovery
mechanics are not selected by this ADR. Their normative security behavior is owned by
`docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md`.

### Relationship to Endpoint enrollment

Site-key verification and Endpoint enrollment are separate approvals:

- Endpoint enrollment answers whether this Endpoint identity is trusted;
- site-key pairing answers whether this public key represents the legitimate Bamep site.

They may share one operator workflow when practical, but neither fact may be inferred from the
other.

### Product boundary

Bamep V1 does **not** claim cryptographically strong zero-touch first-site trust on an
arbitrary previously-unprepared OEM Endpoint.

First trust requires operator verification unless the Endpoint has been pre-provisioned
through a separately supported trust mechanism. After site trust is established, ordinary
subsequent Bamep boots may be unattended.

MOK and direct UEFI `db`/PK enrollment remain validated possible future pre-provisioned modes
for managed environments; this ADR does not select either as a supported V1 mode.

## Alternatives considered

### shim/MOK enrollment as the V1 default

Rejected as the default because its mandatory per-Endpoint interactive ceremony and reboot
cost do not disappear with fleet size and recur for trust maintenance. It remains technically
viable where managed infrastructure can satisfy those prerequisites.

### Direct UEFI `db`/PK enrollment as the V1 default

Rejected as the default because arbitrary previously-unprepared OEM Endpoints cannot be
assumed to offer a generic unattended path into the required initial Setup Mode.

Its post-enrollment authenticated update model is stronger operationally than MOK's and remains
a viable future pre-provisioned option.

### Automatic trust-on-first-use

Rejected. An active network attacker could substitute the first observed key; network
position is not a trust anchor.

### Unverified confirmation prompt

Rejected. Asking an operator to accept an unauthenticated candidate without an independent
verifiable representation does not materially resist key substitution.

## Consequences

- V1 requires an explicit operator-facing first-site-key verification ceremony.
- Failure, ambiguity, mismatch, or absent approval cannot establish site trust.
- A previously trusted key cannot be silently replaced by a newly observed candidate.
- Endpoint enrollment approval remains independent from site-key approval.
- The trusted-bootstrap Specification owns pairing, persistence, reset, rotation, recovery, and
  fail-closed semantics.
- This decision does not change Agent Protocol or introduce remote attestation.

## Related

- ADR-0010 — Secure Boot baseline and mechanism-independent trusted-bootstrap rationale.
- ADR-0004 — Endpoint identity and enrollment rationale.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — normative
  site trust-anchor and trusted-bootstrap contract.
- `docs/reference/site-trust-anchor-provisioning-spike.md` — empirical MOK and UEFI `db`/PK
  evidence.
