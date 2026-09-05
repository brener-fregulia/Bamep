<script lang="ts">
	import SituationIndicator from '$lib/components/endpoints/SituationIndicator.svelte';
	import { summarizeSelection } from '$lib/components/endpoints/situation';
	import type { EndpointSituation } from '$lib/fixtures/endpoints';
	import { t, tCount, type MessageKey } from '$lib/i18n';
	import type { TargetContextRow } from './configuration';

	let { rows }: { rows: readonly TargetContextRow[] } = $props();

	const summary = $derived(summarizeSelection(rows.map((row) => row.endpoint)));
	const heading = $derived(
		tCount(t, 'endpoints.selection.heading.one', 'endpoints.selection.heading.other', summary.total)
	);

	// Short operator-facing note per non-available situation, reusing the same
	// catalog entries the Endpoints list renders for the identical meaning.
	const NOTE_KEY: Partial<Record<EndpointSituation, MessageKey>> = {
		attention: 'endpoints.attention.uncertainResult',
		'not-ready': 'endpoints.detail.notReady',
		'pending-enrollment': 'endpoints.detail.pendingEnrollment'
	};

	const chip =
		'inline-flex items-center gap-1.5 rounded border border-bmp-border bg-bmp-surface px-2 py-0.5 text-[11px] font-medium';
</script>

<section
	aria-label={t('operationsNew.targets.title')}
	class="rounded-[7px] border border-bmp-border bg-bmp-surface"
>
	<header class="border-b border-bmp-border px-3.5 py-2.5">
		<h2 class="text-[12.5px] font-semibold text-bmp-ink">{t('operationsNew.targets.title')}</h2>
		<p class="mt-1.5 text-[12px] font-semibold tabular-nums text-bmp-ink">{heading}</p>
		<p class="mt-1.5 flex flex-wrap gap-1.5">
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
		</p>
	</header>

	<ul class="max-h-[60vh] overflow-y-auto">
		{#each rows as row (row.endpoint.id)}
			{@const noteKey = NOTE_KEY[row.endpoint.situation]}
			<li class="border-b border-bmp-border px-3.5 py-2 last:border-b-0">
				<div class="flex items-center justify-between gap-2">
					<span class="text-[13px] font-semibold tracking-[0.02em] tabular-nums text-bmp-ink">
						{row.endpoint.id}
					</span>
					<SituationIndicator situation={row.endpoint.situation} />
				</div>
				{#if noteKey}
					<p class="mt-0.5 text-[11px] leading-snug text-bmp-ink-faint">{t(noteKey)}</p>
				{/if}
				<p class="mt-1.5">
					<span
						class="inline-block max-w-full truncate rounded border px-1.5 py-0.5 text-[11px] font-medium {row.hasDelta
							? 'border-bmp-accent/25 bg-bmp-selected text-bmp-accent-strong'
							: 'border-bmp-border bg-bmp-surface-2 text-bmp-ink-soft'}"
					>
						{t(row.deltaKey)}
					</span>
				</p>
			</li>
		{/each}
	</ul>

	<p class="border-t border-bmp-border px-3.5 py-2 text-[11px] leading-snug text-bmp-ink-faint">
		{t('operationsNew.targets.note')}
	</p>
</section>
