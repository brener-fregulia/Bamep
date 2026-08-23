# M0 — Agent Protocol Contract (Agent Protocol v1)

Status: **Approved**

This Specification is the authoritative, implementation-language-independent wire contract for Agent Protocol v1. ADR-0005 owns protocol rationale; Endpoint identity owns credential lifecycle; trusted-bootstrap owns assertion verification; Job lifecycle owns dispatch/retry/reconciliation policy; data-plane owns transfer-token use and bulk bytes.

## Transport and handshake

Agent Protocol v1 uses WebSocket over TLS (**WSS**) with **TLS 1.3 only**. Implementations use their TLS library's safe/default TLS 1.3 cipher suites; this Specification defines no custom cipher policy.

Server authentication is exact certificate pinning, not mTLS:

- the expected Server identity is the trusted-bootstrap `ServerCertFingerprint`: SHA-256 over the exact DER bytes of the Server's leaf/end-entity certificate;
- the Agent verifies that exact leaf fingerprint during TLS, before WebSocket Upgrade and before sending any Agent Protocol message;
- no Web/Public PKI, private CA, hostname/DNS certificate-identity validation, or X.509 validity-period check is an additional Server-identity authority in this exact-pin model;
- a self-signed leaf is therefore acceptable when its exact fingerprint is authenticated by trusted bootstrap;
- the TLS handshake must still verify proof of possession of the private key corresponding to that leaf certificate; accepting any certificate, skipping TLS signature verification, or checking the pin only after TLS/WebSocket establishment violates this contract;
- missing trustworthy bootstrap material or a fingerprint mismatch aborts locally at TLS with no Agent Protocol message, no TOFU, and no fallback.

The Agent authenticates with an enrollment/runtime credential after Server authentication; it presents no client certificate.

Handshake:

```text
trusted bootstrap available locally
  -> TCP
  -> TLS 1.3 + exact leaf pin verification
  -> WebSocket Upgrade
  -> AuthRequest{credential}
  -> SessionEstablished{protocol_version, session_id, runtime_credential, credential_expires_at}
     | AuthError{reason}
```

The Server durably commits authentication/credential state before attempting `SessionEstablished`, according to the persistence contract.

`AuthError` is only for Agent Protocol handshake/authentication failures after TLS Server authentication succeeded, including rejected credentials and incompatible `protocol_version`. TLS pin failure is never `AuthError`.

## Runtime credential wire behavior

Every successful `AuthRequest`, including reconnect, returns a fresh opaque `runtime_credential` and `credential_expires_at` in `SessionEstablished`. There is no separate credential-delivery message.

The Agent retains the credential used for authentication until the newly issued successor later authenticates successfully. If the successor is rejected while the predecessor remains valid under the Endpoint credential lifecycle, the Agent may retry with that predecessor.

Credential rejection may use a generic `AuthError`; the protocol must not reveal whether a credential was specifically a superseded successor.

Credential-chain confirmation, replacement, grace, concurrency, revocation, and secret representation are owned by `m0-endpoint-identity-lifecycle.md` and ADR-0012.

## Trusted bootstrap evidence

After `SessionEstablished`, the Agent may send:

`BootstrapEvidence{boot_nonce, bootstrap_assertion, local_boot_trust: Established}`

Rules:

- it is one-way and never part of the authentication handshake; sending it before `SessionEstablished` is a phase violation and cannot establish trust;
- `local_boot_trust` has one V1 wire value: `Established`;
- `boot_nonce` is the canonical 43-character RFC 4648 base64url-without-padding encoding of exactly 32 bytes;
- `bootstrap_assertion` is opaque to Agent Protocol and is the canonical 263-character carrier defined by `m0-trusted-bootstrap-and-server-fingerprint-contract.md`;
- the Server independently performs the full trusted-bootstrap verification contract and never trusts `local_boot_trust` alone;
- accepted evidence may establish only the Endpoint's exact authoritative current boot;
- same-boot reconnect does not require evidence to be resent; resending the same valid evidence is idempotent;
- a genuine reboot requires the new boot's evidence; historical-boot evidence cannot establish the new boot;
- missing or cryptographically/current-boot-rejected syntactically valid evidence causes no state transition, sends no acknowledgement, `AuthError`, or detailed rejection, and does not terminate the already-authenticated session;
- malformed envelope/JSON, binary input, unknown message type, or other post-handshake wire/phase violation is instead a protocol violation and uses generic `ProtocolError`.

