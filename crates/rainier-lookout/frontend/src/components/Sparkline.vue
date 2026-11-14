<script setup>
import { computed } from 'vue'

const props = defineProps({
  points: { type: Array, default: () => [] },
  color: { type: String, default: 'indigo' },
})

const W = 80
const H = 20
const PAD = 2.5

// Literal class names, so Tailwind sees every one.
const COLORS = {
  indigo: ['stroke-indigo-500 dark:stroke-indigo-400', 'fill-indigo-500 dark:fill-indigo-400'],
  rose: ['stroke-rose-500 dark:stroke-rose-400', 'fill-rose-500 dark:fill-rose-400'],
  emerald: ['stroke-emerald-500 dark:stroke-emerald-400', 'fill-emerald-500 dark:fill-emerald-400'],
  amber: ['stroke-amber-500 dark:stroke-amber-400', 'fill-amber-500 dark:fill-amber-400'],
  zinc: ['stroke-zinc-400 dark:stroke-zinc-500', 'fill-zinc-400 dark:fill-zinc-500'],
}
const tone = computed(() => COLORS[props.color] || COLORS.indigo)

const coords = computed(() => {
  const values = props.points.map((v) => (Number.isFinite(v) ? v : 0))
  if (!values.length) return []
  const max = Math.max(...values)
  const min = Math.min(0, ...values)
  const span = max - min || 1
  const step = values.length > 1 ? (W - PAD * 2) / (values.length - 1) : 0
  return values.map((v, i) => [
    values.length > 1 ? PAD + i * step : W / 2,
    H - PAD - ((v - min) / span) * (H - PAD * 2),
  ])
})
const line = computed(() => coords.value.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(' '))
const last = computed(() => coords.value[coords.value.length - 1])
const label = computed(() =>
  props.points.length ? `Trend of ${props.points.length} points, latest ${props.points[props.points.length - 1]}` : 'No data',
)
</script>

<template>
  <svg :viewBox="`0 0 ${W} ${H}`" :width="W" :height="H" class="inline-block shrink-0 align-middle" role="img" :aria-label="label">
    <polyline
      v-if="coords.length > 1"
      :points="line"
      fill="none"
      stroke-width="1.5"
      stroke-linejoin="round"
      stroke-linecap="round"
      :class="tone[0]"
    />
    <circle v-if="last" :cx="last[0]" :cy="last[1]" r="2" :class="tone[1]" />
  </svg>
</template>
