<script setup>
import { computed, onMounted, ref } from 'vue'

import BatchStatus from '../components/BatchStatus.vue'
import BrowserView from '../components/BrowserView.vue'
import ClientCard from '../components/ClientCard.vue'
import PageView from '../components/PageView.vue'
import ReplayPlayer from '../components/ReplayPlayer.vue'
import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Flags from '../components/Flags.vue'
import Json from '../components/Json.vue'
import Pill from '../components/Pill.vue'
import StackTrace from '../components/StackTrace.vue'
import TimeAgo from '../components/TimeAgo.vue'
import UserCard from '../components/UserCard.vue'
import Waterfall from '../components/Waterfall.vue'
import { api } from '../api.js'
import { absolute, caseId, duration, hasTag, issueRoute, itemRoute } from '../format.js'

const props = defineProps({ id: { type: String, required: true } })

const batch = ref(null)
const related = ref([])
const error = ref(null)
const loading = ref(true)
const selected = ref(null)

const KIND = {
  request: 'Request',
  job: 'Job',
  command: 'Command',
  scheduled_task: 'Scheduled task',
  other: 'Other',
  browser: 'API call (browser)',
  page: 'Page view',
}

// The page view an API call report came from, by its `pageview:` tag.
const pageViewTag = computed(() => (batch.value ? (batch.value.tags || []).find((t) => t.startsWith('pageview:')) : null))

// The request's own record: who made it and what with, when Lookout was
// asked to keep them (LOOKOUT_CAPTURE_USER, LOOKOUT_CAPTURE_CLIENT).
const rootContent = computed(() => {
  const root = batch.value ? batch.value.entries.find((e) => e.type === 'request') : null
  return root && root.content && typeof root.content === 'object' ? root.content : {}
})
const user = computed(() => (rootContent.value.user && typeof rootContent.value.user === 'object' ? rootContent.value.user : null))
const client = computed(() => (rootContent.value.client && typeof rootContent.value.client === 'object' ? rootContent.value.client : null))

// How the browser saw it (LOOKOUT_RUM): reports on the same trace. On a
// browser batch, the request it describes, for the server's side of it.
const isReplay = (b) => (b.tags || []).includes('replay')
const browserReports = computed(() => related.value.filter((b) => b.kind === 'browser' && !isReplay(b)))
// Session replays on the same trace (LOOKOUT_RUM_REPLAY): what the page
// looked like before this request failed or was slow.
const replays = computed(() => related.value.filter((b) => b.kind === 'browser' && isReplay(b)))
const serverSide = computed(() => related.value.find((b) => b.kind === 'request') || null)

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
    ['Id', b.id, true],
    ['Trace', b.trace_id, true],
  ]
})

const counts = computed(() => {
  const out = {}
  for (const e of batch.value ? batch.value.entries : []) out[e.type] = (out[e.type] || 0) + 1
  return Object.entries(out)
})

// What the request carried and what it answered, when Lookout was asked to
// keep them (LOOKOUT_CAPTURE_REQUEST_HEADERS / _REQUEST_BODY / _RESPONSE_BODY),
// already redacted on the server.
const payload = computed(() => {
  const root = batch.value ? batch.value.entries.find((e) => e.type === 'request') : null
  const c = root && root.content && typeof root.content === 'object' ? root.content : {}
  const parts = [
    ['Request headers', c['request.headers']],
    ['Request body', c['request.body']],
    ['Response body', c['response.body']],
  ].filter(([, value]) => value !== undefined && value !== null)
  return parts
})

const entry = computed(() =>
  batch.value && selected.value ? batch.value.entries.find((e) => e.id === selected.value) : null,
)

/** What an exception entry says about itself: message, cause chain, stack, and its case. */
const failure = computed(() => {
  const e = entry.value
  if (!e || e.type !== 'exception') return null
  const c = e.content && typeof e.content === 'object' ? e.content : {}
  const ex = c.exception && typeof c.exception === 'object' ? c.exception : {}
  const key = e.family_hash || e.summary
  return {
    key,
    caseId: caseId(key),
    kind: typeof c.kind_label === 'string' ? c.kind_label : null,
    message: typeof ex.message === 'string' ? ex.message : null,
    sources: Array.isArray(ex.sources) ? ex.sources.map(String) : [],
    frames: Array.isArray(c.frames) ? c.frames : [],
    backtrace: typeof ex.backtrace === 'string' ? ex.backtrace : '',
  }
})

