# Static Presentation Serving — Rust HTTP Composition Spike

Status: **Completed empirical reference.**

This document preserves empirical evidence from a Technical Spike into the Rust HTTP
adapter/framework composition candidate for serving Bamep's static SvelteKit Presentation
client and Administrative API from the same `bamepd` origin. It does not define current
Bamep architecture. ADR-0016 owns the accepted static-Presentation/platform-boundary
decision and explicitly deferred the concrete Rust HTTP framework/serving composition to
this Spike. The experiment below supports a candidate for owner architectural approval; it
does not itself accept that architecture.

## Question

Which Rust HTTP adapter/framework composition should Bamep use to expose Administrative
API routes and serve its independently replaceable static SvelteKit Presentation client
from the same `bamepd` origin while reliably preserving ADR-0016's routing, fallback,
caching, and packaging invariants?

## Why existing evidence was insufficient

At the time of this Spike, `crates/server/Cargo.toml` contained no HTTP-server framework
dependency and `crates/server/src` contained no Administrative HTTP route or static-file
module. ADR-0016 accepted the static SvelteKit Presentation stack and delivery model but
explicitly deferred the concrete Rust HTTP framework and serving composition to a future
Technical Spike.

## Environment and toolchain

- Host: Windows 11 Pro 10.0.26200.
- `rustc`/`cargo` 1.96.0; Node.js v24.16.0; npm 12.0.2.
- Loopback-only HTTP server (`127.0.0.1:4173`); no LAN exposure, no PostgreSQL, no real
  credentials.
- All experiment material was created and executed outside the Bamep repository, under a
  disposable temporary directory, and deleted after the experiment.

## Exact versions tested

- `axum` 0.8.9, `tower` 0.5.3, `tower-http` 0.7.0 (features: `fs`, `set-header`), `tokio`
  1.53.1, `http` 1.5.0.
- `svelte` ^5.56.1, `@sveltejs/kit` ^2.63.0, `@sveltejs/adapter-static` 3.0.10, `vite`
  ^8.0.16.
- Actix Web 4.15.0 / `actix-files` 0.7.0 were reviewed from current `docs.rs` API
  documentation only (not built or empirically tested).

## Experiment structure

A minimal SvelteKit `adapter-static` fixture (routes `/` and `/endpoints/demo`, fully
client-rendered: `ssr = false`, `prerender = false`, `fallback: 'index.html'`) was served
by a standalone Axum + Tower HTTP fixture binary composed as:

- `/api/admin/v1/*` — a nested `Router` with its own `.fallback()` returning a JSON 404,
  exposing one mock read handler (`GET /api/admin/v1/endpoints/demo`);
- `/_app/*` — a `tower_http::services::ServeDir` over SvelteKit's content-hashed build
  output directory, with **no** fallback configured, so a miss returns a genuine empty
  404;
- everything else — a top-level `ServeDir` with `.fallback(ServeFile::new(index.html))`
  (SPA navigation fallback), wrapped with `Cache-Control: no-cache`;
- the `_app` branch wrapped with `Cache-Control: public, max-age=31536000, immutable`;
- startup fails closed (`process::exit(1)`) if the configured Web directory or its
  `index.html` is missing.

## Tested request matrix and observed responses

| Request | Status | Observation |
|---|---|---|
| `GET /` | 200 | Real HTML shell, `cache-control: no-cache` |
| `GET /endpoints/demo` | 200 | SPA fallback shell, byte-identical to `/` |
| `GET /api/admin/v1/endpoints/demo` | 200 | JSON, never HTML |
| `GET /api/admin/v1/does-not-exist` | 404 | JSON API 404 (`ADMIN_API_ROUTE_NOT_FOUND`), never the SPA shell |
| `POST /api/admin/v1/endpoints/demo` | 405 | `Allow: GET,HEAD` |
| `POST /endpoints/demo` | 405 | `Allow: GET,HEAD` — no accidental HTML-shell success |
| `GET` existing fingerprinted asset (`/_app/immutable/entry/start.<hash>.js`) | 200 | `cache-control: public, max-age=31536000, immutable` |
| `GET` missing/stale fingerprinted asset under `/_app/**` | 404 | Empty body — never the SPA shell |
| `GET /robots.txt` | 200 | Real non-fingerprinted file, `cache-control: no-cache` |
| Same-origin check | n/a | No `Access-Control-*` headers on API or Web responses |
| Start with a nonexistent configured Web directory | process exits 1 | Clear stderr message; no port bound; `/api/` never silently exposed with a broken static layer |

