<script lang="ts">
	/**
	 * Review stage for `/operations/new` (Issue #54).
	 *
	 * Summarizes the exact current in-memory draft owned by the parent
	 * `NewOperationView` — it renders no editable control and holds no draft
	 * state of its own. `Enviar operação` is a Presentation-only boundary: it
	 * never calls HTTP/fetch, never navigates, and never fabricates a creation
	 * or execution outcome. Its local `submitRequested` flag exists only to
	 * show a placeholder acknowledgement and is discarded when this stage is
	 * unmounted (returning to Configurar).
	 */
	import type { FleetEndpoint } from '$lib/fixtures/endpoints';
	import { t } from '$lib/i18n';
	import { adjustmentText, type TargetContextRow, type TargetPlan } from './configuration';
	import TargetsPanel from './TargetsPanel.svelte';
	import AttentionNotice from './AttentionNotice.svelte';

	let {
		plans,
		targetRows,
		installDrivers,
		adjustSummary,
		attentionTargets,
		notReadyTargets,
		onBack
	}: {
		plans: readonly TargetPlan[];
		targetRows: readonly TargetContextRow[];
		installDrivers: boolean;
		adjustSummary: string;
		attentionTargets: readonly FleetEndpoint[];
		notReadyTargets: readonly FleetEndpoint[];
		onBack: () => void;
	} = $props();

	let submitRequested = $state(false);

	const sectionTitle = 'text-[13px] font-semibold text-bmp-ink';
	const primaryButton =
		'inline-flex items-center gap-2 rounded-md border border-bmp-accent bg-bmp-accent px-3.5 py-2 text-[13px] font-semibold text-bmp-ground hover:bg-bmp-accent-strong hover:border-bmp-accent-strong';
	const ghostButton =
		'inline-flex items-center rounded-md border border-transparent px-2.5 py-1.5 text-[13px] font-semibold text-bmp-ink-soft hover:bg-bmp-surface-2 hover:text-bmp-ink';
</script>

<div class="grid items-start gap-5 lg:grid-cols-[minmax(0,1fr)_320px]">
	<div class="flex min-w-0 flex-col gap-3">
		<div class="divide-y divide-bmp-border rounded-[7px] border border-bmp-border bg-bmp-surface">
			<section class="px-4 py-3">
				<h2 class={sectionTitle}>{t('operationsNew.review.service.title')}</h2>
				<p class="mt-2 text-[13px] font-semibold text-bmp-ink">
					{t('operationsNew.intent.reinstallWindows')}
				</p>
				<p class="mt-1 text-xs leading-relaxed text-bmp-ink-soft">
					{t('operationsNew.intent.reinstallWindowsDesc')}
				</p>
			</section>

			<section class="px-4 py-3">
				<h2 class={sectionTitle}>{t('operationsNew.common.title')}</h2>
				<ul class="mt-1 divide-y divide-bmp-border">
					<li class="flex items-center justify-between gap-4 py-2">
						<span class="text-[13px] font-medium text-bmp-ink">
							{t('operationsNew.common.reinstall')}
						</span>
						<span class="text-[11.5px] font-medium text-bmp-ok">
							{t('operationsNew.common.included')}
						</span>
					</li>
					<li class="flex items-center justify-between gap-4 py-2">
						<span class="text-[13px] font-medium text-bmp-ink">
							{t('operationsNew.common.drivers')}
						</span>
						<span
							class="text-[11.5px] font-medium {installDrivers
								? 'text-bmp-ok'
								: 'text-bmp-ink-faint'}"
						>
							{installDrivers ? t('operationsNew.review.enabled') : t('operationsNew.review.disabled')}
						</span>
					</li>
				</ul>
			</section>

			<section class="px-4 py-3">
				<h2 class={sectionTitle}>{t('operationsNew.adjust.title')}</h2>
				<ul class="mt-1 divide-y divide-bmp-border">
					{#each targetRows as row, i (row.endpoint.id)}
						{@const plan = plans[i]}
						<li class="flex items-center gap-4 py-2">
							<span
								class="w-[104px] shrink-0 text-[13px] font-semibold tracking-[0.02em] tabular-nums text-bmp-ink"
							>
								{row.endpoint.id}
							</span>
							{#if plan.adjustment && row.hasDelta}
								<span class="text-[12.5px] font-medium text-bmp-ink">
									{t(adjustmentText[plan.adjustment].nameKey)}
								</span>
							{:else}
								<span class="text-xs text-bmp-ink-faint">{t('operationsNew.adjust.none')}</span>
							{/if}
						</li>
					{/each}
				</ul>
				<p class="mt-1 border-t border-bmp-border pt-2 text-[11.5px] tabular-nums text-bmp-ink-soft">
					{adjustSummary}
				</p>
			</section>
		</div>

		<footer class="flex flex-wrap items-center justify-between gap-3 border-t border-bmp-border pt-3">
			<button type="button" class={ghostButton} onclick={onBack}>
				{t('operationsNew.review.backToConfig')}
			</button>
			<div class="flex flex-wrap items-center justify-end gap-3.5">
				<p class="max-w-[42ch] text-right text-[11px] leading-snug text-bmp-ink-faint">
					{t('operationsNew.review.submitNote')}
				</p>
				<button type="button" class={primaryButton} onclick={() => (submitRequested = true)}>
					{t('operationsNew.review.submit.cta')}
				</button>
			</div>
		</footer>

		{#if submitRequested}
			<p
				role="status"
				class="rounded-[7px] border border-bmp-border bg-bmp-surface-2 px-3.5 py-2.5 text-xs leading-relaxed text-bmp-ink-soft"
			>
				{t('operationsNew.review.placeholder')}
			</p>
		{/if}
	</div>

	<aside class="flex min-w-0 flex-col gap-3 lg:sticky lg:top-4">
		<TargetsPanel rows={targetRows} />
		<AttentionNotice {attentionTargets} {notReadyTargets} />
	</aside>
</div>
