# ADR-0014: Agent credential lookup and BootContext correlation

Status: Accepted

## Context

ADR-0012 defines credential rotation once the Server knows which Endpoint credential chain
must validate a presented `AuthRequest{credential}`. It did not define how that opaque
credential locates the chain, or how a boot-scoped enrollment credential locates
Server-owned correlation state before an Endpoint record exists.

The accepted Agent Protocol wire shape carries no Endpoint identifier, MAC, inventory signal,
or separate lookup field. Lookup therefore has to be derivable from the opaque credential
without turning hardware/network evidence into authentication.

This ADR originally described a self-verifying HMAC-signed enrollment token backed by an
installation-global signing key. A later owner-approved amendment replaced that design:
because every unresolved enrollment credential already has durable pre-redemption
`BootContext` state, the credential can instead be stateful and verified against a one-way
verifier in that record. The final accepted decision below reflects that amendment.

Normative credential/BootContext lifecycle behavior belongs to
`docs/specifications/m0-endpoint-identity-lifecycle.md`.

## Decision

### Keep `AuthRequest{credential}` unchanged

Agent Protocol does not gain `endpoint_id`, `inventory_signal`, `boot_context_id`, MAC, or
another correlation field.

The credential remains opaque at the protocol boundary.

### Runtime credentials are self-locating

A runtime credential contains logically distinct:

- a non-secret lookup identifier;
- secret credential material.

The lookup identifier narrows the presented credential to one persisted Endpoint credential
chain through an indexed lookup. It never authenticates the Agent by itself; the secret must
still verify against that chain's stored one-way verifier.

Database-wide scanning of credential hashes is not part of the design.

Exact credential serialization remains implementation-time.

### Enrollment credentials use durable `BootContext`

Before first successful redemption, an enrollment credential does not yet belong to an
Endpoint credential chain.

Its non-secret lookup component is `boot_context_id`, which resolves a durable PostgreSQL
`BootContext`.

`BootContext` contains at least:

- `boot_context_id`;
- the exact trusted-bootstrap `boot_nonce` supplied for that boot;
- a one-way verifier of the enrollment secret;
- expiry;
- Server/Boot-Orchestration-observed correlation evidence;
- optional `resolved_endpoint_id`.

No plaintext enrollment secret is persisted.

Correlation evidence remains evidence only. It is not identity, authentication, or sufficient
trust.

### `boot_context_id` and `boot_nonce` remain distinct

`boot_context_id` exists to locate pre-authentication Server state.

`boot_nonce` belongs to trusted-bootstrap freshness/correlation and is later verified through
the trusted-bootstrap evidence path.

They coexist in the same `BootContext`, but one must not substitute for the other. Reusing
`boot_nonce` as credential lookup would couple independent security responsibilities without
adding authentication strength.

### Enrollment credentials are stateful, not globally signed

An unresolved enrollment credential is:

`boot_context_id + high-entropy secret`

Admission verifies the presented secret against the one-way verifier stored in the durable
`BootContext` and checks its unresolved expiry.

No installation-global enrollment signing key, signing-key persistence, rotation, backup, or
compromise domain is required by this design.

### Persist `BootContext` before delivering the credential

Boot Orchestration creates the identifier/secret, receives the current boot nonce, derives the
one-way verifier, and durably commits the `BootContext` before delivering the enrollment
credential.

Therefore:

- crash before commit means no valid credential was delivered;
- crash after commit but before delivery may leave an expiring orphan record;
- crash after delivery does not prevent later redemption after Server restart.

### Promote successful enrollment redemption into the normal chain

On first successful redemption, the enrollment credential becomes the predecessor in the
Endpoint's normal credential chain.

The successful transaction atomically commits the applicable:

- Endpoint identity/credential transition;
- required event/audit effects from the existing persistence contract;
- `BootContext.resolved_endpoint_id`;
- persisted predecessor lookup mapping;
- authoritative current `BootContext`/`boot_nonce`;
- trusted-bootstrap current-boot state initialized to `NotEstablished`.

After that commit, retry/authentication routes through the normal persisted credential index.
The historical `BootContext` record does not remain an independent authentication path and
does not determine which boot is currently authoritative.

### Promotion changes which expiry governs retry

