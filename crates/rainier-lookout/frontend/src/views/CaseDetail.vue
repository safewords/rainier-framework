<script setup>
import { computed, onMounted, ref } from 'vue'

import DataTable from '../components/DataTable.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import LineChart from '../components/LineChart.vue'
import LoadMore from '../components/LoadMore.vue'
import Pill from '../components/Pill.vue'
import SeverityPill from '../components/SeverityPill.vue'
import StackTrace from '../components/StackTrace.vue'
import StatTile from '../components/StatTile.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { absolute, ago, count, duration } from '../format.js'

const props = defineProps({ id: { type: String, required: true } })

const kase = ref(null)
const error = ref(null)
const loading = ref(true)

const series = ref(null)
const seriesError = ref(null)

const entries = ref([])
const next = ref(null)
const entriesLoading = ref(true)
const more = ref(false)
const entriesError = ref(null)

const acting = ref(false)
const actionError = ref(null)
// The action waiting for its optional note: 'resolve' or 'ignore'.
const pending = ref(null)
const note = ref('')

const SEVERITIES = ['low', 'medium', 'high', 'critical']
const STATUS_TONE = { open: 'bad', resolved: 'ok', ignored: 'neutral' }

async function loadSeries(key) {
  try {
    const since = new Date(Date.now() - 7 * 86_400_000).toISOString()
    series.value = await api.series('exception', { since, key })
    seriesError.value = null
  } catch (e) {
    seriesError.value = e
  }
}

async function loadEntries(key) {
  try {
    const page = await api.entries('exception', { key, limit: 200 })
    entries.value = page.data
    next.value = page.next
    entriesError.value = null
  } catch (e) {
    entriesError.value = e
  } finally {
    entriesLoading.value = false
  }
}

async function loadMore() {
  if (!next.value || !kase.value) return
  more.value = true
  try {
    const page = await api.entries('exception', { key: kase.value.key, limit: 200, before: next.value.before })
    const seen = new Set(entries.value.map((e) => `${e.batch_id}/${e.id}`))
    entries.value = entries.value.concat(page.data.filter((e) => !seen.has(`${e.batch_id}/${e.id}`)))
    next.value = page.next
  } catch (e) {
    entriesError.value = e
  } finally {
    more.value = false
  }
}

onMounted(async () => {
  try {
    const answer = await api.kase(props.id)
    kase.value = answer.data
    loadSeries(kase.value.key)
    loadEntries(kase.value.key)
  } catch (e) {
    error.value = e
    entriesLoading.value = false
  } finally {
    loading.value = false
  }
})

function ask(action) {
  pending.value = action
  note.value = ''
}

async function confirmPending() {
  const action = pending.value
  const text = note.value.trim() || undefined
  pending.value = null
  await act(action, {}, text)
}

async function act(action, extra = {}, note) {
  acting.value = true
  actionError.value = null
  try {
    const answer = await api.actOnCase(props.id, { action, note, ...extra })
    if (answer && answer.data) kase.value = answer.data
  } catch (e) {
    actionError.value = e
  } finally {
    acting.value = false
  }
}

function setSeverity(event) {
  const severity = event.target.value
  if (!kase.value || severity === kase.value.severity) return
  act('severity', { severity })
}

const chart = computed(() => {
  const data = series.value && Array.isArray(series.value.data) ? series.value.data : []
  return [{ label: 'Occurrences', color: 'rose', points: data.map((p) => ({ t: p.bucket_start, v: p.count })) }]
})

const HISTORY = {
  opened: ['Opened', 'bad'],
  escalated: ['Escalated', 'bad'],
  resolved: ['Resolved', 'ok'],
  reopened: ['Reopened', 'warn'],
  regressed: ['Regressed', 'bad'],
  ignored: ['Ignored', 'neutral'],
  severity: ['Severity set', 'accent'],
}
const history = computed(() => (kase.value && Array.isArray(kase.value.history) ? kase.value.history.slice().reverse() : []))

const columns = [
  { key: 'summary', label: 'Occurrence', sortable: true, class: 'font-mono text-[12.5px]' },
  { key: 'time', label: 'When', sortable: true, value: (e) => e.occurred_at, text: (e) => ago(e.occurred_at) },
  { key: 'duration', label: 'Duration', sortable: true, align: 'right', value: (e) => e.duration_us },
  { key: 'batch', label: 'Batch', value: (e) => e.batch_id },
]
</script>

