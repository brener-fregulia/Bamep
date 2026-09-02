# ADR-0016: Static SvelteKit Presentation Client and Platform Boundary

Status: Accepted

## Context

`docs/specifications/m0-stack-and-boundaries-baseline.md` already names Presentation as
"Web Administration and Administrative API" and requires that externally relevant
contracts remain explicit and independently versioned rather than defined solely by
shared Rust types. `docs/specifications/m0-administrative-api-web-read-contract.md`
already establishes Administrative API v1 as the only Server ↔ Web boundary and forbids
Web from reading the Server database or internal Rust types directly. Neither document
selects a frontend stack, a static-vs-server-rendered delivery model, a Web
packaging/versioning relationship to the Server executable, a styling/design-system
direction, or a browser/desktop/mobile platform boundary.

Issue #20 originally required a minimal Bamep Web view over Administrative API v1 reads. Making
that planned work architecture-ready required these choices to be decided once, durably, rather
than improvised during implementation. The later owner-approved M1 roadmap rebaseline removed
Presentation delivery from M1 completion; it did not reopen this ADR's accepted decisions.

An owner-reviewed architectural discovery examined the available alternatives for each of
these questions. That discovery was investigation only; its durable conclusions are
recorded here and in the narrow Specification delta this ADR's Consequences describe, and
the discovery material itself is not retained as a permanent repository document.

The prior Pascoal project is used below only as case-study evidence about component-sizing
and styling failure modes; per `AGENTS.md` ("Architecture and dependencies"), Bamep does
not inherit its stack, structure, or decisions as a requirement.

## Decision

### 1. Frontend stack

Bamep Web is a fully static, client-rendered administrative application built with
Svelte 5, SvelteKit, strict TypeScript, and SvelteKit's `adapter-static`.

SvelteKit is used only for frontend application structure: routing, layouts, loading/error
UI organization, static build output, and route-oriented organization. It must not become a
second Bamep backend. Rejected uses: SvelteKit runtime server deployment; SSR as the
application runtime; `+server.*` routes for Bamep business behavior; SvelteKit form actions
for business operations; backend-for-frontend business logic; Web-only persistence or
business authority. Administrative API v1 remains the sole business-state/operation
boundary for Presentation clients, as already established by
`m0-administrative-api-web-read-contract.md`; this ADR does not reopen that contract.

### 2. Administrative API client

The frontend has a narrow, dedicated TypeScript Administrative API client boundary. Given
the current small API surface, it is hand-maintained; OpenAPI/code generation is not
selected now. Frontend code must not import or mirror Server-internal Rust models as an
alternative contract. The existing versioned Administrative API Specification remains the
authoritative contract owner.

### 3. Static Web delivery and independent versioning

Web is built as static assets, packaged/versioned independently from the Server executable.
`bamepd` serves the installed Web assets over the same origin as Administrative API v1. The
frontend bundle is not embedded into the `bamepd` executable: Server and Web already use
independent SemVer, Web compatibility is against the versioned Administrative API rather
than a Server build, and a compatible Web build should be replaceable without rebuilding
the Server. A conventional location such as `/usr/share/bamep/web/` is an illustrative
implementation direction only, not a normative requirement of this ADR. M1 assumes
same-origin root deployment; non-root/subpath deployment is deferred.

Static serving must preserve: frontend navigation may use SPA fallback; `/api/` misses
never return the frontend shell; missing fingerprinted static assets remain real `404`s;
fingerprinted assets may use immutable caching; the HTML application shell is revalidated
rather than permanently cached. The concrete Rust HTTP framework and serving composition
are not selected here; they belong to a future Technical Spike.

### 4. UI/design-system direction

A lightweight layered approach applies: design tokens → accessible interaction primitives →
Bamep-semantic components/patterns → feature components/routes. Semantic
components/patterns are introduced when real Bamep UI demonstrates shared meaning, not by
building a speculative catalog up front. Small, locally owned feature components for
genuinely unique visual behavior are preferred over giant universal components or
prop-heavy abstractions; not every wrapper is extracted preemptively. This preserves a
lesson evidenced by the Pascoal case study, used here only as evidence, not as a dependency.

### 5. Styling: Tailwind v4, selective Bits UI, no shadcn-svelte catalog

Tailwind CSS v4 is the initial styling mechanism — tokens, visual/layout utilities,
responsive styling, and reduced repeated custom declarations — and is not itself the design
system. Bamep Web targets modern evergreen browsers compatible with the selected Tailwind
v4 baseline; legacy-browser support is not an M1 requirement, and exact supported
browser-version matrices are deferred to future product/release support policy. Bits UI is
allowed selectively when a non-trivial accessible interaction primitive is actually needed;
native HTML is preferred where sufficient. The full shadcn-svelte catalog is not adopted as
the project's design system; individual shadcn-svelte implementations may later be
evaluated as source material, but copied code becomes Bamep-owned and must follow Bamep
tokens, primitives, semantics, and review discipline.

### 6. Browser/desktop/mobile/native-shell boundary

