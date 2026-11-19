<script setup>
// Every model written in the header's window, from the aggregates — so every
// write is counted, in batches that were kept or only counted — and, for the
// model picked, its recorded changes: each linked to the request or job that
// made it, and to that unit's whole trace (a request and every job it set off).
import { computed, ref, watch } from 'vue'
import { useRoute } from 'vue-router'

import DataTable from '../components/DataTable.vue'
import ErrorBox from '../components/ErrorBox.vue'
import LoadMore from '../components/LoadMore.vue'
import Pill from '../components/Pill.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { count, itemRoute } from '../format.js'
import { usePolling } from '../live.js'
import { parseRange, rangeQuery } from '../range.js'

const props = defineProps({ model: { type: String, default: '' } })
const route = useRoute()
const range = computed(() => parseRange(route.query))
const windowLabel = computed(() => (range.value.preset ? `the last ${range.value.label}` : range.value.label))

const ACTIONS = ['created', 'updated', 'upserted', 'deleted']
const TONE = { created: 'ok', updated: 'info', upserted: 'accent', deleted: 'bad' }
const KIND = { request: 'Request', job: 'Job', command: 'Command', scheduled_task: 'Task', other: 'Other' }

const models = ref([])
const period = ref(600)
const loadingModels = ref(true)
const modelsError = ref(null)

let modelsGeneration = 0
async function loadModels() {
  const mine = ++modelsGeneration
  try {
    const answer = await api.models({ since: range.value.since })
    if (mine !== modelsGeneration) return
    models.value = answer.data
    period.value = answer.period_s || 600
    modelsError.value = null
  } catch (e) {
    if (mine === modelsGeneration) modelsError.value = e
  } finally {
    if (mine === modelsGeneration) loadingModels.value = false
  }
}

const changes = ref([])
const next = ref(null)
const loadingChanges = ref(false)
const changesError = ref(null)

let changesGeneration = 0
async function loadChanges(more = false) {
  if (!props.model) {
    changes.value = []
    next.value = null
    return
  }
  const mine = ++changesGeneration
  loadingChanges.value = true
  try {
    const before = more && next.value ? next.value.before : undefined
    const page = await api.modelChanges(props.model, { before, limit: 50 })
    if (mine !== changesGeneration) return
    changes.value = more ? [...changes.value, ...page.data] : page.data
    next.value = page.next
    changesError.value = null
  } catch (e) {
    if (mine === changesGeneration) changesError.value = e
  } finally {
    if (mine === changesGeneration) loadingChanges.value = false
  }
}

watch(() => range.value.since, loadModels, { immediate: true })
watch(() => props.model, () => loadChanges(false), { immediate: true })
usePolling(() => {
  loadModels()
  if (!next.value || changes.value.length <= 50) loadChanges(false)
}, 15000)

// Counts come in buckets: a model last written in the current one was written
// at most a bucket ago, so show the bucket's end — capped at now — rather
// than its start, which reads up to a bucket too old.
function lastWritten(m) {
  const end = Date.parse(m.last_bucket) + period.value * 1000
  return new Date(Math.min(end, Date.now())).toISOString()
}

const picked = computed(() => models.value.find((m) => m.model === props.model) || null)

const modelColumns = [
  { key: 'model', label: 'Model', sortable: true, primary: true },
  ...ACTIONS.map((action) => ({
    key: action,
    label: action[0].toUpperCase() + action.slice(1),
    sortable: true,
    align: 'right',
    value: (m) => m.actions[action] || 0,
  })),
  { key: 'total', label: 'Writes', sortable: true, align: 'right' },
  { key: 'last_bucket', label: 'Last written', sortable: true },
]

const madeBy = (e) => (e.content && e.content.batch) || {}
const keysOf = (e) => (e.content && Array.isArray(e.content.keys) ? e.content.keys : null)

const changeColumns = [
  { key: 'action', label: 'Action', text: (e) => e.content && e.content.action },
  { key: 'keys', label: 'Rows', text: (e) => (keysOf(e) || ['all matching']).join(', ') },
  { key: 'by', label: 'Made by', primary: true, text: (e) => madeBy(e).summary || '' },
  { key: 'trace', label: 'Trace', text: (e) => madeBy(e).trace_id || '' },
  { key: 'occurred_at', label: 'When' },
]

const modelRoute = (m) => ({ name: 'model', params: { model: m.model }, query: rangeQuery(route.query) })
const traceRoute = (traceId) => ({ name: 'batches', query: { trace: traceId } })
</script>

