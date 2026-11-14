<script setup>
// The one table Lookout draws: sortable headers, an optional text filter, a
// whole-row link, and — below the md breakpoint — a stack of cards instead of
// columns, so a phone never scrolls sideways.
//
// columns: [{ key, label, sortable?, align?: 'left'|'right', value?: (row) => sort value,
//             text?: (row) => display/filter text, title?: header tooltip, class?, primary? }]
// Every value sits under a labelled column — nothing floats unlabelled.
// The first column (or the one marked `primary`) takes the spare width and
// truncates; it is the row's title on a card. A `#cell-<key>="{ row }"` slot
// draws a cell by hand.
import { computed, onBeforeUnmount, ref, useId, useSlots } from 'vue'
import { useRouter } from 'vue-router'

import Empty from './Empty.vue'

const props = defineProps({
  columns: { type: Array, required: true },
  rows: { type: Array, default: () => [] },
  rowKey: { type: Function, default: (row) => String(row.id ?? row.key) },
  to: { type: Function, default: null },
  filterable: Boolean,
  filterPlaceholder: { type: String, default: 'Filter…' },
  initialSort: { type: Object, default: null },
  empty: { type: String, default: 'Nothing to show.' },
  loading: Boolean,
  label: { type: String, default: '' },
})

const slots = useSlots()
const router = useRouter()
const id = useId()

// One layout at a time, so slots render once and ids stay unique.
const query = typeof window !== 'undefined' && window.matchMedia ? window.matchMedia('(min-width: 48rem)') : null
const wide = ref(query ? query.matches : true)
const onMedia = (event) => (wide.value = event.matches)
if (query) {
  if (query.addEventListener) query.addEventListener('change', onMedia)
  else if (query.addListener) query.addListener(onMedia)
}
onBeforeUnmount(() => {
  if (!query) return
  if (query.removeEventListener) query.removeEventListener('change', onMedia)
  else if (query.removeListener) query.removeListener(onMedia)
})

const primary = computed(() => (props.columns.find((c) => c.primary) || props.columns[0] || {}).key)
const rest = computed(() => props.columns.filter((c) => c.key !== primary.value))

const raw = (col, row) => (col.value ? col.value(row) : row[col.key])
function display(col, row) {
  const v = col.text ? col.text(row) : row[col.key]
  return v === null || v === undefined || v === '' ? '—' : v
}

// Filtering: any column's text contains every word typed.
const filter = ref('')
const filtered = computed(() => {
  const words = filter.value.trim().toLowerCase().split(/\s+/).filter(Boolean)
  if (!words.length) return props.rows
  return props.rows.filter((row) => {
    const hay = props.columns
      .map((c) => {
        const v = c.text ? c.text(row) : c.value ? c.value(row) : row[c.key]
        return v === null || v === undefined ? '' : String(v)
      })
      .join(' \u0000 ')
      .toLowerCase()
    return words.every((w) => hay.includes(w))
  })
})

// Sorting.
const sortKey = ref(props.initialSort ? props.initialSort.key : null)
const sortDir = ref(props.initialSort && props.initialSort.dir === 'asc' ? 'asc' : 'desc')
const sortable = computed(() => props.columns.filter((c) => c.sortable))

function compare(a, b) {
  const blankA = a === null || a === undefined || a === ''
  const blankB = b === null || b === undefined || b === ''
  if (blankA || blankB) return blankA === blankB ? 0 : blankA ? 1 : -1
  if (typeof a === 'number' && typeof b === 'number') return a - b
  if (typeof a === 'boolean' && typeof b === 'boolean') return Number(a) - Number(b)
  return String(a).localeCompare(String(b), undefined, { numeric: true, sensitivity: 'base' })
}

const sorted = computed(() => {
  const col = props.columns.find((c) => c.key === sortKey.value)
  if (!col) return filtered.value
  const sign = sortDir.value === 'asc' ? 1 : -1
  return filtered.value
    .map((row, i) => ({ row, i, v: raw(col, row) }))
    .sort((x, y) => {
      const blank = (v) => v === null || v === undefined || v === ''
      // Blanks always last, whichever way.
      if (blank(x.v) !== blank(y.v)) return blank(x.v) ? 1 : -1
      return sign * compare(x.v, y.v) || x.i - y.i
    })
    .map((x) => x.row)
})