/** Where a slow span was called from, when Lookout captured it. */
const calledFrom = computed(() => {
  const e = entry.value
  const trace = e && e.content && typeof e.content === 'object' ? e.content['code.trace'] : null
  return Array.isArray(trace) && trace.length ? trace : null
})

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
      <h1 class="text-xl font-semibold">{{ error.status === 404 ? 'Not found' : 'Could not load this' }}</h1>
      <p v-if="error.status === 404" class="text-sm text-zinc-500 dark:text-zinc-400">It may have been pruned, or never kept.</p>
      <ErrorBox v-else :error="error" />
    </template>

    <Empty v-else-if="loading">Loading…</Empty>

    <template v-else-if="batch">
      <div>
        <p class="lk-th">{{ KIND[batch.kind] || batch.kind }}</p>
        <h1 class="mt-1 font-mono text-lg font-semibold break-all sm:text-xl">{{ batch.summary }}</h1>
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

      <UserCard
        v-if="batch.kind === 'request' && (user || batch.user_id)"
        :user="user"
        :user-id="batch.user_id"
        :guard="batch.guard"
      />
      <ClientCard v-if="client" :client="client" />
      <PageView v-if="batch.kind === 'page'" :batch="batch" />
      <!-- An API call from the browser, and the request it was on the
           server: kept because it was slow or failed, or paired when the
           browser reported it. Side by side, so the two times compare. -->
      <section v-if="batch.kind === 'browser' && !isReplay(batch) && serverSide" class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">The request on the server</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">the same trace</span>
        </header>
        <RouterLink
          :to="itemRoute(serverSide)"
          class="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-3 hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
        >
          <span class="min-w-0 flex-1 truncate font-mono text-[13px]">{{ serverSide.summary }}</span>
          <BatchStatus :batch="serverSide" />
          <span class="text-sm tabular-nums">{{ duration(serverSide.duration_us) }} on the server</span>
          <span class="text-sm text-zinc-500 tabular-nums dark:text-zinc-400">of {{ duration(batch.duration_us) }} in the browser</span>
          <span class="text-xs text-indigo-600 dark:text-indigo-400">queries and timeline →</span>
        </RouterLink>
      </section>
      <p v-if="batch.kind === 'browser' && pageViewTag" class="text-sm text-zinc-500 dark:text-zinc-400">
        Made by a page view:
        <RouterLink
          :to="{ name: 'batches', query: { kind: 'page', tag: pageViewTag } }"
          class="text-indigo-600 hover:underline dark:text-indigo-400"
        >what that page did</RouterLink>.
      </p>
      <template v-if="batch.kind === 'browser' && isReplay(batch)">
        <ReplayPlayer :batch="batch" />
      </template>
      <template v-else-if="batch.kind === 'browser'">
        <p v-if="!serverSide" class="text-sm text-zinc-500 dark:text-zinc-400">
          The server's side of this request was not kept (only slow and failed requests are).
          <template v-if="batch.route_name">
            Its route:
            <RouterLink :to="issueRoute('request', batch.route_name)" class="text-indigo-600 hover:underline dark:text-indigo-400">{{ batch.route_name }}</RouterLink>.
          </template>
        </p>
        <BrowserView :batch="batch" :server-us="serverSide ? serverSide.duration_us : null" />
      </template>
      <template v-else>
        <BrowserView v-for="report in browserReports" :key="report.id" :id="report.id" :server-us="batch.duration_us" />
      </template>
      <ReplayPlayer v-for="r in (batch.kind === 'browser' && isReplay(batch) ? [] : replays)" :key="r.id" :id="r.id" />

      <section v-if="payload.length" class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Payload</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">redacted: sensitive fields hidden, secret-looking values starred</span>
        </header>
        <div class="space-y-4 p-4">
          <div v-for="[label, value] in payload" :key="label">
            <h3 class="lk-th mb-1.5">{{ label }}</h3>
            <Json :value="value" />
          </div>
        </div>
      </section>

      <section v-if="related.length" class="lk-card">
        <header class="lk-card-header">
          <h2 class="text-sm font-semibold">Related</h2>
          <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">the same trace — what this queued, or what queued it</span>
        </header>
        <ul class="divide-y divide-zinc-100 dark:divide-zinc-800">
          <li v-for="other in related" :key="other.id">
            <RouterLink
              :to="itemRoute(other)"
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
          <div v-if="failure" class="space-y-3 rounded-lg border border-rose-200 p-3 dark:border-rose-500/30">
            <div class="flex flex-wrap items-center gap-2">
              <Pill v-if="failure.kind" tone="bad">{{ failure.kind }}</Pill>
              <span class="flex-1" />
              <RouterLink
                :to="issueRoute('exception', failure.key)"
                class="text-sm text-indigo-600 hover:underline dark:text-indigo-400"
              >Issue →</RouterLink>
              <RouterLink
                :to="{ name: 'case', params: { id: failure.caseId } }"
                class="text-sm text-indigo-600 hover:underline dark:text-indigo-400"
              >Case →</RouterLink>
            </div>
            <p v-if="failure.message" class="font-mono text-[13px] break-words whitespace-pre-wrap">{{ failure.message }}</p>
            <ol v-if="failure.sources.length" class="space-y-1">
              <li v-for="(s, i) in failure.sources" :key="i" class="font-mono text-xs break-words text-zinc-600 dark:text-zinc-400">↳ {{ s }}</li>
            </ol>
            <div>
              <h3 class="lk-th mb-1.5">Stack</h3>
              <StackTrace :frames="failure.frames" :backtrace="failure.backtrace" />
            </div>
          </div>
          <div v-if="calledFrom">
            <h3 class="lk-th mb-1.5">Called from</h3>
            <StackTrace :frames="calledFrom" />
          </div>
          <div v-if="entry.family_hash && entry.type !== 'exception'">
            <RouterLink
              :to="issueRoute(entry.type, entry.family_hash)"
              class="text-sm text-indigo-600 hover:underline dark:text-indigo-400"
            >Every {{ entry.type }} like this →</RouterLink>
          </div>
          <Json :value="entry.content" />
        </div>
      </section>
    </template>
  </div>
</template>
