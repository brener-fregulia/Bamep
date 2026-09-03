/**
 * Primary navigation model for the operator-console shell.
 *
 * Pure data + pure functions so active-route resolution is unit-testable
 * without SvelteKit. The shell component renders this; it derives nothing about
 * navigation on its own.
 */
import type { MessageKey } from '$lib/i18n';

export type NavItemId = 'endpoints' | 'operations' | 'attention' | 'settings';

export interface NavItem {
	id: NavItemId;
	href: string;
	labelKey: MessageKey;
}

export interface ResolvedNavItem extends NavItem {
	active: boolean;
}

export const primaryNav: readonly NavItem[] = [
	{ id: 'endpoints', href: '/endpoints', labelKey: 'nav.endpoints' },
	{ id: 'operations', href: '/operations', labelKey: 'nav.operations' },
	{ id: 'attention', href: '/attention', labelKey: 'nav.attention' },
	{ id: 'settings', href: '/settings', labelKey: 'nav.settings' }
];

/**
 * A nav section is active for its own route and for any nested feature route
 * beneath it (e.g. `/endpoints/LAB-03`).
 */
export function isNavItemActive(item: NavItem, pathname: string): boolean {
	return pathname === item.href || pathname.startsWith(`${item.href}/`);
}

export function resolvePrimaryNav(pathname: string): ResolvedNavItem[] {
	return primaryNav.map((item) => ({
		...item,
		active: isNavItemActive(item, pathname)
	}));
}
