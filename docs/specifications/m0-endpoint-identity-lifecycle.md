# M0 — Endpoint Identity Lifecycle

Status: **Approved**

This Specification defines the normative Endpoint identity, credential, hardware-confidence, current-boot, reconnect, and destructive-operation safety model. Decision rationale belongs to ADR-0004, ADR-0010, ADR-0012, and ADR-0014.

## Identity model

- Durable Endpoint identity is Server-assigned and independent of MAC address or hardware.
- MAC addresses, disk fingerprints, serials, and other inventory signals are evidence attached to an Endpoint; they are never identity or authentication.
- First enrollment is operator-approval-gated by default.

## Independent state dimensions

Endpoint readiness is represented by four independent dimensions. None may be inferred from another.

### 1. Persistent Endpoint identity

States:
- **(no record)** — no persisted Endpoint exists.
- **PendingEnrollment** — first credential exchange succeeded, but explicit trust approval has not occurred.
- **Enrolled** — explicit operator approval established durable Endpoint identity trust.
- **Retired** — explicitly decommissioned; no further Jobs may target the Endpoint.

Transitions:
- `(no record) -> PendingEnrollment` on normal first successful enrollment-credential redemption.
- `(no record) -> Enrolled` only through a valid future pre-authorized enrollment flow.
- `PendingEnrollment -> Enrolled` only by explicit operator approval.
- `PendingEnrollment -> discarded` according to retention policy.
- `Enrolled -> Retired` only by explicit operator action.
- Reconnect, reboot, or credential renewal never returns `Enrolled` to `PendingEnrollment`.

### 2. Credential/session validity

States:
- **NoActiveCredential** — no runtime credential in the current chain is active.
- **CredentialActive** — at least one credential in the current chain is valid.
- **CredentialExpired** — no credential remains valid because its validity window elapsed.
- **CredentialRevoked** — the credential chain was explicitly invalidated.

Agent presence is independent from credential validity. A disconnected Endpoint may still be `CredentialActive`.

#### Credential chain

Every successful `AuthRequest` issues a fresh runtime credential:

```text
same boot:       E1 -> R1 -> R2 -> R3 -> ...
genuine reboot:  E2 -> fresh runtime chain -> ...
```

`E` is a boot-scoped enrollment credential; `R` values are runtime credentials. A genuine reboot obtains a new enrollment credential and starts a new boot-scoped chain. Durable Endpoint identity continuity does not depend on runtime credentials surviving reboot.

#### Credential lookup and BootContext

Credentials are self-locating, but lookup never authenticates.

A runtime credential contains a non-secret lookup identifier plus secret material. The identifier resolves an indexed persisted Endpoint credential chain; the secret is then verified against that chain's one-way verifier.

Before first successful redemption, an enrollment credential instead resolves a durable `BootContext` by `boot_context_id`. The BootContext contains at least:
- `boot_context_id`;
- the exact current 32-byte `boot_nonce`;
- a one-way verifier of the enrollment secret;
- `expires_at`;
- Server/Boot-Orchestration-observed correlation evidence;
- optional `resolved_endpoint_id`.

`boot_context_id` and `boot_nonce` are distinct: the former locates pre-authentication Server state; the latter belongs to trusted-bootstrap freshness/correlation.

Enrollment credentials are stateful `boot_context_id + high-entropy secret` values verified against the BootContext verifier. No installation-global enrollment signing key or plaintext credential persistence is required. BootContext must commit durably before its enrollment credential is delivered.

#### Enrollment-credential promotion

On first successful enrollment-credential redemption, one transaction commits the applicable:
- Endpoint identity/credential transition;
- required domain event/audit effects;
- `BootContext.resolved_endpoint_id`;
- normal persisted predecessor lookup mapping;
- selection of that BootContext/nonce as the authoritative current boot;
- current-boot trusted-bootstrap state initialized to `NotEstablished`.

After promotion, retries use the normal Endpoint credential index. Historical BootContext resolution does not make that boot current.

`BootContext.expires_at` governs only first successful redemption while unresolved. After promotion, normal predecessor expiry/grace rules govern the credential and may extend beyond the original BootContext expiry.

#### Rotation, confirmation, and recovery

For one chain:
- the valid set is bounded to one predecessor plus at most one current unconfirmed successor;
- issuing a successor does not immediately invalidate its predecessor;
- a successor is confirmed only when later presented in an `AuthRequest` and successfully authenticated;
- if a still-valid predecessor is presented while its successor remains unconfirmed, the Server supersedes that successor and mints a fresh replacement rather than reconstructing or redelivering the old secret;
- concurrent redemptions must serialize durably so only one successor remains current;
- an already-established session is not retroactively invalidated solely because its future reconnect credential was superseded.

Durable credential state commits before `SessionEstablished` is sent. Loss after commit is recovered through the predecessor/replacement model, not by reconstructing plaintext secrets. Routine rotation while the dimension remains `CredentialActive` is durable bookkeeping, not by itself a new lifecycle transition or domain event.

#### Revocation

`CredentialRevoked` invalidates every credential still valid in the chain and persists across disconnect, reconnect, and genuine reboot. A new enrollment credential does not clear revocation or establish a new runtime chain while the dimension remains `CredentialRevoked`.

Restoring `CredentialActive` requires a separate explicit authorized reactivation/recovery operation. Reactivation is independent from Endpoint enrollment approval, hardware-confidence resolution, and trusted-bootstrap establishment.

### 3. Hardware confidence

