<script setup>
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

const props = defineProps({
  // [{ label, color?, points: [{ t: ISO, v: number }], dashed?, axis?: 'left'|'right' }]
  series: { type: Array, default: () => [] },
  format: { type: Function, default: (v) => String(Math.round(v * 100) / 100) },
  formatRight: { type: Function, default: null },
  height: { type: Number, default: 160 },
  // Optional [from, to] (ISO strings): pin the x range, so stacked charts line up.
  xDomain: { type: Array, default: null },
  // Optional minimum left-axis width in px, so stacked charts' plots start at one x.
  axisWidth: { type: Number, default: 0 },
})

// Literal class names, so Tailwind sees every one. [line, area, legend/tooltip key]
const COLORS = {
  indigo: ['stroke-indigo-500 dark:stroke-indigo-400', 'fill-indigo-500/15 dark:fill-indigo-400/15', 'bg-indigo-500 dark:bg-indigo-400'],
  rose: ['stroke-rose-500 dark:stroke-rose-400', 'fill-rose-500/15 dark:fill-rose-400/15', 'bg-rose-500 dark:bg-rose-400'],
  emerald: ['stroke-emerald-500 dark:stroke-emerald-400', 'fill-emerald-500/15 dark:fill-emerald-400/15', 'bg-emerald-500 dark:bg-emerald-400'],
  amber: ['stroke-amber-500 dark:stroke-amber-400', 'fill-amber-500/15 dark:fill-amber-400/15', 'bg-amber-500 dark:bg-amber-400'],
  zinc: ['stroke-zinc-400 dark:stroke-zinc-500', 'fill-zinc-400/15 dark:fill-zinc-500/15', 'bg-zinc-400 dark:bg-zinc-500'],
}
// Fixed order for series that name no colour: by position, never cycled past five.
const ORDER = ['indigo', 'emerald', 'amber', 'rose', 'zinc']

const root = ref(null)
const width = ref(0)
let observer = null
onMounted(() => {
  if (!root.value) return
  width.value = root.value.clientWidth
  if (typeof ResizeObserver !== 'undefined') {
    observer = new ResizeObserver((entries) => {
      const w = entries[0] ? entries[0].contentRect.width : root.value.clientWidth
      if (Math.abs(w - width.value) >= 1) width.value = w
    })
    observer.observe(root.value)
  }
})
onBeforeUnmount(() => observer && observer.disconnect())

const rightFormat = computed(() => props.formatRight || props.format)

/** The series, parsed: times as epoch ms, sorted, bad points dropped. */
const parsed = computed(() =>
  (props.series || []).map((s, i) => {
    const color = COLORS[s.color] ? s.color : ORDER[Math.min(i, ORDER.length - 1)]
    const points = (s.points || [])
      .map((p) => ({ t: Date.parse(p.t), v: Number(p.v) }))
      .filter((p) => Number.isFinite(p.t) && Number.isFinite(p.v))
      .sort((a, b) => a.t - b.t)
    const byTime = new Map(points.map((p) => [p.t, p.v]))
    return { label: s.label, color, dashed: !!s.dashed, axis: s.axis === 'right' ? 'right' : 'left', points, byTime, index: i }
  }),
)

const hasRight = computed(() => parsed.value.some((s) => s.axis === 'right' && s.points.length))
const times = computed(() => {
  const set = new Set()
  for (const s of parsed.value) for (const p of s.points) set.add(p.t)
  return [...set].sort((a, b) => a - b)
})
const empty = computed(() => times.value.length === 0)

/** 0 → a round number above `max`, with 3–5 ticks between. */
function niceTicks(max) {
  if (!(max > 0)) return [0, 1]
  const raw = max / 4
  const mag = 10 ** Math.floor(Math.log10(raw))
  const norm = raw / mag
  const step = (norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 2.5 ? 2.5 : norm <= 5 ? 5 : 10) * mag
  const top = Math.ceil(max / step - 1e-9) * step
  const ticks = []
  for (let v = 0; v <= top + step / 2; v += step) ticks.push(Math.round(v * 1e9) / 1e9)
  return ticks
}

