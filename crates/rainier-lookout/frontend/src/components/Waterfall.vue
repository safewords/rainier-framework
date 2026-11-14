<script setup>
import { computed } from 'vue'
import { duration } from '../format.js'

const props = defineProps({
  batch: { type: Object, required: true },
  selected: { type: String, default: null },
})
const emit = defineEmits(['select'])

const COLORS = {
  request: 'bg-indigo-500',
  query: 'bg-emerald-500',
  cache: 'bg-amber-500',
  http_client: 'bg-sky-500',
  job: 'bg-violet-500',
  mail: 'bg-pink-500',
  notification: 'bg-pink-500',
  exception: 'bg-rose-500',
  log: 'bg-zinc-400',
  view: 'bg-teal-500',
  event: 'bg-lime-600',
  n_plus_one: 'bg-orange-500',
  slow: 'bg-orange-500',
}
const INDENT = ['pl-0', 'pl-3', 'pl-6', 'pl-9', 'pl-12', 'pl-15', 'pl-18', 'pl-21', 'pl-24']

function offset(entry) {
  const us = (Date.parse(entry.occurred_at) - Date.parse(props.batch.started_at)) * 1000
  return Number.isFinite(us) && us > 0 ? us : 0
}

/** Each entry after its parent span, with its depth; a cycle cannot lose one. */
function tree(entries) {
  const bySpan = new Map()
  entries.forEach((e, i) => e.span_id && bySpan.set(e.span_id, i))
  const children = new Map()
  const roots = []
  entries.forEach((e, i) => {
    const parent = e.parent_span_id ? bySpan.get(e.parent_span_id) : undefined
    if (parent !== undefined && parent !== i) {
      if (!children.has(parent)) children.set(parent, [])
      children.get(parent).push(i)
    } else {
      roots.push(i)
    }
  })
  const start = (i) => Date.parse(entries[i].occurred_at) || 0
  roots.sort((a, b) => start(a) - start(b))
  for (const list of children.values()) list.sort((a, b) => start(a) - start(b))
  const order = []
  const seen = new Set()
  const stack = roots.slice().reverse().map((i) => [i, 0])
  while (stack.length) {
    const [i, depth] = stack.pop()
    if (seen.has(i)) continue
    seen.add(i)
    order.push([i, depth])
    for (const k of (children.get(i) || []).slice().reverse()) stack.push([k, depth + 1])
  }
  entries.forEach((_, i) => seen.has(i) || order.push([i, 0]))
  return order
}

const total = computed(() => {
  const ends = props.batch.entries.map((e) => offset(e) + (e.duration_us || 0))
  return Math.max(1, props.batch.duration_us || 0, ...ends)
})

const rows = computed(() =>
  tree(props.batch.entries).map(([i, depth]) => {
    const entry = props.batch.entries[i]
    const left = Math.min(99.5, (offset(entry) / total.value) * 100)
    const width = Math.min(100 - left, Math.max(0.4, ((entry.duration_us || 0) / total.value) * 100))
    return { entry, depth, left, width }
  }),
)

const ticks = computed(() => [0, 1, 2, 3, 4].map((q) => duration(Math.round((total.value * q) / 4))))
</script>

<template>
  <div class="py-2">
    <div class="hidden grid-cols-[minmax(12rem,34%)_minmax(0,1fr)_4.5rem] gap-3 px-4 pb-2 text-[11px] text-zinc-400 md:grid dark:text-zinc-500">
      <span class="lk-th">Entry</span>
      <span class="flex justify-between tabular-nums"><span v-for="(t, i) in ticks" :key="i">{{ t }}</span></span>
      <span />
    </div>
    <div class="flex justify-between px-4 pb-2 text-[11px] text-zinc-400 tabular-nums md:hidden dark:text-zinc-500">
      <span>{{ ticks[0] }}</span><span>{{ ticks[4] }}</span>
    </div>
    <ul>
      <li v-for="row in rows" :key="row.entry.id">
        <button
          type="button"
          class="grid w-full grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 gap-y-1 px-4 py-1.5 text-left transition hover:bg-zinc-50 md:grid-cols-[minmax(12rem,34%)_minmax(0,1fr)_4.5rem] md:py-1 dark:hover:bg-zinc-800/50"
          :class="selected === row.entry.id ? 'bg-indigo-50 dark:bg-indigo-500/10' : ''"
          :aria-pressed="selected === row.entry.id"
          @click="emit('select', row.entry.id)"
        >
          <span class="min-w-0 truncate text-[13px]" :class="INDENT[Math.min(row.depth, 8)]" :title="row.entry.summary">
            <span class="mr-1.5 inline-block w-20 text-[10px] font-bold tracking-wide text-zinc-400 uppercase dark:text-zinc-500">{{ row.entry.type }}</span>
            <span class="font-mono text-zinc-800 dark:text-zinc-200">{{ row.entry.summary }}</span>
          </span>
          <span class="relative col-span-2 row-start-2 h-3 overflow-hidden rounded bg-zinc-100 md:col-span-1 md:row-start-auto dark:bg-zinc-800">
            <span class="absolute inset-y-0 left-1/4 w-px bg-zinc-200 dark:bg-zinc-700" />
            <span class="absolute inset-y-0 left-1/2 w-px bg-zinc-200 dark:bg-zinc-700" />
            <span class="absolute inset-y-0 left-3/4 w-px bg-zinc-200 dark:bg-zinc-700" />
            <span
              class="absolute inset-y-0.5 rounded-sm"
              :class="COLORS[row.entry.type] || 'bg-zinc-400'"
              :style="{ left: `${row.left}%`, width: `${row.width}%` }"
            />
          </span>
          <span class="col-start-2 row-start-1 text-right text-xs text-zinc-500 tabular-nums md:col-start-auto md:row-start-auto dark:text-zinc-400">
            {{ duration(row.entry.duration_us) }}
          </span>
        </button>
      </li>
    </ul>
  </div>
</template>
