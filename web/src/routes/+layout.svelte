<script lang="ts">
	import { page } from '$app/state';
	import '$lib/styles/app.css';
	import AppShell from '$lib/components/shell/AppShell.svelte';

	let { children } = $props();
</script>

<AppShell pathname={page.url.pathname}>
	{@render children()}
</AppShell>
