// ADR-0016 + docs/reference/static-presentation-serving-spike.md: Bamep Web is a
// fully client-rendered SPA. No SSR and no prerendering — `@sveltejs/adapter-static`
// emits a single `index.html` application-shell fallback (see svelte.config.js).
// `bamepd` will serve that shell for HTML navigation misses (ADR-0017) and the
// client router resolves the real route, including future deep links such as
// `/endpoints/LAB-03`.
export const ssr = false;
export const prerender = false;
export const trailingSlash = 'never';
