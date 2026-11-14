<script setup>
import { computed, onMounted, ref } from 'vue'

import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Flags from '../components/Flags.vue'
import LoadMore from '../components/LoadMore.vue'
import Pill from '../components/Pill.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { duration, hasTag, isNPlusOne, statusTone, TYPES } from '../format.js'
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
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">Newest first, across every batch kept.</p>
    </div>

    <ErrorBox :error="error" />

    <div v-if="stats.length" class="grid grid-cols-2 gap-3 sm:flex sm:flex-wrap">
      <div v-for="[name, value] in stats" :key="name" class="lk-card px-4 py-3 sm:min-w-32">
        <div class="lk-th">{{ name }}</div>
        <div class="mt-0.5 text-2xl font-semibold tabular-nums">{{ value }}</div>
      </div>
    </div>

    <section v-if="groups.length" class="lk-card">
      <header class="lk-card-header">
        <h2 class="text-sm font-semibold">Grouped</h2>
        <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">by fingerprint, across the {{ entries.length }} shown</span>
      </header>
      <ul class="divide-y divide-zinc-100 dark:divide-zinc-800">
        <li v-for="g in groups" :key="g.key">
          <RouterLink
            :to="{ name: 'batch', params: { id: g.latest.batch_id } }"
            class="block px-4 py-2.5 hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
          >
            <div class="flex items-baseline gap-3">
              <span class="min-w-0 flex-1 truncate font-mono text-[13px]" :title="g.latest.summary">{{ g.latest.summary }}</span>
              <span class="text-sm font-semibold tabular-nums">{{ g.count }}×</span>
              <span class="hidden text-xs text-zinc-500 sm:inline dark:text-zinc-400"><TimeAgo :at="g.latest.occurred_at" /></span>
            </div>
            <div v-if="g.key !== g.latest.summary" class="truncate font-mono text-xs text-zinc-500 dark:text-zinc-400">{{ g.key }}</div>
            <div class="mt-1.5 h-1 overflow-hidden rounded-full bg-zinc-100 dark:bg-zinc-800">
              <div class="h-full rounded-full bg-rose-500" :style="{ width: `${(g.count / topGroup) * 100}%` }" />
            </div>
          </RouterLink>
        </li>
      </ul>
    </section>

    <section class="lk-card">
      <ul v-if="entries.length" class="divide-y divide-zinc-100 dark:divide-zinc-800">
        <li v-for="e in entries" :key="`${e.batch_id}/${e.id}`">
          <RouterLink
            :to="{ name: 'batch', params: { id: e.batch_id } }"
            class="block px-4 py-2.5 hover:bg-zinc-50 md:flex md:items-center md:gap-3 dark:hover:bg-zinc-800/50"
          >
            <span class="block min-w-0 truncate font-mono text-[13px] md:flex-1" :title="e.summary">{{ e.summary }}</span>
            <span class="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-zinc-500 md:mt-0 md:flex-nowrap dark:text-zinc-400">
              <Pill v-if="extra(e)" :tone="extra(e).tone"><span class="max-w-48 truncate">{{ extra(e).text }}</span></Pill>
              <Flags :exception="e.type === 'exception'" :slow="hasTag(e.tags, 'slow')" :tags="e.tags" />
              <span class="tabular-nums md:w-16 md:text-right">{{ duration(e.duration_us) }}</span>
              <span class="md:w-16 md:text-right"><TimeAgo :at="e.occurred_at" /></span>
            </span>
          </RouterLink>
        </li>
      </ul>
      <Empty v-else>{{ loading ? 'Loading…' : `No ${title.toLowerCase()} recorded.` }}</Empty>
      <LoadMore v-if="next" :loading="more" @more="loadMore" />
    </section>
  </div>
</template>