const leftTicks = computed(() =>
  niceTicks(Math.max(0, ...parsed.value.filter((s) => s.axis === 'left').flatMap((s) => s.points.map((p) => p.v)))),
)
const rightTicks = computed(() =>
  niceTicks(Math.max(0, ...parsed.value.filter((s) => s.axis === 'right').flatMap((s) => s.points.map((p) => p.v)))),
)

// Room for the tick labels, estimated from their length.
const CHAR = 6.2
const margin = computed(() => {
  const leftLabel = Math.max(...leftTicks.value.map((v) => props.format(v).length))
  const rightLabel = hasRight.value ? Math.max(...rightTicks.value.map((v) => rightFormat.value(v).length)) : 0
  return {
    top: 8,
    bottom: 22,
    left: Math.max(24, props.axisWidth, Math.ceil(leftLabel * CHAR) + 10),
    right: hasRight.value ? Math.max(24, Math.ceil(rightLabel * CHAR) + 10) : 10,
  }
})
const plotW = computed(() => Math.max(10, width.value - margin.value.left - margin.value.right))
const plotH = computed(() => Math.max(10, props.height - margin.value.top - margin.value.bottom))

const domain = computed(() => {
  if (Array.isArray(props.xDomain) && props.xDomain.length === 2) {
    const from = Date.parse(props.xDomain[0])
    const to = Date.parse(props.xDomain[1])
    if (Number.isFinite(from) && Number.isFinite(to) && to > from) return [from, to]
  }
  const t = times.value
  if (!t.length) return [0, 1]
  if (t.length === 1) return [t[0] - 60_000, t[0] + 60_000]
  return [t[0], t[t.length - 1]]
})
const x = (t) => margin.value.left + ((t - domain.value[0]) / (domain.value[1] - domain.value[0])) * plotW.value
const yFor = (axis) => {
  const ticks = axis === 'right' ? rightTicks.value : leftTicks.value
  const top = ticks[ticks.length - 1] || 1
  return (v) => margin.value.top + plotH.value - (v / top) * plotH.value
}
const baseline = computed(() => margin.value.top + plotH.value)

const paths = computed(() =>
  parsed.value.map((s, i) => {
    const y = yFor(s.axis)
    const pts = s.points.map((p) => [x(p.t), y(p.v)])
    const line = pts.map(([px, py], k) => `${k ? 'L' : 'M'}${px.toFixed(1)},${py.toFixed(1)}`).join('')
    const area =
      i === 0 && pts.length > 1
        ? `${line}L${pts[pts.length - 1][0].toFixed(1)},${baseline.value}L${pts[0][0].toFixed(1)},${baseline.value}Z`
        : null
    return { ...s, line, area, single: pts.length === 1 ? pts[0] : null }
  }),
)

const MINUTE = 60_000
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR
const STEPS = [MINUTE, 5 * MINUTE, 15 * MINUTE, 30 * MINUTE, HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY, 2 * DAY, 7 * DAY, 14 * DAY, 30 * DAY]

const timeTicks = computed(() => {
  if (empty.value) return []
  const [from, to] = domain.value
  const span = to - from
  const room = Math.max(2, Math.floor(plotW.value / 80))
  const step = STEPS.find((s) => span / s <= room) || STEPS[STEPS.length - 1]
  const offset = new Date(from).getTimezoneOffset() * MINUTE
  const first = Math.ceil((from - offset) / step) * step + offset
  const days = span > 36 * HOUR
  const out = []
  for (let t = first; t <= to; t += step) {
    const d = new Date(t)
    const label = days && step >= DAY
      ? d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
      : days && d.getHours() === 0 && d.getMinutes() === 0
        ? d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
        : d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })
    out.push({ t, x: x(t), label })
  }
  return out
})