<template>
  <div class="space-y-5">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">Models</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        Every model written in {{ windowLabel }}, from the <code class="font-mono">ModelChanged</code> events on the bus — each
        committed write through any repository or builder. Pick one to see its changes and what made them.
      </p>
    </div>

    <ErrorBox :error="modelsError" />

    <section class="lk-card">
      <DataTable
        :columns="modelColumns"
        :rows="models"
        :row-key="(m) => m.model"
        :to="modelRoute"
        :initial-sort="{ key: 'total', dir: 'desc' }"
        :loading="loadingModels"
        filterable
        filter-placeholder="Filter models…"
        empty="No model was written in this window."
        label="models"
      >
        <template #cell-model="{ row }">
          <span
            class="block truncate font-mono text-[12.5px]"
            :class="row.model === props.model ? 'font-semibold text-indigo-700 dark:text-indigo-300' : 'text-zinc-900 dark:text-zinc-100'"
          >{{ row.model }}</span>
        </template>
        <template v-for="action in ACTIONS" :key="action" #[`cell-${action}`]="{ row }">
          <span class="tabular-nums" :class="row.actions[action] ? '' : 'text-zinc-400 dark:text-zinc-600'">{{ count(row.actions[action] || 0) }}</span>
        </template>
        <template #cell-total="{ row }"><span class="tabular-nums font-medium">{{ count(row.total) }}</span></template>
        <template #cell-last_bucket="{ row }">
          <span :title="`Counted in ${period / 60}-minute buckets`"><TimeAgo :at="lastWritten(row)" /></span>
        </template>
      </DataTable>
    </section>

    <section v-if="props.model" class="lk-card" aria-live="polite">
      <header class="lk-card-header">
        <h2 class="font-mono text-sm font-semibold">{{ props.model }}</h2>
        <span v-if="picked" class="text-xs text-zinc-500 dark:text-zinc-400">{{ count(picked.total) }} writes in {{ windowLabel }}</span>
        <RouterLink
          :to="{ name: 'models', query: rangeQuery(route.query) }"
          class="ml-auto text-xs text-zinc-500 hover:text-indigo-600 dark:text-zinc-400 dark:hover:text-indigo-400"
        >Close</RouterLink>
      </header>
      <ErrorBox :error="changesError" />
      <p v-if="!loadingChanges && !changes.length && picked" class="px-4 pt-3 text-xs text-zinc-500 dark:text-zinc-400">
        Its writes were counted, but none of the batches that made them was kept. Outside the local environment Lookout keeps
        only failed, slow and monitored batches.
      </p>
      <DataTable
        :columns="changeColumns"
        :rows="changes"
        :row-key="(e) => e.id"
        :to="(e) => itemRoute({ kind: madeBy(e).kind, id: e.batch_id })"
        :loading="loadingChanges && !changes.length"
        empty="No recorded changes."
        label="changes"
      >
        <template #cell-action="{ row }">
          <Pill :tone="TONE[row.content && row.content.action] || 'neutral'">{{ row.content && row.content.action }}</Pill>
        </template>
        <template #cell-keys="{ row }">
          <span v-if="keysOf(row)" class="block max-w-48 truncate font-mono text-xs" :title="keysOf(row).join(', ')">
            {{ keysOf(row).length === 1 ? `#${keysOf(row)[0]}` : `${keysOf(row).length} rows` }}
          </span>
          <span v-else class="text-xs text-zinc-500 italic dark:text-zinc-400">all matching</span>
        </template>
        <template #cell-by="{ row }">
          <span class="flex min-w-0 items-center gap-1.5">
            <Pill>{{ KIND[madeBy(row).kind] || madeBy(row).kind || '—' }}</Pill>
            <span class="truncate font-mono text-[12.5px] text-zinc-900 dark:text-zinc-100">{{ madeBy(row).summary || row.batch_id }}</span>
          </span>
        </template>
        <template #cell-trace="{ row }">
          <RouterLink
            v-if="madeBy(row).trace_id"
            :to="traceRoute(madeBy(row).trace_id)"
            class="font-mono text-xs text-indigo-600 hover:underline dark:text-indigo-400"
            :title="`Every request and job in trace ${madeBy(row).trace_id}`"
            @click.stop
          >{{ madeBy(row).trace_id.slice(0, 8) }}…</RouterLink>
        </template>
        <template #cell-occurred_at="{ row }"><TimeAgo :at="row.occurred_at" /></template>
      </DataTable>
      <LoadMore v-if="next" :loading="loadingChanges" @more="loadChanges(true)" />
    </section>
  </div>
</template>