Product features, routes, semantic UI components, and the Administrative API client must
not import Tauri, Capacitor, or another native shell directly. This rule does not select
Tauri desktop, Tauri mobile, Capacitor, or PWA delivery; those remain future decisions. If
native-only capabilities are later required, they are exposed through narrow,
capability-specific interfaces/adapters (for example: notifications, file
selection/export, secure storage, tray/window integration) rather than a generic `Platform`
god object or a stringly typed generic `invoke(command)` abstraction. Native shells must
never become an alternative Bamep backend: they must not read PostgreSQL directly, invoke
Server-internal application/domain services, bypass Administrative API authorization, or
create hidden administrative write paths.

### 7. Deferred decisions

Explicitly kept outside this ADR: Tauri desktop delivery; Tauri mobile;
Capacitor selection; PWA installability/offline caching; local TLS strategy for PWA/mobile
browser capabilities; push notifications; the final SSE/WebSocket/realtime transport;
remote administration; administrative auth/RBAC; Web-originated writes; OpenAPI/codegen;
non-root Web deployment; the Rust HTTP framework and static-serving composition. Polling
remains an allowed snapshot-refresh mechanism under
`m0-administrative-api-web-read-contract.md`; it is not a new contract or a realtime
decision.

## Alternatives considered

- **Plain Svelte 5 + Vite without SvelteKit.** Rejected: Bamep Web needs routing,
  layout/error organization, and a static-build convention SvelteKit already provides;
  reimplementing that has no evidenced benefit.
- **React/Vite.** Rejected: no Bamep requirement justifies departing from Svelte, and it
  would add stack diversity without a concrete advantage for this narrow admin UI.
- **Embedding Web assets in the `bamepd` executable.** Rejected: breaks independent
  Server/Web SemVer and forces a Server rebuild to ship a compatible Web update, contrary
  to the accepted packaging direction in `m0-stack-and-boundaries-baseline.md`.
- **A separate standalone static Web server process.** Rejected for the selected single-server
  delivery model: adds a second
  deployable/operational surface, a second origin (or a reverse-proxy requirement), and
  cross-origin handling with no current requirement justifying it; serving from `bamepd`'s
  existing origin is simpler for the accepted Presentation target.
- **Direct native-shell imports from product features.** Rejected: creates hidden coupling
  to a specific shell and the risk of an alternative backend/authorization bypass.
- **Scoped/plain CSS as the sole styling strategy.** Rejected: without a token/utility layer,
  repeated presentation drifts into ad hoc spacing/color decisions per component, a failure
  mode the Pascoal case study evidences.
- **Wholesale shadcn-svelte adoption as the design system.** Rejected: risks the
  giant-universal-component/prop-heavy-abstraction pattern the Pascoal case study evidences;
  selective, Bamep-owned adoption preserves the layered design-system direction instead.

## Consequences

- Bamep gains a durable frontend stack (Svelte 5, SvelteKit, strict TypeScript,
  `adapter-static`) and a narrow Administrative API client boundary that future Presentation
  work implements against without re-deciding.
- Web ships as independently versioned static assets served by `bamepd`; ADR-0017 owns the
  concrete Rust HTTP/static-serving composition selected from the completed Technical Spike.
- The design-system layering (tokens → primitives → semantic components → features) and the
  Tailwind v4 / selective-Bits-UI styling direction apply to all future Presentation work
  until reconsidered.
- The native-shell boundary constrains all future desktop/mobile Presentation work
  regardless of which shell (if any) is eventually chosen; no current code depends on Tauri
  or Capacitor.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` gains a short normative
  Presentation dependency-boundary addition recording that Presentation clients consume
  Bamep business state only through the applicable versioned Administrative API contract;
  this ADR owns the rationale, not the restated contract text.
- No frontend source, dependency, build configuration, or Rust HTTP dependency is introduced
  by this ADR; it is a decision record only.

## Related architecture

- `docs/architecture/README.md` is intentionally not updated by this ADR: it describes
  implemented structure only, and no Presentation/static-serving implementation exists yet.
- ADR-0017 owns the Axum/Tower HTTP adapter and static-serving composition selected from the
  completed Technical Spike; this ADR continues to own the Presentation client and platform
  boundary rather than that Server Adapter choice.

Production implementation remains subject to approved product, UX, IAM, session, authorization,
and security work. Those concerns may constrain how this architecture is composed without
reopening the selected frontend stack absent new evidence.

## Related work

- Issue #20 — the original planned M1 implementation vehicle; it is no longer an M1 completion
  requirement after the owner-approved roadmap rebaseline.
- Issue #19 — Transfer/Artifact result state future Bamep Presentation may observe; completed and
  not changed by this ADR.
- Issue #28 — reconciliation state/outcomes Bamep Web observes; completed and owner-accepted
  before this ADR, not a pending dependency.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Presentation component
  boundary and dependency constraints this decision operates within; amended by this ADR
  with a short cross-reference only.
- `docs/specifications/m0-administrative-api-web-read-contract.md` — the Administrative API
  v1 read contract this decision's client and delivery model consume without redefining.

Status: Accepted.
