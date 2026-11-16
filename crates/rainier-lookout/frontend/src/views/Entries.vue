<script setup>
import { computed, onMounted, ref } from 'vue'

import DataTable from '../components/DataTable.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Flags from '../components/Flags.vue'
import LoadMore from '../components/LoadMore.vue'
import Pill from '../components/Pill.vue'
import StatTile from '../components/StatTile.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { field, hasTag, isNPlusOne, issueRoute, statusTone, time, TYPES } from '../format.js'
import { usePolling } from '../live.js'

const props = defineProps({ type: { type: String, required: true } })
const PAGE = 100

const entries = ref([])
const next = ref(null)
const loading = ref(true)
const more = ref(false)
const error = ref(null)

const title = computed(() => TYPES[props.type] || props.type)

function text(content, keys) {
  if (!content || typeof content !== 'object') return null
  for (const key of keys) if (typeof content[key] === 'string' && content[key]) return content[key]
  return null
}

function cacheOutcome(e) {
  if (hasTag(e.tags, 'hit')) return true
  if (hasTag(e.tags, 'miss')) return false
  const c = e.content || {}
  if (typeof c.hit === 'boolean') return c.hit
  const r = text(c, ['result', 'outcome'])
  return r === 'hit' ? true : r === 'miss' ? false : null
}

/** The per-type fact shown beside an entry: a hit, a level, a status. */
function extra(e) {
  const c = e.content || {}
  switch (e.type) {
    case 'cache': {
      const hit = cacheOutcome(e)
      return hit === null ? null : hit ? { tone: 'ok', text: 'hit' } : { tone: 'warn', text: 'miss' }
    }
    case 'log': {
      const level = text(c, ['level'])
      if (!level) return null
      const l = level.toLowerCase()
      return { tone: l === 'error' ? 'bad' : l.startsWith('warn') ? 'warn' : l === 'info' ? 'info' : 'neutral', text: level }
    }
    case 'http_client': {
      const status = c.status ?? c['http.response.status_code']
      if (typeof status === 'number') return { tone: statusTone(status), text: String(status) }
      return hasTag(e.tags, 'failed') ? { tone: 'bad', text: 'failed' } : null
    }
    case 'mail':
    case 'notification': {
      const to = c.to ?? c.recipients
      const shown = Array.isArray(to) ? to.join(', ') : typeof to === 'string' ? to : null
      return shown ? { tone: 'neutral', text: `to ${shown}` } : null
    }
    default:
      return null
  }
}

const stats = computed(() => {
  const list = entries.value
  if (!list.length) return []
  if (props.type === 'query') {
    return [
      ['Shown', list.length],
      ['Slow', list.filter((e) => hasTag(e.tags, 'slow')).length],
      ['In an N+1', list.filter((e) => isNPlusOne(e.tags)).length],
    ]
  }
  if (props.type === 'cache') {
    const hits = list.filter((e) => cacheOutcome(e) === true).length
    const misses = list.filter((e) => cacheOutcome(e) === false).length
    return [
      ['Hits', hits],
      ['Misses', misses],
      ['Hit rate', hits + misses ? `${Math.round((hits * 100) / (hits + misses))}%` : '—'],
    ]
  }
  if (props.type === 'job') {
    return [
      ['Shown', list.length],
      ['Failed', list.filter((e) => hasTag(e.tags, 'failed')).length],
    ]
  }
  return []
})

/** Exceptions by fingerprint, most frequent first. */
const groups = computed(() => {
  if (props.type !== 'exception') return []
  const map = new Map()
  for (const e of entries.value) {
    const key = e.family_hash || e.summary
    const g = map.get(key)
    if (!g) map.set(key, { key, latest: e, count: 1 })
    else {
      g.count += 1
      if (e.occurred_at > g.latest.occurred_at) g.latest = e
    }
  }
  return [...map.values()].sort((a, b) => b.count - a.count || (a.latest.occurred_at < b.latest.occurred_at ? 1 : -1))
})
const topGroup = computed(() => Math.max(1, ...groups.value.map((g) => g.count)))

