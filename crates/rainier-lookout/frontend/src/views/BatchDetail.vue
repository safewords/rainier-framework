<script setup>
import { computed, onMounted, ref } from 'vue'

import BatchStatus from '../components/BatchStatus.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Flags from '../components/Flags.vue'
import Json from '../components/Json.vue'
import Pill from '../components/Pill.vue'
import TimeAgo from '../components/TimeAgo.vue'
import Waterfall from '../components/Waterfall.vue'
import { api } from '../api.js'
import { absolute, duration, hasTag } from '../format.js'

const props = defineProps({ id: { type: String, required: true } })

const batch = ref(null)
const related = ref([])
const error = ref(null)
const loading = ref(true)
const selected = ref(null)

const KIND = { request: 'Request', job: 'Job', command: 'Command', scheduled_task: 'Scheduled task', other: 'Other' }

const facts = computed(() => {
  const b = batch.value
  if (!b) return []
  return [
    ['Kind', KIND[b.kind] || b.kind],
    ['Method', b.method],
    ['Route', b.route_name],
    ['Pattern', b.route_pattern, true],
    ['Status', b.status],
    ['Duration', duration(b.duration_us)],
    ['Started', absolute(b.started_at)],
    ['Guard', b.guard],
    ['User', b.user_id],
    ['Host', b.host],
    ['Batch', b.id, true],
    ['Trace', b.trace_id, true],
  ]
})

const counts = computed(() => {
  const out = {}
  for (const e of batch.value ? batch.value.entries : []) out[e.type] = (out[e.type] || 0) + 1
  return Object.entries(out)
})

const entry = computed(() =>
  batch.value && selected.value ? batch.value.entries.find((e) => e.id === selected.value) : null,
)

function offset(e) {
  return Math.max(0, (Date.parse(e.occurred_at) - Date.parse(batch.value.started_at)) * 1000)
}

function select(id) {
  selected.value = selected.value === id ? null : id
}

onMounted(async () => {
  try {
    const answer = await api.batch(props.id)
    batch.value = answer.data
    if (batch.value.trace_id) {
      try {
        const page = await api.batches({ trace: batch.value.trace_id, limit: 50 })
        related.value = page.data.filter((b) => b.id !== batch.value.id)
      } catch {
        related.value = []
      }
    }
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
})
</script>

<template>
  <div class="space-y-5">
    <RouterLink
      :to="{ name: 'batches', query: { kind: batch ? batch.kind : 'request' } }"
      class="text-sm text-zinc-500 hover:text-indigo-600 dark:text-zinc-400 dark:hover:text-indigo-400"
    >← Back</RouterLink>

    <template v-if="error">
      <h1 class="text-xl font-semibold">{{ error.status === 404 ? 'No such batch' : 'Could not load this batch' }}</h1>
      <p v-if="error.status === 404" class="text-sm text-zinc-500 dark:text-zinc-400">It may have been pruned, or never kept.</p>
      <ErrorBox v-else :error="error" />
    </template>

    <Empty v-else-if="loading">Loading…</Empty>

    <template v-else-if="batch">
      <div>
        <h1 class="font-mono text-lg font-semibold break-all sm:text-xl">{{ batch.summary }}</h1>
        <div class="mt-2 flex flex-wrap items-center gap-2 text-sm text-zinc-500 dark:text-zinc-400">
          <BatchStatus :batch="batch" />
          <span class="tabular-nums">{{ duration(batch.duration_us) }}</span>
          <span>·</span>
          <TimeAgo :at="batch.started_at" />
          <Flags :exception="batch.has_exception" :slow="batch.has_slow" :tags="batch.tags" />
        </div>
      </div>

      <section class="lk-card">
        <header class="lk-card-header"><h2 class="text-sm font-semibold">Details</h2></header>
        <dl class="grid grid-cols-2 gap-x-6 gap-y-4 p-4 sm:grid-cols-3 lg:grid-cols-4">
          <div v-for="[name, value, mono] in facts" :key="name" class="min-w-0">
            <dt class="lk-th">{{ name }}</dt>
            <dd class="mt-0.5 text-sm break-words" :class="mono ? 'font-mono text-[12.5px]' : ''">
              {{ value === null || value === undefined || value === '' ? '—' : value }}
            </dd>
          </div>
          <div class="col-span-2 min-w-0 sm:col-span-3 lg:col-span-4">
            <dt class="lk-th">Tags</dt>
            <dd class="mt-1 flex flex-wrap gap-1">
              <Pill v-for="tag in batch.tags" :key="tag">{{ tag }}</Pill>
              <span v-if="!batch.tags.length" class="text-sm">—</span>
            </dd>
          </div>
        </dl>
      </section>

      <section v-if="related.length" class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Related</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">the same trace — what this queued, or what queued it</span>
        </header>
        <ul class="divide-y divide-zinc-100 dark:divide-zinc-800">
          <li v-for="other in related" :key="other.id">
            <RouterLink
              :to="{ name: 'batch', params: { id: other.id } }"
              class="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2.5 hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
            >
              <Pill tone="accent">{{ KIND[other.kind] || other.kind }}</Pill>
              <span class="min-w-0 flex-1 truncate font-mono text-[13px]">{{ other.summary }}</span>
              <BatchStatus :batch="other" />
              <span class="text-xs text-zinc-500 tabular-nums dark:text-zinc-400">{{ duration(other.duration_us) }}</span>
              <span class="text-xs text-zinc-500 dark:text-zinc-400"><TimeAgo :at="other.started_at" /></span>
            </RouterLink>
          </li>
        </ul>
      </section>

      <section class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Timeline</h2>
          <span class="flex flex-wrap gap-1">
            <Pill v-for="[type, n] in counts" :key="type">{{ n }} {{ type }}</Pill>
          </span>
          <span class="ml-auto hidden text-xs text-zinc-500 sm:inline dark:text-zinc-400">click an entry for its content</span>
        </header>
        <Waterfall v-if="batch.entries.length" :batch="batch" :selected="selected" @select="select" />
        <Empty v-else>This batch recorded no entries.</Empty>

        <div v-if="entry" class="space-y-3 border-t border-zinc-200 p-4 dark:border-zinc-800">
          <div class="flex flex-wrap items-center gap-2">
            <Pill tone="accent">{{ entry.type }}</Pill>
            <Flags :slow="hasTag(entry.tags, 'slow')" :tags="entry.tags" />
            <span class="text-xs text-zinc-500 tabular-nums dark:text-zinc-400">
              +{{ duration(offset(entry)) }} · {{ duration(entry.duration_us) }}
            </span>
            <button type="button" class="lk-btn ml-auto" @click="selected = null">Close</button>
          </div>
          <p class="font-mono text-[13px] break-all">{{ entry.summary }}</p>
          <dl class="grid grid-cols-1 gap-3 text-sm sm:grid-cols-3">
            <div class="min-w-0"><dt class="lk-th">Span</dt><dd class="truncate font-mono text-xs">{{ entry.span_id || '—' }}</dd></div>
            <div class="min-w-0"><dt class="lk-th">Parent</dt><dd class="truncate font-mono text-xs">{{ entry.parent_span_id || '—' }}</dd></div>
            <div class="min-w-0"><dt class="lk-th">Family</dt><dd class="truncate font-mono text-xs" :title="entry.family_hash">{{ entry.family_hash || '—' }}</dd></div>
          </dl>
          <div v-if="entry.tags.length" class="flex flex-wrap gap-1">
            <Pill v-for="tag in entry.tags" :key="tag">{{ tag }}</Pill>
          </div>
          <Json :value="entry.content" />
        </div>
      </section>
    </template>
  </div>
</template>
