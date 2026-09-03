import { describe, expect, it } from 'vitest';
import * as layout from './+layout';
import { load as rootLoad } from './+page';

/**
 * Regression guard for the static SPA presentation foundation
 * (ADR-0016 / docs/reference/static-presentation-serving-spike.md):
 * the build must stay client-rendered with an application-shell fallback, and
 * `index.html` must not regress into a redirect-only document. The
 * `strict: true` adapter-static option in svelte.config.js separately fails the
 * build if the `fallback` is removed.
 */
describe('SPA presentation foundation', () => {
	it('is client-rendered, not prerendered', () => {
		expect(layout.ssr).toBe(false);
		expect(layout.prerender).toBe(false);
	});

	it('redirects "/" to /endpoints through the client (not a prerendered redirect page)', () => {
		let thrown: unknown;
		try {
			rootLoad();
		} catch (error) {
			thrown = error;
		}
		expect(thrown).toMatchObject({ status: 307, location: '/endpoints' });
	});
});
