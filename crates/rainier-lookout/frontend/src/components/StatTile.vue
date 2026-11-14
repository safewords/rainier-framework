<script setup>
// One headline number: a label, the value, and a line of context under it.
// The default slot sits beside the value (a sparkline, a pill).
defineProps({
  label: { type: String, required: true },
  value: { type: [String, Number], default: '—' },
  hint: { type: String, default: '' },
  tone: { type: String, default: 'neutral' },
  loading: Boolean,
})

const tones = {
  neutral: 'text-zinc-900 dark:text-zinc-100',
  bad: 'text-rose-600 dark:text-rose-400',
  warn: 'text-amber-600 dark:text-amber-400',
  good: 'text-emerald-600 dark:text-emerald-400',
  accent: 'text-indigo-600 dark:text-indigo-400',
}
</script>

<template>
  <div class="lk-card min-w-0 px-4 py-3">
    <div class="lk-th truncate" :title="label">{{ label }}</div>
    <div class="mt-0.5 flex min-w-0 items-end justify-between gap-2">
      <div
        class="min-w-0 truncate text-2xl font-semibold tabular-nums"
        :class="[tones[tone] || tones.neutral, loading ? 'animate-pulse opacity-40' : '']"
      >{{ loading ? '—' : value }}</div>
      <div v-if="$slots.default" class="shrink-0 pb-1"><slot /></div>
    </div>
    <div v-if="hint" class="mt-0.5 truncate text-xs text-zinc-500 dark:text-zinc-400" :title="hint">{{ hint }}</div>
  </div>
</template>
