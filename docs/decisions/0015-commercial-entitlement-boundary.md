# ADR-0015: Commercial entitlement boundary — capacity policy, capabilities, offline verification, and plugin gating

Status: Accepted

## Context

Bamep is Apache-2.0 open-core (`docs/discovery/architecture-redesign.md`
"Open-source and commercial boundary"; `docs/specifications/m0-stack-and-boundaries-baseline.md`
"Product boundary and domain vocabulary"). That document already establishes,
without designing, that "future commercial differentiation may exist above or
around the engine" and that Bamep must not carry "customer-conditional forks
or code paths such as `if customer == X`." It does not define how a
commercial capacity/capability constraint would technically enter Bamep
without violating that principle, what happens when no commercial platform is
configured, how such a constraint would remain verifiable without Internet
access, or how it interacts with the already-accepted Job/JobStep/Attempt
model and endpoint-exclusivity lease
(`docs/decisions/0006-job-jobstep-attempt-state-model-and-scheduling.md`,
`docs/specifications/m0-job-lifecycle-and-scheduling.md`).

Three prior decisions establish directly relevant precedent this ADR follows
rather than reinvents:

- **ADR-0001** (`docs/decisions/0001-runtime-topology-modular-monolith.md`)
  establishes the Presentation/Application/Domain/Runtime Services/Ports/
  Adapters/Workers boundary and the rule that Domain must not depend on
  concrete external mechanisms. This ADR treats commercial entitlement the
  same way ADR-0001 already treats GRUB, MikroTik, or a storage device path:
  a concern kept out of Domain by construction.
- **ADR-0009** (`docs/decisions/0009-driver-provider-integration-boundary.md`)
  establishes the pattern of an operator-managed external source consumed
  through a Port/Adapter, with Bamep's own software never embedding,
  fetching, or redistributing the externally-owned artifact on the
  operator's behalf. The entitlement provider boundary below follows the
  same shape: Bamep verifies and consumes a signed artifact it does not
  itself issue.
- **ADR-0011** (`docs/decisions/0011-site-trust-anchor-operator-verified-pairing.md`)
  establishes that Bamep already performs local, offline-capable trust
  verification with an explicit fail-closed posture and no trust-on-first-use
  shortcut. Offline entitlement verification (below) is architecturally the
  same shape of problem — verifying a signed artifact locally, without
  Internet dependency, without silently falling back to an unverified state.

This ADR also relies on, and does not reopen, ADR-0013's persistence baseline
and Port/Adapter discipline for how a resolved entitlement policy would be
held and consumed, and on the Job/JobStep/Attempt state model and
endpoint-exclusivity lease semantics
(`docs/specifications/m0-job-lifecycle-and-scheduling.md`) that the capacity
constraint defined below composes with rather than replaces.

This is an owner-approved architecture-direction checkpoint. It records a
durable boundary and does not implement licensing, plugins, billing, ERP, or
Scheduler behavior; concrete types, serialization, algorithms, and schemas
are explicitly deferred (see "Open questions").

## Decision

### 1. Bamep is commercially agnostic

Bamep's runtime architecture and product model must not encode or branch on
commercial product/catalog vocabulary such as a product/edition identifier
(e.g. "Bamep-4", "Bamep-8", "Bamep-Full"), `customer`, `contract`, a billing
plan, or a SKU. This does not bar architecture documentation, this ADR
itself, or the generic entitlement Adapter/Port boundary from discussing the
commercial boundary conceptually — it constrains what the runtime
architecture *encodes*, not what documentation may *describe*.

The stronger invariant holds specifically for Domain, Application, and
Runtime Services code: it must never define or branch on concepts such as a
product/edition identifier, `customer`, `customer_id`, `contract`,
`subscription`, `invoice`, a billing plan, a SKU, or an ERP tenant. No enum
equivalent to:

```text
enum Edition { Bamep4, Bamep8, Full }
```