function stamp(t) {
  const span = domain.value[1] - domain.value[0]
  return new Date(t).toLocaleString(undefined, span > 36 * HOUR
    ? { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }
    : { hour: '2-digit', minute: '2-digit' })
}

// Hover and keyboard: the crosshair snaps to the nearest recorded time.
const active = ref(null)
function nearest(px) {
  const t = times.value
  if (!t.length) return null
  let best = 0
  let dist = Infinity
  for (let i = 0; i < t.length; i++) {
    const d = Math.abs(x(t[i]) - px)
    if (d < dist) {
      dist = d
      best = i
    }
  }
  return best
}
function move(event) {
  const svg = event.currentTarget.ownerSVGElement || event.currentTarget
  const box = svg.getBoundingClientRect()
  active.value = nearest(event.clientX - box.left)
}
function key(event) {
  const n = times.value.length
  if (!n) return
  if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') {
    event.preventDefault()
    const at = active.value === null ? (event.key === 'ArrowRight' ? -1 : n) : active.value
    active.value = Math.max(0, Math.min(n - 1, at + (event.key === 'ArrowRight' ? 1 : -1)))
  } else if (event.key === 'Home') {
    active.value = 0
  } else if (event.key === 'End') {
    active.value = n - 1
  } else if (event.key === 'Escape') {
    active.value = null
  }
}

const hover = computed(() => {
  if (active.value === null || active.value >= times.value.length) return null
  const t = times.value[active.value]
  const px = x(t)
  return {
    t,
    x: px,
    flip: px > width.value / 2,
    // Only the series with a value at this time: a forecast has none in the past.
    rows: parsed.value
      .filter((s) => s.byTime.has(t))
      .map((s) => {
        const v = s.byTime.get(t)
        return {
          label: s.label,
          color: s.color,
          dashed: s.dashed,
          text: (s.axis === 'right' ? rightFormat.value : props.format)(v),
          y: yFor(s.axis)(v),
        }
      }),
  }
})

const summary = computed(() => {
  if (empty.value) return 'Chart: no data.'
  const [from, to] = domain.value
  const parts = parsed.value
    .filter((s) => s.points.length)
    .map((s) => {
      const f = s.axis === 'right' ? rightFormat.value : props.format
      const peak = Math.max(...s.points.map((p) => p.v))
      const last = s.points[s.points.length - 1].v
      return `${s.label}: peak ${f(peak)}, latest ${f(last)}`
    })
  return `Line chart from ${new Date(from).toLocaleString()} to ${new Date(to).toLocaleString()}. ${parts.join('; ')}.`
})
</script>