`CredentialActive` and trusted-bootstrap establishment remain independent facts.

## Transfer authorization

After `SessionEstablished` and `ActionAck{outcome: Accepted}` for a data-plane transfer action, the Agent may use:

- `TransferAuthorizationRequest{transfer_id, proof_public_key}` — Agent -> Server;
- `TransferAuthorizationGrant{transfer_id, token, expires_at}` — Server -> Agent;
- `TransferAuthorizationDenied{transfer_id, reason}` — Server -> Agent.

`proof_public_key` represents an Agent-generated ephemeral public key. Its private counterpart never leaves the Agent. The exact key algorithm/encoding and data-plane proof-of-possession mechanism belong to `m0-data-plane-and-storage-contracts.md`.

A grant requires the Server to confirm that the transfer has a non-terminal Attempt bound to the requesting Endpoint and that the session has `CredentialActive`. `token` is opaque to Agent Protocol and is sender-constrained to the presented proof key according to the data-plane contract.

A currently authenticated Agent may request renewal for the same legitimate non-terminal `transfer_id` after expiry or Agent restart, using the same or a new proof key. Renewal is not a retry and creates no new Attempt.

`TransferAuthorizationDenied.reason` is intentionally minimally revealing; V1 may use one closed generic value and must not distinguish unknown transfer, wrong Endpoint, terminal transfer, or other internal denial causes.

The token and per-request proof are used on the separate HTTPS data plane. Agent Protocol never carries bulk Artifact bytes.

## Message envelope

Every message contains:

- `message_id` — unique UUID v4;
- `protocol_version` — `"1"` for Agent Protocol v1;
- `type` — a defined message type;
- `timestamp` — RFC 3339 / ISO 8601 UTC string;
- `correlation_id` — optional.

For action-scoped messages, `correlation_id` equals the relevant `action_id`. For a non-action `ProtocolError`, it may identify the offending `message_id`.

## Agent-action state vocabulary

`StatusReport.known_state` uses:

- `Accepted` — dispatch accepted, execution not yet started;
- `Running` — execution in progress;
- `Succeeded` — execution completed successfully;
- `Failed` — execution completed unsuccessfully;
- `Cancelled` — execution cancelled;
- `Unknown` — Agent has no authoritative local state for the `action_id`; this never means "not executed".

`ActionResult.outcome` uses only the terminal execution values `Succeeded | Failed | Cancelled`.

## Message types

- `AuthRequest{credential}`
- `SessionEstablished{protocol_version, session_id, runtime_credential, credential_expires_at}`
- `AuthError{reason}`
- `BootstrapEvidence{boot_nonce, bootstrap_assertion, local_boot_trust: Established}`
- `TransferAuthorizationRequest{transfer_id, proof_public_key}`
- `TransferAuthorizationGrant{transfer_id, token, expires_at}`
- `TransferAuthorizationDenied{transfer_id, reason}`
- `ActionDispatch{action_id, action_type, action_version, parameters, retry_of?}` — Server -> Agent.
- `ActionAck{action_id, outcome: Accepted|Rejected, error?}` — Agent -> Server. `Rejected` means execution did not occur and must not be represented as `ActionResult{Failed}`.
- `ActionProgress{action_id, percent?, bytes_processed?, eta?}` — Agent -> Server; metadata only.
- `ActionResult{action_id, outcome: Succeeded|Failed|Cancelled, detail}` — Agent -> Server.
- `CancelAction{action_id}` — Server -> Agent.
- `CancelAck{action_id, outcome: Cancelled|AlreadyCompleted|CannotCancel|Unknown}` — Agent -> Server. `CannotCancel` means the Agent knows the action but cannot stop it; `Unknown` means no authoritative local state exists.
- `StatusQuery{action_id}` — Server -> Agent.
- `StatusReport{action_id, known_state}` — Agent -> Server.
- `Heartbeat` / `HeartbeatAck` — liveness; interval is implementation-time.
- `ProtocolError{code, message, correlation_id?}` — either direction, for a post-handshake protocol violation. Whether it closes the WebSocket is implementation policy unless a future safety requirement states otherwise.

