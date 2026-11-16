<script setup>
// One session as a timeline: every page view, route change, script error,
// API call (with the server's side when it was kept or paired), and the
// replay of it all, in the order they happened — what to open when a user
// reports a problem.
import { computed, onMounted, ref } from 'vue'

import BatchStatus from '../components/BatchStatus.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Pill from '../components/Pill.vue'
import ReplayPlayer from '../components/ReplayPlayer.vue'
import { api } from '../api.js'
import { absolute, duration, itemRoute } from '../format.js'
import { usePolling } from '../live.js'

const props = defineProps({ id: { type: String, required: true } })

const summaries = ref([])
const pages = ref({})
const error = ref(null)
const loading = ref(true)

async function load() {
  try {
    const answer = await api.batches({ tag: `session:${props.id}`, limit: 200 })
    summaries.value = answer.data
    // A page view's route changes and errors live in its entry: load each
    // page view's detail, once.
    const missing = answer.data.filter((b) => b.kind === 'page' && !pages.value[b.id])
    const loaded = await Promise.all(missing.map(async (b) => [b.id, (await api.batch(b.id)).data]))
    pages.value = { ...pages.value, ...Object.fromEntries(loaded) }
    error.value = null
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}
onMounted(load)
usePolling(load, 15_000)

const users = computed(() => [...new Set(summaries.value.map((b) => b.user_id).filter(Boolean))])
const replays = computed(() =>
  summaries.value.filter((b) => (b.tags || []).includes('replay')).sort((a, b) => (a.started_at < b.started_at ? -1 : 1)),
)
// The server's side of a call, by trace: a kept or paired request.
const servers = computed(() => {
  const out = {}
  for (const b of summaries.value) if (b.kind === 'request') out[b.trace_id] = b
  return out
})

// One row per thing that happened, in time order. A page view contributes
// its load, its route changes and its errors, each at its own time.
const timeline = computed(() => {
  const rows = []
  for (const b of summaries.value) {
    if ((b.tags || []).includes('replay')) continue
    if (b.kind === 'page') {
      const detail = pages.value[b.id]
      const content = detail && detail.entries[0] ? detail.entries[0].content || {} : {}
      const activity = content.activity || {}
      const opened = Date.parse(b.started_at)
      rows.push({ at: opened, kind: 'page', batch: b, text: b.summary.replace(/^page /, ''), load: activity.load })
      for (const n of activity.navigations || []) {
        rows.push({ at: opened + (n.at || 0), kind: 'route', text: n.to, batch: b })
      }
      for (const e of activity.errors || []) {
        rows.push({ at: opened + (e.at || 0), kind: 'error', text: e.message, where: e.source ? `${e.source}:${e.line || ''}` : '', batch: b })
      }
    } else if (b.kind === 'browser') {
      const server = servers.value[b.trace_id] || null
      rows.push({ at: Date.parse(b.started_at), kind: 'call', batch: b, server, text: b.summary.replace(/^api /, '') })
    } else if (b.kind === 'request' && !summaries.value.some((x) => x.kind === 'browser' && x.trace_id === b.trace_id)) {
      rows.push({ at: Date.parse(b.started_at), kind: 'request', batch: b, text: b.summary })
    }
  }
  return rows.sort((a, b) => a.at - b.at)
})
const first = computed(() => (timeline.value.length ? timeline.value[0].at : 0))
const offset = (at) => duration(Math.max(0, at - first.value) * 1000)
const failed = (b) => (b.status && b.status >= 400) || b.has_exception
</script>

<template>
  <div class="space-y-5">
    <div>
      <RouterLink :to="{ name: 'sessions' }" class="text-sm text-indigo-600 hover:underline dark:text-indigo-400">Sessions</RouterLink>
      <h1 class="mt-1 text-xl font-semibold tracking-tight">
        Session <span class="font-mono text-base text-zinc-500">{{ id.slice(0, 12) }}</span>
      </h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        <template v-if="users.length">
          User
          <RouterLink
            v-for="u in users"
            :key="u"
            :to="{ name: 'batches', query: { kind: 'request', user: u } }"
            class="text-indigo-600 hover:underline dark:text-indigo-400"
          >{{ u }}</RouterLink>
          ·
        </template>
        <template v-if="timeline.length">from {{ absolute(new Date(first).toISOString()) }}</template>
      </p>
    </div>

    <ErrorBox :error="error" />
    <Empty v-if="!loading && !timeline.length && !error">Nothing recorded for this session.</Empty>

    <ReplayPlayer v-if="replays.length" :ids="replays.map((r) => r.id)" title="Replay of the session" />

    <section v-if="timeline.length" class="lk-card">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">Timeline</h2>
        <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">page views, route changes, errors and API calls, in order</span>
      </header>
      <ol class="divide-y divide-zinc-100 dark:divide-zinc-800">
        <li v-for="(row, i) in timeline" :key="i" class="flex flex-wrap items-baseline gap-x-3 gap-y-1 px-4 py-2">
          <span class="w-16 shrink-0 text-xs text-zinc-500 tabular-nums dark:text-zinc-400">+{{ offset(row.at) }}</span>
          <template v-if="row.kind === 'page'">
            <Pill tone="accent">page</Pill>
            <RouterLink :to="itemRoute(row.batch)" class="min-w-0 flex-1 truncate text-sm font-medium hover:underline">{{ row.text }}</RouterLink>
          </template>
          <template v-else-if="row.kind === 'route'">
            <span class="w-12 text-xs text-zinc-500">→</span>
            <span class="min-w-0 flex-1 truncate font-mono text-[12.5px]">{{ row.text }}</span>
          </template>
          <template v-else-if="row.kind === 'error'">
            <Pill tone="bad">error</Pill>
            <span class="min-w-0 flex-1 text-sm text-rose-700 dark:text-rose-300">{{ row.text }}</span>
            <span class="truncate font-mono text-[11px] text-zinc-500">{{ row.where }}</span>
          </template>
          <template v-else-if="row.kind === 'call'">
            <Pill :tone="failed(row.batch) ? 'bad' : row.batch.has_slow ? 'warn' : 'neutral'">api</Pill>
            <RouterLink :to="itemRoute(row.batch)" class="min-w-0 flex-1 truncate font-mono text-[12.5px] hover:underline">{{ row.text }}</RouterLink>
            <span class="text-xs text-zinc-500 tabular-nums dark:text-zinc-400">{{ duration(row.batch.duration_us) }} in the browser</span>
            <RouterLink
              v-if="row.server"
              :to="itemRoute(row.server)"
              class="text-xs text-indigo-600 tabular-nums hover:underline dark:text-indigo-400"
            >{{ duration(row.server.duration_us) }} on the server</RouterLink>
          </template>
          <template v-else>
            <Pill>server</Pill>
            <RouterLink :to="itemRoute(row.batch)" class="min-w-0 flex-1 truncate font-mono text-[12.5px] hover:underline">{{ row.text }}</RouterLink>
            <BatchStatus :batch="row.batch" />
            <span class="text-xs text-zinc-500 tabular-nums dark:text-zinc-400">{{ duration(row.batch.duration_us) }}</span>
          </template>
        </li>
      </ol>
    </section>
  </div>
</template>
