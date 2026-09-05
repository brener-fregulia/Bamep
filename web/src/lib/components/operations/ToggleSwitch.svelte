<script lang="ts">
	let {
		checked,
		label,
		onchange
	}: {
		checked: boolean;
		label: string;
		onchange: (checked: boolean) => void;
	} = $props();
</script>

<label class="relative inline-flex shrink-0 cursor-pointer">
	<input
		type="checkbox"
		{checked}
		aria-label={label}
		onchange={(event) => onchange(event.currentTarget.checked)}
		class="peer absolute inset-0 z-10 m-0 h-full w-full cursor-pointer opacity-0"
	/>
	<span
		aria-hidden="true"
		class="flex h-[18px] w-8 items-center rounded-full border border-bmp-border-strong bg-bmp-surface-2 px-[2px] transition-colors after:h-3 after:w-3 after:rounded-full after:bg-bmp-ink-faint after:transition-transform after:content-[''] peer-checked:border-bmp-accent peer-checked:bg-bmp-accent peer-checked:after:translate-x-[14px] peer-checked:after:bg-bmp-ground peer-focus-visible:outline-2 peer-focus-visible:outline-offset-2 peer-focus-visible:outline-bmp-accent motion-reduce:transition-none motion-reduce:after:transition-none"
	></span>
</label>
