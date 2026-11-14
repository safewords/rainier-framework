<script setup>
import { computed, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import BatchList from '../components/BatchList.vue'
import DataTable from '../components/DataTable.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Pill from '../components/Pill.vue'
import SeverityPill from '../components/SeverityPill.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { count, familyKey, issueRoute, time, TYPES } from '../format.js'

const ENTRY_TYPES = ['exception', 'query', 'job', 'http_client']

const route = useRoute()
const router = useRouter()
const q = computed(() => (typeof route.query.q === 'string' ? route.query.q.trim() : ''))
const text = ref(q.value)

const batches = ref([])
const entries = ref(Object.fromEntries(ENTRY_TYPES.map((t) => [t, []])))
const cases = ref([])
const failures = ref({})
const loading = ref(false)

function submit() {
  const value = text.value.trim()
  router.replace({ name: 'search', query: value ? { q: value } : {} })
}

async function attempt(name, promise) {
  try {
    return await promise
  } catch (e) {
    failures.value = { ...failures.value, [name]: e }
    return null
  }
}

async function load() {
  if (!q.value) return
  loading.value = true
  failures.value = {}
  const [b, c, ...e] = await Promise.all([
    attempt('batches', api.batches({ q: q.value, limit: 50 })),
    attempt('cases', api.cases({ q: q.value, limit: 20 })),
    ...ENTRY_TYPES.map((t) => attempt(t, api.entries(t, { q: q.value, limit: 20 }))),
  ])
  batches.value = (b && b.data) || []
  cases.value = (c && c.data) || []
  entries.value = Object.fromEntries(ENTRY_TYPES.map((t, i) => [t, (e[i] && e[i].data) || []]))
  loading.value = false
}
onMounted(load)

const total = computed(
  () => batches.value.length + cases.value.length + ENTRY_TYPES.reduce((n, t) => n + entries.value[t].length, 0),
)

const entryColumns = (type) => [
  { key: 'summary', label: type === 'exception' ? 'Message' : type === 'query' ? 'SQL' : type === 'job' ? 'Job' : 'Request', sortable: true, value: (e) => e.summary, text: (e) => e.summary, class: 'font-mono text-[12.5px]' },
  ...(type === 'exception'
    ? []
    : [{ key: 'duration', label: 'Duration', title: 'How long this one took', align: 'right', sortable: true, value: (e) => e.duration_us, text: (e) => time(e.duration_us) }]),
  { key: 'batch', label: 'Batch', title: 'The request, job or command it happened in', text: (e) => e.batch_id },
  { key: 'when', label: 'When', title: 'When it was recorded', align: 'right', sortable: true, value: (e) => e.occurred_at, text: (e) => e.occurred_at },
]

const STATUS_TONE = { open: 'bad', resolved: 'ok', ignored: 'neutral' }
const caseColumns = [
  { key: 'title', label: 'Case', sortable: true, value: (c) => c.title, text: (c) => `${c.title} ${c.failure_kind || ''}`, class: 'font-mono text-[12.5px]' },
  { key: 'severity', label: 'Severity', sortable: true, value: (c) => ['low', 'medium', 'high', 'critical'].indexOf(c.severity), text: (c) => c.severity },
  { key: 'status', label: 'Status', sortable: true, value: (c) => c.status, text: (c) => c.status },
  { key: 'occurrences', label: 'Count', title: 'How many times it happened', align: 'right', sortable: true, value: (c) => c.occurrences, text: (c) => count(c.occurrences) },
  { key: 'last', label: 'Last occurrence', title: 'When it last happened', align: 'right', sortable: true, value: (c) => c.last_seen, text: (c) => c.last_seen },
]
</script>

<template>
  <div class="space-y-6">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">Search</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">Batches, entries and cases whose summary or fingerprint contains the text.</p>
    </div>

    <form class="flex gap-2" role="search" @submit.prevent="submit">
      <input v-model="text" type="search" class="lk-input min-w-0 flex-1 sm:max-w-md" placeholder="Search Lookout…" aria-label="Search Lookout" />
      <button type="submit" class="lk-btn">Search</button>
    </form>

    <div v-if="!q" class="lk-card"><Empty>Type something to search for.</Empty></div>

    <template v-else>
      <p class="text-sm text-zinc-500 dark:text-zinc-400" aria-live="polite">
        <template v-if="loading">Searching for “{{ q }}”…</template>
        <template v-else>{{ total }} result{{ total === 1 ? '' : 's' }} for “{{ q }}”.</template>
      </p>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Cases</h2>
          <span class="text-xs text-zinc-500 dark:text-zinc-400">{{ cases.length }}</span>
          <RouterLink :to="{ name: 'cases' }" class="ml-auto text-xs font-medium text-indigo-600 hover:underline dark:text-indigo-400">All cases →</RouterLink>
        </header>
        <div v-if="failures.cases" class="px-4 pt-3"><ErrorBox :error="failures.cases" /></div>
        <DataTable
          :columns="caseColumns"
          :rows="cases"
          :row-key="(c) => c.id"
          :to="(c) => ({ name: 'case', params: { id: c.id } })"
          :loading="loading"
          empty="No cases match."
          label="Matching cases"
        >
          <template #cell-severity="{ row }"><SeverityPill :severity="row.severity" /></template>
          <template #cell-status="{ row }"><Pill :tone="STATUS_TONE[row.status] || 'neutral'">{{ row.status }}</Pill></template>
          <template #cell-last="{ row }"><span class="text-zinc-500 dark:text-zinc-400"><TimeAgo :at="row.last_seen" /></span></template>
        </DataTable>
      </section>

      <section v-for="type in ENTRY_TYPES" :key="type" class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">{{ TYPES[type] }}</h2>
          <span class="text-xs text-zinc-500 dark:text-zinc-400">{{ entries[type].length }} · opens the issue</span>
          <RouterLink :to="{ name: 'entries', params: { type } }" class="ml-auto text-xs font-medium text-indigo-600 hover:underline dark:text-indigo-400">All {{ TYPES[type].toLowerCase() }} →</RouterLink>
        </header>
        <div v-if="failures[type]" class="px-4 pt-3"><ErrorBox :error="failures[type]" /></div>
        <DataTable
          :columns="entryColumns(type)"
          :rows="entries[type]"
          :row-key="(e) => `${e.batch_id}/${e.id}`"
          :to="(e) => issueRoute(type, familyKey(e))"
          :loading="loading"
          :empty="`No ${TYPES[type].toLowerCase()} match.`"
          :label="`Matching ${TYPES[type].toLowerCase()}`"
        >
          <template #cell-batch="{ row }">
            <RouterLink
              :to="{ name: 'batch', params: { id: row.batch_id } }"
              class="font-mono text-xs text-indigo-600 hover:underline dark:text-indigo-400"
            >{{ String(row.batch_id).slice(0, 8) }}</RouterLink>
          </template>
          <template #cell-when="{ row }"><span class="text-zinc-500 dark:text-zinc-400"><TimeAgo :at="row.occurred_at" /></span></template>
        </DataTable>
      </section>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Batches</h2>
          <span class="text-xs text-zinc-500 dark:text-zinc-400">{{ batches.length }}</span>
          <RouterLink :to="{ name: 'batches', query: { kind: 'request', q } }" class="ml-auto text-xs font-medium text-indigo-600 hover:underline dark:text-indigo-400">Search in batches →</RouterLink>
        </header>
        <div v-if="failures.batches" class="px-4 pt-3"><ErrorBox :error="failures.batches" /></div>
        <BatchList :batches="batches" :loading="loading" empty="No batches match." label="Matching batches" />
      </section>
    </template>
  </div>
</template>