## Routing/fallback ownership

The four required distinctions — registered Administrative API namespace, real
fingerprinted static assets, recognized frontend navigation fallback, and true missing
resources — are each owned by a structurally separate `Service`/`Router`, not by ad hoc
path-string matching:

- the API namespace is a nested `Router` with its own `.fallback()`;
- the fingerprinted-asset subtree (`/_app/**`, the one guaranteed content-hashed directory
  `adapter-static` produces) is a `ServeDir` with no fallback at all, so a miss there is a
  genuine 404;
- everything else falls through to a `ServeDir` whose fallback is `ServeFile::index.html`,
  producing the SPA shell only for paths outside the API namespace and outside the
  fingerprinted subtree.

This keeps ADR-0016's forbidden "any unknown path → index.html" rule explicitly scoped
away from both the API namespace and the fingerprinted-asset subtree, rather than applied
globally. A regression (e.g. removing the nested API fallback, or adding a fallback to the
`_app` `ServeDir`) changes an observable response shape (HTML instead of JSON, or a shell
instead of a 404), which the request matrix above is intended to catch.

## Filesystem asset-replacement observation

The fixture Web build was replaced on disk (new build output, changed content-hash
filenames, changed shell marker text) while the compiled Rust binary was left untouched
(verified identical file size/mtime). Restarting the same binary against the same
filesystem path served the new build's content without any Rust rebuild. Requesting the
prior build's now-superseded fingerprinted filename against the replaced directory
correctly returned a genuine 404, not the SPA shell — demonstrating the realistic
post-deployment stale-asset case.

## Negative finding

`tower_http::services::ServeDir::not_found_service()` forces the fallback response's HTTP
status to `404`, per its own doc comment and implementation
(`self.fallback(SetStatus::new(new_fallback, StatusCode::NOT_FOUND))` in `tower-http`
0.7.0). This method is intended for a custom **error page**, not SPA navigation fallback,
and using it for SPA fallback produces a `404` shell response, violating the required
`200` SPA-fallback behavior. The corresponding correct API is the plain
`ServeDir::fallback(ServeFile::new(index.html))`, which preserves the fallback service's
own status. This was discovered empirically (an initial fixture iteration returned 404 for
`/endpoints/demo`), root-caused by reading the tower-http 0.7.0 source, corrected, and
re-verified.

## Limitations

- Single deterministic loopback run; no load, concurrency, or TLS-listener composition was
  tested. Nothing observed here suggests Axum/`tower-http` structurally prevents a future
  `rustls`-based TLS composition, but that composition itself is unverified by this Spike.
- Only Axum + Tower HTTP was empirically prototyped; Actix Web + `actix-files` was reviewed
  from current documentation only, not built.
- Executed on native Windows, not the Linux Server reference/production environment; no
  Linux-filesystem-specific behavior (permissions, case sensitivity, symlinks) was
  exercised.
- The SvelteKit fixture is intentionally minimal and does not represent a full ADR-0016
  Web bundle (no Tailwind, Bits UI, i18n, or Tauri).
- `Cache-Control: public, max-age=31536000, immutable` is currently applied uniformly to
  the whole `/_app/**` branch, including its own 404 responses — cosmetically imperfect,
  though it does not compromise the required miss/fallback distinction.

## Conclusion

The experiment supports Axum 0.8 + Tower HTTP (`ServeDir`/`ServeFile` +
`SetResponseHeaderLayer`), composed as a nested API router with its own fallback, a
no-fallback `ServeDir` for the fingerprinted-asset subtree, and a `fallback`-based (not
`not_found_service`-based) top-level `ServeDir` for SPA navigation, as the candidate for
owner architectural approval to satisfy ADR-0016's routing, fallback, caching, and
packaging invariants. This is empirical evidence for that future decision; it does not
itself constitute owner-accepted architecture.

## Related

- `docs/decisions/0016-static-sveltekit-presentation-client-and-platform-boundary.md` —
  the accepted decision this Spike's evidence informs.
- `docs/specifications/m0-administrative-api-web-read-contract.md` — the Administrative
  API v1 read contract the mock fixture routes were shaped after.
- Issue #20 — the Work Package this evidence makes architecture-ready, pending a
  corresponding ADR.