// --- Columns, chosen per type so every value sits under a heading that says
// what it is.

const rowKey = (e) => `${e.batch_id}/${e.id}`
const toBatch = (e) => ({ name: 'batch', params: { id: e.batch_id } })
const c = (e) => e.content || {}

/** `NotFound` and `post 4897 does not exist` out of an exception entry. */
function exceptionParts(e) {
  const content = c(e)
  const inner = content.exception && typeof content.exception === 'object' ? content.exception : {}
  let kind = field(content, 'kind_label') || field(inner, 'kind_label', 'kind', 'type')
  let message = field(inner, 'message') || field(content, 'message')
  const summary = e.summary || ''
  const colon = summary.indexOf(': ')
  if (!kind && colon > 0) kind = summary.slice(0, colon)
  if (!message) message = colon > 0 ? summary.slice(colon + 2) : summary
  return { kind: kind ? String(kind) : null, message: String(message) }
}

function nPlusOne(e) {
  const content = c(e)
  const repeats = Number(field(content, 'count')) || null
  const totalMs = Number(field(content, 'total_ms'))
  const total = Number.isFinite(totalMs) && field(content, 'total_ms') !== null ? totalMs * 1000 : e.duration_us ?? null
  return {
    sql: String(field(content, 'sql') || e.summary),
    count: repeats,
    total_us: total,
    average_us: repeats && total !== null ? total / repeats : null,
  }
}

function cacheParts(e) {
  const content = c(e)
  const op = field(content, 'cache.operation') || (e.summary || '').replace(/^cache\s+/, '').split(/\s+/)[0] || null
  const store = field(content, 'cache.store')
  return { op: op ? `${op}${store ? ` (${store})` : ''}` : null, key: field(content, 'cache.key') }
}

function httpParts(e) {
  const content = c(e)
  const method = field(content, 'http.request.method', 'method')
  const url = field(content, 'url.full', 'url')
  const status = field(content, 'http.response.status_code', 'status')
  return {
    request: method && url ? `${method} ${url}` : e.summary,
    status: typeof status === 'number' ? status : hasTag(e.tags, 'failed') ? 'failed' : null,
  }
}

function jobParts(e) {
  const content = c(e)
  const outcome = field(content, 'rainier.job.outcome', 'outcome') || (hasTag(e.tags, 'failed') ? 'failed' : null)
  return { job: String(field(content, 'rainier.job.name') || e.family_hash || e.summary), outcome }
}

function outcomeTone(outcome) {
  if (!outcome) return 'neutral'
  const o = String(outcome).toLowerCase()
  if (['failed', 'error', 'timed_out', 'panicked'].includes(o)) return 'bad'
  if (['retried', 'released', 'retry'].includes(o)) return 'warn'
  return ['ok', 'done', 'completed', 'processed', 'succeeded', 'success'].includes(o) ? 'ok' : 'neutral'
}

function slowParts(e) {
  const content = c(e)
  const ms = Number(field(content, 'duration_ms'))
  const threshold = Number(field(content, 'threshold_ms'))
  return {
    what: String(field(content, 'family') || e.summary),
    of: field(content, 'entry_type'),
    duration_us: e.duration_us ?? (Number.isFinite(ms) ? ms * 1000 : null),
    threshold_us: Number.isFinite(threshold) && field(content, 'threshold_ms') !== null ? threshold * 1000 : null,
  }
}

const WHEN = {
  key: 'when',
  label: 'When',
  title: 'When this entry was recorded',
  align: 'right',
  sortable: true,
  value: (e) => e.occurred_at,
  text: (e) => e.occurred_at,
}
const DURATION = {
  key: 'duration',
  label: 'Duration',
  title: 'How long this one took',
  align: 'right',
  sortable: true,
  value: (e) => e.duration_us,
  text: (e) => time(e.duration_us),
}

