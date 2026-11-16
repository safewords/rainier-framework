<script setup>
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import { useRoute } from 'vue-router'

import AggregateCard from '../components/AggregateCard.vue'
import BatchList from '../components/BatchList.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import LineChart from '../components/LineChart.vue'
import Pill from '../components/Pill.vue'
import Sparkline from '../components/Sparkline.vue'
import StatTile from '../components/StatTile.vue'
import { api } from '../api.js'
import { config } from '../config.js'
import { count, issueRoute, percent, rate, time, totals, TYPES } from '../format.js'
import { mergePulse, usePolling } from '../live.js'
import { followsNow, parseRange } from '../range.js'

const CARDS = [
  { id: 'routes', title: 'Slowest routes', type: 'request', order: 'max', hint: 'by worst time', keyLabel: 'Route' },
  { id: 'queries', title: 'Slowest queries', type: 'query', order: 'max', hint: 'by worst time', keyLabel: 'Query' },
  { id: 'exceptions', title: 'Most frequent exceptions', type: 'exception', order: 'count', hint: 'by count', timed: false, keyLabel: 'Exception' },
  { id: 'jobs', title: 'Slowest jobs', type: 'job', order: 'max', hint: 'by worst time', keyLabel: 'Job' },
  { id: 'http', title: 'Slowest outgoing requests', type: 'http_client', order: 'max', hint: 'by worst time', keyLabel: 'Request' },
]

const route = useRoute()
const range = computed(() => parseRange(route.query))
const minutes = computed(() => range.value.ms / 60_000)
const windowHint = computed(() => (range.value.preset ? `in the last ${range.value.label}` : range.value.label))

// --- Insight thresholds: the server's, unless this browser has its own.

const SETTINGS_KEY = 'lookout.insights'
const defaults = { window: config.insights.window, min: config.insights.min, change: config.insights.change, forecast: config.insights.forecast }
function storedSettings() {
  try {
    const saved = JSON.parse(window.localStorage.getItem(SETTINGS_KEY) || 'null')
    if (!saved || typeof saved !== 'object') return { ...defaults }
    const out = { ...defaults }
    for (const k of Object.keys(defaults)) if (Number.isFinite(saved[k]) && saved[k] > 0) out[k] = Math.floor(saved[k])
    return out
  } catch {
    return { ...defaults }
  }
}
const settings = reactive(storedSettings())
const draft = reactive({ ...settings })
const customised = computed(() => Object.keys(defaults).some((k) => settings[k] !== defaults[k]))
const settingsOpen = ref(false)
const settingsRoot = ref(null)

function openSettings() {
  Object.assign(draft, settings)
  settingsOpen.value = !settingsOpen.value
}
function saveSettings() {
  for (const k of Object.keys(defaults)) {
    const n = Math.floor(Number(draft[k]))
    settings[k] = Number.isFinite(n) && n > 0 ? n : defaults[k]
  }
  try {
    window.localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings))
  } catch {
    // Storage refused: the thresholds still apply until the page reloads.
  }
  settingsOpen.value = false
  load()
}
function resetSettings() {
  Object.assign(draft, defaults)
  Object.assign(settings, defaults)
  try {
    window.localStorage.removeItem(SETTINGS_KEY)
  } catch {
    // Nothing stored to remove.
  }
  settingsOpen.value = false
  load()
}
function onDocument(event) {
  if (!settingsOpen.value) return
  if (event.type === 'keydown') {
    if (event.key === 'Escape') settingsOpen.value = false
    return
  }
  if (settingsRoot.value && !settingsRoot.value.contains(event.target)) settingsOpen.value = false
}
onMounted(() => {
  document.addEventListener('mousedown', onDocument)
  document.addEventListener('keydown', onDocument)
})
onBeforeUnmount(() => {
  document.removeEventListener('mousedown', onDocument)
  document.removeEventListener('keydown', onDocument)
})

// --- Data.