<template>
  <div class="space-y-5">
    <RouterLink :to="{ name: 'cases' }" class="text-sm text-zinc-500 hover:text-indigo-600 dark:text-zinc-400 dark:hover:text-indigo-400">← Cases</RouterLink>

    <template v-if="error">
      <h1 class="text-xl font-semibold">{{ error.status === 404 ? 'No such case' : 'Could not load this case' }}</h1>
      <p v-if="error.status === 404" class="text-sm text-zinc-500 dark:text-zinc-400">It may have been pruned with the batches it came from.</p>
      <ErrorBox v-else :error="error" />
    </template>

    <Empty v-else-if="loading">Loading…</Empty>

    <template v-else-if="kase">
      <div class="space-y-3">
        <div class="flex flex-wrap items-center gap-2">
          <SeverityPill :severity="kase.severity" />
          <Pill :tone="STATUS_TONE[kase.status] || 'neutral'">{{ kase.status }}</Pill>
          <Pill v-if="kase.failure_kind">{{ kase.failure_kind }}</Pill>
          <Pill v-if="kase.regressions" tone="warn">regressed ×{{ kase.regressions }}</Pill>
        </div>
        <h1 class="font-mono text-lg font-semibold break-words sm:text-xl">{{ kase.title }}</h1>
        <p class="font-mono text-xs break-all text-zinc-500 dark:text-zinc-400">{{ kase.key }}</p>
        <div class="flex flex-wrap items-center gap-2">
          <button v-if="kase.status !== 'resolved'" type="button" class="lk-btn" :disabled="acting" @click="ask('resolve')">Resolve</button>
          <button v-if="kase.status !== 'open'" type="button" class="lk-btn" :disabled="acting" @click="act('reopen')">Reopen</button>
          <button v-if="kase.status !== 'ignored'" type="button" class="lk-btn" :disabled="acting" @click="ask('ignore')">Ignore</button>
          <label class="inline-flex items-center gap-2 text-sm text-zinc-600 dark:text-zinc-400">
            Severity
            <select class="lk-input w-auto capitalize" :value="kase.severity" :disabled="acting" @change="setSeverity">
              <option v-for="s in SEVERITIES" :key="s" :value="s">{{ s }}</option>
            </select>
          </label>
          <span v-if="kase.severity_pinned" class="text-xs text-zinc-500 dark:text-zinc-400">pinned</span>
          <RouterLink
            :to="{ name: 'issue', params: { type: 'exception' }, query: { key: kase.key } }"
            class="ml-auto text-sm text-indigo-600 hover:underline dark:text-indigo-400"
          >Issue drill-down →</RouterLink>
        </div>
        <div v-if="pending" class="lk-card flex flex-col gap-2 p-3 sm:flex-row sm:items-center">
          <label class="flex-1 text-sm">
            <span class="sr-only">Note</span>
            <input
              v-model="note"
              class="lk-input w-full"
              maxlength="2000"
              :placeholder="pending === 'resolve' ? 'What fixed it? (optional — a commit, a PR)' : 'Why ignore it? (optional)'"
              @keydown.enter="confirmPending"
              @keydown.esc="pending = null"
            />
          </label>
          <div class="flex gap-2">
            <button type="button" class="lk-btn bg-indigo-600 text-white hover:bg-indigo-500" :disabled="acting" @click="confirmPending">
              {{ pending === 'resolve' ? 'Resolve case' : 'Ignore case' }}
            </button>
            <button type="button" class="lk-btn" @click="pending = null">Cancel</button>
          </div>
        </div>
        <ErrorBox :error="actionError" />
      </div>

      <div class="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
        <StatTile label="Occurrences" :value="count(kase.occurrences)" tone="bad" />
        <StatTile label="Affected users" :value="count((kase.users || []).length)" :hint="(kase.users || []).length ? 'most recent kept' : 'none signed in'" />
        <StatTile label="Regressions" :value="count(kase.regressions || 0)" :tone="kase.regressions ? 'warn' : 'neutral'" />
        <StatTile label="First seen" :value="kase.first_seen ? ago(kase.first_seen) : '—'" :hint="kase.first_seen ? absolute(kase.first_seen) : ''" />
        <StatTile label="Last seen" :value="kase.last_seen ? ago(kase.last_seen) : '—'" :hint="kase.last_seen ? absolute(kase.last_seen) : ''" />
      </div>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Occurrences over 7 days</h2>
          <span v-if="series && series.period" class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">{{ series.period >= 3600 ? `${series.period / 3600}h` : `${series.period / 60}m` }} buckets</span>
        </header>
        <div class="p-4">
          <ErrorBox v-if="seriesError" :error="seriesError" />
          <Empty v-else-if="!series">Loading…</Empty>
          <LineChart v-else :series="chart" :format="(v) => count(Math.round(v))" :height="160" />
        </div>
      </section>

      <section class="lk-card">
        <header class="lk-card-header"><h2 class="text-sm font-semibold">Where</h2></header>
        <div class="p-4"><StackTrace :frames="kase.frames || []" /></div>
      </section>

      <section class="lk-card">
        <header class="lk-card-header"><h2 class="text-sm font-semibold">Affected</h2></header>
        <div class="grid gap-6 p-4 sm:grid-cols-2">
          <div class="min-w-0">
            <h3 class="lk-th">Routes and jobs</h3>
            <ul v-if="(kase.sources || []).length" class="mt-1.5 space-y-1">
              <li v-for="s in kase.sources" :key="s" class="font-mono text-[12.5px] break-all">{{ s }}</li>
            </ul>
            <p v-else class="mt-1.5 text-sm text-zinc-500 dark:text-zinc-400">—</p>
          </div>
          <div class="min-w-0">
            <h3 class="lk-th">Users</h3>
            <ul v-if="(kase.users || []).length" class="mt-1.5 flex flex-wrap gap-1">
              <li v-for="u in kase.users" :key="u"><Pill>user {{ u }}</Pill></li>
            </ul>
            <p v-else class="mt-1.5 text-sm text-zinc-500 dark:text-zinc-400">No signed-in user was affected.</p>
          </div>
        </div>
      </section>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Traces</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">every occurrence kept — open one for its full trace</span>
        </header>
        <div v-if="(kase.recent_batches || []).length" class="flex flex-wrap items-center gap-2 border-b border-zinc-200 px-4 py-2.5 text-xs dark:border-zinc-800">
          <span class="lk-th">Recent batches</span>
          <RouterLink
            v-for="b in kase.recent_batches"
            :key="b"
            :to="{ name: 'batch', params: { id: b } }"
            class="rounded font-mono text-indigo-600 hover:underline dark:text-indigo-400"
          >{{ b.slice(0, 8) }}</RouterLink>
          <RouterLink
            v-if="kase.first_batch_id"
            :to="{ name: 'batch', params: { id: kase.first_batch_id } }"
            class="ml-auto text-indigo-600 hover:underline dark:text-indigo-400"
          >first occurrence →</RouterLink>
        </div>
        <ErrorBox v-if="entriesError" :error="entriesError" class="m-4" />
        <DataTable
          :columns="columns"
          :rows="entries"
          :row-key="(e) => `${e.batch_id}/${e.id}`"
          :to="(e) => ({ name: 'batch', params: { id: e.batch_id } })"
          filterable
          :initial-sort="{ key: 'time', dir: 'desc' }"
          empty="No occurrences kept — they may have been pruned."
          :loading="entriesLoading"
          label="occurrences"
        >
          <template #cell-time="{ row }"><TimeAgo :at="row.occurred_at" /></template>
          <template #cell-duration="{ row }">{{ duration(row.duration_us) }}</template>
          <template #cell-batch="{ row }"><span class="font-mono text-xs text-zinc-500 dark:text-zinc-400" :title="row.batch_id">{{ row.batch_id.slice(0, 8) }}</span></template>
        </DataTable>
        <LoadMore v-if="next" :loading="more" @more="loadMore" />
      </section>

      <section class="lk-card">
        <header class="lk-card-header"><h2 class="text-sm font-semibold">History</h2></header>
        <ol v-if="history.length" class="space-y-3 p-4">
          <li v-for="(h, i) in history" :key="i" class="flex gap-3">
            <span
              class="mt-1.5 size-2 shrink-0 rounded-full"
              :class="{
                bad: 'bg-rose-500', ok: 'bg-emerald-500', warn: 'bg-amber-500', accent: 'bg-indigo-500', neutral: 'bg-zinc-400',
              }[(HISTORY[h.action] || [])[1] || 'neutral']"
              aria-hidden="true"
            />
            <div class="min-w-0 flex-1">
              <div class="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
                <span class="font-semibold">{{ (HISTORY[h.action] || [h.action])[0] }}</span>
                <SeverityPill v-if="h.severity" :severity="h.severity" />
                <span class="text-xs text-zinc-500 dark:text-zinc-400"><TimeAgo :at="h.at" /></span>
              </div>
              <p v-if="h.note" class="mt-0.5 text-sm break-words whitespace-pre-wrap text-zinc-600 dark:text-zinc-400">{{ h.note }}</p>
            </div>
          </li>
        </ol>
        <Empty v-else>No history.</Empty>
      </section>
    </template>
  </div>
</template>