<template>
  <div class="w-full">
    <ul v-if="series.length > 1" class="mb-2 flex flex-wrap gap-x-4 gap-y-1 text-xs text-zinc-600 dark:text-zinc-400">
      <li v-for="s in parsed" :key="s.index" class="inline-flex items-center gap-1.5">
        <svg viewBox="0 0 16 4" class="h-1 w-4" aria-hidden="true">
          <line x1="0" y1="2" x2="16" y2="2" stroke-width="2.5" :stroke-dasharray="s.dashed ? '3 2' : null" :class="COLORS[s.color][0]" />
        </svg>
        {{ s.label }}<span v-if="hasRight" class="text-zinc-400 dark:text-zinc-500">({{ s.axis }})</span>
      </li>
    </ul>
    <div ref="root" class="relative w-full" :style="{ height: `${height}px` }">
      <div
        v-if="empty"
        class="flex h-full items-center justify-center text-sm text-zinc-500 dark:text-zinc-400"
      >No data in this window.</div>
      <svg
        v-else-if="width > 0"
        :width="width"
        :height="height"
        :viewBox="`0 0 ${width} ${height}`"
        class="block overflow-visible outline-none focus-visible:ring-2 focus-visible:ring-indigo-500/40"
        role="img"
        :aria-label="summary"
        tabindex="0"
        @keydown="key"
        @blur="active = null"
      >
        <!-- Grid and left axis -->
        <g class="text-[10px] tabular-nums">
          <g v-for="v in leftTicks" :key="`l${v}`">
            <line
              :x1="margin.left"
              :x2="width - margin.right"
              :y1="yFor('left')(v)"
              :y2="yFor('left')(v)"
              class="stroke-zinc-200 dark:stroke-zinc-800"
              :stroke-dasharray="v === 0 ? null : '2 3'"
            />
            <text :x="margin.left - 6" :y="yFor('left')(v)" dy="0.32em" text-anchor="end" class="fill-zinc-500 dark:fill-zinc-400">{{ format(v) }}</text>
          </g>
          <template v-if="hasRight">
            <text
              v-for="v in rightTicks"
              :key="`r${v}`"
              :x="width - margin.right + 6"
              :y="yFor('right')(v)"
              dy="0.32em"
              class="fill-zinc-500 dark:fill-zinc-400"
            >{{ rightFormat(v) }}</text>
          </template>
          <text
            v-for="tick in timeTicks"
            :key="`t${tick.t}`"
            :x="tick.x"
            :y="height - 6"
            text-anchor="middle"
            class="fill-zinc-500 dark:fill-zinc-400"
          >{{ tick.label }}</text>
        </g>

        <!-- Series -->
        <g v-for="p in paths" :key="p.index">
          <path v-if="p.area" :d="p.area" :class="COLORS[p.color][1]" stroke="none" />
          <path
            v-if="p.line && !p.single"
            :d="p.line"
            fill="none"
            stroke-width="2"
            stroke-linejoin="round"
            stroke-linecap="round"
            :stroke-dasharray="p.dashed ? '5 4' : null"
            :class="COLORS[p.color][0]"
          />
          <circle v-if="p.single" :cx="p.single[0]" :cy="p.single[1]" r="3" :class="COLORS[p.color][0]" class="fill-white dark:fill-zinc-900" stroke-width="2" />
        </g>

        <!-- Crosshair -->
        <g v-if="hover" pointer-events="none">
          <line :x1="hover.x" :x2="hover.x" :y1="margin.top" :y2="baseline" class="stroke-zinc-400 dark:stroke-zinc-500" stroke-width="1" />
          <template v-for="(r, i) in hover.rows" :key="i">
            <circle
              :cx="hover.x"
              :cy="r.y"
              r="4"
              stroke-width="2"
              class="fill-white dark:fill-zinc-900"
              :class="COLORS[r.color][0]"
            />
          </template>
        </g>

        <rect
          :x="margin.left"
          :y="0"
          :width="plotW"
          :height="height"
          fill="transparent"
          @pointermove="move"
          @pointerdown="move"
          @pointerleave="active = null"
        />
      </svg>

      <div
        v-if="hover"
        class="pointer-events-none absolute top-1 z-10 min-w-36 rounded-lg border border-zinc-200 bg-white/95 px-3 py-2 text-xs shadow-lg backdrop-blur dark:border-zinc-700 dark:bg-zinc-900/95"
        :style="hover.flip ? { right: `${width - hover.x + 10}px` } : { left: `${hover.x + 10}px` }"
        role="status"
        aria-live="polite"
      >
        <div class="mb-1 text-zinc-500 dark:text-zinc-400">{{ stamp(hover.t) }}</div>
        <div v-for="(r, i) in hover.rows" :key="i" class="flex items-center gap-2 whitespace-nowrap">
          <svg viewBox="0 0 12 4" class="h-1 w-3 shrink-0" aria-hidden="true">
            <line x1="0" y1="2" x2="12" y2="2" stroke-width="2.5" :stroke-dasharray="r.dashed ? '3 2' : null" :class="COLORS[r.color][0]" />
          </svg>
          <span class="font-semibold text-zinc-900 tabular-nums dark:text-zinc-100">{{ r.text }}</span>
          <span class="text-zinc-500 dark:text-zinc-400">{{ r.label }}</span>
        </div>
      </div>
    </div>
  </div>
</template>
