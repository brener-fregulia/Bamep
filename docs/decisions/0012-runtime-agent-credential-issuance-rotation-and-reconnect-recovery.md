# ADR-0012: Runtime Agent credential issuance, rotation, and reconnect recovery

Status: Accepted

## Context

ADR-0004 required successful Agent authentication/reconnect to issue a **fresh** runtime
credential, but the original Agent Protocol handshake had no field or message that delivered
that credential to the Agent.

This created a crash/delivery problem: if the Server durably replaced the credential and the
connection dropped before the Agent received the successor, immediate predecessor
invalidation could strand the Agent.

Issue #17 surfaced an alternative interpretation where `CredentialActive` itself would
represent issuance and the Agent would reuse its original credential indefinitely. That was
rejected because it contradicted the accepted fresh-credential model.

Normative credential-chain lifecycle belongs to
`docs/specifications/m0-endpoint-identity-lifecycle.md`; Agent wire behavior belongs to
`docs/specifications/m0-agent-protocol-contract.md`. Credential lookup and BootContext
correlation are separate decisions in ADR-0014.

## Decision

### One boot-scoped credential chain

The same rotation model applies from first contact through reconnects:

```text
same boot:       E1 -> R1 -> R2 -> R3 -> ...
genuine reboot:  E2 -> fresh runtime chain -> ...
```

`E` is the boot-scoped enrollment credential; `R` values are runtime credentials.

Runtime credentials need not survive a genuine Agent reboot. Endpoint identity continuity is
independent from credential-chain continuity.

### Persist before delivery

Successful authentication commits the required durable credential/identity state before the
Server attempts `SessionEstablished`.

Database commit and WebSocket delivery are not atomic, so loss after commit is an expected
failure window that the credential model must tolerate.

### Retain a predecessor until successor confirmation

Issuing a successor does not immediately make the predecessor unusable.

A successor is confirmed only when it is later presented in an `AuthRequest` and
successfully authenticates. Until then, the predecessor remains usable within its bounded
grace/expiry rules.

This avoids stranding the Agent when `SessionEstablished` is lost after persistence.

### Replace, do not reconstruct, an unconfirmed successor

If the still-valid predecessor is presented again while its previous successor remains
unconfirmed, the Server supersedes that successor and mints a new one.

It does not reconstruct or redeliver the old secret.

The valid set is therefore bounded to one predecessor plus at most one current unconfirmed
successor.

### Concurrent redemption is serialized durably

Concurrent use of the same valid predecessor may race, but successor updates are serialized
by durable persistence; only the last committed successor remains current.

An already-established session is not retroactively invalidated merely because a later
concurrent redemption superseded the credential issued for its next reconnect.

Exact locking/isolation mechanics are implementation details.

### Revocation is Endpoint-level durable state

`CredentialRevoked` invalidates all currently valid credentials in the chain, not only the
latest value, and survives disconnect, reconnect, and genuine reboot.

A new boot-scoped enrollment credential does not itself clear revocation.

Restoring `CredentialActive` requires a separate explicit authorized recovery/reactivation
operation; its concrete mechanism is outside this ADR.

### Rotation does not require recoverable plaintext secrets

The Server never needs to store or reconstruct a previously issued plaintext runtime
credential.

The durable representation may use one-way verification or another suitable opaque
mechanism, provided the normative lifecycle contract is satisfied.

### Deliver the successor in `SessionEstablished`

Successful authentication returns:

`SessionEstablished{protocol_version, session_id, runtime_credential, credential_expires_at}`

A separate credential-delivery message is not introduced.

Bundling avoids creating an additional protocol phase such as "session established but
credential-delivery message pending." It does **not** make persistence and WebSocket
delivery atomic; predecessor/replacement recovery still handles loss of the complete
`SessionEstablished` message.

The exact wire contract is normative only in the Agent Protocol Specification.

## Alternatives considered

### Immediate predecessor invalidation

Rejected because a crash or disconnect after durable successor creation but before delivery
could leave the Agent with no usable credential.

### Redeliver the identical successor

Rejected as the baseline because it requires recoverable secret storage or deterministic
secret derivation, introducing a larger secrets-management responsibility and at-rest
exposure surface.

### Explicit credential-delivery acknowledgement

Rejected for the baseline because the next successful use of the successor already provides
confirmation without another protocol round trip/message type.

### Separate `RuntimeCredentialIssued` message

Rejected because issuance is 1:1 with successful session establishment and a second message
would add another partial-delivery phase without eliminating the underlying post-commit
delivery-loss window.

### Reuse the original credential indefinitely

Rejected because it collapses credential issuance into the `CredentialActive` state fact and
contradicts the accepted fresh-runtime-credential model.

## Consequences

- The Endpoint identity Specification owns the normative chain, grace, replacement,
  confirmation, concurrency, and revocation rules.
- The Agent Protocol Specification owns `SessionEstablished` wire fields and Agent-side
  retention/fallback behavior.
- Persistence follows the repository-wide persist-before-send contract.
- No recoverable-runtime-secret subsystem is required.
- Numeric credential TTL/grace values remain implementation-time policy.
- Revocation remains durable until an explicit recovery/reactivation operation clears it.

## Related

- ADR-0004 — Endpoint identity and enrollment bootstrap.
- ADR-0005 — Agent control-plane protocol.
- ADR-0014 — credential lookup and BootContext correlation.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — normative credential lifecycle.
- `docs/specifications/m0-agent-protocol-contract.md` — normative wire contract.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — persist-before-send.
- Issue #17 — M1 trust/enrollment/session implementation history.
