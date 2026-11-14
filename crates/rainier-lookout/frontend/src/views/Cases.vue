<script setup>
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import DataTable from '../components/DataTable.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Pill from '../components/Pill.vue'
import SeverityPill from '../components/SeverityPill.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { count } from '../format.js'
import { usePolling } from '../live.js'

const LIMIT = 200
const STATUSES = [
  { key: 'open', label: 'Open' },
  { key: 'resolved', label: 'Resolved' },
  { key: 'ignored', label: 'Ignored' },
  { key: 'all', label: 'All' },
]
const SEVERITIES = ['low', 'medium', 'high', 'critical']
const RANK = { low: 0, medium: 1, high: 2, critical: 3 }

const status = ref('open')
const severity = ref('')
const q = ref('')
const search = ref('')

// Every case matching the severity and search, whatever its status: the tab
// counts come from it, and the tab filters it.
const all = ref([])
const loading = ref(true)
const error = ref(null)
const capped = computed(() => all.value.length >= LIMIT)

let generation = 0
async function load() {
  const mine = ++generation
  loading.value = true
  try {
    const page = await api.cases({ status: 'all', severity: severity.value, q: search.value, limit: LIMIT })
    if (mine !== generation) return
    all.value = page.data
    error.value = null
  } catch (e) {
    if (mine === generation) error.value = e
  } finally {
    if (mine === generation) loading.value = false
  }
}

let typing = null
watch(q, (value) => {
  clearTimeout(typing)
  typing = setTimeout(() => (search.value = value.trim()), 300)
})
onBeforeUnmount(() => clearTimeout(typing))
watch([severity, search], load)
onMounted(load)
usePolling(load, 15000)

const counts = computed(() => {
  const out = { open: 0, resolved: 0, ignored: 0, all: all.value.length }
  for (const c of all.value) if (out[c.status] !== undefined) out[c.status] += 1
  return out
})
const rows = computed(() => (status.value === 'all' ? all.value : all.value.filter((c) => c.status === status.value)))

// Bulk selection, kept to the rows still on screen.
const selected = ref(new Set())
watch(rows, (list) => {
  const visible = new Set(list.map((c) => c.id))
  const kept = new Set([...selected.value].filter((id) => visible.has(id)))
  if (kept.size !== selected.value.size) selected.value = kept
})
function toggle(id) {
  const next = new Set(selected.value)
  if (next.has(id)) next.delete(id)
  else next.add(id)
  selected.value = next
}
const allSelected = computed(() => rows.value.length > 0 && rows.value.every((c) => selected.value.has(c.id)))
function toggleAll() {
  selected.value = allSelected.value ? new Set() : new Set(rows.value.map((c) => c.id))
}

const acting = ref(false)
const actionError = ref(null)
const notice = ref('')
async function act(action) {
  const ids = [...selected.value]
  if (!ids.length) return
  acting.value = true
  actionError.value = null
  notice.value = ''
  let done = 0
  for (const id of ids) {
    try {
      const answer = await api.actOnCase(id, { action })
      const updated = answer && answer.data
      if (updated) all.value = all.value.map((c) => (c.id === updated.id ? updated : c))
      done += 1
    } catch (e) {
      actionError.value = e
    }
  }
  acting.value = false
  selected.value = new Set()
  notice.value = `${action === 'resolve' ? 'Resolved' : 'Ignored'} ${done} of ${ids.length}.`
}

const columns = [
  { key: 'select', label: '' },
  { key: 'severity', label: 'Severity', sortable: true, value: (c) => RANK[c.severity] ?? 0, text: (c) => c.severity },
  { key: 'title', label: 'Case', sortable: true, primary: true, text: (c) => `${c.title} ${c.failure_kind || ''} ${c.key}` },
  { key: 'occurrences', label: 'Seen', sortable: true, align: 'right', title: 'Occurrences' },
  { key: 'users', label: 'Users', sortable: true, align: 'right', value: (c) => (c.users || []).length, title: 'Affected users' },
  { key: 'first_seen', label: 'First seen', sortable: true },
  { key: 'last_seen', label: 'Last seen', sortable: true },
  { key: 'sources', label: 'Where', value: (c) => (c.sources || []).join(' ') },
]
</script>