const columns = computed(() => {
  switch (props.type) {
    case 'query':
      return [
        { key: 'sql', label: 'SQL', sortable: true, value: (e) => e.summary, text: (e) => e.summary, class: 'font-mono text-[12.5px]' },
        { key: 'flags', label: 'Flags', title: 'Over the slow threshold, or part of an N+1 run', text: (e) => (e.tags || []).join(' ') },
        DURATION,
        { key: 'batch', label: 'Batch', title: 'The request, job or command that ran it', text: (e) => e.batch_id },
        WHEN,
      ]
    case 'n_plus_one':
      return [
        { key: 'sql', label: 'Query', sortable: true, value: (e) => nPlusOne(e).sql, text: (e) => nPlusOne(e).sql, class: 'font-mono text-[12.5px]' },
        { key: 'count', label: 'Count', title: 'How many times this query repeated in one request or job', align: 'right', sortable: true, value: (e) => nPlusOne(e).count, text: (e) => nPlusOne(e).count },
        { key: 'total', label: 'Total time', title: 'Total time spent in all the repeats of this query', align: 'right', sortable: true, value: (e) => nPlusOne(e).total_us, text: (e) => time(nPlusOne(e).total_us) },
        { key: 'average', label: 'Avg per query', title: 'Total time divided by the number of repeats', align: 'right', sortable: true, value: (e) => nPlusOne(e).average_us, text: (e) => time(nPlusOne(e).average_us) },
        { ...WHEN, label: 'Last occurrence', title: 'When this run of repeats happened' },
      ]
    case 'cache':
      return [
        { key: 'op', label: 'Operation', sortable: true, value: (e) => cacheParts(e).op, text: (e) => cacheParts(e).op },
        { key: 'key', label: 'Key', title: 'The cache key (only when key capture is on)', sortable: true, value: (e) => cacheParts(e).key, text: (e) => cacheParts(e).key, class: 'font-mono text-[12.5px]' },
        { key: 'hit', label: 'Hit / miss', sortable: true, value: (e) => cacheOutcome(e), text: (e) => { const h = cacheOutcome(e); return h === null ? null : h ? 'hit' : 'miss' } },
        DURATION,
        WHEN,
      ]
    case 'http_client':
      return [
        { key: 'request', label: 'Request', title: 'Method and URL', sortable: true, value: (e) => httpParts(e).request, text: (e) => httpParts(e).request, class: 'font-mono text-[12.5px]' },
        { key: 'status', label: 'Status', title: 'The response status', sortable: true, value: (e) => httpParts(e).status, text: (e) => httpParts(e).status },
        DURATION,
        WHEN,
      ]
    case 'log':
      return [
        { key: 'message', label: 'Message', sortable: true, value: (e) => field(c(e), 'message') || e.summary, text: (e) => field(c(e), 'message') || e.summary },
        { key: 'level', label: 'Level', sortable: true, value: (e) => field(c(e), 'level'), text: (e) => field(c(e), 'level') },
        WHEN,
      ]
    case 'job':
      return [
        { key: 'job', label: 'Job', sortable: true, value: (e) => jobParts(e).job, text: (e) => jobParts(e).job, class: 'font-mono text-[12.5px]' },
        { key: 'outcome', label: 'Outcome', sortable: true, value: (e) => jobParts(e).outcome, text: (e) => jobParts(e).outcome },
        { ...DURATION, title: 'How long this run of the job took' },
        WHEN,
      ]
    case 'exception':
      return [
        { key: 'message', label: 'Message', sortable: true, value: (e) => exceptionParts(e).message, text: (e) => exceptionParts(e).message, class: 'font-mono text-[12.5px]' },
        { key: 'kind', label: 'Kind', sortable: true, value: (e) => exceptionParts(e).kind, text: (e) => exceptionParts(e).kind },
        WHEN,
      ]
    case 'slow':
      return [
        { key: 'what', label: 'What', sortable: true, value: (e) => slowParts(e).what, text: (e) => slowParts(e).what, class: 'font-mono text-[12.5px]' },
        { key: 'of', label: 'Type', title: 'What kind of entry ran slow', sortable: true, value: (e) => slowParts(e).of, text: (e) => (slowParts(e).of ? TYPES[slowParts(e).of] || slowParts(e).of : null) },
        { ...DURATION, title: 'How long it took', value: (e) => slowParts(e).duration_us, text: (e) => time(slowParts(e).duration_us) },
        { key: 'threshold', label: 'Threshold', title: 'The slow threshold it went over', align: 'right', sortable: true, value: (e) => slowParts(e).threshold_us, text: (e) => time(slowParts(e).threshold_us) },
        WHEN,
      ]
    default:
      return [
        { key: 'summary', label: 'Summary', sortable: true, value: (e) => e.summary, text: (e) => e.summary, class: 'font-mono text-[12.5px]' },
        { key: 'detail', label: 'Detail', text: (e) => (extra(e) ? extra(e).text : null) },
        DURATION,
        WHEN,
      ]
  }
})

