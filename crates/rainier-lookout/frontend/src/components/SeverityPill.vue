<script setup>
defineProps({ severity: { type: String, default: 'low' } })

const tones = {
  critical: 'bg-rose-600 text-white ring-rose-700/30 dark:bg-rose-500 dark:text-white dark:ring-rose-300/30',
  high: 'bg-rose-50 text-rose-700 ring-rose-600/20 dark:bg-rose-500/10 dark:text-rose-400 dark:ring-rose-400/20',
  medium: 'bg-amber-50 text-amber-800 ring-amber-600/20 dark:bg-amber-500/10 dark:text-amber-300 dark:ring-amber-400/20',
  low: 'bg-zinc-100 text-zinc-600 ring-zinc-500/10 dark:bg-zinc-800 dark:text-zinc-300 dark:ring-zinc-400/10',
}
</script>

<template>
  <span
    class="inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-[11px] leading-4 font-semibold whitespace-nowrap capitalize ring-1 ring-inset"
    :class="tones[severity] || tones.low"
  >{{ severity }}</span>
</template>