function sortBy(col) {
  if (!col.sortable) return
  if (sortKey.value === col.key) sortDir.value = sortDir.value === 'asc' ? 'desc' : 'asc'
  else {
    sortKey.value = col.key
    // Numbers read biggest-first; names alphabetically.
    sortDir.value = col.align === 'right' ? 'desc' : 'asc'
  }
}
function ariaSort(col) {
  if (!col.sortable) return undefined
  if (sortKey.value !== col.key) return 'none'
  return sortDir.value === 'asc' ? 'ascending' : 'descending'
}

// The card layout's sort control: one select of "column, direction".
const sortChoice = computed({
  get: () => (sortKey.value ? `${sortKey.value}:${sortDir.value}` : ''),
  set: (value) => {
    if (!value) {
      sortKey.value = null
      return
    }
    const at = value.lastIndexOf(':')
    sortKey.value = value.slice(0, at)
    sortDir.value = value.slice(at + 1) === 'asc' ? 'asc' : 'desc'
  },
})

// Whole-row clicks. The primary cell carries a real link for the keyboard and
// for "open in new tab"; a click anywhere else on the row follows it too,
// unless it landed on something interactive of its own.
function onRowClick(event, row) {
  if (!props.to) return
  if (event.defaultPrevented || event.button !== 0) return
  const target = event.target
  if (target && target.closest && target.closest('a, button, input, select, textarea, label, summary')) return
  if (window.getSelection && String(window.getSelection()).length) return
  const destination = props.to(row)
  if (event.metaKey || event.ctrlKey || event.shiftKey) {
    window.open(router.resolve(destination).href, '_blank', 'noopener')
    return
  }
  router.push(destination)
}

const hasSlot = (key) => !!slots[`cell-${key}`]
const alignClass = (col) => (col.align === 'right' ? 'text-right' : 'text-left')
</script>

