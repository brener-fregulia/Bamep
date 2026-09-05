<script lang="ts">
	import { page } from '$app/state';
	import NewOperationView from '$lib/components/operations/NewOperationView.svelte';
	import { TARGET_PARAM } from '$lib/components/operations/targets';
	import { t } from '$lib/i18n';

	// Presentation-only route handoff (#49 → #51): selected fixture ids arrive as
	// repeated `target` query parameters. Resolution against the local fleet —
	// including duplicate/unknown handling and the no-target guard — lives in
	// NewOperationView so the surface is testable without SvelteKit runtime state.
	const requestedIds = $derived(page.url.searchParams.getAll(TARGET_PARAM));
</script>

<svelte:head>
	<title>{t('operationsNew.title')} · {t('app.brand')}</title>
</svelte:head>

<NewOperationView {requestedIds} />
