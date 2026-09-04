<script lang="ts">
	import type { EndpointSituation } from '$lib/fixtures/endpoints';
	import { t } from '$lib/i18n';
	import { situationMeta, type SituationTone } from './situation';

	let { situation }: { situation: EndpointSituation } = $props();

	const meta = $derived(situationMeta(situation));

	const TONE_CLASS: Record<SituationTone, string> = {
		ok: 'text-bmp-ok',
		work: 'text-bmp-work',
		enroll: 'text-bmp-enroll',
		attention: 'text-bmp-attention font-semibold',
		muted: 'text-bmp-muted',
		offline: 'text-bmp-ink-faint'
	};
</script>

<span class="inline-flex items-center gap-2 whitespace-nowrap text-[12.5px] font-medium {TONE_CLASS[meta.tone]}">
	<svg
		width="15"
		height="15"
		viewBox="0 0 16 16"
		fill="none"
		stroke="currentColor"
		stroke-width="1.6"
		aria-hidden="true"
		class={situation === 'working' ? 'motion-safe:animate-spin' : ''}
	>
		{#if situation === 'available'}
			<circle cx="8" cy="8" r="6.1" />
			<path d="m5.2 8.2 2 2 3.6-4" stroke-linecap="round" stroke-linejoin="round" />
		{:else if situation === 'working'}
			<path d="M8 1.9a6.1 6.1 0 1 1-6.1 6.1" stroke-linecap="round" />
		{:else if situation === 'pending-enrollment'}
			<circle cx="8" cy="8" r="6.1" stroke-dasharray="2.6 2.3" />
			<path d="M8 5.2v5.6M5.2 8h5.6" stroke-linecap="round" />
		{:else if situation === 'attention'}
			<path d="M8 2.4 14.4 13.2H1.6L8 2.4Z" stroke-linejoin="round" />
			<path d="M8 6.2v3.2" stroke-linecap="round" />
			<circle cx="8" cy="11.2" r=".6" fill="currentColor" stroke="none" />
		{:else if situation === 'not-ready'}
			<circle cx="8" cy="8" r="6.1" />
			<path d="M6.4 5.6v4.8M9.6 5.6v4.8" stroke-linecap="round" />
		{:else if situation === 'unavailable'}
			<circle cx="8" cy="8" r="6.1" />
			<path d="m4.2 4.2 7.6 7.6" stroke-linecap="round" />
		{/if}
	</svg>
	{t(meta.labelKey)}
</span>
