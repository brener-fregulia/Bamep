# M0 — Trusted Bootstrap and Server Fingerprint Delivery Contract

Status: **Approved**

This Specification defines the normative trusted-bootstrap contract between the boot boundary,
Agent, and Server. ADR-0010 owns the Secure Boot baseline rationale; ADR-0011 owns first-site
trust rationale; Agent Protocol owns the `BootstrapEvidence` wire shape; Endpoint identity owns
the authoritative current-boot lifecycle and destructive-operation precondition 7.

## Trusted-bootstrap model

`trusted bootstrap established` is a boot-scoped security fact, not a firmware-specific
`SecureBootEnabled` value.

Local establishment requires both:

1. executable boot-chain integrity under the production mechanism accepted by ADR-0010; and
2. an authenticated, fresh Bamep bootstrap assertion whose expected Server TLS fingerprint is
   verified under a legitimately established site trust anchor.

The Agent establishes this fact locally before WSS. The Server establishes its authoritative
current-boot fact only after an authenticated Agent reports `BootstrapEvidence` and the Server
independently validates the evidence and current-boot correlation.

These facts are independent from credential validity. `CredentialActive` neither establishes
nor implies trusted bootstrap, and trusted bootstrap does not authenticate an Agent credential.

Trusted bootstrap is valid for one genuine boot context. Same-boot reconnect does not require a
new assertion or a new local establishment; a genuine reboot requires a fresh `BootNonce`,
assertion, and Server-side establishment. There is no in-boot TTL.

The mechanism-independent current-boot state consumed by Domain is defined by
`m0-endpoint-identity-lifecycle.md`.

## Site trust-anchor establishment

The V1 default for a previously unprepared Endpoint is the operator-verified first-site-key
pairing contract selected by ADR-0011.

Before a site key becomes trusted:

1. the Endpoint is running inside the trusted maintenance/bootstrap executable path;
2. it obtains a candidate Bamep site public key through a transport that is not assumed trusted;
3. it derives a human-verifiable representation from that exact candidate key;
4. the legitimate Bamep installation independently derives/displays the representation of its
   own site public key through an operator-trusted management context;
5. the operator explicitly compares the two representations;
6. only successful explicit verification may persist the candidate as the site trust anchor;
7. mismatch, cancellation, ambiguity, or absent approval persists no candidate key and fails
   closed;
8. successful pairing survives ordinary reboot/reconnect until explicit reset, revocation,
   rotation, or recovery requires otherwise.

The human-verifiable representation is implementation-time, but it must provide meaningful
collision resistance against an active-network attacker. A bare unauthenticated "accept this
key?" prompt is insufficient.

Site-key approval and Endpoint enrollment approval are independent security decisions even when
one operator workflow presents both.

A changed candidate key never silently replaces an already-paired key. Rotation uses an
authenticated path under the existing trusted key where possible. If the prior key is
unavailable or compromised, recovery returns to explicit operator verification rather than
TOFU.

Bamep V1 does not claim cryptographically strong zero-touch first-site trust for an arbitrary
previously unprepared OEM Endpoint. MOK and direct UEFI `db`/PK enrollment remain validated
possible future pre-provisioned trust modes, not V1 default requirements.

## Bootstrap assertion V1

The V1 authenticated/fresh bootstrap material is a nonce-bound signed bootstrap assertion.

A static signed manifest is insufficient because it does not structurally bind valid material
to the current boot.

For each genuine new boot:

1. the trusted bootstrap stage generates a fresh `BootNonce`;
2. it obtains an assertion through a transport that need not itself be trusted;
3. the signer signs exactly the fixed V1 transcript below and must not act as an
   arbitrary-byte signing oracle;
4. the bootstrap stage strictly parses and verifies the assertion against an already-trusted
   site key and the exact current nonce;
5. only successful verification makes the assertion's Server fingerprint available for WSS
   pinning.

Replay under a different `BootNonce`, an unknown signer, a malformed/non-canonical assertion,
or a bad signature fails closed.

### Server certificate fingerprint

`ServerCertFingerprint` is exactly the SHA-256 digest of the exact DER bytes of the
leaf/end-entity certificate presented by the Server for the WSS connection: 32 bytes.

It is:

- not an SPKI/public-key pin;
- not a CA certificate fingerprint;
- not a certificate-chain fingerprint;
- not hostname-derived identity.

A different leaf certificate therefore has a different fingerprint even if it reuses the same
key pair.

This SHA-256 definition is scoped only to the Agent Protocol V1 Server certificate fingerprint;
it does not select a digest algorithm for Artifact/data-plane integrity.