or any commercial branching derived from it, is introduced inside Domain,
Application, or Runtime Services. This generalizes the customer-conditional-
code principle already accepted in `docs/discovery/architecture-redesign.md`
("Do not create customer-conditional forks or code paths such as
`if customer == X`") from customer identity to commercial product/catalog
vocabulary broadly.

### 2. Open-core remains functional without a commercial provider

Bamep must remain independently usable under Apache-2.0 without any
commercial licensing platform. Two configurations are distinguished:

- **(A) No commercial entitlement provider configured** — Bamep operates
  according to its normal open-core baseline, with no capacity or capability
  restriction beyond what the open-core baseline itself already defines
  (physical/runtime resource leases, per
  `docs/specifications/m0-job-lifecycle-and-scheduling.md`).
- **(B) A commercial entitlement provider is explicitly configured/required**
  by an official commercial distribution/appliance — its verified policy
  becomes an additional admission/capability constraint, composed with (not
  replacing) the open-core baseline.

Critically, a missing, invalid, or expired entitlement in a **commercial
configuration** must not be silently bypassable by deleting the entitlement
artifact and falling back to unrestricted open-core behavior. The
mode/provider configuration itself — whether an installation runs open-core
baseline (A) or a commercial-provider configuration (B) — is an explicit
installation/deployment choice, not decided by the presence or absence of an
artifact at runtime. Concrete packaging/configuration mechanics for making
that choice are not designed here (see "Open questions").

### 3. The commercial platform owns business semantics; Bamep does not know its topology

A future private commercial platform may own concepts such as customers,
contracts, subscriptions, a product catalog, SKUs/editions, billing,
license/entitlement issuance, installation/customer association, commercial
plugin entitlement, and later ERP/service-shop modules. Bamep must not know,
and must not be able to infer from its own architecture, whether these
concerns live in one modular monolith, a dedicated licensing service, an ERP,
or several services. That topology is entirely outside Bamep's architecture
and outside the scope of this ADR.

### 4. Generic entitlement boundary

The commercial platform translates business/product concepts into a generic,
signed technical entitlement artifact that Bamep verifies and interprets
without reference to the business concepts that produced it:

```text
Private commercial platform
    |
    | signs
    v
Signed entitlement artifact
    |
    v
Bamep entitlement Adapter/Provider
    |
    v
Application-level EffectiveEntitlements
    |
    +--> ExecutionCapacityPolicy
    |
    +--> CapabilitySet
```

`EffectiveEntitlements` is an Application/Adapter-level concept, not a Domain
concept. Domain must not depend on a `LicenseService`, an `EntitlementService`,
billing, a SKU, or an edition. Downstream components consume only the
specific generic technical policy they need (`ExecutionCapacityPolicy` for
the Scheduler/Resource Arbiter, `CapabilitySet` for plugin gating) — never the
full `EffectiveEntitlements` object, and never the business concepts behind
it. Exact struct/type shapes are implementation-time, not decided here.

### 5. Domain remains commercially unaware

Domain must contain zero commercial vocabulary. In particular, the
Scheduler/Resource Arbiter must never do anything equivalent to
`LicenseService::is_bamep_8()` or otherwise inspect a commercial edition. It
may receive a generic `ExecutionCapacityPolicy` as one additional Scheduler
admission/resource-policy input, evaluated at Job admission /
endpoint-exclusivity level and kept explicitly distinct from the existing
Attempt-scoped resource leases (`docs/specifications/m0-job-lifecycle-and-scheduling.md`
"Other resource leases (Attempt-scoped)") — see "Capacity composes with
physical resource limits" below for how the two levels compose. A future
plugin runtime may receive/query a
generic `CapabilitySet` or `has_capability(CapabilityId)`. No component below
the Application/commercial boundary needs to know *why* a capability or
capacity value has the value it has.

### 6. Commercial capacity semantic

The technical meaning of the commercial capacity represented by product tiers
such as Bamep-4, Bamep-8, or a project-specific Bamep-Full is:

> the maximum number of simultaneously active Endpoint Jobs, measured by the
> number of Job-scoped endpoint-exclusivity leases currently granted
> (`docs/specifications/m0-job-lifecycle-and-scheduling.md`
> "Endpoint-exclusivity lease (Job-scoped)").

This corresponds to Jobs in `Running` or `Cancelling` — states in which the
Job holds its endpoint-exclusivity lease. A Job in `Pending` does not consume
a commercial-capacity slot, because its endpoint-exclusivity lease has not yet
been granted. Conceptually:

```text
Bamep-4    -> max_active_endpoint_jobs = 4
Bamep-8    -> max_active_endpoint_jobs = 8
Bamep-Full -> max_active_endpoint_jobs = installation/contract-specific value
```

The exact field name is implementation-time, but should be semantically
precise (e.g. `max_active_endpoint_jobs` or `max_active_endpoint_leases`),
not a vague term like "supported machines." This capacity constraint does
**not** limit total registered Endpoints, total inventory records,
connected-but-idle Agents, or historical Endpoints — only the count of
currently active Job-scoped endpoint-exclusivity leases.

> **Revision note (ADR-0020, Proposed):** this section's equivalence between commercial
> capacity and the Job-scoped endpoint-exclusivity lease is revised by ADR-0020. A Job parked
> at a planned operator-intervention checkpoint retains its endpoint-exclusivity lease but
> consumes no automated-execution capacity slot. When ADR-0020 is accepted, the generic
> capacity unit becomes "the maximum number of Endpoints concurrently admitted to automated
> execution"; every other decision in this ADR is unaffected. Normative wording is owned by
> `docs/specifications/m0-job-lifecycle-and-scheduling.md` "Job admission and capacity".

### 7. No technical "Full/Unlimited" edition

Bamep itself receives only a numeric effective capacity value, never an
enum/branch such as `Unlimited`, `Full`, or `Enterprise`, unless a future
concrete requirement proves a true unbounded semantic is needed — none is
evidenced today. The current M0/M1 20–24 Endpoint validation target
(`docs/specifications/m0-architecture-baseline.md`,
`docs/discovery/architecture-redesign.md` "Capacity and scheduling") is a
validation target, not a software/product maximum; the architecture must
support project-specific capacities larger than that baseline without a
product-vocabulary change.

### 8. Capacity composes with physical resource limits

Commercial capacity is an additional Scheduler admission constraint. It does
not replace the existing resource-lease model. Conceptually:

```text
effective admission capacity =
    commercial policy constraint
    composed with
    currently available physical/runtime resources
```

Existing Attempt-scoped resource leases (network, storage, CPU/worker
capacity — `docs/specifications/m0-job-lifecycle-and-scheduling.md` "Other
resource leases (Attempt-scoped)") remain independent constraints. Commercial
capacity operates at Job admission / endpoint-exclusivity level, the same
level already defined for the endpoint-exclusivity lease. This ADR does not
redesign the Scheduler algorithm; ordering/fairness among competing leases
and among `Pending` Jobs remains the already-deferred implementation-time
question (`docs/specifications/m0-job-lifecycle-and-scheduling.md` "Out of
scope").

### 9. Offline-verifiable entitlements

Commercial deployments must continue operating without Internet access, using
the same architectural shape already validated by ADR-0011 for site-trust
verification — local, offline-capable verification with an explicit
fail-closed posture and no trust-on-first-use shortcut:

```text
commercial/private platform
    |
    | produces signed entitlement
    v
Bamep installation
    |
    +-- verifies locally
    +-- derives EffectiveEntitlements
    +-- persists/caches sufficient verified state locally
    +-- continues operating without platform connectivity
```

The private signing key never belongs in the Bamep open-core distribution.
Public verification material may be openly distributed; its secrecy is
neither required nor useful, mirroring the asymmetric-verification pattern
already accepted for boot-chain/site trust (ADR-0010, ADR-0011). Cryptographic
algorithm, concrete serialization, license-server protocol, key-storage
implementation, and rotation mechanics are not chosen here (see "Open
questions").

### 10. Installation identity

Bamep owns a durable installation/site identity concept, conceptually
`installation_id`. It is generated/owned by Bamep; durable across a normal
restart; independent of customer identity, Endpoint identity, NIC/MAC, and
motherboard/CPU serial (consistent with the already-accepted principle that a
MAC address is inventory signal, never a trust anchor or permanent identity —
`AGENTS.md` "Safety"; `docs/decisions/0004-endpoint-identity-and-enrollment-bootstrap.md`);
and suitable for preservation across a legitimate Server migration/restore.
The private commercial platform may externally map `customer` × `contract` ×
`installation_id`, but Bamep itself does not require or store `customer_id`.
Final schema and generation format are not designed here.

### 11. Failure semantics

For a commercial installation/provider configuration, admission of new work
is separated from continuation of already-authorized work:

- **Missing entitlement**: fail closed for new commercially-gated
  admission/capability use; already-`Running`/`Cancelling` Jobs are never
  terminated.
- **Invalid signature**: same behavior — reject new gated admission/use; do
  not terminate active destructive operations.
- **Expired entitlement**: block new gated admissions/operations;
  already-`Running` Jobs continue normally to a terminal state; they are
  never automatically cancelled.
- **Commercial platform unreachable**: if a locally verified entitlement
  remains valid, Bamep continues operating normally offline — platform
  reachability is not itself an entitlement-validity condition (consistent
  with "Offline-verifiable entitlements" above).
- **Capacity exceeded**: a new Job remains `Pending` until capacity becomes
  available, composing with the existing scheduler/lease queue semantics
  (`docs/specifications/m0-job-lifecycle-and-scheduling.md`) rather than
  inventing a new Job failure mode solely because commercial capacity is
  temporarily full.
- **Downgrade below current active usage** (e.g. limit 8 → 4 while 7 Jobs are
  active): the seven active Jobs are never cancelled; no new Job is admitted
  until active usage falls below the new limit.
- **Expiry during destructive work**: a commercial entitlement expiring never
  aborts a provisioning/recovery operation already in progress. Commercial
  policy must never create a destructive-safety hazard — this is subordinate
  to, and must never override, the destructive-operation safety invariants
  already established in `AGENTS.md` "Safety" and
  `docs/specifications/m0-job-lifecycle-and-scheduling.md`.

### 12. Open-core / Apache-2.0 reality

License enforcement distributed inside the Apache-2.0 repository is
inspectable, modifiable, and removable by a determined fork. It must never be
described as unbreakable DRM, and this ADR does not introduce anti-tamper or
security-through-obscurity mechanisms. The commercial model instead relies on
the legitimate official ecosystem — official Bamep appliances/distributions,
signed commercial entitlements, proprietary plugins, support, commercial
update/distribution services, and private integrations/platform
functionality. The open-core may enforce entitlements for official/legitimate
deployments, but the architecture must not pretend a local check inside
open-source code prevents a fork from removing it.

### 13. Plugin commercial boundary

Plugin ABI/runtime design is out of scope for this ADR; only the
commercial/capability boundary is established. Future plugin metadata may
conceptually declare `plugin_id`, `version`, `bamep_api_version`, and
`required_capabilities`. Bamep evaluates only generic `CapabilityId` values —
it does not decide whether a plugin is open-source or proprietary, and plugin
licensing must never depend on that distinction. A plugin requiring no
entitlement declares `required_capabilities = []`; a plugin requiring gated
functionality declares `required_capabilities = ["some.generic.capability"]`,
regardless of whether the plugin itself is open or proprietary. The
commercial platform decides *why* a capability is granted; Bamep only
evaluates the technical capability contract. Whether capability absence
blocks plugin loading entirely or only specific operations is not decided by
this ADR (see "Open questions") unless an existing approved contract already
requires one answer — none does.

### 14. ERP / private-platform integration

This ADR preserves, and does not reopen, the already-accepted rule that a
future ERP/private commercial platform integrates with Bamep only through
public/versioned APIs, Domain Events, and explicitly supported
artifact/entitlement/plugin contracts — never through Bamep's internal
PostgreSQL schema (`docs/discovery/architecture-redesign.md` "Product
boundary"; `docs/specifications/m0-stack-and-boundaries-baseline.md`
"Product boundary and domain vocabulary"). Bamep's contract must remain
stable regardless of whether the private platform is licensing-only, a
commercial modular monolith, ERP + licensing, or multiple private services —
no private-platform topology becomes part of Bamep architecture (restates
"The commercial platform owns business semantics" above at the integration
boundary).

### 15. Architectural placement

This decision is placed within the current M0 responsibility taxonomy
(`docs/specifications/m0-stack-and-boundaries-baseline.md` "Component
responsibilities and boundaries") without requiring a one-to-one
crate/module mapping:

```text
Presentation
Application
Domain
Runtime Services
Ports
Adapters
Workers
```

Entitlement flow aligns conceptually as:

```text
Adapter / entitlement provider
    ->
Application resolves verified EffectiveEntitlements
    ->
Runtime Service receives narrow technical policy
    ->
Domain stays commercially agnostic
```

For capacity: `Application -> ExecutionCapacityPolicy -> Scheduler / Resource
Arbiter`. For capabilities/plugins: `Application -> CapabilitySet / capability
query boundary -> future plugin infrastructure`.

### 16. Port/Adapter boundary

Entitlement acquisition and verification sits behind a generic Port/Adapter
boundary, in the same sense already accepted for repositories, Agent
transport, boot, discovery, storage, and infrastructure metrics
(`docs/specifications/m0-stack-and-boundaries-baseline.md` "Component
responsibilities and boundaries"), and directly following the external-source
pattern already accepted in ADR-0009 (driver-provider boundary): Bamep
consumes an externally-produced artifact through an Adapter, and does not
embed or fetch it on the operator's behalf. Final Rust trait names are not
chosen here. The architecture supports, at minimum, an open-core baseline
provider/policy and a commercial signed-entitlement provider, so that
scattered conditionals such as `if commercial_mode`, `if license_valid`, or
`if edition == ...` do not spread through the codebase — one resolved policy
source feeds the components that need it.

## Alternatives considered

- **Embedding an `Edition`/SKU enum or `LicenseService` directly in
  Domain/Application.** Rejected: reopens exactly the customer-conditional-
  code principle `docs/discovery/architecture-redesign.md` already
  establishes, and would make the Scheduler/plugin runtime depend on
  business vocabulary they do not need to interpret.
- **Cloud-only entitlement verification requiring live connectivity.**
  Rejected: contradicts Bamep's already-accepted offline-operation
  requirement (`docs/specifications/m0-stack-and-boundaries-baseline.md`:
  "does not depend on Internet access once required artifacts are available
  locally") and the local-verification precedent already validated by
  ADR-0011.
- **Silent fallback to unrestricted open-core mode whenever an entitlement
  artifact is missing/invalid, regardless of configured mode.** Rejected:
  makes commercial enforcement for legitimate deployments trivially
  bypassable by deletion, contradicting "Open-core remains functional
  without a commercial provider" above, which requires the mode/provider
  choice itself, not artifact presence, to determine behavior.
- **Anti-tamper / obfuscation / license-check hardening inside the
  open-source distribution.** Rejected outright: contradicts the accepted
  Apache-2.0/open-core reality (§12) and would misrepresent what a local
  check inside open-source code can actually guarantee against a determined
  fork.
- **Limiting total registered/inventoried Endpoints instead of active
  Job-scoped endpoint-exclusivity leases.** Rejected: conflates inventory
  size with concurrent execution capacity, contradicts the endpoint-
  exclusivity lease model already defined in
  `docs/specifications/m0-job-lifecycle-and-scheduling.md`, and would
  penalize idle/historical Endpoints that consume no active execution
  resource.
- **A technical `Unlimited`/`Full`/`Enterprise` edition enum.** Rejected: no
  concrete requirement evidences a true unbounded semantic; a plain numeric
  capacity value already supports arbitrarily large project-specific
  deployments without adding product vocabulary to the technical model.
- **Making plugin capability gating depend on whether a plugin is
  open-source or proprietary.** Rejected: Bamep does not need, and must not
  assume, knowledge of a plugin's commercial/legal distribution model; only
  the declared `required_capabilities` contract matters technically.
- **Cancelling active Jobs immediately on entitlement expiry, invalidity, or
  downgrade.** Rejected: creates a destructive-safety hazard during
  in-progress provisioning/recovery work, directly contradicting `AGENTS.md`
  "Safety" ("Safety takes precedence over implementation convenience");
  admission-time gating achieves the commercial constraint without this risk.

## Consequences

- Domain, Application-level Runtime Services, and Ports/Adapters gain a new
  conceptual boundary (`EffectiveEntitlements`, `ExecutionCapacityPolicy`,
  `CapabilitySet`) that any future licensing, plugin, or ERP-adjacent Work
  Package must design against, rather than inventing ad hoc commercial
  branching.
- The Scheduler/Resource Arbiter gains one more admission-policy input
  category (commercial capacity), composed with, not replacing, the existing
  endpoint-exclusivity and Attempt-scoped resource-lease model
  (`docs/specifications/m0-job-lifecycle-and-scheduling.md`). No Job state,
  JobStep state, or Attempt state is added or changed by this ADR.
- A future entitlement Adapter, installation-identity mechanism, and plugin
  capability-query boundary are anticipated architecturally but not
  implemented, designed in schema form, or scheduled by this ADR.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` is amended with a
  short cross-reference recording that Bamep remains commercially/ERP
  agnostic, that commercial entitlement verification is a
  Port/Adapter/Application concern, and that ERP/private integrations remain
  API/domain-event based — without altering its M0 scope history, product
  boundary statement, or component-boundary list.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` is amended with a
  concise post-M0 clarification under "Job admission" recording that
  admission may additionally be gated by a generic effective capacity policy
  supplied to the Scheduler/Resource Arbiter — without redesigning Job
  states, the endpoint-exclusivity lease, or the scheduling algorithm, all of
  which remain as already approved.
- No code, migration, plugin manifest, or commercial API client is
  introduced by this ADR. This checkpoint is architecture documentation
  only.

## Related architecture

- `docs/discovery/architecture-redesign.md` — "Open-source and commercial
  boundary" and "Product boundary" — the already-accepted direction this ADR
  formalizes and elaborates; not rewritten by this ADR.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — the
  Presentation/Application/Domain/Runtime Services/Ports/Adapters/Workers
  boundary this decision's entitlement flow is placed within; amended by
  this ADR with a cross-reference only.
- `docs/specifications/m0-job-lifecycle-and-scheduling.md` — the Job/JobStep/
  Attempt state model and endpoint-exclusivity lease this decision's
  capacity semantic (§6) and admission composition (§8) build on; amended by
  this ADR with a cross-reference only.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` —
  the durable/transient boundary and Port/Adapter persistence discipline
  this decision's `installation_id` and locally-cached verified entitlement
  state (§9, §10) would eventually be subject to; not amended by this ADR.
- `docs/decisions/0001-runtime-topology-modular-monolith.md` — the
  Domain/Adapter dependency-direction discipline this decision extends to
  commercial concerns.
- `docs/decisions/0009-driver-provider-integration-boundary.md` — the
  external-source-through-a-Port precedent this decision's entitlement
  Port/Adapter boundary (§16) follows directly.
- `docs/decisions/0011-site-trust-anchor-operator-verified-pairing.md` — the
  local/offline verification and fail-closed precedent this decision's
  offline entitlement model (§9) follows directly.
- `docs/decisions/0013-postgresql-persistence-backend-baseline.md` — the
  Port/Adapter and atomic-transaction discipline any future durable
  entitlement/installation-identity persistence would be subject to; not
  reopened.

## Related work

This ADR records an owner-approved architecture-direction checkpoint. No
concrete GitHub Work Package implements licensing, plugins, billing, ERP, or
Scheduler capacity gating as of this ADR; a future Work Package implementing
any part of this boundary must reference this ADR rather than re-deciding the
boundary it establishes.

## Open questions

Explicitly deferred, not decided by this ADR:

1. Company/private-platform name and final ERP topology.
2. Pricing, billing, and payment-provider selection.
3. Exact entitlement artifact serialization and cryptographic signing
   algorithm.
4. Verification-key storage and rotation mechanism.
5. License renewal/grace duration.
6. Entitlement local storage/caching schema.
7. `installation_id` concrete representation and generation format.
8. Plugin ABI/runtime design.
9. Marketplace mechanics, if any.
10. Whether capability absence blocks plugin loading entirely or only
    specific operations.
11. Packaging/configuration mechanics that select open-core baseline (A) vs.
    a commercial entitlement provider (B) for a given installation.

Status: Accepted.