Concrete `action_type` definitions belong to the Specifications that introduce those operations. Unknown or malformed action types are rejected; the Agent never best-effort interprets them or exposes generic command execution.

## Idempotency, retry, and uncertain delivery

`action_id` is the protocol idempotency key while the Agent retains authoritative local state.

For a duplicate known `action_id`:

- if completed, return the stored `ActionResult` without re-executing;
- if still active, report retained current state without starting a second execution.

This is not exactly-once execution across Agent restart or loss of local state. If local state was lost, `StatusQuery` returns `Unknown`. A subsequently received `ActionDispatch` cannot be recognized as a duplicate by the Agent; whether the Server may send another dispatch in that situation is Job-lifecycle policy.

A retry is a new `ActionDispatch` with a fresh `action_id` and optional `retry_of` referencing the prior action.

Failure to receive `ActionAck` is an **uncertain delivery outcome**, never proof of non-delivery. It must not by itself cause blind redispatch of destructive work.

## Reconnect and reconciliation

A fresh connection performs the full handshake again.

For actions the Server still considers in flight, it sends `StatusQuery` instead of blindly redispatching. The Agent reports only authoritative local knowledge, including `Unknown` after local-state loss.

`Unknown` never proves that execution did not occur. Retry, cancellation, escalation, or operator intervention based on uncertain status belongs to the Job lifecycle contract.

## Wire encoding and compatibility

Agent Protocol v1 uses **UTF-8 JSON in WebSocket text frames**, unless a concrete implementation blocker requires the contract to be revisited.

- timestamps, including `credential_expires_at` and transfer `expires_at`: RFC 3339 / ISO 8601 UTC strings;
- `action_id`, `message_id`, and `session_id`: lowercase hyphenated UUID v4 strings;
- absent optional fields are omitted, never sent as `null`;
- unknown top-level `type`: reject explicitly with `AuthError` during handshake or `ProtocolError` after session establishment;
- unknown fields inside an otherwise valid known message type are ignored for forward-compatible minor additions;
- binary frames are not Agent Protocol v1 messages;
- incompatible `protocol_version` is rejected explicitly, never best-effort interpreted.

## Safety invariants

- no generic/arbitrary remote execution path exists;
- TLS pin failure occurs before Agent Protocol and fails closed;
- `Rejected` dispatch is not execution failure;
- missing `ActionAck`, `StatusReport{Unknown}`, disconnect, or Agent restart never prove non-execution;
- cancellation reports actual known state and never fabricates success;
- reconnect/session establishment never authorizes automatic destructive replay;
- trusted bootstrap is never inferred from credential validity, TLS connectivity, or fingerprint match alone.

## Out of scope

- concrete action catalog and action-specific schemas;
- Job/action authorization, retry, reconciliation, or destructive-resumption policy;
- data-plane chunk transfer, token internals, and proof-of-possession verification;
- Administrative API / Browser-Server protocol;
- heartbeat/liveness tuning;
- bootstrap-assertion internals, signing, site trust-anchor provisioning, and firmware/Secure Boot mechanics.

## Related

- ADR-0005 — Agent control-plane protocol rationale.
- ADR-0012 — runtime credential rotation/recovery rationale.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — credential/current-boot lifecycle.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — trusted-bootstrap verification and fingerprint authority.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — retry/reconciliation and destructive-dispatch policy.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — transfer authorization capability and data-plane semantics.
- `docs/development/testing.md` — contract-test responsibility.