### BootNonce V1

`BootNonce` is exactly 32 random bytes generated fresh for every genuine boot context from the
operating-system CSPRNG.

- CSPRNG failure aborts creation of that boot context; there is no insecure fallback.
- Same-boot reconnect retains the same nonce.
- Genuine reboot requires a new nonce.
- `BootNonce` is not a UUID.

When carried as Agent Protocol text, the only valid representation is RFC 4648 base64url without
padding over the exact 32 bytes: exactly 43 ASCII characters.

Parsing is strict. Reject padding (`=`), the standard-base64 `+`/`/` alphabet, whitespace,
wrong length, non-canonical trailing bits, or any value that does not round-trip byte-for-byte
through the canonical encoder.

### Site signing key and SiteKeyId

Assertion V1 uses ordinary Ed25519:

- no prehash mode;
- no optional Ed25519 context mode;
- no algorithm negotiation inside V1;
- verification must use strict Ed25519 semantics rejecting non-canonical/problematic
  representations and weak-key cases where supported.

The concrete cryptographic library/version is implementation-time.

The site bootstrap signing key is distinct from:

- the Agent Protocol TLS Server key;
- UEFI/Secure Boot `db`/PK keys;
- enrollment/runtime credential secrets.

`SiteKeyId` is exactly:

```text
SHA-256(exact raw 32-byte Ed25519 public-key value)
```

SPKI, PEM, DER wrappers, and human-readable text are not hashed. `SiteKeyId` is exactly 32
bytes and is included in the signed transcript.

Verification must use `SiteKeyId` only to select a previously accepted site public key, recompute
the ID from that accepted key's raw bytes, require exact equality, and then verify the signature.
An assertion can never introduce or automatically trust a new site key.

### Exact signed representation

The V1 signed payload is exactly:

```text
u16be(33)
|| ASCII("bamep.trusted-bootstrap.assertion")
|| u16be(1)
|| boot_nonce[32]
|| expected_server_fingerprint[32]
|| site_key_id[32]
```

| Offset | Exact content |
| --- | --- |
| `0..2` | domain length, unsigned 16-bit big-endian, value `33` |
| `2..35` | exact 33-byte ASCII `bamep.trusted-bootstrap.assertion` |
| `35..37` | schema version, unsigned 16-bit big-endian, value `1` |
| `37..69` | `BootNonce` raw 32 bytes |
| `69..101` | expected Server TLS leaf-certificate fingerprint raw 32 bytes |
| `101..133` | `SiteKeyId` raw 32 bytes |

The signed payload is exactly 133 bytes. The complete assertion is exactly:

```text
signed_payload_v1[133] || ed25519_signature[64]
```

Total length: **197 bytes**.

V1 has no optional/default fields, JSON, CBOR, variable-width numbers, structural padding,
algorithm field, trailing bytes, or Unicode other than the fixed ASCII discriminator. Unknown
schema versions are rejected. A future version defines a separate complete transcript; V1 is
never best-effort parsed as another version.

### Agent Protocol carrier

`BootstrapEvidence.bootstrap_assertion` is opaque to Agent Protocol.

Assertion V1 is carried as canonical RFC 4648 base64url without padding over the exact 197
assertion bytes: exactly **263 ASCII characters**.

Parsing rejects padding, `+`/`/`, whitespace, wrong length, non-canonical trailing bits, and
every representation that does not round-trip byte-for-byte through the canonical encoder.
Assertion-internal fields are not duplicated as Agent Protocol JSON fields.

### Local verification result

Successful local cryptographic verification must produce a trusted result that carries, at
minimum:

- the verified `BootNonce`;
- the authenticated `ServerCertFingerprint`;
- the original signed assertion material needed for Server-side evidence.

The normal Agent path must consume this verified result to obtain the WSS pin and subsequent
evidence. An unchecked boolean such as `trusted = true`, an unchecked constructor, or equivalent
bypass must not substitute for cryptographic verification.

### Confidentiality boundary

The assertion provides authenticity/integrity, not confidentiality. The Server certificate
fingerprint is not treated as secret.

Assertion V1 contains no enrollment/pre-authorization secret. A future extension carrying
bearer-secret or otherwise confidential bootstrap context must define its own confidentiality
and binding requirements in a new complete contract/version.

## Server-side BootstrapEvidence

`BootstrapEvidence` is an authenticated Agent report, not hardware-backed remote attestation.

The Server MUST NOT infer trusted bootstrap merely from:

- TCP/WSS connectivity;
- TLS fingerprint match alone;
- successful Agent credential authentication;
- possession of a syntactically valid assertion.

