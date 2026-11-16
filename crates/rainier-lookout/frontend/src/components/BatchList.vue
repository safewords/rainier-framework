<script setup>
import BatchStatus from './BatchStatus.vue'
import DataTable from './DataTable.vue'
import Flags from './Flags.vue'
import TimeAgo from './TimeAgo.vue'
import { time, itemRoute } from '../format.js'

defineProps({
  batches: { type: Array, required: true },
  filterable: Boolean,
  loading: Boolean,
  empty: { type: String, default: 'Nothing recorded yet.' },
  label: { type: String, default: 'Batches' },
})

function total(counts) {
  return Object.values(counts || {}).reduce((sum, n) => sum + n, 0)
}
function breakdown(counts) {
  return Object.entries(counts || {})
    .map(([type, n]) => `${n} ${type}`)
    .join(' · ')
}
function flags(batch) {
  const out = []
  if (batch.has_exception) out.push('exception')
  if (batch.has_slow) out.push('slow')
  return out.concat(batch.tags || []).join(' ')
}

const columns = [
  {
    key: 'summary',
    label: 'Summary',
    sortable: true,
    text: (b) => [b.summary, b.route_name].filter(Boolean).join(' '),
    value: (b) => b.summary,
  },
  {
    key: 'status',
    label: 'Status',
    sortable: true,
    value: (b) => b.status || (b.has_exception ? 600 : 0),
    text: (b) => (b.status ? String(b.status) : b.has_exception ? 'failed' : 'done'),
  },
  { key: 'duration', label: 'Duration', title: 'How long this whole unit of work took, start to finish', align: 'right', sortable: true, value: (b) => b.duration_us, text: (b) => time(b.duration_us) },
  { key: 'entries', label: 'Entries', title: 'How many entries (queries, cache calls, logs, …) it recorded', align: 'right', sortable: true, value: (b) => total(b.counts), text: (b) => breakdown(b.counts) },
  { key: 'user', label: 'User', sortable: true, value: (b) => b.user_id || '', text: (b) => (b.user_id ? `user ${b.user_id}` : 'guest') },
  { key: 'flags', label: 'Flags', text: flags },
  { key: 'when', label: 'When', title: 'When it started', align: 'right', sortable: true, value: (b) => b.started_at, text: (b) => b.started_at },
]
</script>

<template>
  <DataTable
    :columns="columns"
    :rows="batches"
    :row-key="(b) => b.id"
    :to="(b) => (itemRoute(b))"
    :filterable="filterable"
    filter-placeholder="Filter these batches…"
    :loading="loading"
    :empty="empty"
    :label="label"
  >
    <template #cell-summary="{ row }">
      <span class="block truncate font-mono text-[13px] text-zinc-900 dark:text-zinc-100">{{ row.summary }}</span>
      <span v-if="row.route_name" class="block truncate text-xs text-zinc-500 dark:text-zinc-400">{{ row.route_name }}</span>
    </template>
    <template #cell-status="{ row }"><BatchStatus :batch="row" /></template>
    <template #cell-duration="{ row }">{{ time(row.duration_us) }}</template>
    <template #cell-entries="{ row }"><span :title="breakdown(row.counts)">{{ total(row.counts) }}</span></template>
    <template #cell-user="{ row }">
      <template v-if="row.user_id">user {{ row.user_id }}</template>
      <span v-else class="text-zinc-400 dark:text-zinc-500">guest</span>
    </template>
    <template #cell-flags="{ row }"><Flags :exception="row.has_exception" :slow="row.has_slow" :tags="row.tags || []" /></template>
    <template #cell-when="{ row }"><span class="text-zinc-500 dark:text-zinc-400"><TimeAgo :at="row.started_at" /></span></template>
  </DataTable>
</template>
