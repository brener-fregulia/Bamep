<script lang="ts">
	import { fleet } from '$lib/fixtures/endpoints';
	import { t, tCount } from '$lib/i18n';
	import { adjustmentText, planTargets, type TargetContextRow } from './configuration';
	import { resolveTargets } from './targets';
	import TargetsPanel from './TargetsPanel.svelte';
	import ToggleSwitch from './ToggleSwitch.svelte';
	import AttentionNotice from './AttentionNotice.svelte';
	import ReviewStage from './ReviewStage.svelte';

	let { requestedIds }: { requestedIds: readonly string[] } = $props();

	const targets = $derived(resolveTargets(requestedIds, fleet));
	const plans = $derived(planTargets(targets));

	// Local Presentation state only: the common driver choice, which seeded
	// per-Endpoint adjustments are currently disabled, and which of the two
	// local stages (Configurar/Revisar) is visible. Nothing is persisted and
	// nothing leaves this client. Switching `stage` never resets this state —
	// both stages read/write the same draft owned by this component.
	let installDrivers = $state(true);
	let disabledAdjustments = $state<string[]>([]);
	let stage = $state<'configure' | 'review'>('configure');

	function adjustmentOn(endpointId: string): boolean {
		return !disabledAdjustments.includes(endpointId);
	}

	function setAdjustment(endpointId: string, on: boolean): void {
		disabledAdjustments = on
			? disabledAdjustments.filter((id) => id !== endpointId)
			: [...disabledAdjustments, endpointId];
	}

	const targetRows: TargetContextRow[] = $derived(
		plans.map((plan) => {
			const hasDelta = plan.adjustment !== undefined && adjustmentOn(plan.endpoint.id);
			return {
				endpoint: plan.endpoint,
				deltaKey:
					hasDelta && plan.adjustment
						? adjustmentText[plan.adjustment].deltaKey
						: 'operationsNew.targets.deltaCommon',
				hasDelta
			};
		})
	);

	const adjustSummary = $derived.by(() => {
		const adjusted = plans.filter(
			(plan) => plan.adjustment !== undefined && adjustmentOn(plan.endpoint.id)
		).length;
		if (adjusted === 0) return t('operationsNew.adjust.allCommon');
		const common = plans.length - adjusted;
		const parts = [
			tCount(t, 'operationsNew.adjust.adjusted.one', 'operationsNew.adjust.adjusted.other', adjusted)
		];
		if (common > 0) {
			parts.push(
				tCount(t, 'operationsNew.adjust.commonOnly.one', 'operationsNew.adjust.commonOnly.other', common)
			);
		}
		return parts.join(' · ');
	});

	const attentionTargets = $derived(targets.filter((target) => target.situation === 'attention'));
	const notReadyTargets = $derived(targets.filter((target) => target.situation === 'not-ready'));

	const sectionTitle = 'text-[13px] font-semibold text-bmp-ink';
	const scopeTag =
		'rounded border border-bmp-border bg-bmp-surface-2 px-2 py-0.5 text-[10.5px] font-semibold uppercase tracking-[0.05em] whitespace-nowrap text-bmp-ink-faint';
	const primaryButton =
		'inline-flex items-center gap-2 rounded-md border border-bmp-accent bg-bmp-accent px-3.5 py-2 text-[13px] font-semibold text-bmp-ground hover:bg-bmp-accent-strong hover:border-bmp-accent-strong';
	const ghostButton =
		'inline-flex items-center rounded-md border border-transparent px-2.5 py-1.5 text-[13px] font-semibold text-bmp-ink-soft hover:bg-bmp-surface-2 hover:text-bmp-ink';
</script>

