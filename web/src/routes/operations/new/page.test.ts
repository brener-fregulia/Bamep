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

	it('Revisar operação transitions from Configurar to Revisar locally, without a form or navigation', async () => {
		const { container } = renderScenario();
		expect(container.querySelector('form')).toBeNull();

		const review = screen.getByRole('button', { name: 'Revisar operação' });
		expect((review as HTMLButtonElement).type).toBe('button');

		await fireEvent.click(review);

		expect(container.querySelector('form')).toBeNull();
		// the editable Configurar controls are gone — this is a distinct stage, not a placeholder overlay
		expect(screen.queryByRole('checkbox', { name: 'Instalar drivers em todos os alvos' })).toBeNull();
		expect(screen.queryByRole('button', { name: 'Revisar operação' })).toBeNull();
		// Revisar is now the active step
		const reviewStep = screen.getByText('Revisar').closest('li');
		expect(reviewStep?.getAttribute('aria-current')).toBe('step');
	});

	it('reflects the exact current in-memory draft on Revisar rather than reseeding the scenario', async () => {
		renderScenario();

		await fireEvent.click(toggle('Instalar drivers em todos os alvos'));
		await fireEvent.click(toggle('Preservar e restaurar os dados do usuário em LAB-03'));
		await fireEvent.click(screen.getByRole('button', { name: 'Revisar operação' }));

		expect(screen.getByText('Reinstalar Windows')).toBeTruthy();
		expect(screen.getByText('Desativado')).toBeTruthy();
		expect(screen.getByText('1 Endpoint com ajustes · 2 seguem apenas a configuração comum')).toBeTruthy();
		// LAB-03's disabled adjustment must show as common-only, not the seeded scenario delta
		expect(screen.getAllByText('Sem ajustes — segue a configuração comum.').length).toBe(2);
		expect(screen.getByText('Aplicar o debloat configurado')).toBeTruthy();
		expect(screen.getByText('Comum + debloat')).toBeTruthy();
	});

	it('keeps LAB-03/LAB-07/LAB-09 visible with their mixed situations on Revisar', async () => {
		renderScenario();
		await fireEvent.click(screen.getByRole('button', { name: 'Revisar operação' }));

		for (const id of ACCEPTANCE_TARGETS) {
			expect(screen.getAllByText(id).length).toBeGreaterThan(0);
		}
		expect(screen.getByText('Disponível')).toBeTruthy();
		expect(screen.getByText('Requer atenção')).toBeTruthy();
		expect(screen.getByText('Não pronto')).toBeTruthy();
		expect(
			screen.getByText(
				'LAB-07 possui um resultado anterior ainda incerto — configurar esta operação não resolve a condição.'
			)
		).toBeTruthy();
		expect(
			screen.getByText('LAB-09 não está pronto para uma nova operação neste momento.')
		).toBeTruthy();
	});

	it('communicates on Revisar that acceptance/execution is independent and not guaranteed, without exposing an outcome', async () => {
		renderScenario();
		await fireEvent.click(screen.getByRole('button', { name: 'Revisar operação' }));

		expect(
			screen.getByText(
				'Enviar não garante aceitação nem execução para todos os Endpoints selecionados; cada um é avaliado de forma independente.'
			)
		).toBeTruthy();
		expect(screen.queryByRole('status')).toBeNull();
	});

	it('Editar configuração returns to Configurar and preserves the local draft in memory', async () => {
		renderScenario();

		await fireEvent.click(toggle('Preservar e restaurar os dados do usuário em LAB-03'));
		await fireEvent.click(screen.getByRole('button', { name: 'Revisar operação' }));
		await fireEvent.click(screen.getByRole('button', { name: 'Editar configuração' }));

		expect(screen.getByText('Ajustes por Endpoint')).toBeTruthy();
		const configureStep = screen.getByText('Configurar').closest('li');
		expect(configureStep?.getAttribute('aria-current')).toBe('step');
		// the earlier toggle-off is still in effect
		expect((toggle('Preservar e restaurar os dados do usuário em LAB-03') as HTMLInputElement).checked).toBe(
			false
		);
		expect(screen.getByText('1 Endpoint com ajustes · 2 seguem apenas a configuração comum')).toBeTruthy();
	});

	it('Enviar operação never submits or executes: no form, no HTTP/navigation, only a local notice', async () => {
		const { container } = renderScenario();
		await fireEvent.click(screen.getByRole('button', { name: 'Revisar operação' }));
		expect(container.querySelector('form')).toBeNull();

		const submit = screen.getByRole('button', { name: 'Enviar operação' });
		expect((submit as HTMLButtonElement).type).toBe('button');
		expect(screen.queryByRole('status')).toBeNull();

		await fireEvent.click(submit);

		expect(container.querySelector('form')).toBeNull();
		expect(screen.getByRole('status').textContent).toContain('Nada foi enviado ou executado');
		// still on Revisar, no fabricated creation/execution outcome
		expect(screen.getByText('Serviço solicitado')).toBeTruthy();
	});
});
