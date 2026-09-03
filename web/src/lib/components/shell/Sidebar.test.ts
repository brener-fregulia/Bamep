import { render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import Sidebar from './Sidebar.svelte';

describe('Sidebar', () => {
	it('renders the localized pt-BR navigation', () => {
		render(Sidebar, { props: { pathname: '/endpoints' } });

		expect(screen.getByRole('link', { name: 'Endpoints' })).toBeTruthy();
		expect(screen.getByRole('link', { name: 'Operações' })).toBeTruthy();
		expect(screen.getByRole('link', { name: 'Atenção' })).toBeTruthy();
		expect(screen.getByRole('link', { name: 'Configurações' })).toBeTruthy();
	});

	it('marks only the active route with aria-current', () => {
		render(Sidebar, { props: { pathname: '/attention' } });

		expect(
			screen.getByRole('link', { name: 'Atenção' }).getAttribute('aria-current')
		).toBe('page');
		expect(
			screen.getByRole('link', { name: 'Endpoints' }).getAttribute('aria-current')
		).toBeNull();
	});

	it('moves active state when the pathname changes', async () => {
		const { rerender } = render(Sidebar, { props: { pathname: '/endpoints' } });
		expect(
			screen.getByRole('link', { name: 'Endpoints' }).getAttribute('aria-current')
		).toBe('page');

		await rerender({ pathname: '/operations' });

		expect(
			screen.getByRole('link', { name: 'Operações' }).getAttribute('aria-current')
		).toBe('page');
		expect(
			screen.getByRole('link', { name: 'Endpoints' }).getAttribute('aria-current')
		).toBeNull();
	});
});
