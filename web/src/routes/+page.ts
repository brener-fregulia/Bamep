import { redirect } from '@sveltejs/kit';

// `/` resolves to the default Endpoints view. With SSR disabled this runs in the
// browser: the SPA application shell loads first, then the client router performs
// the redirect — the fallback shell is never a redirect-only document.
export function load() {
	redirect(307, '/endpoints');
}
