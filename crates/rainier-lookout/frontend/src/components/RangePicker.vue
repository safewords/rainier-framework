<script setup>
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

import { formatDay, parseDay } from '../range.js'

const props = defineProps({
  active: Boolean,
  from: { type: String, default: '' },
  to: { type: String, default: '' },
})
const emit = defineEmits(['apply', 'clear'])

const MAX_DAYS = 60
const WEEKDAYS = ['Su', 'Mo', 'Tu', 'We', 'Th', 'Fr', 'Sa']

const open = ref(false)
const root = ref(null)
const cursor = ref(startOfMonth(new Date()))
const start = ref(null)
const end = ref(null)
const error = ref('')

function startOfMonth(date) {
  return new Date(date.getFullYear(), date.getMonth(), 1)
}
function strip(date) {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate())
}
function today() {
  return strip(new Date())
}
function addMonths(date, n) {
  return new Date(date.getFullYear(), date.getMonth() + n, 1)
}

const monthLabel = computed(() => cursor.value.toLocaleDateString(undefined, { month: 'long', year: 'numeric' }))
const days = computed(() => {
  const year = cursor.value.getFullYear()
  const month = cursor.value.getMonth()
  const pad = new Date(year, month, 1).getDay()
  const count = new Date(year, month + 1, 0).getDate()
  const cells = Array.from({ length: pad }, () => null)
  for (let day = 1; day <= count; day++) cells.push(new Date(year, month, day))
  return cells
})

function same(a, b) {
  return a && b && a.getTime() === b.getTime()
}
function between(day) {
  if (!start.value || !end.value || !day) return false
  return day > start.value && day < end.value
}
function future(day) {
  return day && day > today()
}
function tooLong(a, b) {
  if (!a || !b) return false
  return Math.round((b.getTime() - a.getTime()) / 86_400_000) > MAX_DAYS
}

function pick(day) {
  if (!day || future(day)) return
  error.value = ''
  if (!start.value || end.value) {
    start.value = day
    end.value = null
    return
  }
  if (day < start.value) {
    end.value = start.value
    start.value = day
  } else {
    end.value = day
  }
  if (tooLong(start.value, end.value)) error.value = `Choose ${MAX_DAYS} days or fewer.`
}

function dayClass(day) {
  if (!day) return ''
  const on = same(day, start.value) || same(day, end.value)
  if (on) return 'bg-indigo-600 text-white'
  if (between(day)) return 'bg-indigo-50 text-indigo-800 dark:bg-indigo-500/15 dark:text-indigo-200'
  if (future(day)) return 'cursor-not-allowed text-zinc-300 dark:text-zinc-600'
  if (same(day, today())) return 'text-indigo-700 ring-1 ring-indigo-400 ring-inset dark:text-indigo-300'
  return 'text-zinc-800 hover:bg-zinc-100 dark:text-zinc-100 dark:hover:bg-zinc-800'
}

function show() {
  const from = parseDay(props.from)
  const to = parseDay(props.to)
  start.value = from
  end.value = to && to >= from ? to : null
  cursor.value = startOfMonth(from || new Date())
  error.value = ''
  open.value = true
}
function toggle() {
  if (open.value) open.value = false
  else show()
}
function apply() {
  if (!start.value || !end.value || error.value) return
  emit('apply', { from: formatDay(start.value), to: formatDay(end.value) })
  open.value = false
}
function clear() {
  emit('clear')
  open.value = false
}

function onDocument(event) {
  if (!open.value) return
  if (event.type === 'keydown') {
    if (event.key === 'Escape') open.value = false
    return
  }
  if (root.value && !root.value.contains(event.target)) open.value = false
}
onMounted(() => {
  document.addEventListener('mousedown', onDocument)
  document.addEventListener('keydown', onDocument)
})
onBeforeUnmount(() => {
  document.removeEventListener('mousedown', onDocument)
  document.removeEventListener('keydown', onDocument)
})

const summary = computed(() => {
  if (!start.value) return 'Choose a start date, then an end date.'
  if (!end.value) return `${formatDay(start.value)} – …`
  return `${formatDay(start.value)} – ${formatDay(end.value)}`
})
</script>

<template>
  <div ref="root" class="relative">
    <button
      type="button"
      class="lk-btn px-2"
      :class="active ? 'border-indigo-500 bg-indigo-50 text-indigo-700 dark:border-indigo-400 dark:bg-indigo-500/15 dark:text-indigo-300' : ''"
      :aria-expanded="open"
      aria-haspopup="dialog"
      aria-controls="lookout-range-picker"
      aria-label="Custom date range"
      title="Custom date range"
      @click="toggle"
    >
      <svg viewBox="0 0 20 20" class="size-4" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true">
        <rect x="3" y="4" width="14" height="13" rx="1.5" />
        <path d="M3 8h14M7 2.5v3M13 2.5v3" />
      </svg>
    </button>
    <div
      v-if="open"
      id="lookout-range-picker"
      role="dialog"
      aria-label="Custom date range"
      class="absolute right-0 z-40 mt-2 w-72 rounded-lg border border-zinc-200 bg-white p-3 shadow-lg sm:left-0 sm:right-auto dark:border-zinc-700 dark:bg-zinc-900"
    >
      <div class="mb-2 flex items-center justify-between">
        <button type="button" class="lk-btn px-2" aria-label="Previous month" @click="cursor = addMonths(cursor, -1)">
          <svg viewBox="0 0 20 20" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M12 4l-6 6 6 6" /></svg>
        </button>
        <span class="text-sm font-semibold">{{ monthLabel }}</span>
        <button type="button" class="lk-btn px-2" aria-label="Next month" @click="cursor = addMonths(cursor, 1)">
          <svg viewBox="0 0 20 20" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M8 4l6 6-6 6" /></svg>
        </button>
      </div>
      <div class="grid grid-cols-7 gap-1 text-center text-[11px] font-semibold tracking-wide text-zinc-400 uppercase">
        <span v-for="name in WEEKDAYS" :key="name">{{ name }}</span>
      </div>
      <div class="mt-1 grid grid-cols-7 gap-1">
        <span v-for="(day, i) in days" :key="i" class="aspect-square">
          <button
            v-if="day"
            type="button"
            class="flex h-full w-full items-center justify-center rounded-md text-sm tabular-nums transition"
            :class="dayClass(day)"
            :disabled="future(day)"
            :aria-pressed="same(day, start) || same(day, end)"
            :aria-label="formatDay(day)"
            @click="pick(day)"
          >{{ day.getDate() }}</button>
        </span>
      </div>
      <p class="mt-2 text-xs text-zinc-500 dark:text-zinc-400">{{ summary }}</p>
      <p v-if="error" class="mt-1 text-xs text-rose-600 dark:text-rose-400">{{ error }}</p>
      <div class="mt-3 flex gap-2">
        <button type="button" class="lk-btn" :disabled="!start || !end || !!error" @click="apply">Apply</button>
        <button type="button" class="lk-btn" @click="clear">Clear</button>
      </div>
    </div>
  </div>
</template>