`BootContext.expires_at` governs only the unresolved enrollment credential's first successful
redemption.

After promotion, the credential is a normal predecessor; its persisted credential-slot
expiry/grace semantics govern retries, even if they extend beyond the original
`BootContext.expires_at`.

Otherwise a lost first `SessionEstablished` could strand the Agent despite ADR-0012's
predecessor-recovery model.

### Routing is indexed and fail-closed

Conceptually:

1. parse credential kind and non-secret lookup identifier;
2. try the persisted Endpoint credential index;
3. if found, lock/serialize the owning Endpoint and verify the credential secret;
4. only for an enrollment credential not found there, locate its `BootContext`;
5. under the `BootContext` lock, re-read its state;
6. if already resolved, route to the existing Endpoint/normal chain;
7. if unresolved, verify secret and expiry, then perform first-contact/genuine-reboot
   resolution.

Lookup never substitutes for secret verification.

### Concurrent first redemption resolves once

Two requests may race before either commits.

After acquiring the `BootContext` lock, the implementation must re-read it. Only a still-
unresolved context may run first-contact/genuine-reboot resolution.

If `resolved_endpoint_id` is already present, the request uses that existing Endpoint and
must not create/resolve another one.

Specific SQL locking syntax is implementation-time.

### Retry after commit uses the normal chain

If promotion commits but `SessionEstablished` is lost, retry finds the promoted predecessor
in the Endpoint credential index and ADR-0012 recovery applies normally.

Later `BootContext` expiry or cleanup therefore cannot invalidate an already-promoted
credential solely because its original boot-context expiry elapsed.

### Existing reboot, revocation, and trust semantics remain independent

A genuine reboot creates a new `BootContext` and `boot_nonce`; successful resolution updates
the authoritative current boot according to the Endpoint Specification.

`CredentialRevoked` still blocks establishing a new credential chain until explicit recovery.

Neither hardware correlation nor resolving a `BootContext` establishes trusted bootstrap.

## Alternatives considered

### Add lookup fields to Agent Protocol

Rejected. A self-locating opaque credential solves routing without exposing internal
correlation concepts or changing `AuthRequest`.

### Scan every credential verifier

Rejected. Direct indexed lookup is simpler and avoids verification work proportional to all
persisted credentials.

### Use MAC/inventory evidence as authentication

Rejected. Hardware/network observations are continuity/correlation evidence only.

### Keep `BootContext` only in memory

Rejected. A Server restart between issuance and redemption would strand an otherwise valid
enrollment credential.

### Put correlation evidence in a stateless token

Rejected. Correlation evidence is Server-observed state and should remain Server-owned rather
than being trusted because a client returned it.

### Reuse `boot_nonce` as `boot_context_id`

Rejected because the values serve different security responsibilities and the nonce does not
add authentication strength to credential lookup.

### Installation-global HMAC-signed enrollment token

Superseded by the owner-approved amendment. Durable `BootContext` state makes a global signing
key unnecessary; a stateful identifier + secret with one-way verification has a smaller
secrets-management surface.

## Consequences

- Runtime credentials locate existing Endpoint chains directly through indexed non-secret IDs.
- Unresolved enrollment credentials locate durable Server-owned `BootContext` state.
- Promoted enrollment credentials converge onto the same normal lookup path as runtime
  credentials.
- `boot_context_id`, `boot_nonce`, hardware evidence, and credential secret retain separate
  responsibilities.
- Enrollment issuance and promotion remain restart-safe through persist-before-deliver and
  atomic persistence.
- No installation-global enrollment signing key is required.
- Exact token encoding, SQL schema/index definitions, lock mechanism, TTL values, and cleanup
  policy remain implementation concerns.

## Related

- ADR-0004 — Endpoint identity and enrollment bootstrap.
- ADR-0012 — runtime credential rotation/reconnect recovery.
- ADR-0013 — PostgreSQL persistence backend.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — normative credential and
  BootContext lifecycle.
- `docs/specifications/m0-agent-protocol-contract.md` — `AuthRequest` wire contract.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` —
  `boot_nonce` authority.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — atomic persistence
  and persist-before-send.
- Issue #17 — implementation history that surfaced the lookup/correlation gap.
