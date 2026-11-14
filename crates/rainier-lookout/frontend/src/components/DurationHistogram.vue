<script setup>
import { computed, ref } from 'vue'
import { duration } from '../format.js'

// Durations in microseconds.
const props = defineProps({
  values: { type: Array, default: () => [] },
  height: { type: Number, default: 96 },
})

const BINS = 16
const W = 640

/** Log-spaced bins from the fastest to the slowest: durations span orders of magnitude. */
const bins = computed(() => {
  const values = props.values.filter((v) => Number.isFinite(v)).map((v) => Math.max(1, v))
  if (!values.length) return []
  const lo = Math.log10(Math.min(...values))
  const hi = Math.log10(Math.max(...values))
  if (hi - lo < 1e-9) return [{ from: 10 ** lo, to: 10 ** hi, n: values.length }]
  const width = (hi - lo) / BINS
  const out = Array.from({ length: BINS }, (_, i) => ({ from: 10 ** (lo + i * width), to: 10 ** (lo + (i + 1) * width), n: 0 }))
  for (const v of values) out[Math.min(BINS - 1, Math.floor((Math.log10(v) - lo) / width))].n += 1
  return out
})
const top = computed(() => Math.max(1, ...bins.value.map((b) => b.n)))
const step = computed(() => W / Math.max(1, bins.value.length))
const bars = computed(() =>
  bins.value.map((b, i) => {
    const h = b.n ? Math.max(2, (b.n / top.value) * (props.height - 4)) : 0
    return { ...b, x: i * step.value + 1, w: Math.max(1, step.value - 2), y: props.height - h, h }
  }),
)
const active = ref(null)
const label = (b) => `${duration(Math.round(b.from))} – ${duration(Math.round(b.to))}: ${b.n}`
const summary = computed(() =>
  bins.value.length
    ? `Duration histogram of ${props.values.length} occurrences, from ${duration(Math.round(bins.value[0].from))} to ${duration(Math.round(bins.value[bins.value.length - 1].to))}.`
    : 'No durations.',
)
</script>

<template>
  <div>
    <svg
      :viewBox="`0 0 ${W} ${height}`"
      preserveAspectRatio="none"
      class="block w-full"
      :style="{ height: `${height}px` }"
      role="img"
      :aria-label="summary"
      @pointerleave="active = null"
    >
      <line x1="0" :x2="W" :y1="height - 0.5" :y2="height - 0.5" class="stroke-zinc-200 dark:stroke-zinc-800" vector-effect="non-scaling-stroke" />
      <g v-for="(b, i) in bars" :key="i" @pointerenter="active = i">
        <rect
          :x="b.x"
          :y="b.y"
          :width="b.w"
          :height="b.h"
          rx="2"
          class="transition-colors"
          :class="active === i ? 'fill-indigo-600 dark:fill-indigo-300' : 'fill-indigo-500 dark:fill-indigo-400'"
        />
        <rect :x="i * step" y="0" :width="step" :height="height" fill="transparent"><title>{{ label(b) }}</title></rect>
      </g>
    </svg>
    <div class="mt-1 flex items-baseline justify-between gap-2 text-[11px] text-zinc-500 tabular-nums dark:text-zinc-400">
      <span>{{ bins.length ? duration(Math.round(bins[0].from)) : '' }}</span>
      <span v-if="active !== null && bars[active]" class="font-semibold text-zinc-800 dark:text-zinc-200">{{ label(bars[active]) }}</span>
      <span>{{ bins.length ? duration(Math.round(bins[bins.length - 1].to)) : '' }}</span>
    </div>
  </div>
</template>
