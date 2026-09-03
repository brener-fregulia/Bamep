import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	preprocess: vitePreprocess(),
	kit: {
		// ADR-0016 + static-presentation-serving-spike: a fully static, client-rendered
		// SPA. `fallback: 'index.html'` emits the application-shell fallback that ADR-0017's
		// future bamepd rule serves for HTML navigation misses (GET/HEAD + Accept:text/html
		// + static 404 -> application shell). `strict: true` fails the build if that
		// fallback is ever removed while routes are not prerendered. No runtime SvelteKit
		// server, no SSR.
		adapter: adapter({
			pages: 'build',
			assets: 'build',
			fallback: 'index.html',
			precompress: false,
			strict: true
		})
	}
};

export default config;
