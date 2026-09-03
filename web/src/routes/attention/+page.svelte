<script lang="ts">
	import { t } from '$lib/i18n';
</script>

<svelte:head>
	<title>{t('attention.title')} · {t('app.brand')}</title>
</svelte:head>

<header class="border-b border-bmp-border pb-4">
	<h1 class="text-xl font-semibold tracking-tight text-bmp-ink">{t('attention.title')}</h1>
	<p class="mt-2 max-w-[65ch] text-sm text-bmp-ink-soft">{t('attention.lead')}</p>
</header>

<p class="mt-6 text-sm text-bmp-ink-faint">{t('route.placeholderNotice')}</p>
