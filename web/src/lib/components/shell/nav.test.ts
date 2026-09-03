import { describe, expect, it } from 'vitest';
import { isNavItemActive, primaryNav, resolvePrimaryNav } from './nav';

describe('primary navigation model', () => {
	it('exposes the four M2 shell routes, in order', () => {
		expect(primaryNav.map((item) => item.href)).toEqual([
			'/endpoints',
			'/operations',
			'/attention',
			'/settings'
		]);
	});

	it('marks exactly the matching route active', () => {
		const items = resolvePrimaryNav('/operations');
		expect(items.find((item) => item.href === '/operations')?.active).toBe(true);
		expect(items.filter((item) => item.active)).toHaveLength(1);
	});

	it('keeps a section active for a nested feature route', () => {
		expect(
			isNavItemActive(
				{ id: 'endpoints', href: '/endpoints', labelKey: 'nav.endpoints' },
				'/endpoints/LAB-03'
			)
		).toBe(true);
	});

	it('activates nothing for an unrelated path', () => {
		expect(resolvePrimaryNav('/').some((item) => item.active)).toBe(false);
	});
});