const groupColumns = [
  { key: 'message', label: 'Message', sortable: true, value: (g) => exceptionParts(g.latest).message, text: (g) => `${exceptionParts(g.latest).message} ${g.key}`, class: 'font-mono text-[12.5px]' },
  { key: 'kind', label: 'Kind', sortable: true, value: (g) => exceptionParts(g.latest).kind, text: (g) => exceptionParts(g.latest).kind },
  { key: 'count', label: 'Count', title: 'How many of the entries shown share this fingerprint', align: 'right', sortable: true, value: (g) => g.count, text: (g) => g.count },
  { key: 'last', label: 'Last occurrence', title: 'When the latest of them happened', align: 'right', sortable: true, value: (g) => g.latest.occurred_at, text: (g) => g.latest.occurred_at },
]

async function load() {
  try {
    const page = await api.entries(props.type, { limit: PAGE })
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
    const page = await api.entries(props.type, { limit: PAGE, before: next.value.before })
    const seen = new Set(entries.value.map((e) => `${e.batch_id}/${e.id}`))
    entries.value = entries.value.concat(page.data.filter((e) => !seen.has(`${e.batch_id}/${e.id}`)))
    next.value = page.next
  } catch (e) {
    error.value = e
  } finally {
    more.value = false
  }
}

async function refresh() {
  try {
    const page = await api.entries(props.type, { limit: PAGE })
    const seen = new Set(entries.value.map((e) => `${e.batch_id}/${e.id}`))
    const fresh = page.data.filter((e) => !seen.has(`${e.batch_id}/${e.id}`))
    if (fresh.length) entries.value = fresh.concat(entries.value)
  } catch {
    // The next tick tries again.
  }
}

onMounted(load)
usePolling(refresh)
</script>