States:
- **Consistent** — observed evidence is consistent with the recorded Endpoint.
- **LoweredConfidence** — a meaningful hardware change requires review but does not by itself break identity continuity.
- **Conflict** — evidence is sufficiently inconsistent that trusted identity continuity cannot be assumed.

`LoweredConfidence` permits connection, authentication, credential renewal, inventory, and other non-destructive activity, but blocks destructive execution. `Conflict` blocks destructive execution and also breaks identity continuity for reconnect/renewal.

Returning to `Consistent` requires explicit operator review/confirmation or explicit revalidation; it is never silently rewritten from new observations. Exact thresholds and escalation policy are implementation-time.

### 4. Authoritative current boot

The Endpoint owns at most one authoritative current-boot projection:

```text
CurrentBoot {
    boot_context_id,
    boot_nonce,
    trusted_bootstrap: NotEstablished | Established
}
```

The projection may be absent only when legacy/unknown data cannot establish an authenticated current boot. Absence fails closed for operations requiring trusted bootstrap.

Rules:
- **First contact:** atomically select the redeemed BootContext/nonce and initialize `NotEstablished` before `SessionEstablished`.
- **Genuine reboot:** atomically replace the current BootContext/nonce and reset to `NotEstablished` before `SessionEstablished`.
- **Same-boot reconnect/rotation:** preserve the current-boot projection and trusted-bootstrap state.
- **Valid independently Server-verified evidence for the exact current boot:** `NotEstablished -> Established`.
- **Repeated valid evidence for the same current boot:** remain `Established` idempotently.
- **Rejected, stale, or historical-boot evidence:** no mutation.

Current-boot state is durable Domain state, not Agent presence or an open-session fact. Server restart and same-boot reconnect preserve it; evidence need not be re-presented on every same-boot reconnect.

Evidence from an older boot can never establish a newer current boot, even when correctly signed. Acceptance must recheck the exact authoritative `boot_context_id`/`boot_nonce` immediately before mutation. Trusted-bootstrap verification details belong to `m0-trusted-bootstrap-and-server-fingerprint-contract.md`.

## Future pre-authorized enrollment

A future flow may allow an operator to authorize enrollment before first contact. That authorization is separate from the Endpoint identity lifecycle. A valid pre-authorization may create the resulting Endpoint directly as `Enrolled` because operator approval occurred earlier.

It must never become unrestricted automatic enrollment. Token format, scope, expiry, issuance UX, and its own lifecycle are future work.

## Reconnect and credential renewal

Identity continuity for reconnect/renewal exists when:
- persistent identity is `Enrolled`; and
- hardware confidence is not `Conflict`.

`LoweredConfidence` does not require repeated operator approval and does not block reconnect, authentication, renewal, or non-destructive activity, although it still blocks destructive execution. A reconnect still authenticates and obtains a fresh runtime credential.

Identity continuity alone does not override credential state: `CredentialRevoked` independently blocks credential re-establishment.

Reconnect never implies that an interrupted destructive operation may be replayed or resumed. That decision belongs to the Job lifecycle contract.

## Destructive-operation authorization preconditions

Before any destructive operation executes, **all seven independent preconditions must hold**. None may be inferred from another:

1. **Trusted persistent Endpoint identity** — identity is `Enrolled`.
2. **Authenticated current Agent** — credential state is `CredentialActive` for the current authenticated Agent.
3. **Authorized Job/action** — the specific Job/action is authorized.
4. **Fresh inventory** — the authorized inventory revision matches the Endpoint's current inventory revision.
5. **Target-disk revalidation** — target disk/volume identity or fingerprint matches the authorized target immediately before execution.
6. **Consistent hardware confidence** — state is `Consistent`; both `LoweredConfidence` and `Conflict` fail this gate.
7. **Trusted current bootstrap** — an authoritative current boot exists and its trusted-bootstrap state is `Established` through independent Server verification.

Precondition 7 is independent from credential authentication: a valid Agent credential does not prove that the current boot path was trusted.

Failure of any precondition must block the destructive operation. No silent retry, override, or inference from another precondition is allowed.

The Job lifecycle Specification composes/revalidates this complete gate before destructive dispatch. Data-plane/Artifact-specific gates are additive and may not narrow it.

## Out of scope

- exact `LoweredConfidence`/`Conflict` thresholds and revalidation mechanics;
- numeric credential expiry/grace durations;
- future pre-authorized-enrollment design;
- concrete credential-reactivation workflow;
- Agent Protocol authentication wire mechanics;
- Job/action authorization and destructive resumption semantics;
- production enrollment UX;
- SQL schema, locking syntax, and expired BootContext cleanup policy.

## Validation

Implementations must cover at least:
- valid and rejected transitions for each independent dimension;
- combinations of identity, credential, confidence, and current-boot states;
- concurrent first BootContext redemption resolving to one Endpoint;
- predecessor replacement and superseded-successor rejection;
- revocation surviving reconnect/reboot;
- old-boot evidence unable to establish a newer current boot;
- destructive rejection for each missing precondition independently, including the case where preconditions 1–6 pass and only trusted current bootstrap is absent.

## Related

- ADR-0004 — Endpoint identity and enrollment bootstrap rationale.
- ADR-0010 — trusted-bootstrap/Secure Boot baseline rationale.
- ADR-0011 — site trust-anchor establishment.
- ADR-0012 — runtime credential rotation/recovery rationale.
- ADR-0014 — credential lookup and BootContext rationale.
- `docs/specifications/m0-agent-protocol-contract.md` — authentication/session wire contract.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — trusted-bootstrap verification contract.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — destructive dispatch composition.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — atomic persistence and persist-before-send.
