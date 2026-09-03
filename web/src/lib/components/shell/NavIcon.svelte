<script lang="ts">
	import type { NavItemId } from './nav';

	let { name }: { name: NavItemId } = $props();
</script>

<svg
	width="16"
	height="16"
	viewBox="0 0 16 16"
	fill="none"
	stroke="currentColor"
	stroke-width="1.5"
	aria-hidden="true"
	class="shrink-0 opacity-85"
>
	{#if name === 'endpoints'}
		<rect x="2" y="2.5" width="12" height="4.5" rx="1" />
		<rect x="2" y="9" width="12" height="4.5" rx="1" />
		<circle cx="5" cy="4.75" r=".6" fill="currentColor" stroke="none" />
		<circle cx="5" cy="11.25" r=".6" fill="currentColor" stroke="none" />
	{:else if name === 'operations'}
		<path d="M2.5 4.5h11M2.5 8h11M2.5 11.5h7" />
		<circle cx="12" cy="11.5" r="1.6" />
	{:else if name === 'attention'}
		<path d="M8 2.6 14.2 13H1.8L8 2.6Z" stroke-linejoin="round" />
		<path d="M8 6.4v3.1" stroke-linecap="round" />
		<circle cx="8" cy="11.2" r=".55" fill="currentColor" stroke="none" />
	{:else if name === 'settings'}
		<circle cx="8" cy="8" r="2.4" />
		<path
			d="M8 1.6v2M8 12.4v2M14.4 8h-2M3.6 8h-2M12.5 3.5l-1.4 1.4M4.9 11.1l-1.4 1.4M12.5 12.5l-1.4-1.4M4.9 4.9 3.5 3.5"
		/>
	{/if}
</svg>
