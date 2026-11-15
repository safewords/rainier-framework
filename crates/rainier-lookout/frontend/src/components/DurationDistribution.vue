<script setup>
// How a family's durations are spread: a smooth density curve over a log time
// axis, with the percentiles marked.
//
// A kernel density estimate — a small Gaussian at each occurrence, added up —
// rather than one Gaussian fitted to them all. Durations are not normally
// distributed: they bunch near the fast end with a long slow tail, and often
// have two humps (the usual case, and the case stuck behind a lock or a slow
// disk). One fitted bell would draw a symmetric tail that is not there and
// merge the humps into one; the estimate keeps both. On a log axis a typical
// latency spread does look like a bell, which is the shape being asked for.
//
// Drawn from the family's **histogram** when there is one — every occurrence
// in the range, counted by the aggregates — and otherwise from the durations
// of the occurrences loaded on the page, which in production are mostly the
// slow ones Lookout kept, and would overstate the slow hump.
import { computed, ref } from 'vue'
import { duration, histogramPercentile } from '../format.js'

const props = defineProps({
  // Durations in microseconds, one per occurrence: the fallback.
  values: { type: Array, default: () => [] },
  // `{ slot: count }` from `/api/series`: slot `s ≥ 1` holds
  // `[2^((s-1)/4), 2^(s/4))` µs. Preferred when it has anything in it.
  histogram: { type: Object, default: null },
  // The slowest occurrence, exactly — caps a histogram's percentiles.
  max: { type: Number, default: null },
  height: { type: Number, default: 120 },
})

const W = 640
const SAMPLES = 200
const TOP = 14 // room above the curve for the percentile labels
const MARKS = [
  { name: 'p50', p: 50 },
  { name: 'p90', p: 90 },
  { name: 'p95', p: 95 },
  { name: 'p99', p: 99 },
]
// A histogram slot is a quarter of a doubling wide: this many decades.
const SLOT_DECADES = Math.log10(2) / 4

const fromHistogram = computed(() =>
  Object.values(props.histogram || {}).some((n) => n > 0),
)

/**
 * What the curve is built from, as `{ log, w }` — log10 of a duration and how
 * many occurrences sit there — ascending. A histogram slot becomes its
 * midpoint, weighted by its count; a loaded duration, itself with weight one.
 */
const points = computed(() => {
  if (fromHistogram.value) {
    return Object.entries(props.histogram)
      .map(([slot, w]) => [Number(slot), w])
      .filter(([, w]) => w > 0)
      .map(([slot, w]) => ({ log: slot === 0 ? 0 : (slot - 0.5) * SLOT_DECADES, w }))
      .sort((a, b) => a.log - b.log)
  }
  return props.values
    .filter((v) => Number.isFinite(v))
    .map((v) => ({ log: Math.log10(Math.max(1, v)), w: 1 }))
    .sort((a, b) => a.log - b.log)
})

const total = computed(() => points.value.reduce((t, p) => t + p.w, 0))

/** The log10 at which `p` percent of the weight lies at or below. */
function quantile(p) {
  const pts = points.value
  if (!pts.length) return 0
  const rank = (p / 100) * total.value
  let seen = 0
  for (const point of pts) {
    seen += point.w
    if (seen >= rank) return point.log
  }
  return pts[pts.length - 1].log
}

/**
 * The bandwidth, in decades: Silverman's rule of thumb, which suits one or two
 * humps, never narrower than a histogram slot (or the curve shows the slots
 * rather than the spread), with a floor so identical values still draw a
 * narrow bell rather than a spike of zero width.
 */
const bandwidth = computed(() => {
  const pts = points.value
  const n = total.value
  const floor = fromHistogram.value ? SLOT_DECADES : 0.03
  if (n < 2) return Math.max(floor, 0.05)
  const mean = pts.reduce((a, p) => a + p.w * p.log, 0) / n
  const sd = Math.sqrt(pts.reduce((a, p) => a + p.w * (p.log - mean) ** 2, 0) / (n - 1))
  const iqr = quantile(75) - quantile(25)
  const spread = Math.min(sd, iqr > 0 ? iqr / 1.34 : sd) || sd
  return Math.max(floor, 0.9 * spread * n ** -0.2)
})

