# ADR-0004: Endpoint identity and enrollment/trust bootstrap model

Status: Accepted

## Context

During M0, Bamep needed a durable Endpoint identity model before destructive workflow,
Agent authentication, persistence, and data-plane contracts could safely depend on
Endpoint trust.

Issue #2 (`[WP] Define endpoint identity and trust model`) established the decision
requirements:

- Endpoint identity must survive NIC or MAC replacement;
- MAC addresses and other hardware fingerprints must remain evidence, never authentication
  or permanent identity;
- first enrollment must establish trust explicitly rather than treating provisioning-LAN
  reachability as proof of identity;
- reconnect must not blindly restore trust from a matching MAC or hardware signal;
- later destructive-operation contracts need an explicit identity/trust precondition they
  can consume.

The provisioning LAN is controlled operationally but is not itself a trust anchor.

The original ADR referenced M0 Discovery documents that have since been reduced or retired
after their durable conclusions were promoted. Issue #2 and Git history preserve that
planning history.

This ADR preserves **why the identity and first-enrollment model was selected**.

The normative lifecycle, credential, hardware-confidence, current-boot, and destructive-
authorization semantics are owned by
`docs/specifications/m0-endpoint-identity-lifecycle.md`.

## Decision

### Durable Endpoint identity is Server-assigned

A Bamep Endpoint has a durable Server-assigned identifier.

That identifier is independent from:

- MAC address;
- NIC identity;
- disk fingerprint;
- DMI/SMBIOS data;
- hardware serial numbers;
- IP address;
- current network connection.

Hardware and network observations may support continuity/confidence decisions, but none is
the Endpoint identity itself.

This separation is required because legitimate lifecycle changes can replace individual
hardware components without turning the machine into a new logical Endpoint.

### Hardware signals are evidence, not trust anchors

Observed hardware attributes are attached to the Endpoint as evidence.

They may contribute to:

- inventory;
- continuity assessment;
- hardware-confidence evaluation;
- target-disk revalidation.

They must not be used as a substitute for authenticated Endpoint/session state.

In particular:

- a matching MAC does not authenticate an Agent;
- a matching disk fingerprint does not establish Endpoint identity by itself;
- a changed MAC/NIC does not automatically create a new Endpoint;
- hardware divergence must not be silently accepted by rewriting stored identity evidence.

The exact hardware-confidence states, transitions, and destructive-use rules belong to the
Endpoint identity Specification.

### Boot-scoped enrollment bootstrap

A new Agent boot begins with a short-lived enrollment credential/context provided through
the boot-orchestration boundary.

The purpose of that credential is to bootstrap an authenticated relationship without
requiring a permanent secret to be manually preinstalled per endpoint.

The credential is scoped to the boot/enrollment context rather than to a MAC address.

The detailed credential representation, lookup mechanism, BootContext correlation,
rotation, recovery, and expiry semantics were refined later by ADR-0012 and ADR-0014 and
are normative through the Endpoint identity and Agent Protocol Specifications.

This ADR does not duplicate those later mechanics.

### The Agent must authenticate the Server before presenting credentials

An Agent must establish the expected Server identity before sending its enrollment/runtime
credential.

The provisioning network alone is insufficient evidence of Server identity.

The concrete trust mechanism was resolved later by the trusted-bootstrap/site-trust and
Agent Protocol decisions. This ADR establishes the requirement, not their wire or
cryptographic details.

### First enrollment is operator-approval gated

A first-seen device does not become trusted solely because it:

- PXE-boots from Bamep;
- receives a Bamep boot artifact;
- reaches the Server;
- presents plausible inventory;
- possesses a valid boot-scoped enrollment credential.

The default V1/M0 trust transition requires explicit operator approval.

A successfully authenticated first-seen endpoint therefore enters the untrusted/pending
enrollment path defined by the Endpoint identity Specification, and only an explicit
operator decision establishes durable `Enrolled` identity.

This choice keeps the provisioning network and boot reachability from becoming implicit
authorization to participate in destructive workflows.

