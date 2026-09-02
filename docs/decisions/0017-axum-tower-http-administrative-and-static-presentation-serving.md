# ADR-0017: Axum and Tower HTTP for Administrative and Static Presentation Serving

Status: Accepted

## Context

ADR-0016 selected the static SvelteKit Presentation architecture: Svelte 5, SvelteKit,
strict TypeScript, `adapter-static`, same-origin Web + Administrative API, independently
versioned filesystem-backed Web assets, and no embedded Web bundle. It explicitly deferred
one question: the concrete Rust HTTP framework and static-serving composition needed to
actually expose Administrative API v1 and the static Presentation build from the same
`bamepd` origin while preserving ADR-0016's routing, fallback, caching, and packaging
invariants.

`crates/server` already depended on Tokio for its runtime, but at the time this question was
raised it had no HTTP-server framework dependency and no Administrative HTTP route or
static-file module. Issue #20 was the original planned implementation vehicle. The later
owner-approved M1 roadmap rebaseline removed Presentation delivery from M1 completion; it did
not reopen the composition selected here.

Routing, SPA-fallback, and cache-header behavior are correctness- and security-sensitive:
an incorrect composition can let the Administrative API namespace fall through to the SPA
shell, serve a stale fingerprinted asset as if it were current, or cache a transient miss as
if it were a permanent one. This composition required its own evidence and decision rather than
being improvised during future Presentation implementation.

A Technical Spike empirically investigated Axum 0.8 + Tower/`tower-http` against ADR-0016's
invariants using a disposable fixture outside the repository. The original experiment
surfaced two composition defects on review — uniform immutable caching applied to `/_app/**`
miss responses, and an unconditional SPA-shell fallback for any miss outside `/api/**` and
`/_app/**` regardless of navigation intent — and a corrective pass proved fixes for both
inside the same Axum/Tower composition without requiring a different framework. The full
request matrix, exact prototype code, discovered failure modes, and tested dependency
versions are preserved in `docs/reference/static-presentation-serving-spike.md`; this ADR
does not reproduce that evidence.

## Decision

1. Axum 0.8 is the HTTP routing/framework baseline for Bamep Server's Administrative HTTP
   surface.

2. Tower and `tower-http` provide the middleware/static-file composition used for
   Presentation delivery (nested routing, static-file serving, and response-header
   middleware).

3. Administrative API routes remain structurally owned by their own API `Router` namespace,
   with its own fallback/error behavior, and must not fall through into the SPA shell.

4. Static Presentation assets remain filesystem-backed and independently replaceable, per
   ADR-0016. This ADR does not prescribe a permanent installation path.

5. SvelteKit's fingerprinted assets, under their own static asset subtree, are served
   without SPA fallback: a miss there is a genuine `404`, never the application shell.

6. Long-lived immutable cache policy applies only to successfully served fingerprinted
   assets.

7. Failed or missing fingerprinted resources remain genuine error responses and must not be
   transformed into the shell, nor receive immutable long-lived caching.

8. SPA fallback exists only for valid HTML-navigation semantics, not an unconditional "any
   unknown path → `index.html`" rule. The accepted baseline may use the empirically
   validated conservative rule:

   ```text
   GET or HEAD
   + request accepts text/html
   + static lookup produced 404
   -> serve application shell
   ```

   This semantic rule is architectural. The exact helper-function source in the Reference
   evidence is not normative; implementation may be refined as long as this observable
   behavior is preserved.

9. Unsupported HTTP methods and non-navigation resource misses retain genuine
   framework/static-serving error behavior (e.g. `405` for an unsupported method on an
   existing route, a real empty `404` for a missing subresource) rather than being coerced
   into the shell or into a success response.

10. Missing or unusable configured Web assets at startup must fail understandably (a clear
    startup error) rather than silently exposing a broken or partial Web/API composition.

The decision is on the compatible Axum 0.8 / Tower / `tower-http` major-minor architecture
line, not on frozen patch versions. Exact dependency versions are an implementation/release
concern tracked by `Cargo.lock`; `docs/reference/static-presentation-serving-spike.md` may
continue to record versions empirically tested during the Spike.

## Alternatives considered

- **Actix Web + `actix-files`.** A credible alternative; reviewed from current API
  documentation. Not selected because Axum/Tower composes directly on the Tokio runtime
  Bamep Server already uses, and the Axum/Tower prototype empirically satisfied every
  Bamep-specific serving invariant (API/static separation, fingerprinted-asset `404`
  behavior, status-aware caching, navigation-aware fallback) without requiring an additional
  application model or runtime. Actix was not empirically necessary once the Axum evidence
  became conclusive.
- **Ad hoc/manual HTTP and static-file implementation.** Rejected: routing, MIME/range/method
  handling, and static-file/middleware semantics should rely on mature framework/service
  crates rather than custom protocol plumbing, which would reintroduce the exact class of
  defects (incorrect fallback status, unconditional caching) the Spike found and corrected
  even inside a mature framework.
- **Embedding Web assets into `bamepd`.** Already rejected by ADR-0016 for independent
  Server/Web SemVer and replaceability reasons; not re-decided here.

## Consequences

- Future Presentation implementation may add Axum and Tower/`tower-http` dependencies to
  `crates/server`.
- Administrative API and static Presentation delivery can share one HTTP origin and one
  router/service composition.
- The routing/fallback/cache-header boundaries this ADR records need automated regression
  tests once implemented, so a future regression cannot silently reintroduce the corrected
  defects (API routes falling through to the shell, unconditional immutable caching on a
  miss, or unconditional shell fallback on any miss).
- The Reference Spike's negative cases (status-aware `/_app/**` caching, navigation-aware
  fallback, `ServeDir::not_found_service()` misuse) should become production test cases where
  applicable during implementation.
- Axum/Tower is an Adapter/runtime-composition concern; it does not enter Domain and does not
  change Domain's dependency constraints from `m0-stack-and-boundaries-baseline.md`.
- This ADR does not define Administrative API business behavior, request/response bodies, or
  resource representations; those remain owned by
  `m0-administrative-api-web-read-contract.md`.
- This ADR does not define TLS, authentication, RBAC, or realtime/push transport for the
  Administrative HTTP surface; those remain deferred per ADR-0016 and the read contract.
- Before production implementation, this composition must be revalidated against the eventual
  approved IAM, session, TLS, CSP, and related security constraints. This requirement does not
  select or design those mechanisms here.
- `docs/architecture/README.md` remains unchanged until this composition is actually
  implemented; Architecture describes implemented structure only.

## Related architecture

- ADR-0016 — the static SvelteKit Presentation client and platform boundary this ADR's
  serving composition implements; not reopened here.
- `docs/reference/static-presentation-serving-spike.md` — the empirical evidence supporting
  this decision: exact prototype, request matrix, negative findings, tested versions, and
  limitations. This ADR is the accepted decision; the Spike is supporting evidence, not
  itself an architectural decision.
- `docs/specifications/m0-administrative-api-web-read-contract.md` — the Administrative API
  v1 contract this composition exposes without redefining.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — the Presentation dependency
  boundary and packaging/versioning constraints this composition operates within.

## Related work

- Issue #20 — the original planned M1 implementation vehicle; it is no longer an M1 completion
  requirement after the owner-approved roadmap rebaseline.

Status: Accepted.
