<script lang="ts">
	import type { FleetEndpoint } from '$lib/fixtures/endpoints';
	import { t } from '$lib/i18n';

	let {
		attentionTargets,
		notReadyTargets
	}: {
		attentionTargets: readonly FleetEndpoint[];
		notReadyTargets: readonly FleetEndpoint[];
	} = $props();
</script>

{#if attentionTargets.length > 0 || notReadyTargets.length > 0}
	<section
		aria-label={t('operationsNew.attention.title')}
		class="rounded-[7px] border border-bmp-border border-l-[3px] border-l-bmp-attention-bar bg-bmp-surface px-3.5 py-2.5"
	>
		<h2 class="flex items-center gap-2 text-[12.5px] font-semibold text-bmp-ink">
			<svg
				width="14"
				height="14"
				viewBox="0 0 16 16"
				fill="none"
				stroke="currentColor"
				stroke-width="1.6"
				aria-hidden="true"
				class="shrink-0 text-bmp-attention"
			>
				<path d="M8 2.4 14.4 13.2H1.6L8 2.4Z" stroke-linejoin="round" />
				<path d="M8 6.2v3.2" stroke-linecap="round" />
				<circle cx="8" cy="11.2" r=".6" fill="currentColor" stroke="none" />
			</svg>
			{t('operationsNew.attention.title')}
		</h2>
		<ul class="mt-1.5 flex flex-col gap-1">
			{#each attentionTargets as target (target.id)}
				<li class="text-[12px] leading-snug text-bmp-ink">
					{t('operationsNew.attention.uncertain', { id: target.id })}
				</li>
			{/each}
			{#each notReadyTargets as target (target.id)}
				<li class="text-[12px] leading-snug text-bmp-ink">
					{t('operationsNew.attention.notReady', { id: target.id })}
				</li>
			{/each}
		</ul>
		<p class="mt-2 border-t border-bmp-border pt-2 text-[11px] leading-snug text-bmp-ink-soft">
			{t('operationsNew.attention.eligibility')}
		</p>
	</section>
{/if}
