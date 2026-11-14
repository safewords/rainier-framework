<script setup>
import { computed, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import DataTable from '../components/DataTable.vue'
import ErrorBox from '../components/ErrorBox.vue'
import LineChart from '../components/LineChart.vue'
import Pill from '../components/Pill.vue'
import Sparkline from '../components/Sparkline.vue'
import StatTile from '../components/StatTile.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { config } from '../config.js'
import { count, field, hasTag, issueRoute, percent, percentile, RANGES, rate, since as sinceHours, time } from '../format.js'
import { usePolling } from '../live.js'

const route = useRoute()
const router = useRouter()
const range = RANGES.find((r) => r.key === route.query.range) || RANGES[2]
const minutes = range.hours * 60

// Publishing a job is recorded as a job entry too (`dispatch:<name>`); its
// time is the enqueue, not the work, so it stays out unless asked for.
const dispatches = ref(route.query.dispatches === '1')
const isDispatch = (key) => typeof key === 'string' && key.startsWith('dispatch:')

const aggregates = ref([])
const series = ref(null)
const entries = ref([])
const loading = ref(true)
const error = ref(null)

async function load() {
  const since = sinceHours(range.hours)
  try {
    const [rows, points, recent] = await Promise.all([
      api.aggregates('job', { period: range.period, since, order: 'count', limit: 100 }),
      api.series('job', { since, period: range.period, keys: 50, forecast: config.insights.enabled ? undefined : 0 }),
      api.entries('job', { limit: 100 }),
    ])
    aggregates.value = rows.data || []
    series.value = points
    entries.value = recent.data || []
    error.value = null
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}
onMounted(load)
usePolling(load, 10_000)

function toggleDispatches() {
  dispatches.value = !dispatches.value
  router.replace({ query: { ...route.query, dispatches: dispatches.value ? '1' : undefined } })
}

// --- Per job.

const jobs = computed(() => aggregates.value.filter((r) => dispatches.value || !isDispatch(r.key)))
const sumOf = computed(() => {
  let runs = 0
  let spent = 0
  let worst = 0
  for (const r of jobs.value) {
    runs += r.count
    spent += r.sum_us
    worst = Math.max(worst, r.max_us)
  }
  return { runs, spent, worst, average: runs ? spent / runs : null }
})

const trend = (key) => {
  const own = series.value && series.value.keys ? series.value.keys[key] : null
  return own ? own.map((p) => p.count) : null
}
const avg = (r) => (r.count ? r.sum_us / r.count : 0)
const share = (r) => (sumOf.value.spent ? (r.sum_us * 100) / sumOf.value.spent : 0)

const columns = [
  { key: 'key', label: 'Job', sortable: true, value: (r) => r.key, text: (r) => r.key, class: 'font-mono text-[12.5px]' },
  { key: 'count', label: 'Runs', title: `How many times it ran in the last ${range.key}`, align: 'right', sortable: true, value: (r) => r.count, text: (r) => count(r.count) },
  { key: 'rate', label: 'Runs/min', title: `Runs per minute, averaged over the last ${range.key}`, align: 'right', sortable: true, value: (r) => r.count / minutes, text: (r) => rate(r.count / minutes) },
  { key: 'average', label: 'Avg', title: 'Average time per run', align: 'right', sortable: true, value: avg, text: (r) => time(avg(r)) },
  { key: 'max', label: 'Max', title: 'The single slowest run', align: 'right', sortable: true, value: (r) => r.max_us, text: (r) => time(r.max_us) },
  { key: 'total', label: 'Total time', title: 'Runs × average: all the time spent in this job', align: 'right', sortable: true, value: (r) => r.sum_us, text: (r) => time(r.sum_us) },
  { key: 'share', label: 'Share', title: 'Its part of the time spent in every job shown', sortable: true, value: share, text: (r) => percent(share(r)) },
  { key: 'trend', label: 'Trend', title: `Runs per bucket over the last ${range.key} (the 50 busiest jobs)`, text: () => '' },
]

// --- Chart: runs (left) and average time (right), with the forecast.

function chartParts() {
  const s = series.value
  const data = (s && s.data) || []
  const tail = data[data.length - 1]
  const forecast = (s && s.forecast) || []
  const join = (pick) => (tail && forecast.length ? [{ t: tail.bucket_start, v: pick(tail) }] : [])
  return { data, forecast, join }
}
const runsChart = computed(() => {
  const { data, forecast, join } = chartParts()
  return [
    { label: 'Runs', color: 'amber', points: data.map((p) => ({ t: p.bucket_start, v: p.count })) },
    { label: 'Forecast', color: 'amber', dashed: true, points: join((p) => p.count).concat(forecast.map((p) => ({ t: p.bucket_start, v: p.count }))) },
  ]
})
const timeChart = computed(() => {
  const { data, forecast, join } = chartParts()
  return [
    { label: 'Avg time', color: 'indigo', points: data.filter((p) => p.count > 0).map((p) => ({ t: p.bucket_start, v: p.average_us })) },
    { label: 'Forecast', color: 'indigo', dashed: true, points: join((p) => p.average_us).concat(forecast.map((p) => ({ t: p.bucket_start, v: p.average_us }))) },
  ]
})
const bucketLabel = computed(() => {
  const p = (series.value && series.value.period) || range.period
  return p >= 3600 ? `${p / 3600} h` : `${p / 60} min`
})

// --- Recent runs, from the raw entries.

const jobName = (e) => String(field(e.content, 'rainier.job.name') || e.family_hash || e.summary)
const outcome = (e) => field(e.content, 'rainier.job.outcome', 'outcome') || (hasTag(e.tags, 'failed') ? 'failed' : null)
function outcomeTone(o) {
  const v = String(o || '').toLowerCase()
  if (['failed', 'error', 'timed_out', 'panicked'].includes(v)) return 'bad'
  if (['retried', 'released', 'retry'].includes(v)) return 'warn'
  return v ? 'ok' : 'neutral'
}
const runs = computed(() =>
  entries.value.filter((e) => typeof e.duration_us === 'number' && (dispatches.value || !isDispatch(e.family_hash))),
)
const recentStats = computed(() => {
  const d = runs.value.map((e) => e.duration_us)
  return { n: d.length, p50: percentile(d, 50), p95: percentile(d, 95), p99: percentile(d, 99), failed: runs.value.filter((e) => outcomeTone(outcome(e)) === 'bad').length }
})
const runColumns = [
  { key: 'job', label: 'Job', sortable: true, value: jobName, text: jobName, class: 'font-mono text-[12.5px]' },
  { key: 'outcome', label: 'Outcome', sortable: true, value: outcome, text: outcome },
  { key: 'duration', label: 'Duration', title: 'How long this run took', align: 'right', sortable: true, value: (e) => e.duration_us, text: (e) => time(e.duration_us) },
  { key: 'when', label: 'When', title: 'When this run happened', align: 'right', sortable: true, value: (e) => e.occurred_at, text: (e) => e.occurred_at },
]
</script>

<template>
  <div class="space-y-6">
    <div class="flex flex-wrap items-end justify-between gap-3">
      <div>
        <h1 class="text-xl font-semibold tracking-tight">Job timing</h1>
        <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">How often each queued job ran, how long it took, and where the time went.</p>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <label class="flex items-center gap-2 text-sm text-zinc-600 dark:text-zinc-300">
          <input type="checkbox" class="size-4 accent-indigo-600" :checked="dispatches" @change="toggleDispatches" /> Include dispatches
        </label>
        <div class="inline-flex overflow-hidden rounded-md border border-zinc-300 dark:border-zinc-700" role="group" aria-label="Period">
          <button
            v-for="r in RANGES"
            :key="r.key"
            type="button"
            class="px-3 py-1.5 text-sm font-medium transition not-first:border-l not-first:border-zinc-300 dark:not-first:border-zinc-700"
            :class="r.key === range.key
              ? 'bg-indigo-50 text-indigo-700 dark:bg-indigo-500/15 dark:text-indigo-300'
              : 'bg-white text-zinc-600 hover:bg-zinc-50 dark:bg-zinc-900 dark:text-zinc-300 dark:hover:bg-zinc-800'"
            :aria-pressed="r.key === range.key"
            @click="router.replace({ query: { ...route.query, range: r.key } })"
          >{{ r.key }}</button>
        </div>
      </div>
    </div>

    <ErrorBox :error="error" />

    <div class="grid grid-cols-2 gap-3 sm:grid-cols-3 xl:grid-cols-5">
      <StatTile label="Jobs run" :value="count(sumOf.runs)" :hint="`in the last ${range.key}`" :loading="loading" />
      <StatTile label="Throughput" :value="`${rate(sumOf.runs / minutes)}/min`" hint="runs per minute, averaged" :loading="loading" />
      <StatTile label="Avg run" :value="time(sumOf.average)" hint="mean time per run" :loading="loading" />
      <StatTile label="Worst run" :value="time(sumOf.runs ? sumOf.worst : null)" hint="the single slowest" :loading="loading" />
      <StatTile label="Time spent" :value="time(sumOf.spent)" hint="all runs together" :loading="loading" />
    </div>

    <section class="lk-card min-w-0">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">Runs and average time</h2>
        <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">
          per {{ bucketLabel }} · dashed: forecast<template v-if="!dispatches"> · includes dispatches</template>
        </span>
      </header>
      <div class="space-y-3 p-4">
        <div>
          <h3 class="lk-th mb-1">Runs</h3>
          <LineChart :series="runsChart" :format="(v) => count(v)" :height="140" />
        </div>
        <div>
          <h3 class="lk-th mb-1">Average time per run</h3>
          <LineChart :series="timeChart" :format="time" :height="140" />
        </div>
      </div>
    </section>

    <section class="lk-card">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">Every job</h2>
        <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">the 100 busiest in the last {{ range.key }} · opens the job's issue page</span>
      </header>
      <DataTable
        :columns="columns"
        :rows="jobs"
        :row-key="(r) => r.key"
        :to="(r) => issueRoute('job', r.key)"
        :initial-sort="{ key: 'total', dir: 'desc' }"
        filterable
        filter-placeholder="Filter jobs…"
        :loading="loading"
        empty="No jobs ran in this window."
        label="Jobs"
      >
        <template #cell-share="{ row }">
          <span class="flex items-center gap-2">
            <span class="h-1.5 w-16 overflow-hidden rounded-full bg-zinc-100 dark:bg-zinc-800" aria-hidden="true">
              <span class="block h-full rounded-full bg-indigo-500" :style="{ width: `${Math.max(1, share(row))}%` }" />
            </span>
            <span class="text-xs tabular-nums">{{ percent(share(row)) }}</span>
          </span>
        </template>
        <template #cell-trend="{ row }">
          <Sparkline v-if="trend(row.key)" :points="trend(row.key)" color="amber" />
          <span v-else class="text-xs text-zinc-400" title="Only the 50 busiest jobs carry a trend">—</span>
        </template>
      </DataTable>
    </section>

    <section class="space-y-3">
      <div class="flex flex-wrap items-baseline gap-2">
        <h2 class="text-base font-semibold">Slowest recent runs</h2>
        <span class="text-xs text-zinc-500 dark:text-zinc-400">percentiles from the {{ recentStats.n }} newest runs kept, whatever the range</span>
      </div>
      <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <StatTile label="p50" :value="time(recentStats.p50)" hint="half the runs were faster" :loading="loading" />
        <StatTile label="p95" :value="time(recentStats.p95)" hint="1 in 20 was slower" :loading="loading" />
        <StatTile label="p99" :value="time(recentStats.p99)" hint="1 in 100 was slower" :loading="loading" />
        <StatTile label="Failed" :value="count(recentStats.failed)" :tone="recentStats.failed ? 'bad' : 'neutral'" :hint="`of ${recentStats.n} runs`" :loading="loading" />
      </div>
      <div class="lk-card">
        <DataTable
          :columns="runColumns"
          :rows="runs"
          :row-key="(e) => `${e.batch_id}/${e.id}`"
          :to="(e) => ({ name: 'batch', params: { id: e.batch_id } })"
          :initial-sort="{ key: 'duration', dir: 'desc' }"
          filterable
          filter-placeholder="Filter runs…"
          :loading="loading"
          empty="No job runs recorded."
          label="Recent job runs"
        >
          <template #cell-outcome="{ row }">
            <Pill v-if="outcome(row)" :tone="outcomeTone(outcome(row))">{{ outcome(row) }}</Pill>
            <span v-else class="text-zinc-400">—</span>
          </template>
          <template #cell-when="{ row }"><span class="text-zinc-500 dark:text-zinc-400"><TimeAgo :at="row.occurred_at" /></span></template>
        </DataTable>
      </div>
    </section>
  </div>
</template>