const rows = reactive({})
const failures = ref([])
const series = reactive({ request: null, exception: null, job: null })
const insights = ref([])
const insightsEnabled = ref(config.insights.enabled)
const insightsError = ref(null)
const loading = ref(true)
const error = ref(null)

async function loadInsights() {
  try {
    const answer = await api.insights({ ...settings, limit: 20 })
    insights.value = (answer && answer.data) || []
    insightsEnabled.value = !answer || answer.enabled !== false
    insightsError.value = null
  } catch (e) {
    insightsError.value = e
  }
}

async function load() {
  const since = range.value.since
  const until = range.value.until || undefined
  const forecast = config.insights.enabled && followsNow(range.value) ? settings.forecast : 0
  const seriesParams = { since, until, period: range.value.period, forecast }
  loadInsights()
  try {
    const [aggregates, failed, requests, exceptions, jobs] = await Promise.all([
      Promise.all(CARDS.map((card) => api.aggregates(card.type, { period: range.value.period, since, until, order: card.order, limit: 8 }))),
      api.batches({ exception: true, limit: 8 }),
      api.series('request', seriesParams),
      api.series('exception', seriesParams),
      api.series('job', seriesParams),
    ])
    CARDS.forEach((card, i) => (rows[card.id] = aggregates[i].data))
    failures.value = failed.data
    series.request = requests
    series.exception = exceptions
    series.job = jobs
    error.value = null
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}

onMounted(load)
watch(() => [range.value.key, range.value.since, range.value.until], () => load())
// The totals and charts move with every pulse, in place; the cards, the
// failures and the insights reload at most every five seconds.
usePolling(load, 10_000, {
  on: 'both',
  onPulse: (pulse) => {
    if (!followsNow(range.value)) return
    mergePulse(series.request, pulse, 'request', range.value.period)
    mergePulse(series.exception, pulse, 'exception', range.value.period)
    mergePulse(series.job, pulse, 'job', range.value.period)
  },
})

// --- Headline numbers.

const sum = (s) => totals(s ? s.data : [])
const requestTotals = computed(() => sum(series.request))
const exceptionTotals = computed(() => sum(series.exception))
const jobTotals = computed(() => sum(series.job))
const counts = (s) => (s ? s.data.map((p) => p.count) : [])

// --- Charts. A forecast starts at the last real point so the line joins.

const perMinute = (s) => (s && s.period ? 60 / s.period : 1)
function actual(s, pick, skipEmpty = false) {
  if (!s) return []
  return s.data.filter((p) => !skipEmpty || p.count > 0).map((p) => ({ t: p.bucket_start, v: pick(p) }))
}
function projected(s, pick, last) {
  if (!s || !s.forecast || !s.forecast.length) return []
  const points = s.forecast.map((p) => ({ t: p.bucket_start, v: pick(p) }))
  const tail = s.data[s.data.length - 1]
  return tail && last ? [{ t: tail.bucket_start, v: last(tail) }, ...points] : points
}

// Throughput and time share one chart: requests per minute on the left
// axis; on the right, the mean and each bucket's p90, p95 and p99, read off
// that bucket's histogram (a bucket recorded before histograms were kept has
// none, so those lines start where the data does).
const percentileLine = (s, label, color, field) => ({
  label,
  color,
  axis: 'right',
  points: s ? s.data.filter((p) => p.count > 0 && p[field] != null).map((p) => ({ t: p.bucket_start, v: p[field] })) : [],
})
const requestChart = computed(() => {
  const s = series.request
  const f = perMinute(s)
  return [
    { label: 'Requests / min', color: 'indigo', points: actual(s, (p) => p.count * f) },
    { label: 'Throughput forecast', color: 'indigo', dashed: true, points: projected(s, (p) => p.count * f, (p) => p.count * f) },
    { label: 'Avg request time', color: 'emerald', axis: 'right', points: actual(s, (p) => p.average_us, true) },
    percentileLine(s, 'p90', 'zinc', 'p90_us'),
    percentileLine(s, 'p95', 'amber', 'p95_us'),
    percentileLine(s, 'p99', 'rose', 'p99_us'),
    { label: 'Time forecast', color: 'emerald', axis: 'right', dashed: true, points: projected(s, (p) => p.average_us, (p) => p.average_us) },
  ]
})
const exceptionChart = computed(() => {
  const s = series.exception
  return [
    { label: 'Exceptions', color: 'rose', points: actual(s, (p) => p.count) },
    { label: 'Forecast', color: 'rose', dashed: true, points: projected(s, (p) => p.count, (p) => p.count) },
  ]
})
const jobChart = computed(() => {
  const s = series.job
  return [
    { label: 'Jobs run', color: 'amber', points: actual(s, (p) => p.count) },
    { label: 'Forecast', color: 'amber', dashed: true, points: projected(s, (p) => p.count, (p) => p.count) },
  ]
})
const jobTimeChart = computed(() => {
  const s = series.job
  return [
    { label: 'Avg job time', color: 'indigo', points: actual(s, (p) => p.average_us, true) },
    { label: 'Forecast', color: 'indigo', dashed: true, points: projected(s, (p) => p.average_us, (p) => p.average_us) },
  ]
})
const bucketLabel = computed(() => {
  const p = (series.request && series.request.period) || range.value.period
  return p >= 3600 ? `${p / 3600} h` : `${p / 60} min`
})
const whole = (v) => count(v)
const perMin = (v) => rate(v)

// --- Insights as sentences.

const SINGULAR = {
  request: 'route',
  query: 'query',
  exception: 'exception',
  job: 'job',
  http_client: 'outgoing request',
  cache: 'cache',
  command: 'command',
  scheduled_task: 'scheduled task',
}

function shorten(text, n = 90) {
  return text.length > n ? `${text.slice(0, n - 1)}…` : text
}

/** What an insight is about, in words. */
function subject(i) {
  if (!i.key) return (TYPES[i.type] || i.type).toLowerCase()
  if (i.type === 'exception') {
    const parts = i.key.split('|')
    if (parts.length >= 3) return shorten(`${parts[1]}: ${parts[2]}`)
  }
  const noun = SINGULAR[i.type]
  return shorten(noun && i.type !== 'exception' ? `${noun} ${i.key}` : i.key)
}

const timedKind = (i) => i.kind === 'slower' || i.kind === 'faster'
const fmt = (i, v) => (timedKind(i) ? time(v) : count(v))

function sentence(i) {
  const what = subject(i)
  const window = `${settings.window} min`
  const change = i.change_percent === null || i.change_percent === undefined ? null : Math.abs(i.change_percent)
  switch (i.kind) {
    case 'new':
      return `${what} is new (${count(i.current)} in the last ${window})`
    case 'rising':
      return `${what} is up ${percent(change)} (${count(i.previous)} → ${count(i.current)} per ${window})`
    case 'falling':
      return `${what} is down ${percent(change)} (${count(i.previous)} → ${count(i.current)} per ${window})`
    case 'slower':
      return `${what} is ${percent(change)} slower (avg ${time(i.previous)} → ${time(i.current)})`
    case 'faster':
      return `${what} is ${percent(change)} faster (avg ${time(i.previous)} → ${time(i.current)})`
    case 'projected': {
      const up = i.current >= i.previous
      return `${what} projected to ${up ? 'rise' : 'fall'} ${percent(change)} in the next window (${fmt(i, i.previous)} → ~${fmt(i, i.current)})`
    }
    default:
      return `${what}: ${i.kind}`
  }
}
const SEVERITY = { bad: 'bad', notice: 'warn', good: 'ok' }
function insightLink(i) {
  if (i.key) return issueRoute(i.type, i.key)
  if (i.type === 'request') return { name: 'batches', query: { kind: 'request' } }
  return { name: 'entries', params: { type: i.type } }
}
</script>

<template>
  <div class="space-y-6">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">Dashboard</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        What was slowest and what failed most — from rolled-up measurements that outlive the individual requests and jobs kept.
      </p>
    </div>

    <ErrorBox :error="error" />

    <div class="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4 2xl:grid-cols-7">
      <StatTile label="Requests" :value="count(requestTotals.count)" :hint="windowHint" :loading="loading">
        <Sparkline :points="counts(series.request)" color="indigo" />
      </StatTile>
      <StatTile label="Throughput" :value="`${rate(requestTotals.count / minutes)}/min`" hint="requests per minute, averaged" :loading="loading" />
      <StatTile label="Avg request" :value="time(requestTotals.count ? requestTotals.average_us : null)" hint="mean time per request" :loading="loading" />
      <StatTile label="Worst request" :value="time(requestTotals.count ? requestTotals.max_us : null)" hint="the single slowest" :loading="loading" />
      <StatTile
        label="Exceptions"
        :value="count(exceptionTotals.count)"
        :tone="exceptionTotals.count ? 'bad' : 'neutral'"
        :hint="requestTotals.count ? `${percent((exceptionTotals.count * 100) / requestTotals.count)} of requests` : 'no requests to compare'"
        :loading="loading"
      >
        <Sparkline :points="counts(series.exception)" color="rose" />
      </StatTile>
      <StatTile label="Jobs run" :value="count(jobTotals.count)" :hint="`${rate(jobTotals.count / minutes)}/min`" :loading="loading">
        <Sparkline :points="counts(series.job)" color="amber" />
      </StatTile>
      <StatTile label="Avg job time" :value="time(jobTotals.count ? jobTotals.average_us : null)" :hint="jobTotals.count ? `worst ${time(jobTotals.max_us)}` : 'no jobs ran'" :loading="loading" />
    </div>

    <div class="grid gap-4 md:grid-cols-2">
      <section class="lk-card min-w-0 md:col-span-2">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Requests</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">per minute on the left; average, p90, p95 and p99 on the right · {{ bucketLabel }} buckets · dashed: forecast</span>
        </header>
        <div class="p-4">
          <LineChart :series="requestChart" :format="perMin" :format-right="time" :height="200" />
        </div>
      </section>
      <section class="lk-card min-w-0">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Exceptions</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">per {{ bucketLabel }} · dashed: forecast</span>
        </header>
        <div class="p-4"><LineChart :series="exceptionChart" :format="whole" :height="160" /></div>
      </section>
      <section class="lk-card min-w-0">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Jobs</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">runs per {{ bucketLabel }}, then avg time · dashed: forecast</span>
        </header>
        <div class="space-y-3 p-4">
          <div>
            <h3 class="lk-th mb-1">Runs</h3>
            <LineChart :series="jobChart" :format="whole" :height="110" />
          </div>
          <div>
            <h3 class="lk-th mb-1">Avg job time</h3>
            <LineChart :series="jobTimeChart" :format="time" :height="110" />
          </div>
        </div>
      </section>
    </div>

    <section class="lk-card overflow-visible">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">Insights</h2>
        <span class="text-xs text-zinc-500 dark:text-zinc-400">
          last {{ settings.window }} min vs the {{ settings.window }} before<template v-if="customised"> · your thresholds</template>
        </span>
        <div ref="settingsRoot" class="relative ml-auto">
          <button
            type="button"
            class="lk-btn px-2"
            :aria-expanded="settingsOpen"
            aria-controls="insight-settings"
            aria-label="Insight thresholds"
            title="Insight thresholds"
            @click="openSettings"
          >
            <svg viewBox="0 0 20 20" class="size-4" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
              <path d="M3 5h8M15 5h2M3 10h2M9 10h8M3 15h10M17 15h0" />
              <circle cx="13" cy="5" r="2" /><circle cx="7" cy="10" r="2" /><circle cx="15" cy="15" r="2" />
            </svg>
            <span class="hidden sm:inline">Thresholds</span>
          </button>
          <form
            v-if="settingsOpen"
            id="insight-settings"
            class="absolute right-0 z-20 mt-2 w-72 max-w-[calc(100vw-2rem)] space-y-3 rounded-lg border border-zinc-200 bg-white p-4 shadow-lg dark:border-zinc-700 dark:bg-zinc-900"
            @submit.prevent="saveSettings"
          >
            <p class="text-xs text-zinc-500 dark:text-zinc-400">Kept in this browser. Server defaults: {{ defaults.window }} min, {{ defaults.min }}, {{ defaults.change }}%, {{ defaults.forecast }} buckets.</p>
            <label class="block text-sm">
              <span class="lk-th">Window (minutes)</span>
              <input v-model.number="draft.window" type="number" min="1" step="1" class="lk-input mt-1" />
            </label>
            <label class="block text-sm">
              <span class="lk-th">Minimum count</span>
              <input v-model.number="draft.min" type="number" min="1" step="1" class="lk-input mt-1" />
            </label>
            <label class="block text-sm">
              <span class="lk-th">Change (%)</span>
              <input v-model.number="draft.change" type="number" min="1" step="1" class="lk-input mt-1" />
            </label>
            <label class="block text-sm">
              <span class="lk-th">Forecast buckets</span>
              <input v-model.number="draft.forecast" type="number" min="1" step="1" class="lk-input mt-1" />
            </label>
            <div class="flex gap-2">
              <button type="submit" class="lk-btn">Apply</button>
              <button type="button" class="lk-btn" @click="resetSettings">Defaults</button>
            </div>
          </form>
        </div>
      </header>
      <div v-if="insightsError" class="px-4 py-3 text-sm text-rose-700 dark:text-rose-400">{{ insightsError.message }}</div>
      <Empty v-else-if="!insightsEnabled">Insights are off (LOOKOUT_INSIGHTS_ENABLED).</Empty>
      <Empty v-else-if="!insights.length">{{ loading ? 'Loading…' : 'Nothing has changed enough to mention.' }}</Empty>
      <ul v-else class="divide-y divide-zinc-100 dark:divide-zinc-800">
        <li v-for="(i, n) in insights" :key="`${i.kind}:${i.type}:${i.key}:${n}`">
          <RouterLink
            :to="insightLink(i)"
            class="flex items-start gap-3 px-4 py-2.5 text-sm hover:bg-zinc-50 focus-visible:bg-zinc-50 focus-visible:outline-none dark:hover:bg-zinc-800/50 dark:focus-visible:bg-zinc-800/50"
          >
            <Pill :tone="SEVERITY[i.severity] || 'neutral'" class="mt-0.5">{{ i.severity }}</Pill>
            <span class="min-w-0 flex-1 break-words">{{ sentence(i) }}</span>
            <span class="hidden shrink-0 text-xs text-zinc-500 sm:inline dark:text-zinc-400">{{ TYPES[i.type] || i.type }}</span>
          </RouterLink>
        </li>
      </ul>
    </section>

    <div class="grid gap-4 xl:grid-cols-2">
      <AggregateCard
        v-for="card in CARDS"
        :key="card.id"
        :title="card.title"
        :hint="card.hint"
        :order="card.order"
        :timed="card.timed !== false"
        :key-label="card.keyLabel"
        :rows="rows[card.id] || []"
        :link="(row) => issueRoute(card.type, row.key)"
        :secondary="card.type === 'request' ? (row) => ({ to: { name: 'batches', query: { kind: 'request', route: row.key } }, label: 'Batches →' }) : null"
        :loading="loading"
      />
    </div>

    <section class="lk-card">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">Recent failures</h2>
        <RouterLink
          :to="{ name: 'batches', query: { kind: 'request', exception: '1' } }"
          class="ml-auto text-xs font-medium text-indigo-600 hover:underline dark:text-indigo-400"
        >All failures →</RouterLink>
      </header>
      <BatchList :batches="failures" :loading="loading" empty="No failures recorded." label="Recent failures" />
    </section>
  </div>
</template>
