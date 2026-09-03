<script lang="ts">
	import { t } from '$lib/i18n';
	import NavIcon from './NavIcon.svelte';
	import { resolvePrimaryNav } from './nav';

	let { pathname }: { pathname: string } = $props();

	const items = $derived(resolvePrimaryNav(pathname));
</script>

<aside
	class="sticky top-0 flex h-screen w-56 flex-col gap-6 overflow-y-auto border-r border-bmp-border bg-bmp-surface px-3.5 py-4"
>
	<div class="flex flex-col gap-0.5 px-2 py-1">
		<span class="text-[17px] font-bold tracking-wide text-bmp-ink">
			<span class="text-bmp-accent">B</span>amep
		</span>
		<span class="text-[11px] tracking-wide text-bmp-ink-faint">{t('app.tagline')}</span>
	</div>

	<nav class="flex flex-col gap-0.5" aria-label={t('shell.primaryNavLabel')}>
		<span
			class="px-2 py-1 text-[10.5px] font-medium uppercase tracking-[0.09em] text-bmp-ink-faint"
		>
			{t('nav.sectionFleet')}
		</span>
		<ul class="flex flex-col gap-0.5">
			{#each items as item (item.id)}
				<li>
					<a
						href={item.href}
						aria-current={item.active ? 'page' : undefined}
						data-active={item.active}
						class="flex items-center gap-2.5 rounded-md border border-transparent px-2 py-1.5 text-[13.5px] font-medium text-bmp-ink-soft hover:bg-bmp-surface-2 hover:text-bmp-ink data-[active=true]:border-bmp-accent/25 data-[active=true]:bg-bmp-selected data-[active=true]:text-bmp-accent-strong"
					>
						<NavIcon name={item.id} />
						{t(item.labelKey)}
					</a>
				</li>
			{/each}
		</ul>
	</nav>

	<div class="mt-auto px-2 py-1">
		<span
			class="inline-flex items-center gap-1.5 rounded border border-bmp-border bg-bmp-surface-2 px-2 py-1 text-[11.5px] text-bmp-ink-soft"
		>
			{t('app.environment')}
		</span>
	</div>
</aside>
