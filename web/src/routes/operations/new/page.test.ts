import { fireEvent, render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import NewOperationView from '$lib/components/operations/NewOperationView.svelte';

// The surface under test is NewOperationView; +page.svelte is only a thin
// wrapper that reads the repeated `target` query parameters from the SvelteKit
// route state and forwards them as `requestedIds`.

const ACCEPTANCE_TARGETS = ['LAB-03', 'LAB-07', 'LAB-09'];

const renderScenario = (requestedIds: readonly string[] = ACCEPTANCE_TARGETS) =>
	render(NewOperationView, { requestedIds });

const toggle = (name: string) => screen.getByRole('checkbox', { name });

describe('/operations/new', () => {
	it('keeps the acceptance target set visible with mixed operator-facing situations', () => {
		renderScenario();
		for (const id of ACCEPTANCE_TARGETS) {
			expect(screen.getAllByText(id).length).toBeGreaterThan(0);
		}
		expect(screen.getByText('Disponível')).toBeTruthy();
		expect(screen.getByText('Requer atenção')).toBeTruthy();
		expect(screen.getByText('Não pronto')).toBeTruthy();
		// selection did not resolve the conditions
		expect(screen.getByText(/não resolve a condição indicada/)).toBeTruthy();
		expect(
			screen.getByText(
				'LAB-07 possui um resultado anterior ainda incerto — configurar esta operação não resolve a condição.'
			)
		).toBeTruthy();
		expect(
			screen.getByText('LAB-09 não está pronto para uma nova operação neste momento.')
		).toBeTruthy();
	});

	it('renders the fixed mock intent and the common configuration', () => {
		renderScenario();
		expect(screen.getByText('Reinstalar Windows')).toBeTruthy();
		expect(screen.getByRole('heading', { name: 'Configuração comum' })).toBeTruthy();
		expect(screen.getByText('Reinstalação do Windows')).toBeTruthy();
		expect(screen.getByText('Instalar drivers')).toBeTruthy();
		expect((toggle('Instalar drivers em todos os alvos') as HTMLInputElement).checked).toBe(true);
	});

	it('renders only the representative differences per Endpoint', () => {
		renderScenario();
		expect(screen.getByText('Ajustes por Endpoint')).toBeTruthy();
		expect(screen.getByText('Preservar e restaurar os dados do usuário')).toBeTruthy();
		expect(screen.getByText('Aplicar o debloat configurado')).toBeTruthy();
		// LAB-09 stays the no-override/common case
		expect(screen.getByText('Sem ajustes — segue a configuração comum.')).toBeTruthy();
		expect(screen.getByText('2 Endpoints com ajustes · 1 segue apenas a configuração comum')).toBeTruthy();
		// target panel mirrors common + difference
		expect(screen.getByText('Comum + preservação de dados')).toBeTruthy();
		expect(screen.getByText('Comum + debloat')).toBeTruthy();
		expect(screen.getAllByText('Configuração comum').length).toBeGreaterThan(1);
	});

	it('updates local Presentation state when a per-Endpoint adjustment is toggled off', async () => {
		renderScenario();
		const commonChips = screen.getAllByText('Configuração comum').length;

		await fireEvent.click(toggle('Preservar e restaurar os dados do usuário em LAB-03'));

		expect(screen.queryByText('Comum + preservação de dados')).toBeNull();
		expect(screen.getAllByText('Configuração comum').length).toBe(commonChips + 1);
		expect(screen.getByText('1 Endpoint com ajustes · 2 seguem apenas a configuração comum')).toBeTruthy();
	});

	it('updates the common driver choice locally', async () => {
		renderScenario();
		const drivers = toggle('Instalar drivers em todos os alvos') as HTMLInputElement;
		await fireEvent.click(drivers);
		expect(drivers.checked).toBe(false);
	});

	it('ignores duplicate and unknown target parameters deterministically', () => {
		renderScenario(['LAB-07', 'LAB-03', 'LAB-07', 'LAB-99']);
		expect(screen.getByText('2 Endpoints selecionados')).toBeTruthy();
		expect(screen.getByText('Requer atenção')).toBeTruthy();
		expect(screen.queryByText('LAB-99')).toBeNull();
		expect(screen.queryByText('Não pronto')).toBeNull();
	});

	it('guards the no-valid-target case with a localized path back to Endpoints', () => {
		renderScenario(['LAB-99', 'LAB-99']);
		expect(screen.getByText('Nenhum Endpoint válido selecionado')).toBeTruthy();
		const back = screen.getByRole('link', { name: 'Voltar para Endpoints' });
		expect(back.getAttribute('href')).toBe('/endpoints');
		expect(screen.queryByRole('button', { name: 'Revisar operação' })).toBeNull();
	});

	it('Revisar operação never submits or executes: no form, no navigation, only a local notice', async () => {
		const { container } = renderScenario();
		expect(container.querySelector('form')).toBeNull();

		const review = screen.getByRole('button', { name: 'Revisar operação' });
		expect((review as HTMLButtonElement).type).toBe('button');
		expect(screen.queryByRole('status')).toBeNull();

		await fireEvent.click(review);

		expect(screen.getByRole('status').textContent).toContain('Nada foi enviado ou executado');
		// still on the configuration surface with the draft in memory
		expect(screen.getByText('Ajustes por Endpoint')).toBeTruthy();
	});
});
