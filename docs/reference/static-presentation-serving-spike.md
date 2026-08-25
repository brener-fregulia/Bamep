# Static Presentation Serving — Rust HTTP Composition Spike

Status: **Completed empirical reference (amended).**

This document preserves empirical evidence from a Technical Spike into the Rust HTTP
adapter/framework composition candidate for serving Bamep's static SvelteKit Presentation
client and Administrative API from the same `bamepd` origin. It does not define current
Bamep architecture. ADR-0016 owns the accepted static-Presentation/platform-boundary
decision and explicitly deferred the concrete Rust HTTP framework/serving composition to
this Spike. The experiment below supports a candidate for owner architectural approval; it
does not itself accept that architecture.

This document was amended by a narrow corrective evidence pass after the original
experiment. The correction addresses two composition defects identified during review of
the original evidence (uniform immutable caching on `_app` 404 responses, and unconditional
SPA-shell fallback for any miss outside `/api/`/`/_app/**` regardless of whether the request
was HTML navigation or a genuine resource fetch). The original framework selection (Axum
0.8 + Tower HTTP) was not reopened; both corrections were proven inside that same
composition. Sections rebuilt/re-verified for this correction are noted inline; the rest of
the document reflects the original experiment, re-verified where stated.

## Question

Which Rust HTTP adapter/framework composition should Bamep use to expose Administrative
API routes and serve its independently replaceable static SvelteKit Presentation client
from the same `bamepd` origin while reliably preserving ADR-0016's routing, fallback,
caching, and packaging invariants?

## Why existing evidence was insufficient

At the time of the original Spike, `crates/server/Cargo.toml` contained no HTTP-server
framework dependency and `crates/server/src` contained no Administrative HTTP route or
static-file module. ADR-0016 accepted the static SvelteKit Presentation stack and delivery
model but explicitly deferred the concrete Rust HTTP framework and serving composition to a
future Technical Spike.