### Enrollment approval is a one-time trust establishment, not a reconnect tax

Once an Endpoint has been explicitly enrolled, normal reconnect/reboot/credential-rotation
flows must not require the operator to approve the same Endpoint again when the current
identity-continuity rules permit continuity.

The identity lifecycle is durable.

Reconnect therefore separates two questions:

1. is this still the same trusted Endpoint according to the current continuity/confidence
   contract?
2. can the current Agent authenticate with valid current credential/bootstrap state?

A previous approval answers only the durable identity question. It does not bypass current
authentication, credential, hardware-confidence, or trusted-bootstrap checks.

The exact continuity rules and credential behavior belong to the Endpoint identity
Specification and later credential ADRs.

### Hardware change lowers or breaks confidence rather than rewriting identity silently

When current hardware evidence differs from recorded evidence, Bamep must not silently
update its trust record and continue as if nothing changed.

The discrepancy is represented through the hardware-confidence model.

A legitimate hardware replacement can preserve durable Endpoint identity while still
requiring review/revalidation before destructive use.

This is especially important for:

- NIC replacement;
- disk replacement;
- multiple-disk endpoints;
- migration/restore workflows.

The exact distinction between `LoweredConfidence` and `Conflict`, their transitions, and
their operational consequences belong to the Endpoint identity Specification.

### Pre-authorized enrollment is a future authorization mechanism, not automatic enrollment

A future workflow may allow an operator to authorize an enrollment context **before** the
Endpoint's first connection.

That would move the explicit operator decision earlier in time.

It must not be interpreted as unrestricted automatic enrollment.

Pre-authorization is therefore modeled as a separate enrollment-authorization mechanism,
not as another Endpoint identity state and not as permission for arbitrary devices on the
provisioning LAN to self-enroll.

Its concrete design is outside this ADR.

### Destructive-operation safety remains independently composed

Trusted Endpoint identity is one required fact for destructive execution.

It is not sufficient authorization by itself.

This ADR does not own the complete destructive-operation precondition set.

The current normative set is defined by
`docs/specifications/m0-endpoint-identity-lifecycle.md` and composed at dispatch by
`docs/specifications/m0-job-lifecycle-and-scheduling.md`.

The original ADR text enumerated an earlier six-item set. That enumeration is intentionally
removed here because the current contract contains seven independent preconditions,
including trusted current bootstrap context.

No destructive precondition may be inferred from `Enrolled` identity.

## Alternatives considered

### MAC address as Endpoint identity

Rejected.

MAC addresses can change legitimately, can be duplicated/spoofed, and are network-interface
attributes rather than durable machine identity.

Using MAC as permanent identity would also make NIC replacement unnecessarily destructive
to the Endpoint lifecycle.

### Hardware fingerprint as the permanent identity

Rejected.

A compound hardware fingerprint can be useful continuity evidence, but hardware evolves.

Treating the fingerprint as the identity itself would make legitimate component replacement
either impossible or dependent on silently redefining identity when hardware changed.

The selected model instead keeps stable Server identity and evaluates hardware evidence
separately.

### Automatic trust for any device that reaches the provisioning network

Rejected.

The provisioning LAN is not a trust anchor.

Boot reachability and possession of a lease/boot artifact are insufficient reasons to
grant durable identity trusted enough for destructive operations.

### Automatic re-trust from matching hardware on reconnect

Rejected.

Matching evidence supports continuity assessment but is not authentication.

Reconnect still requires the currently approved authentication/credential/bootstrap
contract.

### Pre-shared per-device secret installed manually out of band

Rejected as the V1 default.

It can provide a strong bootstrap, but it reintroduces a manual provisioning step for
every endpoint and undermines the operational goal of network bootstrap.

It may still be appropriate for a future higher-assurance deployment profile.

### Per-Agent client-certificate PKI / mTLS as the V1 identity baseline

Rejected as the baseline.

A deployment-specific client-certificate lifecycle would require additional issuance,
storage, rotation, revocation, recovery, and operator machinery.