After `SessionEstablished`, Server-side establishment requires all of the following:

1. `BootstrapEvidence` is valid for the authenticated-session phase and uses a supported
   `protocol_version`;
2. `local_boot_trust` is exactly `Established`;
3. `boot_nonce` and `bootstrap_assertion` decode in their strict canonical forms;
4. the assertion has the exact V1 length, layout, discriminator, and schema version;
5. the wire `BootNonce` exactly equals the signed assertion `BootNonce`;
6. `SiteKeyId` selects an already accepted site public key and its recomputed ID exactly
   matches;
7. the strict Ed25519 signature verifies;
8. the assertion's expected Server fingerprint exactly equals the fingerprint of the leaf
   certificate used to establish **this WSS connection**;
9. immediately before mutation, the Server atomically revalidates that the Endpoint's
   authoritative current boot is still the exact `BootContext`/`BootNonce` represented by the
   evidence.

The TLS comparison authority is the immutable connection-bound leaf-certificate identity, not a
later read of mutable Server certificate configuration.

Only successful verification for the exact authoritative current boot may establish the
Server-side trusted-bootstrap state. Repeated valid evidence for that same current boot is
idempotent. Rejected, stale, or historical-boot evidence makes no trust-state mutation.

Evidence for an old boot can never establish a newer boot even if its assertion remains
cryptographically valid. Current-boot lifecycle and persistence ordering are authoritative in
`m0-endpoint-identity-lifecycle.md` and
`m0-persistence-observability-and-domain-events.md`.

Same-boot reconnect does not require evidence to be re-presented, but re-presentation is allowed
and must reverify idempotently.

## Assurance boundary and Agent integrity

Server verification proves that:

- the assertion was signed by an accepted site key;
- its authenticated fields are intact;
- it is bound to the reported/current boot nonce;
- its fingerprint matches the certificate of the current WSS connection.

It does **not independently prove** that firmware Secure Boot actually ran, that the intended
executable chain executed, or that a fully compromised Endpoint did not run a counterfeit Agent
capable of fabricating a report.

M0 protects against an untrusted provisioning network, Server/fingerprint substitution,
bootstrap-material tampering/replay, and stale/accidental bootstrap context. It does not claim
cryptographic remote attestation against a malicious fully compromised Endpoint. TPM/measured
boot or equivalent hardware-backed attestation is not an M0 requirement.

This assurance limitation does not permit bypassing ADR-0010. Production still requires the
trusted executable-bootstrap chain.

For `BootstrapEvidence` to be meaningful, the production trust chain must extend through the
Agent code and logic that generates the report. The exact Agent-integrity packaging mechanism
(GRUB/UKI/WinPE/initramfs or another validated design) is implementation/integration work.

## Rotation, revocation, and recovery

- Routine Server TLS certificate rotation does not require site trust-anchor rotation. New
  assertions carry the new authenticated leaf fingerprint under the still-valid site key.
- A boot context already established under valid material may finish under that material; a
  genuine new boot uses current material. Exact overlap duration is implementation-time.
- Site signing/trust-anchor rotation uses an authenticated path under the existing trusted key
  where possible. Compromised/unavailable-key recovery returns to explicit operator
  verification.
- Revoked/compromised bootstrap material or key fails closed.
- Rotation never introduces TOFU or silent replacement by an unverified fingerprint/key.
- Production Server certificate/key identity must not be regenerated merely because the Server
  process restarts; changing the leaf certificate is an explicit rotation event.

Fixture-driven tests may use ephemeral certificates only when their trusted-bootstrap material
authenticates the matching test certificate.

## Bootstrap sequence

```text
1. The production executable trust chain reaches the trusted bootstrap stage and Agent.
2. The bootstrap stage creates the current BootNonce and obtains the signed assertion.
3. The Agent verifies assertion structure, accepted site key, signature, and exact nonce.
4. Local trusted bootstrap becomes Established; the verified assertion supplies the WSS pin.
5. The Agent opens WSS and verifies the presented leaf certificate against that pin.
6. Agent Protocol credential authentication completes.
7. After SessionEstablished, the Agent sends BootstrapEvidence.
8. The Server independently verifies the evidence and exact current-boot correlation.
9. Only then may Server-side trusted bootstrap become Established for that current boot.
```

Steps 5–7 use Agent Protocol semantics owned by `m0-agent-protocol-contract.md`.

## Failure semantics

- If local trusted-bootstrap verification fails, the Agent must not proceed to normal WSS/Agent
  Protocol authentication under another trust assumption. No fallback and no TOFU.
