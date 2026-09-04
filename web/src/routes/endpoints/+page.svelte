<script lang="ts">
	import EndpointsTable from '$lib/components/endpoints/EndpointsTable.svelte';
	import SelectionSummary from '$lib/components/endpoints/SelectionSummary.svelte';
	import { summarizeSelection } from '$lib/components/endpoints/situation';
	import { fleet } from '$lib/fixtures/endpoints';
	import { t, tCount } from '$lib/i18n';

	// Selection is local Presentation state for this feature only. It is not
	// persisted and is intentionally not carried across routes yet (see
	// /operations/new).
	let selectedIds = $state<string[]>([]);

	const selectedEndpoints = $derived(fleet.filter((endpoint) => selectedIds.includes(endpoint.id)));
	const summary = $derived(summarizeSelection(selectedEndpoints));

	function toggle(id: string): void {
		selectedIds = selectedIds.includes(id)
			? selectedIds.filter((current) => current !== id)
			: [...selectedIds, id];
	}

	function toggleAll(checked: boolean): void {
		selectedIds = checked ? fleet.map((endpoint) => endpoint.id) : [];
	}

	function clearSelection(): void {
		selectedIds = [];
	}

	const badge = $derived(
		tCount(t, 'endpoints.selection.badge.one', 'endpoints.selection.badge.other', summary.total)
	);

	const primaryButton =
		'inline-flex items-center gap-2 rounded-md border border-bmp-accent bg-bmp-accent px-3.5 py-2 text-[13px] font-semibold text-bmp-ground hover:bg-bmp-accent-strong hover:border-bmp-accent-strong';
</script>

<svelte:head>
	<title>{t('endpoints.title')} · {t('app.brand')}</title>
</svelte:head>

<div class="flex flex-col gap-4">
	<header class="flex flex-wrap items-start justify-between gap-4 border-b border-bmp-border pb-4">
		<div>
			<h1 class="text-xl font-semibold tracking-tight text-bmp-ink">{t('endpoints.title')}</h1>
			<p class="mt-1.5 text-xs text-bmp-ink-faint">
				{t('endpoints.subtitle', { count: fleet.length })}
			</p>
		</div>

		<div class="flex items-center gap-3">
			{#if summary.total > 0}
				<span
					class="rounded border border-bmp-accent/25 bg-bmp-selected px-2.5 py-1 text-xs font-semibold tabular-nums text-bmp-accent-strong"
				>
					{badge}
				</span>
				<a href="/operations/new" class={primaryButton}>{t('endpoints.newOperation')}</a>
			{:else}
				<button type="button" class="{primaryButton} cursor-not-allowed opacity-45" disabled>
					{t('endpoints.newOperation')}
				</button>
			{/if}
		</div>
	</header>

	{#if summary.total > 0}
		<SelectionSummary {summary} onclear={clearSelection} />
	{/if}

	<EndpointsTable endpoints={fleet} {selectedIds} onToggle={toggle} onToggleAll={toggleAll} />

	<p class="text-xs text-bmp-ink-faint">{t('endpoints.footNote')}</p>
</div>
