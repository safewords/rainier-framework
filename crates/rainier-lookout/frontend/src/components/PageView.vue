<script setup>
// A page view in a visitor's browser (LOOKOUT_RUM): what the page itself did
// — its load, the routes it moved through, the errors it threw, the long
// tasks that froze it — as opposed to the API calls it made, which are
// listed below it and have pages of their own.
import { computed, onMounted, ref } from 'vue'

import BatchStatus from './BatchStatus.vue'
import ReplayPlayer from './ReplayPlayer.vue'
import Pill from './Pill.vue'
import TimeAgo from './TimeAgo.vue'
import { api } from '../api.js'
import { duration, itemRoute } from '../format.js'

const props = defineProps({ batch: { type: Object, required: true } })

const content = computed(() => {
  const e = props.batch.entries && props.batch.entries[0]
  return e && e.content && typeof e.content === 'object' ? e.content : {}
})
const activity = computed(() => content.value.activity || {})
const device = computed(() => content.value.device || {})
const page = computed(() => content.value.page || {})
const view = computed(() => {
  const tag = (props.batch.tags || []).find((t) => t.startsWith('pageview:'))
  return tag ? tag.slice('pageview:'.length) : null
})

const us = (ms) => (typeof ms === 'number' && isFinite(ms) ? Math.round(ms * 1000) : null)
const when = (ms) => (us(ms) === null ? '—' : duration(us(ms)))

const load = computed(() => activity.value.load || null)
const loadFacts = computed(() => {
  const l = load.value
  if (!l) return []
  return [
    ['First byte', l.ttfb],
    ['Ready', l.dom_ready],
    ['Loaded', l.load],
    ['DNS', l.dns],
    ['Connect', l.connect],
  ].filter(([, v]) => typeof v === 'number').map(([n, v]) => [n, when(v)])
    .concat(l.protocol ? [['Protocol', l.protocol]] : [])
    .concat(l.type ? [['How', l.type]] : [])
})
const vitals = computed(() => {
  const v = activity.value.vitals || page.value.vitals || {}
  return [
    ['Largest paint', v.lcp !== undefined ? when(v.lcp) : null],
    ['First paint', v.fcp !== undefined ? when(v.fcp) : null],
    ['Layout shift', v.cls !== undefined ? v.cls : null],
    ['Slowest input', v.inp !== undefined ? when(v.inp) : null],
  ].filter(([, v]) => v !== null)
})
const navigations = computed(() => (Array.isArray(activity.value.navigations) ? activity.value.navigations : []))
const errors = computed(() => (Array.isArray(activity.value.errors) ? activity.value.errors : []))
const longTasks = computed(() => activity.value.long_tasks || null)

const versioned = (o) => (o && o.name ? [o.name, o.version].filter(Boolean).join(' ') : null)
const deviceFacts = computed(() => {
  const d = device.value
  const c = d.connection || {}
  return [
    ['Browser', versioned(d.browser)],
    ['System', versioned(d.os) || d.platform],
    ['Device', d.kind],
    ['Connection', c.type ? [c.type, c.rtt !== undefined ? `${c.rtt} ms rtt` : null].filter(Boolean).join(' · ') : null],
    ['Screen', d.screen],
    ['Viewport', d.viewport],
    ['Address', d.ip],
    ['Language', d.language],
  ].filter(([, v]) => v !== null && v !== undefined && v !== '')
})

const session = computed(() => {
  const tag = (props.batch.tags || []).find((t) => t.startsWith('session:'))
  return tag ? tag.slice('session:'.length) : null
})

// The API calls this page view made, each with its own page — and its
// recording, when the visitor was recorded.
const calls = ref([])
const replays = ref([])
const callsLoaded = ref(false)
onMounted(async () => {
  if (!view.value) return
  try {
    const answer = await api.batches({ kind: 'browser', tag: `pageview:${view.value}`, limit: 200 })
    const isReplay = (b) => (b.tags || []).includes('replay')
    calls.value = answer.data.filter((b) => !isReplay(b))
    replays.value = answer.data.filter(isReplay).sort((a, b) => (a.started_at < b.started_at ? -1 : 1))
  } catch {
    calls.value = []
  } finally {
    callsLoaded.value = true
  }
})
</script>

