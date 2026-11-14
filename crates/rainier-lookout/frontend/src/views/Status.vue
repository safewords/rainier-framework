<script setup>
import { computed, onMounted, ref } from 'vue'

import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Pill from '../components/Pill.vue'
import { api } from '../api.js'
import { TYPES } from '../format.js'

const status = ref(null)
const error = ref(null)

const settings = computed(() => {
  const s = status.value
  if (!s) return []
  return [
    ['Recording', s.enabled ? 'on' : 'off'],
    ['Environment', s.environment || '—'],
    ['Store', `${s.store} (${s.store_kind})`],
    ['Request sample rate', s.sample_rates.request],
    ['Job sample rate', s.sample_rates.job],
    ['Command sample rate', s.sample_rates.command],
    ['Batches kept', `${s.retention.batches_hours} hours`],
    ['Aggregates kept', `${s.retention.aggregates_days} days`],
    ['Lowest log level', s.log_level],
    [
      'N+1 detection',
      s.n_plus_one.enabled ? `${s.n_plus_one.min_repeats}+ repeats, ${s.n_plus_one.min_total_ms}ms+` : 'off',
    ],
  ]
})

const watchers = computed(() => (status.value ? Object.entries(status.value.watchers) : []))

onMounted(async () => {
  try {
    status.value = (await api.status()).data
  } catch (e) {
    error.value = e
  }
})
</script>

<template>
  <div class="space-y-5">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">Status</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">What Lookout records, how much of it, and for how long.</p>
    </div>
    <ErrorBox :error="error" />
    <Empty v-if="!status && !error">Loading…</Empty>
    <template v-if="status">
      <section class="lk-card">
        <header class="lk-card-header"><h2 class="text-sm font-semibold">Settings</h2></header>
        <dl class="divide-y divide-zinc-100 dark:divide-zinc-800">
          <div v-for="[name, value] in settings" :key="name" class="flex flex-wrap justify-between gap-2 px-4 py-2.5 text-sm">
            <dt class="text-zinc-500 dark:text-zinc-400">{{ name }}</dt>
            <dd class="font-medium">{{ value }}</dd>
          </div>
        </dl>
      </section>
      <section class="lk-card">
        <header class="lk-card-header"><h2 class="text-sm font-semibold">Watchers</h2></header>
        <ul class="grid divide-y divide-zinc-100 sm:grid-cols-2 sm:divide-y-0 lg:grid-cols-3 dark:divide-zinc-800">
          <li
            v-for="[type, w] in watchers"
            :key="type"
            class="flex items-center gap-2 px-4 py-2.5 text-sm sm:border-b sm:border-zinc-100 dark:sm:border-zinc-800"
          >
            <span class="flex-1">{{ TYPES[type] || type }}</span>
            <span v-if="w.slow_ms" class="text-xs text-zinc-500 dark:text-zinc-400">slow ≥ {{ w.slow_ms }}ms</span>
            <span v-if="w.enabled && w.sample_rate < 1" class="text-xs text-zinc-500 dark:text-zinc-400">{{ Math.round(w.sample_rate * 100) }}%</span>
            <Pill :tone="w.enabled ? 'ok' : 'neutral'">{{ w.enabled ? 'on' : 'off' }}</Pill>
          </li>
        </ul>
      </section>
    </template>
  </div>
</template>
