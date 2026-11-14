<script setup>
import BatchStatus from './BatchStatus.vue'
import Flags from './Flags.vue'
import TimeAgo from './TimeAgo.vue'
import { duration } from '../format.js'

defineProps({ batches: { type: Array, required: true } })

function total(counts) {
  return Object.values(counts || {}).reduce((sum, n) => sum + n, 0)
}
function breakdown(counts) {
  return Object.entries(counts || {})
    .map(([type, n]) => `${n} ${type}`)
    .join(' · ')
}
</script>

<template>
  <div>
    <div
      class="hidden grid-cols-[minmax(0,1fr)_4.5rem_5.5rem_4.5rem_6rem_9rem_5.5rem] gap-3 border-b border-zinc-200 px-4 py-2 md:grid dark:border-zinc-800"
    >
      <span class="lk-th">Summary</span>
      <span class="lk-th">Status</span>
      <span class="lk-th text-right">Duration</span>
      <span class="lk-th text-right">Entries</span>
      <span class="lk-th">User</span>
      <span class="lk-th">Flags</span>
      <span class="lk-th text-right">When</span>
    </div>
    <ul class="divide-y divide-zinc-200 dark:divide-zinc-800">
      <li v-for="batch in batches" :key="batch.id">
        <RouterLink
          :to="{ name: 'batch', params: { id: batch.id } }"
          class="block px-4 py-3 transition hover:bg-zinc-50 md:grid md:grid-cols-[minmax(0,1fr)_4.5rem_5.5rem_4.5rem_6rem_9rem_5.5rem] md:items-center md:gap-3 md:py-2.5 dark:hover:bg-zinc-800/50"
        >
          <div class="min-w-0">
            <div class="truncate font-mono text-[13px] text-zinc-900 dark:text-zinc-100" :title="batch.summary">
              {{ batch.summary }}
            </div>
            <div v-if="batch.route_name" class="truncate text-xs text-zinc-500 dark:text-zinc-400">
              {{ batch.route_name }}
            </div>
          </div>
          <!-- On a phone the columns become one wrapped line of facts. -->
          <div class="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1.5 text-xs text-zinc-500 md:contents dark:text-zinc-400">
            <span><BatchStatus :batch="batch" /></span>
            <span class="tabular-nums md:text-right md:text-sm md:text-zinc-700 md:dark:text-zinc-300">{{ duration(batch.duration_us) }}</span>
            <span class="tabular-nums md:text-right md:text-sm" :title="breakdown(batch.counts)">
              {{ total(batch.counts) }}<span class="md:hidden"> entries</span>
            </span>
            <span class="truncate md:text-sm">
              <template v-if="batch.user_id">user {{ batch.user_id }}</template>
              <span v-else class="text-zinc-400 dark:text-zinc-500">guest</span>
            </span>
            <span><Flags :exception="batch.has_exception" :slow="batch.has_slow" :tags="batch.tags" /></span>
            <span class="md:text-right"><TimeAgo :at="batch.started_at" /></span>
          </div>
        </RouterLink>
      </li>
    </ul>
  </div>
</template>