The original experiment's fixture applied `Cache-Control: public, max-age=31536000,
immutable` uniformly to the whole `/_app/**` branch, including its own `404` responses, and
recorded this as merely "cosmetically imperfect." On review, that characterization
understated the defect: a missing fingerprinted asset must never receive the long-lived
immutable caching policy meant for successfully served content-addressed assets, because a
client or intermediary caching a `404` as if it were an immutable asset can mask a
genuinely available asset (e.g. after a corrected deployment) for up to a year. Separately,
the original fixture's top-level SPA fallback returned the shell for any miss outside
`/api/**` and `/_app/**`, without distinguishing genuine HTML browser navigation from a
missing subresource (e.g. a dangling `.css`/`.js` reference), which is a weaker guarantee
than ADR-0016's description of "frontend navigation may use SPA fallback" alongside
"missing fingerprinted static assets remain real `404`s" implies for resources in general.
This correction re-ran a rebuilt disposable fixture to prove both distinctions hold before
the evidence is used for owner architectural approval.

## Environment and toolchain

- Host: Windows 11 Pro 10.0.26200.
- `rustc`/`cargo` 1.96.0; Node.js v24.16.0; npm 12.0.2.
- Loopback-only HTTP server (`127.0.0.1:4174`); no LAN exposure, no PostgreSQL, no real
  credentials.
- All experiment material (original and corrective) was created and executed outside the
  Bamep repository, under a disposable OS temporary directory, and deleted after the
  experiment. The corrective pass used a freshly recreated fixture; the original fixture no
  longer existed on disk.

## Exact versions tested

Original experiment:

- `axum` 0.8.9, `tower` 0.5.3, `tower-http` 0.7.0 (features: `fs`, `set-header`), `tokio`
  1.53.1, `http` 1.5.0.
- `svelte` ^5.56.1, `@sveltejs/kit` ^2.63.0, `@sveltejs/adapter-static` 3.0.10, `vite`
  ^8.0.16.
- Actix Web 4.15.0 / `actix-files` 0.7.0 were reviewed from current `docs.rs` API
  documentation only (not built or empirically tested).

Corrective pass (rebuilt fixture, same day):

- `axum` 0.8.9, `tower` 0.5.3, `tower-http` 0.7.0 (features: `fs`, `set-header`), `tokio`
  1.53.1, `http` 1.5.0 - identical Rust dependency versions resolved again from crates.io,
  confirming the composition is reproducible against the same Axum/Tower HTTP line.
- `svelte` 5.56.10, `@sveltejs/kit` 2.70.3, `@sveltejs/adapter-static` 3.0.10, `vite` 6.4.3.
  `vite` moved from the originally recorded `^8.0.16` constraint to `^6.0.0` because the
  currently resolvable `@sveltejs/vite-plugin-svelte` (pulled in transitively by the now-
  current `@sveltejs/kit`) only supports `vite` `^6`; this is a routine JavaScript-ecosystem
  version drift unrelated to either correction and does not affect the Rust-side findings.
- Actix Web was not reconsidered; the corrective pass stayed inside Axum + Tower HTTP as
  instructed.

## Experiment structure

A minimal SvelteKit `adapter-static` fixture (routes `/` and `/endpoints/demo`, fully
client-rendered: `ssr = false`, `prerender = false`, `fallback: 'index.html'`) was rebuilt
and served by a rebuilt standalone Axum + Tower HTTP fixture binary, composed as:

- `/api/admin/v1/*` - a nested `Router` with its own `.fallback()` returning a JSON 404,
  exposing one mock read handler (`GET /api/admin/v1/endpoints/demo`); unchanged from the
  original experiment.
- `/_app/*` - a `tower_http::services::ServeDir` over SvelteKit's content-hashed build
  output directory, with no fallback configured (a miss is a genuine empty `404`), wrapped
  in a status-aware cache-header layer (corrected; see below).
- everything else - a `tower_http::services::ServeDir` over the build root (serving real
  files such as `/robots.txt` as-is) whose miss path is handled by a navigation-aware
  fallback handler (corrected; see below) instead of an unconditional
  `ServeFile::new(index.html)` fallback, wrapped with `Cache-Control: no-cache`.
- startup fails closed (`process::exit(1)`) if the configured Web directory or its
  `index.html` is missing; unchanged from the original experiment, re-verified.

### Corrected mechanism 1 - status-aware immutable caching for /_app/**

The `_app` branch no longer applies `Cache-Control` unconditionally to the whole
`ServeDir`. Instead it uses one
`tower_http::set_header::SetResponseHeaderLayer::overriding` layer whose value-producing
closure inspects the wrapped response's status before choosing a header value:

```rust
ServiceBuilder::new()
    .layer(SetResponseHeaderLayer::overriding(
        header::CACHE_CONTROL,
        |response: &Response<_>| {
            let value = if response.status().is_success() {
                "public, max-age=31536000, immutable"
            } else {
                "no-store"
            };
            Some(HeaderValue::from_static(value))
        },
    ))
    .service(ServeDir::new(&app_dir))
```

This works because `tower_http::set_header::MakeHeaderValue<T>` (the trait
`SetResponseHeaderLayer` accepts as its value source) is implemented for any
`FnMut(&T) -> Option<HeaderValue>`, not only for a fixed `HeaderValue`; here `T` is
`Response<ResBody>`, so the closure receives the actual outgoing response - including its
status - before the header is inserted. `overriding` mode replaces any existing value for
the header. Confirmed against the `tower-http` 0.7.0 source (`src/set_header/mod.rs`,
`src/set_header/response/single_header.rs`): the closure implementation of
`MakeHeaderValue` and the `Override` insertion mode are exactly as used here. No additional
crate or middleware was needed beyond `tower-http`'s existing `set_header` module.

### Corrected mechanism 2 - navigation-aware SPA fallback outside /api/ and /_app/**

The top-level "everything else" branch no longer configures
`ServeDir::fallback(ServeFile::new(index.html))` (which would unconditionally turn any miss
into the SPA shell). Instead it uses a single `tower::service_fn` handler that:

1. Calls the root `ServeDir` (over the Web build root, no fallback) with the incoming
   request and inspects the resulting response.
2. If that response is not a `404` (a real static file, a `2xx`, or e.g. a `405 Method Not
   Allowed` for an unsupported method), it is returned unchanged.
3. If it is a `404`, and the request is classified as HTML navigation, a fresh request
   (same method/URI/headers, empty body) is dispatched to `ServeFile::new(index.html)` and
   its `200` response becomes the result.
4. If it is a `404` and the request is not classified as HTML navigation, the `ServeDir`'s
   own genuine (empty) `404` is returned unchanged - never the shell.

Navigation classification (`is_html_navigation`) uses only request-observable semantics,
not a route allowlist:

```rust
fn is_html_navigation<B>(req: &http::Request<B>) -> bool {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return false;
    }
    match req.headers().get(header::ACCEPT) {
        Some(value) => value.to_str().map(|a| a.contains("text/html")).unwrap_or(false),
        None => false,
    }
}
```

This mirrors how real browsers distinguish a page navigation (`Accept: text/html,...`) from
a subresource fetch (`Accept: text/css,*/*;q=0.1` for stylesheets, `Accept: */*` for many
script/module fetches) or a non-`GET`/`HEAD` request. No SvelteKit route catalog is
hardcoded; the same handler serves any current or future route path without maintenance
when new frontend routes are added, because it only inspects method and `Accept`, not a
fixed path list.

## Tested request matrix and observed responses

Re-verified original cases (rebuilt fixture, same expected outcomes):

| Request | Status | Observation |
|---|---|---|
| `GET /` | 200 | Real HTML shell, `cache-control: no-cache` |
| `GET /endpoints/demo` (`Accept: text/html,...`) | 200 | SPA fallback shell, byte-identical to `/` |
| `GET /api/admin/v1/endpoints/demo` | 200 | JSON, never HTML |
| `GET /api/admin/v1/does-not-exist` | 404 | JSON API 404 (`ADMIN_API_ROUTE_NOT_FOUND`), never the SPA shell |
| `POST /api/admin/v1/endpoints/demo` | 405 | `Allow: GET,HEAD` |
| `POST /endpoints/demo` | 405 | `Allow: GET,HEAD` - no accidental HTML-shell success; the 404-vs-passthrough logic in mechanism 2 preserves this because the underlying `ServeDir` result was `405`, not `404`, so it is returned unchanged |
| `GET` existing fingerprinted asset (`/_app/immutable/entry/start.<hash>.js`) | 200 | `cache-control: public, max-age=31536000, immutable` |
| `GET /robots.txt` | 200 | Real non-fingerprinted file, `cache-control: no-cache` |
| Same-origin check | n/a | No `Access-Control-*` headers observed on any API or Web response in this matrix |
| Start with a nonexistent configured Web directory | process exits 1 | Clear stderr message; no port bound; `/api/` never silently exposed with a broken static layer |

New/corrected cases proven by this pass:

| Request | Status | Observation |
|---|---|---|
| `GET` missing/stale fingerprinted asset under `/_app/**` | 404 | Empty body, `cache-control: no-store` - no longer `immutable`; corrects the original defect |
| `GET /missing.css` with `Accept: text/css,*/*;q=0.1` | 404 | Empty body (0 bytes) - genuine resource miss, not the SPA shell, because the request is not classified as HTML navigation |
| `GET /missing.js` with `Accept: */*` | 404 | Empty body (0 bytes) - genuine resource miss, not the SPA shell, for the same reason |

The `POST /endpoints/demo` case above additionally demonstrates that mechanism 2's `404`
check does not accidentally swallow non-`404` outcomes such as `405`: a `POST` is never
navigation (fails the method check before the `Accept` header is even inspected), and
independently the underlying `ServeDir` already returns `405` rather than `404` for that
method, so the response is passed through untouched either way.

## Routing/fallback ownership

The distinctions required by ADR-0016 - registered Administrative API namespace, real
fingerprinted static assets, recognized frontend navigation fallback, and true missing
resources - are owned by a combination of structurally separate `Service`/`Router`
composition and, where a single branch must serve more than one of those cases, explicit
request-semantics logic rather than ad hoc path-string matching:

- the API namespace is a nested `Router` with its own `.fallback()`;
- the fingerprinted-asset subtree (`/_app/**`, the one guaranteed content-hashed directory
  `adapter-static` produces) is a `ServeDir` with no fallback at all, so a miss there is a
  genuine 404, and its cache header is now chosen per-response by status (mechanism 1
  above) rather than applied to the whole branch;
- everything else falls through to a `ServeDir` over the build root; a `404` from that
  `ServeDir` is only replaced by the SPA shell when the request is classified as HTML
  navigation (mechanism 2 above) - a real resource miss (wrong `Accept`, or a non-`GET`/
  `HEAD` method) is passed through as `ServeDir`'s own response.

This keeps ADR-0016's forbidden "any unknown path -> index.html" rule explicitly scoped
away from the API namespace, the fingerprinted-asset subtree, and, after this correction,
away from non-navigation resource misses within the remaining branch too. A regression
(e.g. removing the nested API fallback, adding a fallback to the `_app` `ServeDir`, making
the `_app` cache header unconditional again, or dropping the navigation check so any miss
becomes the shell) changes an observable response shape (HTML instead of JSON, a shell
instead of a 404, or an immutable cache header on a 404), which the request matrix above is
intended to catch.

## Filesystem asset-replacement observation

Re-verified with the corrected fixture. A second SvelteKit build ("version B", changed page
content and freshly-hashed `_app` filenames) was produced into a separate directory. The
already-compiled fixture binary (unchanged, same file, no Rust rebuild) was restarted with
its configured Web-directory environment variable pointed at the new build directory only.
Observed:

- `GET /` served version B's shell, referencing version B's fingerprinted script filename
  (`start.Dp0_KX5F.js` instead of version A's `start.DmBW_tpK.js`), confirming served
  content follows the configured directory rather than anything baked into the binary.
- The new build's fingerprinted asset was served `200` with the immutable cache header
  (mechanism 1, success path).
- Requesting version A's now-superseded fingerprinted filename against the version B
  directory correctly returned a genuine `404` with `cache-control: no-store` - not the SPA
  shell and not an immutable cache header - demonstrating the realistic post-deployment
  stale-asset case under the corrected caching mechanism.
- Restarting the same unmodified binary against a nonexistent configured directory again
  exited `1` before binding a port, matching the original fail-closed finding.

## Negative finding

`tower_http::services::ServeDir::not_found_service()` forces the fallback response's HTTP
status to `404`, per its own doc comment and implementation
(`self.fallback(SetStatus::new(new_fallback, StatusCode::NOT_FOUND))` in `tower-http`
0.7.0). This method is intended for a custom error page, not SPA navigation fallback, and
using it for SPA fallback produces a `404` shell response, violating the required `200`
SPA-fallback behavior. The corresponding correct API is the plain
`ServeDir::fallback(ServeFile::new(index.html))`, which preserves the fallback service's
own status. This was discovered empirically in the original experiment (an initial fixture
iteration returned 404 for `/endpoints/demo`), root-caused by reading the tower-http 0.7.0
source, corrected, and re-verified. This corrective pass re-read the same source location
in a freshly downloaded `tower-http` 0.7.0 crate and confirms the implementation is
unchanged: the finding remains valid. It also confirms a related, previously unstated
detail from the same source: `ServeDir` defaults `call_fallback_on_method_not_allowed` to
`false`, i.e. it returns `405 Method Not Allowed` (not the configured fallback) for
non-`GET`/`HEAD` requests unless explicitly reconfigured - which is why mechanism 2's
`404`-vs-passthrough check above is safe for the `POST` case in the request matrix.

## Limitations

- Single deterministic loopback run per corrected mechanism; no load, concurrency, or
  TLS-listener composition was tested. Nothing observed here suggests Axum/`tower-http`
  structurally prevents a future `rustls`-based TLS composition, but that composition
  itself is unverified by this Spike.
- Only Axum + Tower HTTP was empirically prototyped; Actix Web + `actix-files` was reviewed
  from current documentation only, not built, in the original experiment and was not
  reconsidered by this corrective pass.
- Executed on native Windows, not the Linux Server reference/production environment; no
  Linux-filesystem-specific behavior (permissions, case sensitivity, symlinks) was
  exercised.
- The SvelteKit fixture is intentionally minimal and does not represent a full ADR-0016
  Web bundle (no Tailwind, Bits UI, i18n, or Tauri).
- The navigation-vs-resource distinction (mechanism 2) relies on the request's `Accept`
  header and method. This matches how real browsers request page navigations versus
  subresources, and was exercised here with representative `Accept` values
  (`text/html,...`, `text/css,*/*;q=0.1`, `*/*`), but an HTTP client that omits or
  misrepresents `Accept` for a genuine navigation (some non-browser tooling) will be
  classified as a non-navigation resource miss and receive a real `404` instead of the
  shell. This is a deliberate conservative choice (never serve the shell when navigation
  intent is unclear) rather than a defect, but it is a real limitation of an `Accept`-based
  heuristic and is recorded here for future reference.
- The status-aware cache mechanism (mechanism 1) was only exercised for the two response
  shapes `ServeDir` actually produces under `/_app/**` in this fixture (`200` and `404`);
  it was not exercised against other status codes (e.g. `304 Not Modified` from
  conditional requests, or `416` from unsatisfiable range requests), which `ServeDir` can
  also produce. `response.status().is_success()` (the `2xx` range) would route a future
  `304` to the `no-store` branch rather than treating it as a cache-relevant success; this
  was not observed to occur in this fixture's test matrix and is left as a documented gap
  rather than an evidenced defect.

## Conclusion

The experiment continues to support Axum 0.8 + Tower HTTP (`ServeDir`/`ServeFile` +
`SetResponseHeaderLayer`), composed as a nested API router with its own fallback, a
no-fallback `ServeDir` for the fingerprinted-asset subtree with a status-aware cache-header
layer, and a navigation-aware (not unconditional) fallback handler for the remaining
static/SPA branch, as the candidate for owner architectural approval to satisfy ADR-0016's
routing, fallback, caching, and packaging invariants. This corrective pass fixed both
composition defects identified in the original evidence (uniform immutable caching on
`_app` misses; unconditional SPA-shell fallback for any miss) without requiring a different
framework or additional dependencies beyond what the original experiment already used. This
remains empirical evidence for a future architectural decision; it does not itself
constitute owner-accepted architecture.

## Related

- `docs/decisions/0016-static-sveltekit-presentation-client-and-platform-boundary.md` -
  the accepted decision this Spike's evidence informs.
- `docs/specifications/m0-administrative-api-web-read-contract.md` - the Administrative
  API v1 read contract the mock fixture routes were shaped after.
- Issue #20 - the Work Package this evidence makes architecture-ready, pending a
  corresponding ADR.
