<script lang="ts">
	import type { Snippet } from 'svelte';
	import { t } from '$lib/i18n';
	import Sidebar from './Sidebar.svelte';

	let { pathname, children }: { pathname: string; children: Snippet } = $props();
</script>

<a
	href="#main-content"
	class="sr-only focus:not-sr-only focus:absolute focus:left-4 focus:top-4 focus:z-10 focus:rounded focus:border focus:border-bmp-border-strong focus:bg-bmp-surface focus:px-3 focus:py-2 focus:text-sm focus:text-bmp-ink"
>
	{t('shell.skipToContent')}
</a>

<!--
	Fluid shell (carried forward from #44 owner validation): the content column
	has no global max-width. `minmax(0, 1fr)` lets `<main>` grow with the
	viewport; each feature route chooses its own content width later.
-->
<div class="grid min-h-screen grid-cols-[14rem_minmax(0,1fr)]">
	<Sidebar {pathname} />
	<main id="main-content" class="min-w-0 px-8 py-7">
		{@render children()}
	</main>
</div>