The selected boot-scoped credential + runtime-credential model satisfies the V1 trust
requirements without making client PKI a prerequisite.

This does not prohibit a future requirement from reconsidering the decision.

### TPM/hardware-rooted endpoint attestation as a V1 requirement

Rejected.

Bamep targets hardware where suitable TPM/attestation capability cannot be assumed
uniformly, and no V1 requirement justified making that hardware dependency mandatory.

Trusted bootstrap and Endpoint identity remain separate properties.

## Consequences

- Endpoint identity survives legitimate NIC/MAC replacement.
- Inventory/hardware fingerprints remain useful evidence without becoming authentication.
- First enrollment requires a durable explicit operator decision.
- Presentation/Application surfaces that expose enrollment must preserve the operator
  approval boundary; an Agent must never approve its own enrollment.
- The boot-orchestration boundary has security responsibility because it participates in
  creating the boot-scoped enrollment context.
- Existing enrollment approval does not waive future authentication or safety checks.
- Hardware discrepancies must be surfaced through confidence/revalidation semantics rather
  than silently rewriting trusted evidence.
- Future pre-authorized enrollment must preserve explicit operator intent.
- Destructive workflows consume Endpoint identity as one independent safety fact and must
  use the complete current safety contract rather than a copied list from this ADR.
- Later credential, BootContext, trusted-bootstrap, and protocol decisions refine how this
  identity model is realized without replacing the durable identity decision itself.

## Authority boundary

This ADR owns the rationale for:

- Server-assigned durable Endpoint identity;
- hardware/network attributes as evidence rather than identity/authentication;
- boot-scoped enrollment bootstrap;
- authenticating the Server before credential presentation;
- operator-gated first enrollment;
- avoiding repeated operator approval when trusted identity continuity remains valid;
- rejecting silent hardware-trust rewriting;
- treating future pre-authorization as explicit operator authorization rather than
  unrestricted automatic enrollment.

It does **not** own:

- identity lifecycle state tables/transitions;
- credential lifecycle/rotation/revocation;
- credential lookup or BootContext schema/correlation;
- current-boot trusted-bootstrap state;
- hardware-confidence state transitions;
- the complete destructive-operation precondition list;
- Agent Protocol wire messages;
- TLS/trusted-bootstrap cryptographic details;
- operator-facing enrollment API/UX.

Those belong to the applicable Specifications and later ADRs.

## Current implementation relationship

Issue #17 (`[WP] Establish simulated Endpoint trust, enrollment, and Agent session`) is
completed.

It established the M1 simulated vertical slice for:

- real Agent Protocol v1 WSS session establishment;
- durable `PendingEnrollment`;
- explicit operator-driven transition to `Enrolled`;
- identity continuity across reconnect without repeated approval;
- runtime credential behavior required by the current credential contract;
- trusted-bootstrap evidence semantics required by that Work Package.

That completed Work Package does not mean every Endpoint identity capability or future
operator UX described by the broader Specification is implemented.

`docs/architecture/README.md` remains authoritative for current repository structure.

## Related specifications and decisions

- `docs/specifications/m0-endpoint-identity-lifecycle.md` — normative Endpoint identity,
  credential, hardware-confidence, current-boot, and destructive-precondition contract.
- `docs/specifications/m0-agent-protocol-contract.md` — normative Agent authentication and
  session wire contract.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — normative
  trusted-bootstrap and Server-identity contract.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — complete destructive-dispatch
  composition.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — Artifact-specific safety
  gates and source/target-disk distinction.
- ADR-0010 — trusted-bootstrap / Secure Boot architectural decision.
- ADR-0011 — site trust-anchor establishment decision.
- ADR-0012 — runtime Agent credential issuance/rotation/reconnect-recovery decision.
- ADR-0014 — credential lookup and BootContext correlation decision.

## Related work

- Issue #2 — historical M0 Work Package that produced this identity/enrollment decision and
  normative lifecycle baseline.
- Issue #17 — completed M1 Work Package implementing and validating the simulated Endpoint
  trust/enrollment/session slice.