<template>
  <div class="space-y-5">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">Cases</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">Every distinct failure, grouped by fingerprint: resolve one and it comes back as a regression if it happens again.</p>
    </div>

    <div class="flex flex-wrap items-center gap-2">
      <div class="flex flex-wrap gap-1" role="tablist" aria-label="Status">
        <button
          v-for="s in STATUSES"
          :key="s.key"
          type="button"
          role="tab"
          class="lk-btn"
          :class="status === s.key ? 'border-indigo-500 bg-indigo-50 text-indigo-700 dark:border-indigo-400 dark:bg-indigo-500/10 dark:text-indigo-300' : ''"
          :aria-selected="status === s.key"
          @click="status = s.key"
        >
          {{ s.label }}
          <span class="text-xs text-zinc-500 tabular-nums dark:text-zinc-400">{{ counts[s.key] }}{{ capped ? '+' : '' }}</span>
        </button>
      </div>
      <select v-model="severity" class="lk-input w-auto" aria-label="Minimum severity">
        <option value="">Any severity</option>
        <option v-for="s in SEVERITIES" :key="s" :value="s">{{ s }} and up</option>
      </select>
      <input v-model="q" type="search" class="lk-input min-w-0 flex-1 sm:max-w-xs" placeholder="Search cases…" aria-label="Search cases" />
    </div>

    <ErrorBox :error="error" />
    <ErrorBox :error="actionError" />

    <section class="lk-card">
      <header class="lk-card-header">
        <label class="inline-flex items-center gap-2 text-sm">
          <input type="checkbox" class="size-4 accent-indigo-600" :checked="allSelected" :disabled="!rows.length" @change="toggleAll" />
          <span>{{ selected.size ? `${selected.size} selected` : 'Select all' }}</span>
        </label>
        <button type="button" class="lk-btn" :disabled="!selected.size || acting" @click="act('resolve')">Resolve selected</button>
        <button type="button" class="lk-btn" :disabled="!selected.size || acting" @click="act('ignore')">Ignore selected</button>
        <span v-if="notice" class="text-xs text-zinc-500 dark:text-zinc-400" role="status">{{ notice }}</span>
        <span v-if="capped" class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">showing the first {{ LIMIT }} — narrow with a search</span>
      </header>
      <DataTable
        :columns="columns"
        :rows="rows"
        :row-key="(c) => c.id"
        :to="(c) => ({ name: 'case', params: { id: c.id } })"
        :initial-sort="{ key: 'last_seen', dir: 'desc' }"
        :empty="status === 'open' ? 'No open cases.' : 'No cases here.'"
        :loading="loading"
        label="cases"
      >
        <template #cell-select="{ row }">
          <label class="inline-flex items-center gap-2 text-xs text-zinc-600 dark:text-zinc-400">
            <input
              type="checkbox"
              class="size-4 accent-indigo-600"
              :checked="selected.has(row.id)"
              :aria-label="`Select ${row.title}`"
              @click.stop
              @change="toggle(row.id)"
            />
            <span class="md:hidden" aria-hidden="true">Select</span>
          </label>
        </template>
        <template #cell-severity="{ row }"><SeverityPill :severity="row.severity" /></template>
        <template #cell-title="{ row }">
          <span class="block truncate font-mono text-[12.5px] text-zinc-900 dark:text-zinc-100">{{ row.title }}</span>
          <span class="mt-0.5 flex min-w-0 items-center gap-1.5 text-xs text-zinc-500 dark:text-zinc-400">
            <span v-if="row.failure_kind" class="truncate">{{ row.failure_kind }}</span>
            <Pill v-if="row.regressions" tone="warn">regressed ×{{ row.regressions }}</Pill>
            <Pill v-if="status === 'all' && row.status !== 'open'">{{ row.status }}</Pill>
          </span>
        </template>
        <template #cell-occurrences="{ row }">{{ count(row.occurrences) }}</template>
        <template #cell-users="{ row }">{{ (row.users || []).length }}</template>
        <template #cell-first_seen="{ row }"><TimeAgo v-if="row.first_seen" :at="row.first_seen" /></template>
        <template #cell-last_seen="{ row }"><TimeAgo v-if="row.last_seen" :at="row.last_seen" /></template>
        <template #cell-sources="{ row }">
          <span class="block max-w-40 truncate font-mono text-xs text-zinc-600 dark:text-zinc-400" :title="(row.sources || []).join('\n')">
            {{ (row.sources || [])[0] || '—' }}<template v-if="(row.sources || []).length > 1"> +{{ row.sources.length - 1 }}</template>
          </span>
        </template>
      </DataTable>
    </section>
  </div>
</template>
