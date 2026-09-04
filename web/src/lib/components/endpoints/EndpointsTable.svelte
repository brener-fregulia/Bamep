<script lang="ts">
	import type { FleetEndpoint } from '$lib/fixtures/endpoints';
	import { t } from '$lib/i18n';
	import EndpointRow from './EndpointRow.svelte';

	let {
		endpoints,
		selectedIds,
		onToggle,
		onToggleAll
	}: {
		endpoints: readonly FleetEndpoint[];
		selectedIds: readonly string[];
		onToggle: (id: string) => void;
		onToggleAll: (checked: boolean) => void;
	} = $props();

	const selectedSet = $derived(new Set(selectedIds));
	const allSelected = $derived(endpoints.length > 0 && selectedIds.length === endpoints.length);
	const someSelected = $derived(selectedIds.length > 0 && selectedIds.length < endpoints.length);

	let headCheckbox = $state<HTMLInputElement>();
	$effect(() => {
		if (headCheckbox) headCheckbox.indeterminate = someSelected;
	});

	const headCell =
		'bg-bmp-surface px-3.5 py-2.5 text-left text-[10.5px] font-semibold uppercase tracking-[0.08em] text-bmp-ink-faint whitespace-nowrap';
</script>

<div class="overflow-x-auto rounded-[7px] border border-bmp-border bg-bmp-surface">
	<table class="w-full min-w-[900px] border-collapse text-sm">
		<caption class="sr-only">{t('endpoints.tableCaption')}</caption>
		<thead>
			<tr class="border-b border-bmp-border-strong">
				<th class="{headCell} w-11">
					<input
						bind:this={headCheckbox}
						type="checkbox"
						checked={allSelected}
						onchange={(event) => onToggleAll(event.currentTarget.checked)}
						class="block h-[15px] w-[15px] cursor-pointer accent-bmp-accent"
						aria-label={t('endpoints.select.all')}
					/>
				</th>
				<th class="{headCell} w-[160px]">{t('endpoints.column.endpoint')}</th>
				<th class="{headCell} w-[180px]">{t('endpoints.column.situation')}</th>
				<th class="{headCell} w-[230px]">{t('endpoints.column.activity')}</th>
				<th class={headCell}>{t('endpoints.column.hardware')}</th>
				<th class="{headCell} w-[120px]">{t('endpoints.column.lastContact')}</th>
			</tr>
		</thead>
		<tbody>
			{#each endpoints as endpoint (endpoint.id)}
				<EndpointRow {endpoint} selected={selectedSet.has(endpoint.id)} {onToggle} />
			{/each}
		</tbody>
	</table>
</div>
