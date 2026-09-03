# Bamep Web

Bamep operator-console Presentation client.

- Svelte 5 + SvelteKit, strict TypeScript, `@sveltejs/adapter-static`.
- Tailwind CSS v4 for styling; local design tokens in `src/lib/styles/app.css`.
- Fully static, client-rendered build (ADR-0016). No runtime SvelteKit server,
  no `+server` business routes, no backend-for-frontend.
- Independently versioned; intended to be served later by `bamepd` as static
  assets (ADR-0017). That serving integration is not part of this component.

At this stage the component is **foundation only**: the operator-console shell,
navigation between `Endpoints`, `Operações`, `Atenção` and `Configurações`, a
local localization boundary (`pt-BR`), and the styling/token/test harness. It
does not call any Administrative API and contains no product feature flows yet.

## Commands

```sh
npm install        # install from package-lock.json
npm run check      # svelte-kit sync + svelte-check (strict TS)
npm test           # vitest (localization boundary + shell navigation)
npm run build      # static production build -> build/
npm run dev        # local dev server
npm run preview    # serve the static build locally
```

No Bamep Server process is required for any of the above.

## Layout

The application shell is **fluid**: there is no global `max-width` on the
content area. Each feature route chooses the width appropriate to its own
information density; available horizontal space is used before introducing
avoidable vertical scrolling.

## Structure

```
src/
  lib/
    components/shell/   AppShell, Sidebar, NavIcon, nav model
    i18n/               local localization boundary + pt-BR catalog
    styles/             Tailwind entry + design tokens
  routes/               /endpoints /operations /attention /settings
static/                 static assets copied verbatim
```
