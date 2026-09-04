<script lang="ts">
	import { t, tCount } from '$lib/i18n';
	import type { SelectionSummary } from './situation';

	let { summary, onclear }: { summary: SelectionSummary; onclear: () => void } = $props();

	const heading = $derived(
		tCount(
			t,
			'endpoints.selection.heading.one',
			'endpoints.selection.heading.other',
			summary.total
		)
	);

	const chip = 'inline-flex items-center gap-1.5 rounded border border-bmp-border bg-bmp-surface px-2 py-0.5 text-[11.5px] font-medium';
</script>

<section
	class="flex flex-wrap items-center justify-between gap-3 rounded-[7px] border border-bmp-accent/25 bg-bmp-selected px-3.5 py-2.5"
	aria-label={t('endpoints.selection.regionLabel')}
>
	<div class="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2">
		<strong class="text-[13px] font-semibold text-bmp-accent-strong">{heading}</strong>

		<span class="flex flex-wrap gap-1.5">
			{#if summary.ready > 0}
				<span class="{chip} text-bmp-ok">
					{tCount(t, 'endpoints.selection.ready.one', 'endpoints.selection.ready.other', summary.ready)}
				</span>
			{/if}
			{#if summary.attention > 0}
				<span class="{chip} text-bmp-attention">
					{tCount(
						t,
						'endpoints.selection.attention.one',
						'endpoints.selection.attention.other',
						summary.attention
					)}
				</span>
			{/if}
			{#if summary.other > 0}
				<span class="{chip} text-bmp-muted">
					{tCount(t, 'endpoints.selection.other.one', 'endpoints.selection.other.other', summary.other)}
				</span>
			{/if}
		</span>

		<span class="max-w-[56ch] text-[11.5px] leading-snug text-bmp-ink-soft">
			{t('endpoints.selection.note')}
		</span>
	</div>

	<button
		type="button"
		onclick={onclear}
		class="rounded-md border border-transparent px-2.5 py-1.5 text-[13px] font-semibold text-bmp-ink-soft hover:bg-bmp-surface-2 hover:text-bmp-ink"
	>
		{t('endpoints.selection.clear')}
	</button>
</section>
