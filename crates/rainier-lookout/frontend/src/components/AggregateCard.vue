<script setup>
import { computed } from 'vue'
import Empty from './Empty.vue'
import { average, duration } from '../format.js'

const props = defineProps({
  title: { type: String, required: true },
  hint: { type: String, default: '' },
  rows: { type: Array, default: () => [] },
  order: { type: String, default: 'max' },
  timed: { type: Boolean, default: true },
  link: { type: Function, default: null },
  loading: Boolean,
})

const metric = (row) => (props.order === 'count' ? row.count : props.order === 'average' ? average(row) : row.max_us)
const top = computed(() => Math.max(1, ...props.rows.map(metric)))
</script>

<template>
  <section class="lk-card flex flex-col">
    <header class="lk-card-header">
      <h2 class="text-sm font-semibold">{{ title }}</h2>
      <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">{{ hint }}</span>
    </header>
    <Empty v-if="!rows.length">{{ loading ? 'Loading…' : 'Nothing recorded in this window.' }}</Empty>
    <ul v-else class="divide-y divide-zinc-100 dark:divide-zinc-800">
      <li v-for="row in rows" :key="row.key" class="px-4 py-2.5">
        <div class="flex items-baseline gap-3">
          <component
            :is="link ? 'RouterLink' : 'span'"
            :to="link ? link(row) : undefined"
            class="min-w-0 flex-1 truncate font-mono text-[12.5px] text-zinc-800 hover:text-indigo-600 dark:text-zinc-200 dark:hover:text-indigo-400"
            :title="row.key"
          >{{ row.key }}</component>
          <span class="text-xs whitespace-nowrap text-zinc-500 tabular-nums dark:text-zinc-400">
            {{ row.count }}×<template v-if="timed"> · avg {{ duration(average(row)) }} · max <b class="font-semibold text-zinc-800 dark:text-zinc-200">{{ duration(row.max_us) }}</b></template>
          </span>
        </div>
        <div class="mt-1.5 h-1 overflow-hidden rounded-full bg-zinc-100 dark:bg-zinc-800">
          <div
            class="h-full rounded-full"
            :class="timed ? 'bg-indigo-500' : 'bg-rose-500'"
            :style="{ width: `${Math.max(1, (metric(row) / top) * 100)}%` }"
          />
        </div>
      </li>
    </ul>
  </section>
</template>
