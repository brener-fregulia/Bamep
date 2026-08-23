# ADR-0005: Agent control-plane protocol and typed-action model

Status: Accepted

## Context

The Agent control plane needs low-latency, bidirectional, Server-initiated dispatch with acknowledgement, progress, cancellation, reconnect recovery, and explicit protocol versioning. The Agent must not expose unrestricted remote command execution, and the protocol must remain independently implementable even if Server and Agent share Rust.

Endpoint identity also requires the Agent to authenticate the expected Server before presenting its enrollment/runtime credential. The normative wire contract belongs to `docs/specifications/m0-agent-protocol-contract.md`.

## Decision

### Transport and authentication

Agent ↔ Server control traffic uses WebSocket over TLS (WSS) with a typed, versioned application protocol. This decision applies only to the Agent control plane; Browser/Web ↔ Server communication may use another mechanism.

The Agent authenticates the Server by verifying the expected Server TLS certificate fingerprint before normal protocol authentication. A mismatch fails closed; there is no TOFU or "warn and continue" fallback. The expected fingerprint comes from the independently authenticated trusted-bootstrap contract in `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md`.

After Server authentication, the Agent authenticates with its enrollment/runtime credential. The Agent does not present a client certificate; this is intentionally not mTLS. Exact handshake fields, fingerprint definition, credential rotation, errors, and `BootstrapEvidence` behavior are normative in the Specifications.

### Typed actions and execution identity

Agent actions are a closed, versioned catalog with explicit parameter and result schemas. The Agent must never accept a generic arbitrary shell/command payload as a substitute for a typed action. The catalog may grow without changing this decision.

Dispatch acknowledgement is distinct from execution result so the Server can distinguish an action not known to be accepted, accepted/running, rejected before execution, and finished. A rejected dispatch is not an execution failure because no execution occurred.

One `action_id` represents one logical dispatched execution while the Agent retains authoritative local state for it. Redelivery of the same known `action_id` must not start a duplicate execution; the Agent reports its retained state/result instead. This is bounded idempotency, not exactly-once execution across Agent restart or loss of local state.

A retry, when policy permits one, is a new action with a fresh identity and may reference the prior action for correlation. Whether retry is allowed is Job-lifecycle policy, not transport policy.

### Cancellation, progress, and reconnect

Long-running actions may report progress correlated to their action identity. Progress carries control/metadata only; bulk bytes belong to the data plane.

Cancellation is explicit and must report the real outcome. A cancellation request never permits the Server to invent `Cancelled` when execution already completed or cannot safely stop.

Connection loss does not imply that an in-flight action stopped or never executed. After reconnect, uncertain actions are reconciled through explicit status queries/reports. If the Agent no longer knows an action, that means **unknown execution outcome**, not proof of non-execution.

The protocol must never automatically redispatch destructive work merely because a new session was established or Agent-local action state was lost. Job lifecycle owns reconciliation, retry, authorization, and operator-decision policy.

## Alternatives considered

- **REST + short polling:** rejected as the default because dispatch latency follows the polling interval and frequent polling adds avoidable request overhead.
- **REST + long polling:** not selected because acknowledgement, progress, cancellation, and bidirectional status traffic would still require repeated requests or additional channels; it remains a possible fallback for environments that cannot sustain WebSocket connections.
- **SSE + HTTP commands:** rejected for the Agent control plane because SSE is one-directional and Agent-originated acknowledgement/progress/results/status would require a second channel.
- **Full PKI / mTLS:** rejected for V1 because client-certificate issuance, rotation, revocation, and CA lifecycle add substantial machinery without a demonstrated requirement.

## Consequences

- Agent control is a persistent WSS conversation with an independently versioned application protocol.
- Server authentication, Agent authentication, and trusted-bootstrap establishment remain separate security facts.
- Generic remote shell execution is not an acceptable implementation shortcut.
- Transport/action identity does not define retry or destructive resumption policy.
- Wire details remain authoritative only in the Agent Protocol Specification and must remain implementable without reading Rust source.
- Bulk data transfer remains outside this control-plane channel.

## Related

- ADR-0003 — Worker/Agent language strategy and contract independence.
- ADR-0004 — Endpoint identity and credential rationale.
- ADR-0006 — Job/JobStep/Attempt retry and reconciliation rationale.
- ADR-0010 — trusted-bootstrap/Secure Boot rationale.
- `docs/specifications/m0-agent-protocol-contract.md` — normative Agent Protocol v1 wire contract.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — authenticated Server-fingerprint delivery.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — retry/reconciliation policy.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — bulk transfer boundary.