<template>
  <ReplayPlayer v-if="replays.length" :ids="replays.map((r) => r.id)" title="Replay of this page view" class="mb-5" />
  <section class="lk-card">
    <header class="lk-card-header">
      <h2 class="text-sm font-semibold">Page view</h2>
      <RouterLink
        v-if="session"
        :to="{ name: 'session', params: { id: session } }"
        class="text-xs text-indigo-600 hover:underline dark:text-indigo-400"
      >its session</RouterLink>
      <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">what the page did in the visitor's browser — reported by it, not verified</span>
    </header>
    <div class="space-y-5 p-4">
      <p class="truncate font-mono text-[12.5px]">{{ content.url || batch.host }}</p>

      <div v-if="loadFacts.length">
        <h3 class="lk-th mb-1.5">Load</h3>
        <dl class="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4 lg:grid-cols-7">
          <div v-for="[name, value] in loadFacts" :key="name" class="min-w-0">
            <dt class="lk-th">{{ name }}</dt>
            <dd class="mt-0.5 text-sm tabular-nums">{{ value }}</dd>
          </div>
        </dl>
      </div>

      <div v-if="vitals.length">
        <h3 class="lk-th mb-1.5">Vitals</h3>
        <dl class="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4">
          <div v-for="[name, value] in vitals" :key="name" class="min-w-0">
            <dt class="lk-th">{{ name }}</dt>
            <dd class="mt-0.5 text-sm tabular-nums">{{ value }}</dd>
          </div>
        </dl>
      </div>

      <div v-if="navigations.length">
        <h3 class="lk-th mb-1.5">Route changes</h3>
        <ol class="space-y-1 text-sm">
          <li v-for="(n, i) in navigations" :key="i" class="flex gap-3">
            <span class="w-20 shrink-0 text-xs text-zinc-500 tabular-nums dark:text-zinc-400">{{ when(n.at) }}</span>
            <span class="font-mono text-[12.5px]">{{ n.to }}</span>
          </li>
        </ol>
      </div>

      <div v-if="errors.length">
        <h3 class="lk-th mb-1.5">Script errors</h3>
        <ul class="space-y-2">
          <li v-for="(e, i) in errors" :key="i" class="rounded-lg border border-rose-200 bg-rose-50 px-3 py-2 text-sm dark:border-rose-500/30 dark:bg-rose-500/10">
            <p class="font-medium text-rose-700 dark:text-rose-300">{{ e.message }}</p>
            <p class="mt-0.5 font-mono text-[11.5px] text-zinc-500 dark:text-zinc-400">
              <template v-if="e.source">{{ e.source }}<template v-if="e.line">:{{ e.line }}<template v-if="e.col">:{{ e.col }}</template></template></template>
              <template v-if="e.page"> · on {{ e.page }}</template>
              · {{ when(e.at) }} after the page opened
            </p>
          </li>
        </ul>
      </div>

      <div v-if="longTasks">
        <h3 class="lk-th mb-1.5">Long tasks</h3>
        <p class="text-sm">
          {{ longTasks.count }} time{{ longTasks.count === 1 ? '' : 's' }} the page froze for 50 ms or more —
          {{ when(longTasks.total) }} in all, the longest {{ when(longTasks.longest) }}.
        </p>
      </div>

      <div v-if="deviceFacts.length">
        <h3 class="lk-th mb-1.5">Device</h3>
        <dl class="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4">
          <div v-for="[name, value] in deviceFacts" :key="name" class="min-w-0">
            <dt class="lk-th">{{ name }}</dt>
            <dd class="mt-0.5 text-sm break-words">{{ value }}</dd>
          </div>
        </dl>
      </div>
    </div>

    <div class="border-t border-zinc-100 dark:border-zinc-800">
      <h3 class="lk-th px-4 pt-3">API calls this page made</h3>
      <p v-if="callsLoaded && !calls.length" class="px-4 py-3 text-sm text-zinc-500 dark:text-zinc-400">
        None reported — only a sampled page view reports its calls, and a slow call from any.
      </p>
      <ul class="divide-y divide-zinc-100 dark:divide-zinc-800">
        <li v-for="c in calls" :key="c.id">
          <RouterLink
            :to="itemRoute(c)"
            class="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2.5 hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
          >
            <Pill tone="accent">API call</Pill>
            <span class="min-w-0 flex-1 truncate font-mono text-[13px]">{{ c.summary }}</span>
            <BatchStatus :batch="c" />
            <span class="text-xs text-zinc-500 tabular-nums dark:text-zinc-400">{{ duration(c.duration_us) }}</span>
            <span class="text-xs text-zinc-500 dark:text-zinc-400"><TimeAgo :at="c.started_at" /></span>
          </RouterLink>
        </li>
      </ul>
    </div>
  </section>
</template>
