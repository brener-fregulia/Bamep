<script lang="ts">
	import type { FleetEndpoint } from '$lib/fixtures/endpoints';
	import { t } from '$lib/i18n';
	import { activityLabelKey } from './situation';
	import SituationIndicator from './SituationIndicator.svelte';

	let {
		endpoint,
		selected,
		onToggle
	}: {
		endpoint: FleetEndpoint;
		selected: boolean;
		onToggle: (id: string) => void;
	} = $props();

	const attention = $derived(endpoint.situation === 'attention');
	const offline = $derived(endpoint.situation === 'unavailable');

	// Selection state wins the row background over the attention tint (matches #41),
	// but attention stays legible through the situation cell, the left bar, and the
	// selection summary breakdown.
	const rowClass = $derived(
		[
			'border-b border-bmp-border transition-colors last:border-b-0',
			selected
				? 'bg-bmp-selected'
				: attention
					? 'bg-bmp-attention-surface'
					: 'hover:bg-bmp-surface-2'
		].join(' ')
	);

	const markerClass = $derived(
		selected
			? 'shadow-[inset_3px_0_0_var(--color-bmp-accent)]'
			: attention
				? 'shadow-[inset_3px_0_0_var(--color-bmp-attention-bar)]'
				: ''
	);

	function contactText(minutes: number): string {
		return minutes === 0
			? t('endpoints.contact.now')
			: t('endpoints.contact.minutesAgo', { count: minutes });
	}
</script>

<tr class={rowClass} data-situation={endpoint.situation} data-selected={selected}>
	<td class="w-11 px-3.5 py-2.5 align-middle {markerClass}">
		<input
			type="checkbox"
			checked={selected}
			onchange={() => onToggle(endpoint.id)}
			class="block h-[15px] w-[15px] cursor-pointer accent-bmp-accent"
			aria-label={t('endpoints.select.row', { id: endpoint.id })}
		/>
	</td>

	<td class="px-3.5 py-2.5 align-middle">
		<span class="font-semibold tabular-nums tracking-[0.02em] {offline ? 'text-bmp-ink-soft' : 'text-bmp-ink'}">
			{endpoint.id}
		</span>
		<span class="block font-mono text-[10.5px] text-bmp-ink-faint">
			{t('endpoints.bench', { code: endpoint.bench })}
		</span>
	</td>

	<td class="px-3.5 py-2.5 align-middle">
		<SituationIndicator situation={endpoint.situation} />
	</td>

	<td class="px-3.5 py-2.5 align-middle text-[12.5px]">
		{#if endpoint.situation === 'working' && endpoint.activity}
			<span class="text-bmp-ink">{t(activityLabelKey(endpoint.activity))}</span>
		{:else if endpoint.situation === 'attention'}
			<span class="text-bmp-attention">
				{t(endpoint.attentionKey ?? 'endpoints.attention.uncertainResult')}
			</span>
			{#if endpoint.attentionHintKey}
				<span class="block text-[11.5px] text-bmp-ink-faint">{t(endpoint.attentionHintKey)}</span>
			{/if}
		{:else if endpoint.situation === 'pending-enrollment'}
			<span class="text-bmp-ink-faint">{t('endpoints.detail.pendingEnrollment')}</span>
		{:else if endpoint.situation === 'not-ready'}
			<span class="text-bmp-ink-faint">{t('endpoints.detail.notReady')}</span>
		{:else}
			<span class="text-bmp-ink-faint">{t('endpoints.activity.none')}</span>
		{/if}
	</td>

	<td class="px-3.5 py-2.5 align-middle text-[12.5px] {offline ? 'text-bmp-ink-faint' : 'text-bmp-ink-soft'}">
		{endpoint.hardware}
	</td>

	<td class="px-3.5 py-2.5 align-middle text-[12.5px] tabular-nums {offline ? 'text-bmp-ink-faint' : 'text-bmp-ink-soft'}">
		{contactText(endpoint.contactMinutesAgo)}
	</td>
</tr>