/** The x range: the data, plus room for the tails to come down. */
const domain = computed(() => {
  const pts = points.value
  if (!pts.length) return [0, 1]
  const pad = 2.5 * bandwidth.value
  return [pts[0].log - pad, pts[pts.length - 1].log + pad]
})

const xOf = (log) => ((log - domain.value[0]) / (domain.value[1] - domain.value[0] || 1)) * W

/** The curve: density at evenly spaced points across the domain. */
const curve = computed(() => {
  const pts = points.value
  if (!pts.length) return []
  const h = bandwidth.value
  const norm = 1 / (total.value * h * Math.sqrt(2 * Math.PI))
  const [lo, hi] = domain.value
  const out = []
  for (let i = 0; i < SAMPLES; i++) {
    const at = lo + ((hi - lo) * i) / (SAMPLES - 1)
    let sum = 0
    for (const p of pts) {
      const z = (at - p.log) / h
      if (z > -5 && z < 5) sum += p.w * Math.exp(-0.5 * z * z)
    }
    out.push({ log: at, density: sum * norm })
  }
  return out
})

const peak = computed(() => Math.max(1e-12, ...curve.value.map((c) => c.density)))
const yOf = (density) => props.height - 1 - (density / peak.value) * (props.height - TOP - 2)

const line = computed(() =>
  curve.value.map((c, i) => `${i ? 'L' : 'M'}${xOf(c.log).toFixed(1)},${yOf(c.density).toFixed(1)}`).join(''),
)
const area = computed(() =>
  curve.value.length ? `${line.value}L${W},${props.height}L0,${props.height}Z` : '',
)

/**
 * The percentiles, from the durations rather than the curve — and from a
 * histogram the same way the page's percentile tiles read it, so the two agree.
 */
const marks = computed(() => {
  if (!points.value.length) return []
  return MARKS.map((m) => {
    const us = fromHistogram.value
      ? Math.max(1, histogramPercentile(props.histogram, m.p, props.max ?? undefined) ?? 1)
      : Math.round(10 ** quantile(m.p))
    const log = Math.log10(Math.max(1, us))
    return { ...m, log, x: xOf(log), us }
  })
})

/**
 * Where each percentile's label sits, in percent of the width: at its line,
 * pushed right just enough not to overlap the one before — p95 and p99 are
 * often a hair apart.
 */
const labels = computed(() => {
  const GAP = 5
  let last = -Infinity
  return marks.value.map((m) => {
    const left = Math.max((m.x / W) * 100, last + GAP)
    last = left
    return { ...m, left: Math.min(left, 100) }
  })
})

/** Gridlines at each power of ten the range covers: 1ms, 10ms, 100ms, 1s… */
const ticks = computed(() => {
  const [lo, hi] = domain.value
  const out = []
  const UNITS = ['1µs', '10µs', '100µs', '1ms', '10ms', '100ms', '1s', '10s', '100s']
  for (let k = Math.ceil(lo); k <= Math.floor(hi); k++) {
    out.push({ x: xOf(k), label: UNITS[k] || duration(10 ** k) })
  }
  return out
})

/**
 * The two ends of the axis, labelled when no power of ten falls near them —
 * a family whose times all sit within one decade would otherwise have no
 * scale at all.
 */
const edges = computed(() => {
  if (!points.value.length) return []
  const [lo, hi] = domain.value
  const near = (x) => ticks.value.some((t) => Math.abs(t.x - x) < W * 0.1)
  const out = []
  if (!near(0)) out.push({ key: 'lo', left: 0, align: 'translate-x-0', label: duration(Math.round(10 ** lo)) })
  if (!near(W)) out.push({ key: 'hi', left: 100, align: '-translate-x-full', label: duration(Math.round(10 ** hi)) })
  return out
})

