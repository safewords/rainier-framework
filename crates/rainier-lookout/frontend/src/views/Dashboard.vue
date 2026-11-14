<script setup>
import { onMounted, reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import AggregateCard from '../components/AggregateCard.vue'
import BatchList from '../components/BatchList.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import { api } from '../api.js'
import { usePolling } from '../live.js'

const RANGES = [
  { key: '1h', period: 60, hours: 1 },
  { key: '6h', period: 600, hours: 6 },
  { key: '24h', period: 600, hours: 24 },
  { key: '7d', period: 3600, hours: 168 },
]
const CARDS = [
  { id: 'routes', title: 'Slowest routes', type: 'request', order: 'max', hint: 'by worst time' },
  { id: 'queries', title: 'Slowest queries', type: 'query', order: 'max', hint: 'by worst time' },
  { id: 'exceptions', title: 'Most frequent exceptions', type: 'exception', order: 'count', hint: 'by count', timed: false },
  { id: 'jobs', title: 'Slowest jobs', type: 'job', order: 'max', hint: 'by worst time' },
  { id: 'http', title: 'Slowest outgoing requests', type: 'http_client', order: 'max', hint: 'by worst time' },
]

const route = useRoute()
const router = useRouter()
const range = RANGES.find((r) => r.key === route.query.range) || RANGES[2]

const rows = reactive({})
const failures = ref([])
const loading = ref(true)
const error = ref(null)

function link(card) {
  if (card.type === 'request') return (row) => ({ name: 'batches', query: { kind: 'request', route: row.key } })
  return () => ({ name: 'entries', params: { type: card.type } })
}

async function load() {
  const since = new Date(Date.now() - range.hours * 3600 * 1000).toISOString()
  try {
    const [aggregates, failed] = await Promise.all([
      Promise.all(
        CARDS.map((card) => api.aggregates(card.type, { period: range.period, since, order: card.order, limit: 8 })),
      ),
      api.batches({ exception: true, limit: 8 }),
    ])
    CARDS.forEach((card, i) => (rows[card.id] = aggregates[i].data))
    failures.value = failed.data
    error.value = null
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}

onMounted(load)
usePolling(load, 10_000)
</script>

<template>
  <div class="space-y-6">
    <div class="flex flex-wrap items-end justify-between gap-3">
      <div>
        <h1 class="text-xl font-semibold tracking-tight">Dashboard</h1>
        <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
          What was slowest and what failed most — from rolled-up measurements that outlive the raw batches.
        </p>
      </div>
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
          @click="router.replace({ query: { range: r.key } })"
        >{{ r.key }}</button>
      </div>
    </div>

    <ErrorBox :error="error" />

    <div class="grid gap-4 md:grid-cols-2 2xl:grid-cols-3">
      <AggregateCard
        v-for="card in CARDS"
        :key="card.id"
        :title="card.title"
        :hint="card.hint"
        :order="card.order"
        :timed="card.timed !== false"
        :rows="rows[card.id] || []"
        :link="link(card)"
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
      <BatchList v-if="failures.length" :batches="failures" />
      <Empty v-else>{{ loading ? 'Loading…' : 'No failures recorded.' }}</Empty>
    </section>
  </div>
</template>
