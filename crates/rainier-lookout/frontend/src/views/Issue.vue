<script setup>
import { computed, onMounted, ref, watch } from 'vue'
import { useRoute } from 'vue-router'

import DataTable from '../components/DataTable.vue'
import DurationHistogram from '../components/DurationHistogram.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import LineChart from '../components/LineChart.vue'
import LoadMore from '../components/LoadMore.vue'
import Pill from '../components/Pill.vue'
import StackTrace from '../components/StackTrace.vue'
import StatTile from '../components/StatTile.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { caseId, count, duration, percentile, TYPES } from '../format.js'

const props = defineProps({ type: { type: String, required: true }, id: { type: String, default: '' } })
const route = useRoute()
// The family's key: from `?key=` (the earlier links), or looked up from the
// short id in the path.
const resolved = ref('')
const resolving = ref(false)
const key = computed(() => (typeof route.query.key === 'string' && route.query.key) || resolved.value)

const RANGES = [
  { key: '1h', label: '1h', minutes: 60 },
  { key: '6h', label: '6h', minutes: 360 },
  { key: '24h', label: '24h', minutes: 1440 },
  { key: '7d', label: '7d', minutes: 7 * 1440 },
]
const range = ref('24h')
const minutes = computed(() => (RANGES.find((r) => r.key === range.value) || RANGES[2]).minutes)

const series = ref(null)
const seriesError = ref(null)
const seriesLoading = ref(false)

const entries = ref([])
const next = ref(null)
const loading = ref(true)
const more = ref(false)
const error = ref(null)

const kase = ref(null)
const caseKey = computed(() => (props.type === 'exception' && key.value ? caseId(key.value) : null))

async function loadSeries() {
  if (!key.value) return
  seriesLoading.value = true
  try {
    const since = new Date(Date.now() - minutes.value * 60_000).toISOString()
    series.value = await api.series(props.type, { since, key: key.value, forecast: 6 })
    seriesError.value = null
  } catch (e) {
    series.value = null
    seriesError.value = e
  } finally {
    seriesLoading.value = false
  }
}