<template>
  <div class="min-w-0">
    <div
      v-if="filterable || $slots.toolbar || (!wide && sortable.length > 1 && rows.length > 1)"
      class="flex flex-wrap items-center gap-2 border-b border-zinc-200 px-4 py-2.5 dark:border-zinc-800"
    >
      <input
        v-if="filterable"
        v-model="filter"
        type="search"
        class="lk-input min-w-0 flex-1 sm:max-w-xs"
        :placeholder="filterPlaceholder"
        :aria-label="label ? `Filter ${label}` : 'Filter'"
        :aria-controls="`${id}-rows`"
      />
      <select
        v-if="!wide && sortable.length > 1 && rows.length > 1"
        v-model="sortChoice"
        class="lk-input w-auto max-w-44"
        aria-label="Sort by"
      >
        <option value="">Sort: default</option>
        <template v-for="col in sortable" :key="col.key">
          <option :value="`${col.key}:desc`">{{ col.label }} {{ col.align === 'right' ? '↓ high' : '↓ Z–A' }}</option>
          <option :value="`${col.key}:asc`">{{ col.label }} {{ col.align === 'right' ? '↑ low' : '↑ A–Z' }}</option>
        </template>
      </select>
      <slot name="toolbar" />
      <span
        v-if="filterable && filter.trim()"
        class="ml-auto text-xs text-zinc-500 tabular-nums dark:text-zinc-400"
        aria-live="polite"
      >{{ sorted.length }} of {{ rows.length }}</span>
    </div>

    <Empty v-if="!sorted.length">
      {{ loading && !rows.length ? 'Loading…' : rows.length ? 'Nothing matches that filter.' : empty }}
    </Empty>

    <!-- md and up: a real table. -->
    <div v-else-if="wide" class="overflow-x-auto">
      <table :id="`${id}-rows`" class="w-full border-collapse text-sm" :aria-label="label || undefined">
        <thead>
          <tr class="border-b border-zinc-200 dark:border-zinc-800">
            <th
              v-for="col in columns"
              :key="col.key"
              scope="col"
              class="lk-th px-4 py-2 whitespace-nowrap first:pl-4"
              :class="[alignClass(col), col.key === primary ? 'w-full' : '']"
              :aria-sort="ariaSort(col)"
              :title="col.title || undefined"
            >
              <button
                v-if="col.sortable"
                type="button"
                class="lk-th inline-flex items-center gap-1 rounded hover:text-zinc-800 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-500 dark:hover:text-zinc-200"
                :class="[col.align === 'right' ? 'flex-row-reverse' : '', sortKey === col.key ? 'text-zinc-800 dark:text-zinc-200' : '']"
                @click="sortBy(col)"
              >
                {{ col.label }}
                <svg viewBox="0 0 10 10" class="size-2.5 shrink-0" :class="sortKey === col.key ? '' : 'opacity-30'" fill="currentColor" aria-hidden="true">
                  <path v-if="sortKey !== col.key" d="M5 1l3 3H2zM5 9L2 6h6z" />
                  <path v-else-if="sortDir === 'asc'" d="M5 2l4 5H1z" />
                  <path v-else d="M5 8L1 3h8z" />
                </svg>
              </button>
              <template v-else>{{ col.label }}</template>
            </th>
          </tr>
        </thead>
        <tbody class="divide-y divide-zinc-100 dark:divide-zinc-800">
          <tr
            v-for="row in sorted"
            :key="rowKey(row)"
            class="align-middle transition"
            :class="to ? 'cursor-pointer hover:bg-zinc-50 focus-within:bg-zinc-50 dark:hover:bg-zinc-800/50 dark:focus-within:bg-zinc-800/50' : ''"
            @click="onRowClick($event, row)"
          >
            <td
              v-for="col in columns"
              :key="col.key"
              class="px-4 py-2.5"
              :class="[
                alignClass(col),
                col.key === primary ? 'max-w-0 w-full' : 'whitespace-nowrap',
                col.align === 'right' ? 'tabular-nums' : '',
                col.class || '',
              ]"
            >
              <template v-if="col.key === primary && to">
                <RouterLink
                  :to="to(row)"
                  class="block min-w-0 truncate rounded focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-500"
                  :title="typeof display(col, row) === 'string' ? display(col, row) : undefined"
                >
                  <slot v-if="hasSlot(col.key)" :name="`cell-${col.key}`" :row="row" />
                  <template v-else>{{ display(col, row) }}</template>
                </RouterLink>
              </template>
              <template v-else-if="col.key === primary">
                <div class="min-w-0 truncate" :title="typeof display(col, row) === 'string' ? display(col, row) : undefined">
                  <slot v-if="hasSlot(col.key)" :name="`cell-${col.key}`" :row="row" />
                  <template v-else>{{ display(col, row) }}</template>
                </div>
              </template>
              <template v-else>
                <slot v-if="hasSlot(col.key)" :name="`cell-${col.key}`" :row="row" />
                <template v-else>{{ display(col, row) }}</template>
              </template>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <!-- Below md: one card per row, label beside value. -->
    <ul v-else :id="`${id}-rows`" class="divide-y divide-zinc-200 dark:divide-zinc-800" :aria-label="label || undefined">
      <li
        v-for="row in sorted"
        :key="rowKey(row)"
        class="min-w-0 px-4 py-3 transition"
        :class="to ? 'cursor-pointer hover:bg-zinc-50 focus-within:bg-zinc-50 dark:hover:bg-zinc-800/50 dark:focus-within:bg-zinc-800/50' : ''"
        @click="onRowClick($event, row)"
      >
        <template v-for="col in columns" :key="col.key">
          <div v-if="col.key === primary" class="min-w-0 text-sm" :class="col.class || ''">
            <RouterLink v-if="to" :to="to(row)" class="block min-w-0 truncate rounded focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-500">
              <slot v-if="hasSlot(col.key)" :name="`cell-${col.key}`" :row="row" />
              <template v-else>{{ display(col, row) }}</template>
            </RouterLink>
            <div v-else class="min-w-0 truncate">
              <slot v-if="hasSlot(col.key)" :name="`cell-${col.key}`" :row="row" />
              <template v-else>{{ display(col, row) }}</template>
            </div>
          </div>
        </template>
        <dl v-if="rest.length" class="mt-2 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 text-xs">
          <template v-for="col in rest" :key="col.key">
            <dt class="lk-th self-center" :title="col.title || undefined">{{ col.label }}</dt>
            <dd class="min-w-0 truncate text-zinc-700 tabular-nums dark:text-zinc-300">
              <slot v-if="hasSlot(col.key)" :name="`cell-${col.key}`" :row="row" />
              <template v-else>{{ display(col, row) }}</template>
            </dd>
          </template>
        </dl>
      </li>
    </ul>
  </div>
</template>