{#if targets.length === 0}
	<div class="flex flex-col gap-4">
		<header class="border-b border-bmp-border pb-3">
			<h1 class="text-xl font-semibold tracking-tight text-bmp-ink">{t('operationsNew.title')}</h1>
		</header>
		<section class="max-w-[56ch] rounded-[7px] border border-bmp-border bg-bmp-surface px-4 py-4">
			<h2 class="text-sm font-semibold text-bmp-ink">{t('operationsNew.guard.heading')}</h2>
			<p class="mt-1.5 text-xs leading-relaxed text-bmp-ink-soft">{t('operationsNew.guard.body')}</p>
			<a href="/endpoints" class="{primaryButton} mt-3.5">{t('operationsNew.backToEndpoints')}</a>
		</section>
	</div>
{:else}
	<div class="flex flex-col gap-4">
		<header
			class="flex flex-wrap items-end justify-between gap-x-6 gap-y-2 border-b border-bmp-border pb-3"
		>
			<div>
				<h1 class="text-xl font-semibold tracking-tight text-bmp-ink">{t('operationsNew.title')}</h1>
				<p class="mt-1 text-xs text-bmp-ink-faint">
					{stage === 'configure' ? t('operationsNew.lead') : t('operationsNew.review.lead')}
				</p>
			</div>
			<ol class="flex items-center gap-2 text-[11.5px]" aria-label={t('operationsNew.steps.label')}>
				<li
					aria-current={stage === 'configure' ? 'step' : undefined}
					class="flex items-center gap-1.5 font-semibold {stage === 'configure'
						? 'text-bmp-accent-strong'
						: 'font-medium text-bmp-ink-faint'}"
				>
					<span
						class="flex h-4 w-4 items-center justify-center rounded-full text-[10px] tabular-nums {stage ===
						'configure'
							? 'bg-bmp-accent text-bmp-ground'
							: 'border border-current'}"
					>
						1
					</span>
					{t('operationsNew.steps.configure')}
				</li>
				<li aria-hidden="true" class="text-bmp-border-strong">→</li>
				<li
					aria-current={stage === 'review' ? 'step' : undefined}
					class="flex items-center gap-1.5 font-semibold {stage === 'review'
						? 'text-bmp-accent-strong'
						: 'font-medium text-bmp-ink-faint'}"
				>
					<span
						class="flex h-4 w-4 items-center justify-center rounded-full text-[10px] tabular-nums {stage ===
						'review'
							? 'bg-bmp-accent text-bmp-ground'
							: 'border border-current'}"
					>
						2
					</span>
					{t('operationsNew.steps.review')}
				</li>
			</ol>
		</header>

		{#if stage === 'review'}
			<ReviewStage
				{plans}
				{targetRows}
				{installDrivers}
				{adjustSummary}
				{attentionTargets}
				{notReadyTargets}
				onBack={() => (stage = 'configure')}
			/>
		{:else}
			<div class="grid items-start gap-5 lg:grid-cols-[minmax(0,1fr)_320px]">
				<div class="flex min-w-0 flex-col gap-3">
					<div class="divide-y divide-bmp-border rounded-[7px] border border-bmp-border bg-bmp-surface">
						<section class="px-4 py-3">
							<h2 class={sectionTitle}>{t('operationsNew.intent.title')}</h2>
							<div class="mt-2" role="radiogroup" aria-label={t('operationsNew.intent.title')}>
								<label
									class="flex items-start gap-3 rounded-md border border-bmp-accent bg-bmp-selected px-3 py-2.5 shadow-[inset_3px_0_0_var(--color-bmp-accent)]"
								>
									<input
										type="radio"
										name="intent"
										checked
										class="mt-0.5 h-[15px] w-[15px] shrink-0 accent-bmp-accent"
									/>
									<span class="min-w-0">
										<span class="block text-[13px] font-semibold text-bmp-ink">
											{t('operationsNew.intent.reinstallWindows')}
										</span>
										<span class="block text-xs leading-snug text-bmp-ink-soft">
											{t('operationsNew.intent.reinstallWindowsDesc')}
										</span>
									</span>
								</label>
							</div>
						</section>

						<section class="px-4 py-3">
							<div class="flex items-center justify-between gap-3">
								<h2 class={sectionTitle}>{t('operationsNew.common.title')}</h2>
								<span class={scopeTag}>{t('operationsNew.common.scope')}</span>
							</div>
							<ul class="mt-1 divide-y divide-bmp-border">
								<li class="flex items-center justify-between gap-4 py-2">
									<span class="min-w-0">
										<span class="block text-[13px] font-medium text-bmp-ink">
											{t('operationsNew.common.reinstall')}
										</span>
										<span class="block text-[11.5px] text-bmp-ink-faint">
											{t('operationsNew.common.reinstallHint')}
										</span>
									</span>
									<span
										class="inline-flex shrink-0 items-center gap-1.5 text-[11px] font-medium text-bmp-ink-soft"
									>
										<svg
											width="13"
											height="13"
											viewBox="0 0 16 16"
											fill="none"
											stroke="currentColor"
											stroke-width="1.8"
											stroke-linecap="round"
											stroke-linejoin="round"
											aria-hidden="true"
											class="text-bmp-ok"
										>
											<path d="m3.5 8.5 3 3 6-7" />
										</svg>
										{t('operationsNew.common.included')}
									</span>
								</li>
								<li class="flex items-center justify-between gap-4 py-2">
									<span class="min-w-0">
										<span class="block text-[13px] font-medium text-bmp-ink">
											{t('operationsNew.common.drivers')}
										</span>
										<span class="block text-[11.5px] text-bmp-ink-faint">
											{t('operationsNew.common.driversHint')}
										</span>
									</span>
									<ToggleSwitch
										checked={installDrivers}
										label={t('operationsNew.common.driversToggle')}
										onchange={(checked) => (installDrivers = checked)}
									/>
								</li>
							</ul>
						</section>

						<section class="px-4 py-3">
							<div class="flex items-center justify-between gap-3">
								<h2 class={sectionTitle}>{t('operationsNew.adjust.title')}</h2>
								<span class={scopeTag}>{t('operationsNew.adjust.scope')}</span>
							</div>
							<p class="mt-0.5 text-[11.5px] text-bmp-ink-faint">{t('operationsNew.adjust.hint')}</p>
							<ul class="mt-1 divide-y divide-bmp-border">
								{#each plans as plan (plan.endpoint.id)}
									<li class="flex items-center gap-4 py-2">
										<span
											class="w-[104px] shrink-0 text-[13px] font-semibold tracking-[0.02em] tabular-nums text-bmp-ink"
										>
											{plan.endpoint.id}
										</span>
										{#if plan.adjustment}
											{@const text = adjustmentText[plan.adjustment]}
											{@const on = adjustmentOn(plan.endpoint.id)}
											<span class="min-w-0 flex-1">
												<span
													class="block text-[12.5px] {on
														? 'font-medium text-bmp-ink'
														: 'text-bmp-ink-soft'}"
												>
													{t(text.nameKey)}
												</span>
												<span class="block text-[11.5px] text-bmp-ink-faint">{t(text.hintKey)}</span>
											</span>
											<ToggleSwitch
												checked={on}
												label={t(text.toggleKey, { id: plan.endpoint.id })}
												onchange={(checked) => setAdjustment(plan.endpoint.id, checked)}
											/>
										{:else}
											<span class="min-w-0 flex-1 text-xs text-bmp-ink-faint">
												{t('operationsNew.adjust.none')}
											</span>
										{/if}
									</li>
								{/each}
							</ul>
							<p
								class="mt-1 border-t border-bmp-border pt-2 text-[11.5px] tabular-nums text-bmp-ink-soft"
							>
								{adjustSummary}
							</p>
						</section>
					</div>

					<footer
						class="flex flex-wrap items-center justify-between gap-3 border-t border-bmp-border pt-3"
					>
						<a href="/endpoints" class={ghostButton}>{t('operationsNew.backToEndpoints')}</a>
						<div class="flex items-center gap-3.5">
							<span class="text-[11.5px] text-bmp-ink-faint">{t('operationsNew.review.next')}</span>
							<button type="button" class={primaryButton} onclick={() => (stage = 'review')}>
								{t('operationsNew.review.cta')}
								<svg
									width="14"
									height="14"
									viewBox="0 0 16 16"
									fill="none"
									stroke="currentColor"
									stroke-width="1.8"
									stroke-linecap="round"
									stroke-linejoin="round"
									aria-hidden="true"
								>
									<path d="M6 3.5 10.5 8 6 12.5" />
								</svg>
							</button>
						</div>
					</footer>
				</div>

				<aside class="flex min-w-0 flex-col gap-3 lg:sticky lg:top-4">
					<TargetsPanel rows={targetRows} />
					<AttentionNotice {attentionTargets} {notReadyTargets} />
				</aside>
			</div>
		{/if}
	</div>
{/if}
