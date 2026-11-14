<script setup>
import { computed } from 'vue'
import DataTable from './DataTable.vue'
import { average, time } from '../format.js'

const props = defineProps({
  title: { type: String, required: true },
  hint: { type: String, default: '' },
  rows: { type: Array, default: () => [] },
  order: { type: String, default: 'max' },
  timed: { type: Boolean, default: true },
  // (row) => route: the primary click, normally the issue page.
  link: { type: Function, default: null },
  // (row) => { to, label } | null: a second, smaller link on the row.
  secondary: { type: Function, default: null },
  keyLabel: { type: String, default: 'Name' },
  loading: Boolean,
})

const metric = (row) => (props.order === 'count' ? row.count : props.order === 'average' ? average(row) : row.max_us)
const top = computed(() => Math.max(1, ...props.rows.map(metric)))
const barKey = computed(() => (props.order === 'count' ? 'count' : props.order === 'average' ? 'average' : 'max'))

const columns = computed(() => {
  const cols = [
    { key: 'key', label: props.keyLabel, sortable: true, value: (r) => r.key, text: (r) => r.key, class: 'font-mono text-[12.5px]' },
    { key: 'count', label: 'Count', title: 'How many were recorded in this window', align: 'right', sortable: true, value: (r) => r.count, text: (r) => r.count.toLocaleString() },
  ]
  if (props.timed) {
    cols.push(
      { key: 'average', label: 'Avg', title: 'Average duration in this window', align: 'right', sortable: true, value: average, text: (r) => time(average(r)) },
      { key: 'max', label: 'Max', title: 'The single slowest in this window', align: 'right', sortable: true, value: (r) => r.max_us, text: (r) => time(r.max_us) },
    )
  }
  if (props.secondary) cols.push({ key: 'more', label: 'More', text: () => '' })
  return cols
})
const sort = computed(() => ({ key: barKey.value, dir: 'desc' }))
</script>

<template>
  <section class="lk-card flex min-w-0 flex-col">
    <header class="lk-card-header">
      <h2 class="text-sm font-semibold">{{ title }}</h2>
      <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">{{ hint }}</span>
    </header>
    <DataTable
      :columns="columns"
      :rows="rows"
      :row-key="(r) => r.key"
      :to="link"
      :initial-sort="sort"
      :loading="loading"
      empty="Nothing recorded in this window."
      :label="title"
    >
      <template #cell-key="{ row }">
        <span class="block truncate text-zinc-800 dark:text-zinc-200">{{ row.key }}</span>
        <span class="mt-1 block h-1 overflow-hidden rounded-full bg-zinc-100 dark:bg-zinc-800" aria-hidden="true">
          <span
            class="block h-full rounded-full"
            :class="timed ? 'bg-indigo-500' : 'bg-rose-500'"
            :style="{ width: `${Math.max(1, (metric(row) / top) * 100)}%` }"
          />
        </span>
      </template>
      <template #cell-count="{ row }">
        <span :class="barKey === 'count' ? 'font-semibold text-zinc-900 dark:text-zinc-100' : ''">{{ row.count.toLocaleString() }}</span>
      </template>
      <template #cell-average="{ row }">
        <span :class="barKey === 'average' ? 'font-semibold text-zinc-900 dark:text-zinc-100' : ''">{{ time(average(row)) }}</span>
      </template>
      <template #cell-max="{ row }">
        <span :class="barKey === 'max' ? 'font-semibold text-zinc-900 dark:text-zinc-100' : ''">{{ time(row.max_us) }}</span>
      </template>
      <template #cell-more="{ row }">
        <RouterLink
          v-if="secondary && secondary(row)"
          :to="secondary(row).to"
          class="text-xs font-medium text-indigo-600 hover:underline dark:text-indigo-400"
        >{{ secondary(row).label }}</RouterLink>
      </template>
    </DataTable>
  </section>
</template>
