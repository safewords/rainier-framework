<script setup>
import { computed, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import BatchList from '../components/BatchList.vue'
import DataTable from '../components/DataTable.vue'
import ErrorBox from '../components/ErrorBox.vue'
import LineChart from '../components/LineChart.vue'
import Sparkline from '../components/Sparkline.vue'
import StatTile from '../components/StatTile.vue'
import { api } from '../api.js'
import { config } from '../config.js'
import { count, histogramPercentile, issueRoute, mergeHistograms, percent, RANGES, rate, since as sinceHours, time } from '../format.js'
import { usePolling } from '../live.js'

// One page per measured kind of work: every distinct job class, or every
// route, with its timing over the whole range — percentiles included, read
// off the histograms the aggregates keep — and the latest batches below.
const props = defineProps({
  type: { type: String, required: true },
})

const KINDS = {
  job: {
    title: 'Jobs',
    blurb: 'Every job class: how often it ran, how long its runs took — percentiles over every run in the range — and where the time went.',
    unit: 'Runs',
    units: 'runs',
    thing: 'Job',
    things: 'job classes',
    batchKind: 'job',
  },
  request: {
    title: 'Requests',
    blurb: 'Every route: how often it was hit, how long its responses took — percentiles over every request in the range — and where the time went.',
    unit: 'Requests',
    units: 'requests',
    thing: 'Route',
    things: 'routes',
    batchKind: 'request',
  },
}
const kind = computed(() => KINDS[props.type] || KINDS.request)

const route = useRoute()
const router = useRouter()
const range = computed(() => RANGES.find((r) => r.key === route.query.range) || RANGES[2])
const minutes = computed(() => range.value.hours * 60)

// Publishing a job is recorded as a job entry too (`dispatch:<name>`); its
// time is the enqueue, not the work, so it stays out unless asked for.
const dispatches = computed(() => route.query.dispatches === '1')
const isDispatch = (key) => typeof key === 'string' && key.startsWith('dispatch:')

const aggregates = ref([])
const series = ref(null)
const batches = ref([])
const loading = ref(true)
const error = ref(null)

async function load() {
  const since = sinceHours(range.value.hours)
  const type = props.type
  try {
    const [rows, points, recent] = await Promise.all([
      api.aggregates(type, { period: range.value.period, since, order: 'count', limit: 100 }),
      api.series(type, { since, period: range.value.period, keys: 50, forecast: config.insights.enabled ? undefined : 0 }),
      api.batches({ kind: kind.value.batchKind, limit: 50 }),
    ])
    if (type !== props.type) return
    aggregates.value = rows.data || []
    series.value = points
    batches.value = recent.data || []
    error.value = null
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}
watch([() => props.type, () => range.value.key], () => {
  loading.value = true
  aggregates.value = []
  series.value = null
  batches.value = []
  load()
}, { immediate: true })
usePolling(load, 10_000)

function setQuery(patch) {
  router.replace({ query: { ...route.query, ...patch } })
}

// --- Every class, and the totals over them.

const rows = computed(() => aggregates.value.filter((r) => props.type !== 'job' || dispatches.value || !isDispatch(r.key)))
const totals = computed(() => {
  let n = 0
  let spent = 0
  let worst = 0
  for (const r of rows.value) {
    n += r.count
    spent += r.sum_us
    worst = Math.max(worst, r.max_us)
  }
  const histogram = mergeHistograms(rows.value.map((r) => r.histogram))
  const p = (q) => histogramPercentile(histogram, q, worst)
  return { n, spent, worst, average: n ? spent / n : null, p50: p(50), p90: p(90), p95: p(95), p99: p(99) }
})

const trend = (key) => {
  const own = series.value && series.value.keys ? series.value.keys[key] : null
  return own ? own.map((p) => p.count) : null
}
const avg = (r) => (r.count ? r.sum_us / r.count : 0)
const share = (r) => (totals.value.spent ? (r.sum_us * 100) / totals.value.spent : 0)
const pct = (name, hint) => ({
  key: name,
  label: name,
  title: `${hint} (over every one in the last ${range.value.key}; to within ~9%)`,
  align: 'right',
  sortable: true,
  value: (r) => r[`${name}_us`] ?? -1,
  text: (r) => time(r[`${name}_us`]),
})

const columns = computed(() => [
  { key: 'key', label: kind.value.thing, sortable: true, value: (r) => r.key, text: (r) => r.key, class: 'font-mono text-[12.5px]' },
  { key: 'count', label: kind.value.unit, title: `How many in the last ${range.value.key}`, align: 'right', sortable: true, value: (r) => r.count, text: (r) => count(r.count) },
  { key: 'rate', label: 'Per min', title: `Per minute, averaged over the last ${range.value.key}`, align: 'right', sortable: true, value: (r) => r.count / minutes.value, text: (r) => rate(r.count / minutes.value) },
  { key: 'average', label: 'Avg', title: 'Mean time', align: 'right', sortable: true, value: avg, text: (r) => time(avg(r)) },
  pct('p50', 'Half took less'),
  pct('p90', '1 in 10 took longer'),
  pct('p95', '1 in 20 took longer'),
  pct('p99', '1 in 100 took longer'),
  { key: 'max', label: 'Max', title: 'The single slowest', align: 'right', sortable: true, value: (r) => r.max_us, text: (r) => time(r.max_us) },
  { key: 'total', label: 'Total time', title: 'Count × average: all the time spent here', align: 'right', sortable: true, value: (r) => r.sum_us, text: (r) => time(r.sum_us) },
  { key: 'share', label: 'Share', title: 'Its part of all the time spent in what is shown', sortable: true, value: share, text: (r) => percent(share(r)) },
  { key: 'trend', label: 'Trend', title: `Count per bucket over the last ${range.value.key} (the 50 busiest)`, text: () => '' },
])

// --- Chart: count and average time, with the forecast.

function chartParts() {
  const s = series.value
  const data = (s && s.data) || []
  const tail = data[data.length - 1]
  const forecast = (s && s.forecast) || []
  const join = (pick) => (tail && forecast.length ? [{ t: tail.bucket_start, v: pick(tail) }] : [])
  return { data, forecast, join }
}
const countChart = computed(() => {
  const { data, forecast, join } = chartParts()
  return [
    { label: kind.value.unit, color: 'amber', points: data.map((p) => ({ t: p.bucket_start, v: p.count })) },
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
  const p = (series.value && series.value.period) || range.value.period
  return p >= 3600 ? `${p / 3600} h` : `${p / 60} min`
})
</script>

<template>
  <div class="space-y-6">
    <div class="flex flex-wrap items-end justify-between gap-3">
      <div>
        <h1 class="text-xl font-semibold tracking-tight">{{ kind.title }}</h1>
        <p class="mt-1 max-w-3xl text-sm text-zinc-500 dark:text-zinc-400">{{ kind.blurb }}</p>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <label v-if="type === 'job'" class="flex items-center gap-2 text-sm text-zinc-600 dark:text-zinc-300">
          <input type="checkbox" class="size-4 accent-indigo-600" :checked="dispatches" @change="setQuery({ dispatches: dispatches ? undefined : '1' })" /> Include dispatches
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
            @click="setQuery({ range: r.key })"
          >{{ r.key }}</button>
        </div>
      </div>
    </div>

    <ErrorBox :error="error" />

    <section class="space-y-3" aria-label="Over everything">
      <div class="grid grid-cols-2 gap-3 sm:grid-cols-3 xl:grid-cols-5">
        <StatTile :label="kind.unit" :value="count(totals.n)" :hint="`in the last ${range.key}`" :loading="loading" />
        <StatTile label="Throughput" :value="`${rate(totals.n / minutes)}/min`" :hint="`${kind.units} per minute, averaged`" :loading="loading" />
        <StatTile label="Avg" :value="time(totals.average)" hint="mean time" :loading="loading" />
        <StatTile label="Max" :value="time(totals.n ? totals.worst : null)" hint="the single slowest" :loading="loading" />
        <StatTile label="Time spent" :value="time(totals.spent)" :hint="`all ${kind.units} together`" :loading="loading" />
      </div>
      <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <StatTile label="p50" :value="time(totals.p50)" :hint="`half the ${kind.units} took less`" :loading="loading" />
        <StatTile label="p90" :value="time(totals.p90)" hint="1 in 10 took longer" :loading="loading" />
        <StatTile label="p95" :value="time(totals.p95)" hint="1 in 20 took longer" :loading="loading" />
        <StatTile label="p99" :value="time(totals.p99)" hint="1 in 100 took longer" :loading="loading" />
      </div>
    </section>

    <section class="lk-card min-w-0">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">{{ kind.unit }} and average time</h2>
        <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">
          per {{ bucketLabel }} · dashed: forecast<template v-if="type === 'job' && !dispatches"> · includes dispatches</template>
        </span>
      </header>
      <div class="space-y-3 p-4">
        <div>
          <h3 class="lk-th mb-1">{{ kind.unit }}</h3>
          <LineChart :series="countChart" :format="(v) => count(v)" :height="140" />
        </div>
        <div>
          <h3 class="lk-th mb-1">Average time</h3>
          <LineChart :series="timeChart" :format="time" :height="140" />
        </div>
      </div>
    </section>

    <section class="lk-card">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">Every {{ kind.thing.toLowerCase() }}</h2>
        <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">the 100 busiest {{ kind.things }} in the last {{ range.key }} · opens its issue page</span>
      </header>
      <DataTable
        :columns="columns"
        :rows="rows"
        :row-key="(r) => r.key"
        :to="(r) => issueRoute(type, r.key)"
        :initial-sort="{ key: 'total', dir: 'desc' }"
        filterable
        :filter-placeholder="`Filter ${kind.things}…`"
        :loading="loading"
        :empty="`Nothing in the last ${range.key}.`"
        :label="kind.title"
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
          <span v-else class="text-xs text-zinc-400" title="Only the 50 busiest carry a trend">—</span>
        </template>
      </DataTable>
    </section>

    <section class="space-y-3">
      <div class="flex flex-wrap items-baseline gap-2">
        <h2 class="text-base font-semibold">Latest {{ kind.units }}</h2>
        <router-link
          :to="{ name: 'batches', query: { kind: kind.batchKind } }"
          class="ml-auto text-sm text-indigo-600 hover:underline dark:text-indigo-400"
        >All {{ kind.units }}, with filters →</router-link>
      </div>
      <div class="lk-card">
        <BatchList :batches="batches" :loading="loading" :label="`Latest ${kind.units}`" :empty="`No ${kind.units} recorded.`" />
      </div>
    </section>
  </div>
</template>