- A TLS fingerprint mismatch follows the Agent Protocol fail-closed contract.
- Missing valid `BootstrapEvidence` leaves Server-side trusted bootstrap `NotEstablished`.
  It does not undo already successful credential authentication and does not by itself block
  permitted non-destructive activity.
- `SessionEstablished` is final credential authentication, not a provisional state waiting for
  trusted-bootstrap evidence.
- Missing evidence causes no timeout-driven transition to `Established` or another inferred
  trust state.
- A malformed post-auth Agent Protocol message follows the generic protocol violation behavior
  owned by the Agent Protocol Specification.
- A syntactically valid but rejected assertion/evidence produces no `AuthError`, evidence-
  specific acknowledgement, or detailed verification oracle; trusted bootstrap simply remains
  `NotEstablished`.
- Wrong signer, invalid signature, nonce mismatch/replay, fingerprint mismatch, revoked trust,
  stale/historical boot, or failed current-boot revalidation all fail closed.

Destructive execution remains governed by the complete gate in
`m0-endpoint-identity-lifecycle.md`; this Specification does not duplicate that list.

## Simulator contract

The Simulator uses real WSS, leaf-certificate pinning, Agent Protocol authentication, and
`BootstrapEvidence`. It may fake only the physical production boot/pairing mechanism.

Required fixture semantics:

- **Positive:** valid nonce-bound assertion signed by the fixture site key, matching the real
  test Server leaf certificate, plus authenticated `BootstrapEvidence` with
  `local_boot_trust: Established` and the matching nonce.
- **Wrong signer/signature:** rejected before normal WSS when local verification fails.
- **Nonce mismatch/replay:** rejected; an assertion for another boot cannot establish this one.
- **Absent evidence:** `SessionEstablished` may succeed, but Server-side trusted bootstrap stays
  `NotEstablished`.
- **Independent safety gate:** the scenario where all other six destructive preconditions hold
  but trusted current bootstrap does not must remain denied.
- **Structural gate:** the ordinary Simulator path obtains the WSS pin/evidence only from the
  successful typed cryptographic-verification result; no unchecked trusted flag/bypass.
- **Key separation:** fixture signing owns the Ed25519 private key; the Simulated Agent receives
  only accepted site public key(s), never the signing key.

The Simulator does not prove real Secure Boot, real operator pairing, firmware behavior, or real
Agent-integrity packaging.

## Validation

Contract-specific validation must cover at least:

- exact V1 transcript offsets, lengths, discriminator, schema version, and canonical carrier;
- strict `BootNonce` parsing/canonicalization and fresh-boot semantics;
- valid accepted-site-key assertion verification;
- unknown/wrong signer and corrupted signature rejection;
- nonce mismatch/replay rejection;
- Server leaf-fingerprint mismatch rejection;
- missing evidence preserving `NotEstablished`;
- stale/historical boot evidence unable to establish the current boot;
- repeated valid current-boot evidence remaining idempotent;
- independent failure of local verification and Server-side verification;
- Simulator positive/negative cases above.

Real production Secure Boot, operator pairing, physical firmware behavior, and Agent-integrity
packaging require Integration Environment validation according to
`docs/development/testing.md`.

## Out of scope

- concrete PXE/network bootstrap-delivery transport or bootloader selection;
- production Secure Boot deployment/configuration and firmware enrollment tooling;
- human-verifiable pairing representation/encoding/UX beyond its security requirement;
- paired-key local storage format and concrete rotation/recovery UX/protocol;
- cryptographic library/version;
- exact material/key rotation overlap duration;
- concrete Simulator fixture file/configuration technique;
- concrete Agent-integrity packaging mechanism;
- future confidential/pre-authorized enrollment-bootstrap extension;
- hardware-backed remote attestation;
- adopting MOK or direct UEFI `db`/PK as a supported optional mode.

## Related

- ADR-0010 — Secure Boot and mechanism-independent trusted-bootstrap rationale.
- ADR-0011 — operator-verified first-site-key pairing rationale.
- `docs/specifications/m0-agent-protocol-contract.md` — WSS, TLS pinning, authentication, and
  `BootstrapEvidence` wire contract.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — authoritative current boot and
  destructive-operation gate.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — durable current-boot
  and atomic mutation ordering.
- `docs/specifications/m0-simulator-contract-and-validation-strategy.md` — generic Simulator
  fidelity boundary.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Boot Port/Adapter boundary.
- `docs/reference/secure-boot-hardened-chain-spike.md` — executable-chain evidence.
- `docs/reference/site-trust-anchor-provisioning-spike.md` — site-trust provisioning evidence.