async function loadEntries() {
  if (!key.value) {
    loading.value = false
    return
  }
  try {
    const page = await api.entries(props.type, { key: key.value, limit: 200 })
    entries.value = page.data
    next.value = page.next
    error.value = null
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}

async function loadMore() {
  if (!next.value) return
  more.value = true
  try {
    const page = await api.entries(props.type, { key: key.value, limit: 200, before: next.value.before })
    const seen = new Set(entries.value.map((e) => `${e.batch_id}/${e.id}`))
    entries.value = entries.value.concat(page.data.filter((e) => !seen.has(`${e.batch_id}/${e.id}`)))
    next.value = page.next
  } catch (e) {
    error.value = e
  } finally {
    more.value = false
  }
}

async function loadCase() {
  if (!caseKey.value) return
  try {
    const answer = await api.kase(caseKey.value)
    kase.value = answer.data
  } catch {
    // No case (a 404) or none yet: the page stands without one.
    kase.value = null
  }
}

async function resolveKey() {
  if (!props.id || typeof route.query.key === 'string') return
  resolving.value = true
  try {
    resolved.value = (await api.family(props.type, props.id)).data.key
  } catch (e) {
    error.value = e
  } finally {
    resolving.value = false
  }
}

onMounted(async () => {
  await resolveKey()
  loadSeries()
  loadEntries()
  loadCase()
})
watch(range, loadSeries)

const points = computed(() => (series.value && Array.isArray(series.value.data) ? series.value.data : []))
const forecast = computed(() => (series.value && Array.isArray(series.value.forecast) ? series.value.forecast : []))

const totals = computed(() => {
  let n = 0
  let sum = 0
  let worst = 0
  for (const p of points.value) {
    n += p.count
    sum += p.sum_us
    worst = Math.max(worst, p.max_us)
  }
  return { n, sum, worst, avg: n ? Math.floor(sum / n) : null }
})

const timed = computed(() => entries.value.filter((e) => typeof e.duration_us === 'number'))
const durations = computed(() => timed.value.map((e) => e.duration_us))
const hasTimes = computed(() => durations.value.length > 0 || totals.value.sum > 0)

// Two stacked charts over one x range — never two y-scales on one chart.
const occurrencesChart = computed(() => {
  const data = points.value
  const out = [{ label: 'Occurrences', color: 'indigo', points: data.map((p) => ({ t: p.bucket_start, v: p.count })) }]
  const last = data[data.length - 1]
  if (forecast.value.length && last) {
    out.push({
      label: 'Forecast',
      color: 'indigo',
      dashed: true,
      points: [{ t: last.bucket_start, v: last.count }].concat(forecast.value.map((f) => ({ t: f.bucket_start, v: f.count }))),
    })
  }
  return out
})
const averageChart = computed(() => {
  const busy = points.value.filter((p) => p.count > 0)
  const out = [{ label: 'Average duration', color: 'amber', points: busy.map((p) => ({ t: p.bucket_start, v: p.average_us })) }]
  const lastBusy = busy[busy.length - 1]
  if (forecast.value.length && lastBusy) {
    out.push({
      label: 'Forecast',
      color: 'amber',
      dashed: true,
      points: [{ t: lastBusy.bucket_start, v: lastBusy.average_us }].concat(
        forecast.value.map((f) => ({ t: f.bucket_start, v: f.average_us })),
      ),
    })
  }
  return out
})
/** The x range both charts share: the first bucket to the last forecast one. */
const xDomain = computed(() => {
  const data = points.value
  if (!data.length) return null
  const end = forecast.value.length ? forecast.value[forecast.value.length - 1].bucket_start : data[data.length - 1].bucket_start
  return data[0].bucket_start === end ? null : [data[0].bucket_start, end]
})

const latest = computed(() => entries.value[0] || null)
const exception = computed(() => {
  const e = latest.value
  if (!e || props.type !== 'exception') return null
  const c = e.content && typeof e.content === 'object' ? e.content : {}
  const ex = c.exception && typeof c.exception === 'object' ? c.exception : {}
  return {
    entry: e,
    kind: typeof c.kind_label === 'string' ? c.kind_label : null,
    message: typeof ex.message === 'string' ? ex.message : e.summary,
    sources: Array.isArray(ex.sources) ? ex.sources.map(String) : [],
    frames: Array.isArray(c.frames) ? c.frames : [],
    backtrace: typeof ex.backtrace === 'string' ? ex.backtrace : '',
  }
})
const calledFrom = computed(() => {
  if (props.type === 'exception') return null
  const e = entries.value.find((x) => x.content && Array.isArray(x.content['code.trace']))
  return e ? { entry: e, frames: e.content['code.trace'] } : null
})

const bucket = (s) => (s >= 3600 ? `${s / 3600}h` : s >= 60 ? `${s / 60}m` : `${s}s`)

const columns = [
  { key: 'summary', label: 'Summary', sortable: true, class: 'font-mono text-[12.5px]' },
  { key: 'time', label: 'When', sortable: true, value: (e) => e.occurred_at },
  { key: 'duration', label: 'Duration', sortable: true, align: 'right', value: (e) => e.duration_us },
  { key: 'tags', label: 'Tags', value: (e) => (e.tags || []).join(' ') },
]
</script>

<template>
  <div class="space-y-5">
    <RouterLink
      :to="{ name: 'entries', params: { type } }"
      class="text-sm text-zinc-500 hover:text-indigo-600 dark:text-zinc-400 dark:hover:text-indigo-400"
    >← {{ TYPES[type] || type }}</RouterLink>

    <template v-if="!key">
      <ErrorBox v-if="error" :error="error" />
      <p v-else-if="resolving" class="text-sm text-zinc-500 dark:text-zinc-400">Loading…</p>
      <template v-else>
        <h1 class="text-xl font-semibold">No family chosen</h1>
        <p class="text-sm text-zinc-500 dark:text-zinc-400">This page needs a family id in its path.</p>
      </template>
    </template>

    <template v-else>
      <div class="flex flex-wrap items-start gap-3">
        <div class="min-w-0 flex-1">
          <div class="flex flex-wrap items-center gap-2">
            <Pill tone="accent">{{ type }}</Pill>
            <Pill v-if="kase" :tone="kase.status === 'open' ? 'bad' : 'neutral'">case {{ kase.status }}</Pill>
          </div>
          <h1 class="mt-2 font-mono text-base font-semibold break-all sm:text-lg">{{ key }}</h1>
        </div>
        <RouterLink v-if="kase" :to="{ name: 'case', params: { id: kase.id } }" class="lk-btn">Case →</RouterLink>
      </div>

      <div class="flex flex-wrap items-center gap-2" role="group" aria-label="Time range">
        <button
          v-for="r in RANGES"
          :key="r.key"
          type="button"
          class="lk-btn"
          :class="range === r.key ? 'border-indigo-500 bg-indigo-50 text-indigo-700 dark:border-indigo-400 dark:bg-indigo-500/10 dark:text-indigo-300' : ''"
          :aria-pressed="range === r.key"
          @click="range = r.key"
        >{{ r.label }}</button>
        <span v-if="seriesLoading" class="text-xs text-zinc-500 dark:text-zinc-400">Loading…</span>
      </div>

      <div class="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-7">
        <StatTile label="Occurrences" :value="series ? count(totals.n) : '—'" :hint="`last ${range}`" :tone="type === 'exception' && totals.n ? 'bad' : 'neutral'" />
        <StatTile label="Per minute" :value="series ? (totals.n / minutes).toFixed(totals.n / minutes >= 10 ? 0 : 2) : '—'" :hint="`over ${range}`" />
        <StatTile label="Average" :value="series && totals.avg !== null && hasTimes ? duration(totals.avg) : '—'" />
        <StatTile label="Worst" :value="series && hasTimes && totals.n ? duration(totals.worst) : '—'" :tone="totals.worst > 1_000_000 ? 'warn' : 'neutral'" />
        <StatTile label="p50" :value="duration(percentile(durations, 50))" :hint="durations.length ? `of ${durations.length} loaded` : 'no durations'" />
        <StatTile label="p95" :value="duration(percentile(durations, 95))" :hint="durations.length ? `of ${durations.length} loaded` : ''" />
        <StatTile label="p99" :value="duration(percentile(durations, 99))" :hint="durations.length ? `of ${durations.length} loaded` : ''" />
      </div>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Over time</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">
            <template v-if="series && series.period">{{ bucket(series.period) }} buckets</template>
            <template v-if="forecast.length"> · dashed is the forecast</template>
          </span>
        </header>
        <div class="p-4" :class="seriesLoading && series ? 'opacity-60 transition-opacity' : ''">
          <ErrorBox v-if="seriesError" :error="seriesError" />
          <Empty v-else-if="!series">{{ seriesLoading ? 'Loading…' : 'No series.' }}</Empty>
          <div v-else class="space-y-5">
            <div>
              <h3 class="lk-th mb-1.5">Occurrences per bucket</h3>
              <LineChart :series="occurrencesChart" :x-domain="xDomain" :axis-width="56" :format="(v) => count(Math.round(v))" :height="150" />
            </div>
            <div v-if="hasTimes">
              <h3 class="lk-th mb-1.5">Average duration</h3>
              <LineChart :series="averageChart" :x-domain="xDomain" :axis-width="56" :format="(v) => duration(Math.round(v))" :height="130" />
            </div>
          </div>
        </div>
      </section>

      <section v-if="durations.length > 1" class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Durations</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">{{ durations.length }} loaded occurrences, log scale</span>
        </header>
        <div class="p-4"><DurationHistogram :values="durations" /></div>
      </section>

      <section v-if="exception" class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Latest occurrence</h2>
          <Pill v-if="exception.kind" tone="bad">{{ exception.kind }}</Pill>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400"><TimeAgo :at="exception.entry.occurred_at" /></span>
        </header>
        <div class="space-y-4 p-4">
          <p class="font-mono text-[13px] break-words whitespace-pre-wrap">{{ exception.message }}</p>
          <div v-if="exception.sources.length">
            <h3 class="lk-th">Caused by</h3>
            <ol class="mt-1 space-y-1">
              <li v-for="(s, i) in exception.sources" :key="i" class="font-mono text-xs break-words text-zinc-600 dark:text-zinc-400">↳ {{ s }}</li>
            </ol>
          </div>
          <div>
            <h3 class="lk-th mb-1.5">Stack</h3>
            <StackTrace :frames="exception.frames" :backtrace="exception.backtrace" />
          </div>
          <RouterLink
            :to="{ name: 'batch', params: { id: exception.entry.batch_id } }"
            class="inline-block text-sm text-indigo-600 hover:underline dark:text-indigo-400"
          >Open its batch →</RouterLink>
        </div>
      </section>

      <section v-if="calledFrom" class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Called from</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">a slow occurrence, <TimeAgo :at="calledFrom.entry.occurred_at" /></span>
        </header>
        <div class="p-4"><StackTrace :frames="calledFrom.frames" /></div>
      </section>

      <ErrorBox :error="error" />

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Occurrences</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">{{ entries.length }} loaded, newest first</span>
        </header>
        <DataTable
          :columns="columns"
          :rows="entries"
          label="occurrences"
          :row-key="(e) => `${e.batch_id}/${e.id}`"
          :to="(e) => ({ name: 'batch', params: { id: e.batch_id } })"
          filterable
          :initial-sort="{ key: 'time', dir: 'desc' }"
          empty="No occurrences kept."
          :loading="loading"
        >
          <template #cell-time="{ row }"><TimeAgo :at="row.occurred_at" /></template>
          <template #cell-duration="{ row }"><span class="tabular-nums">{{ duration(row.duration_us) }}</span></template>
          <template #cell-tags="{ row }">
            <span class="inline-flex flex-wrap gap-1"><Pill v-for="tag in row.tags" :key="tag">{{ tag }}</Pill></span>
          </template>
        </DataTable>
        <LoadMore v-if="next" :loading="more" @more="loadMore" />
      </section>
    </template>
  </div>
</template>
