import { fireEvent, render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import Page from './+page.svelte';

const rowCheckbox = (id: string) => screen.getByRole('checkbox', { name: `Selecionar ${id}` });

describe('/endpoints', () => {
	it('renders the full 12-endpoint fleet', () => {
		render(Page);
		for (let n = 1; n <= 12; n += 1) {
			const id = `LAB-${String(n).padStart(2, '0')}`;
			expect(screen.getByText(id)).toBeTruthy();
		}
	});

	it('shows the representative situations as distinct operator-facing text', () => {
		render(Page);
		expect(screen.getAllByText('Disponível').length).toBeGreaterThan(0);
		expect(screen.getAllByText('Em operação').length).toBe(2);
		expect(screen.getByText('Inclusão pendente')).toBeTruthy();
		expect(screen.getByText('Requer atenção')).toBeTruthy();
		expect(screen.getByText('Não pronto')).toBeTruthy();
		expect(screen.getByText('Sem contato')).toBeTruthy();
	});

	it('keeps current work and attention as separate, distinguishable rows', () => {
		render(Page);
		// current work: its own situation label plus a running-activity description
		expect(screen.getAllByText('Em operação').length).toBeGreaterThan(0);
		expect(screen.getByText('Capturar imagem')).toBeTruthy();
		// attention: a different label plus an exceptional-condition description
		expect(screen.getByText('Requer atenção')).toBeTruthy();
		expect(screen.getByText('Resultado incerto')).toBeTruthy();
		expect(screen.getByText('operação anterior terminou sem confirmação')).toBeTruthy();
	});

	it('starts with nothing selected and a non-submitting Nova operação control', () => {
		render(Page);
		expect(screen.queryByRole('region', { name: 'Resumo da seleção' })).toBeNull();
		expect(screen.queryByText(/selecionad/i)).toBeNull();
		const cta = screen.getByRole('button', { name: 'Nova operação' });
		expect((cta as HTMLButtonElement).disabled).toBe(true);
		expect(screen.queryByRole('link', { name: 'Nova operação' })).toBeNull();
	});

	it('selects LAB-03, LAB-07 and LAB-09 together and summarises the mix', async () => {
		render(Page);
		await fireEvent.click(rowCheckbox('LAB-03'));
		await fireEvent.click(rowCheckbox('LAB-07'));
		await fireEvent.click(rowCheckbox('LAB-09'));

		expect((rowCheckbox('LAB-03') as HTMLInputElement).checked).toBe(true);
		expect((rowCheckbox('LAB-07') as HTMLInputElement).checked).toBe(true);
		expect((rowCheckbox('LAB-09') as HTMLInputElement).checked).toBe(true);

		const summary = screen.getByRole('region', { name: 'Resumo da seleção' });
		expect(summary.textContent).toContain('3 Endpoints selecionados');
		expect(summary.textContent).toContain('1 pronto');
		expect(summary.textContent).toContain('1 requer atenção');
		expect(summary.textContent).toContain('1 não pronto');

		expect(screen.getByText('3 selecionados')).toBeTruthy();

		const link = screen.getByRole('link', { name: 'Nova operação' });
		expect(link.getAttribute('href')).toBe(
			'/operations/new?target=LAB-03&target=LAB-07&target=LAB-09'
		);
	});

	it('hands off selected targets in deterministic fleet order regardless of click order', async () => {
		render(Page);
		await fireEvent.click(rowCheckbox('LAB-09'));
		await fireEvent.click(rowCheckbox('LAB-03'));

		const link = screen.getByRole('link', { name: 'Nova operação' });
		expect(link.getAttribute('href')).toBe('/operations/new?target=LAB-03&target=LAB-09');
	});

	it('deselecting a row updates the count and breakdown', async () => {
		render(Page);
		await fireEvent.click(rowCheckbox('LAB-03'));
		await fireEvent.click(rowCheckbox('LAB-07'));
		await fireEvent.click(rowCheckbox('LAB-09'));
		await fireEvent.click(rowCheckbox('LAB-07'));

		const summary = screen.getByRole('region', { name: 'Resumo da seleção' });
		expect(summary.textContent).toContain('2 Endpoints selecionados');
		expect(summary.textContent).not.toContain('requer atenção');
	});

	it('clears the selection', async () => {
		render(Page);
		await fireEvent.click(rowCheckbox('LAB-03'));
		await fireEvent.click(rowCheckbox('LAB-07'));

		await fireEvent.click(screen.getByRole('button', { name: 'Limpar seleção' }));

		expect(screen.queryByRole('region', { name: 'Resumo da seleção' })).toBeNull();
		expect((rowCheckbox('LAB-03') as HTMLInputElement).checked).toBe(false);
		expect((screen.getByRole('button', { name: 'Nova operação' }) as HTMLButtonElement).disabled).toBe(
			true
		);
	});

	it('select-all selects the whole fleet, including attention and not-ready rows', async () => {
		render(Page);
		await fireEvent.click(screen.getByRole('checkbox', { name: 'Selecionar todos os Endpoints' }));

		const summary = screen.getByRole('region', { name: 'Resumo da seleção' });
		expect(summary.textContent).toContain('12 Endpoints selecionados');
		expect((rowCheckbox('LAB-07') as HTMLInputElement).checked).toBe(true);
		expect((rowCheckbox('LAB-09') as HTMLInputElement).checked).toBe(true);
		expect((rowCheckbox('LAB-12') as HTMLInputElement).checked).toBe(true);
	});
});