// Hover: the time under the pointer, and how many occurrences were at most that.
const svg = ref(null)
const hover = ref(null)
function move(event) {
  const box = svg.value && svg.value.getBoundingClientRect()
  if (!box || !box.width || !points.value.length) return
  const x = ((event.clientX - box.left) / box.width) * W
  const log = domain.value[0] + (x / W) * (domain.value[1] - domain.value[0])
  const below = points.value.filter((p) => p.log <= log).reduce((t, p) => t + p.w, 0)
  hover.value = { x, us: Math.max(1, Math.round(10 ** log)), share: below / total.value }
}
const readout = computed(() =>
  hover.value
    ? `${duration(hover.value.us)}: ${Math.round(hover.value.share * 100)}% of occurrences took this long or less`
    : null,
)

const summary = computed(() => {
  if (!marks.value.length) return 'No durations.'
  const [p50, p90, , p99] = marks.value
  return `Distribution of ${total.value} durations on a log scale: median ${duration(p50.us)}, p90 ${duration(p90.us)}, p99 ${duration(p99.us)}.`
})
</script>

<template>
  <div>
    <svg
      ref="svg"
      :viewBox="`0 0 ${W} ${height}`"
      preserveAspectRatio="none"
      class="block w-full"
      :style="{ height: `${height}px` }"
      role="img"
      :aria-label="summary"
      @pointermove="move"
      @pointerleave="hover = null"
    >
      <line
        v-for="t in ticks"
        :key="`t${t.x}`"
        :x1="t.x"
        :x2="t.x"
        :y1="TOP"
        :y2="height"
        class="stroke-zinc-100 dark:stroke-zinc-800"
        vector-effect="non-scaling-stroke"
      />
      <path :d="area" class="fill-indigo-500/15 dark:fill-indigo-400/20" />
      <path :d="line" fill="none" class="stroke-indigo-500 dark:stroke-indigo-400" stroke-width="2" vector-effect="non-scaling-stroke" />
      <g v-for="m in marks" :key="m.name">
        <line
          :x1="m.x"
          :x2="m.x"
          :y1="TOP - 2"
          :y2="height"
          stroke-dasharray="3 3"
          class="stroke-zinc-400 dark:stroke-zinc-500"
          vector-effect="non-scaling-stroke"
        />
      </g>
      <line
        v-if="hover"
        :x1="hover.x"
        :x2="hover.x"
        y1="0"
        :y2="height"
        class="stroke-zinc-700 dark:stroke-zinc-200"
        vector-effect="non-scaling-stroke"
      />
      <line :x1="0" :x2="W" :y1="height - 0.5" :y2="height - 0.5" class="stroke-zinc-200 dark:stroke-zinc-800" vector-effect="non-scaling-stroke" />
    </svg>
    <!-- Labels outside the stretched SVG, so the text is never squashed. -->
    <div class="relative h-4 text-[10px] font-medium text-zinc-600 tabular-nums dark:text-zinc-300">
      <span
        v-for="m in labels"
        :key="m.name"
        class="absolute -translate-x-1/2 whitespace-nowrap"
        :style="{ left: `${m.left}%` }"
        :title="`${m.name}: ${duration(m.us)}`"
      >{{ m.name }}</span>
    </div>
    <div class="relative h-4 text-[10px] text-zinc-400 tabular-nums dark:text-zinc-500">
      <span
        v-for="t in ticks"
        :key="`l${t.x}`"
        class="absolute -translate-x-1/2 whitespace-nowrap"
        :style="{ left: `${(t.x / W) * 100}%` }"
      >{{ t.label }}</span>
      <span
        v-for="e in edges"
        :key="e.key"
        class="absolute whitespace-nowrap"
        :class="e.align"
        :style="{ left: `${e.left}%` }"
      >{{ e.label }}</span>
    </div>
    <div class="mt-1 flex flex-wrap items-baseline gap-x-4 gap-y-1 text-[11px] text-zinc-500 tabular-nums dark:text-zinc-400">
      <span v-if="readout" class="font-semibold text-zinc-800 dark:text-zinc-200">{{ readout }}</span>
      <template v-else>
        <span v-for="m in marks" :key="m.name">{{ m.name }} <b class="font-semibold text-zinc-800 dark:text-zinc-200">{{ duration(m.us) }}</b></span>
      </template>
    </div>
  </div>
</template>