<template>
  <div class="space-y-5">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">{{ title }}</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">Newest first, across every request and job kept.</p>
    </div>

    <ErrorBox :error="error" />

    <div v-if="stats.length" class="grid grid-cols-2 gap-3 sm:flex sm:flex-wrap">
      <StatTile v-for="[name, value] in stats" :key="name" :label="name" :value="value" class="sm:min-w-32" />
    </div>

    <section v-if="groups.length" class="lk-card">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">Grouped</h2>
        <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">by fingerprint, across the {{ entries.length }} shown · opens the issue</span>
      </header>
      <DataTable
        :columns="groupColumns"
        :rows="groups"
        :row-key="(g) => g.key"
        :to="(g) => issueRoute(type, g.key)"
        :initial-sort="{ key: 'count', dir: 'desc' }"
        filterable
        filter-placeholder="Filter exceptions…"
        label="Grouped exceptions"
      >
        <template #cell-message="{ row }">
          <span class="block truncate">{{ exceptionParts(row.latest).message }}</span>
          <span v-if="row.key !== row.latest.summary" class="block truncate text-xs text-zinc-500 dark:text-zinc-400">{{ row.key }}</span>
        </template>
        <template #cell-kind="{ row }">
          <Pill v-if="exceptionParts(row.latest).kind" tone="bad">{{ exceptionParts(row.latest).kind }}</Pill>
        </template>
        <template #cell-count="{ row }">
          <span class="inline-flex items-center gap-2">
            <span class="hidden h-1 w-16 overflow-hidden rounded-full bg-zinc-100 sm:inline-block dark:bg-zinc-800" aria-hidden="true">
              <span class="block h-full rounded-full bg-rose-500" :style="{ width: `${(row.count / topGroup) * 100}%` }" />
            </span>
            <b class="font-semibold">{{ row.count }}</b>
          </span>
        </template>
        <template #cell-last="{ row }"><span class="text-zinc-500 dark:text-zinc-400"><TimeAgo :at="row.latest.occurred_at" /></span></template>
      </DataTable>
    </section>

    <section class="lk-card">
      <header v-if="groups.length" class="lk-card-header">
        <h2 class="text-sm font-semibold">Every occurrence</h2>
        <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">opens the request or job it happened in</span>
      </header>
      <DataTable
        :columns="columns"
        :rows="entries"
        :row-key="rowKey"
        :to="toBatch"
        :loading="loading"
        filterable
        :filter-placeholder="`Filter these ${title.toLowerCase()}…`"
        :empty="`No ${title.toLowerCase()} recorded.`"
        :label="title"
      >
        <template #cell-sql="{ row }">{{ type === 'n_plus_one' ? nPlusOne(row).sql : row.summary }}</template>
        <template #cell-flags="{ row }"><Flags :slow="hasTag(row.tags, 'slow')" :tags="row.tags || []" /></template>
        <template #cell-batch="{ row }">
          <RouterLink
            :to="toBatch(row)"
            class="font-mono text-xs text-indigo-600 hover:underline dark:text-indigo-400"
          >{{ String(row.batch_id).slice(0, 8) }}</RouterLink>
        </template>
        <template #cell-count="{ row }"><b class="font-semibold">{{ nPlusOne(row).count ?? '—' }}</b></template>
        <template #cell-hit="{ row }">
          <Pill v-if="cacheOutcome(row) !== null" :tone="cacheOutcome(row) ? 'ok' : 'warn'">{{ cacheOutcome(row) ? 'hit' : 'miss' }}</Pill>
          <span v-else class="text-zinc-400">—</span>
        </template>
        <template #cell-status="{ row }">
          <Pill v-if="typeof httpParts(row).status === 'number'" :tone="statusTone(httpParts(row).status)">{{ httpParts(row).status }}</Pill>
          <Pill v-else-if="httpParts(row).status" tone="bad">{{ httpParts(row).status }}</Pill>
          <span v-else class="text-zinc-400">—</span>
        </template>
        <template #cell-level="{ row }">
          <Pill v-if="extra(row)" :tone="extra(row).tone">{{ extra(row).text }}</Pill>
          <span v-else class="text-zinc-400">—</span>
        </template>
        <template #cell-outcome="{ row }">
          <Pill v-if="jobParts(row).outcome" :tone="outcomeTone(jobParts(row).outcome)">{{ jobParts(row).outcome }}</Pill>
          <span v-else class="text-zinc-400">—</span>
        </template>
        <template #cell-kind="{ row }">
          <Pill v-if="exceptionParts(row).kind" tone="bad">{{ exceptionParts(row).kind }}</Pill>
          <span v-else class="text-zinc-400">—</span>
        </template>
        <template #cell-detail="{ row }">
          <Pill v-if="extra(row)" :tone="extra(row).tone"><span class="max-w-48 truncate">{{ extra(row).text }}</span></Pill>
          <span v-else class="text-zinc-400">—</span>
        </template>
        <template #cell-when="{ row }"><span class="text-zinc-500 dark:text-zinc-400"><TimeAgo :at="row.occurred_at" /></span></template>
      </DataTable>
      <LoadMore v-if="next" :loading="more" @more="loadMore" />
    </section>
  </div>
</template>
