<script setup>
// Browser sessions (LOOKOUT_RUM): page views grouped by the session they
// belong to — the application's own, through `sessions_with`, or the
// browser tab's — newest first. A session's page lays out everything in it
// as one timeline.
import { computed, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import Empty from '../components/Empty.vue'
import ErrorBox from '../components/ErrorBox.vue'
import Pill from '../components/Pill.vue'
import TimeAgo from '../components/TimeAgo.vue'
import { api } from '../api.js'
import { duration } from '../format.js'
import { usePolling } from '../live.js'

const route = useRoute()
const router = useRouter()
const user = ref(typeof route.query.user === 'string' ? route.query.user : '')
const batches = ref([])
const error = ref(null)
const loading = ref(true)

async function load() {
  try {
    const params = { limit: 200 }
    if (user.value.trim()) params.user = user.value.trim()
    const [pages, replays] = await Promise.all([
      api.batches({ ...params, kind: 'page' }),
      api.batches({ ...params, kind: 'browser', tag: 'replay' }),
    ])
    batches.value = pages.data.concat(replays.data)
    error.value = null
  } catch (e) {
    error.value = e
  } finally {
    loading.value = false
  }
}
onMounted(load)
usePolling(load, 15_000)
watch(user, () => {
  router.replace({ query: user.value.trim() ? { user: user.value.trim() } : {} })
})

const sessionOf = (b) => {
  const tag = (b.tags || []).find((t) => t.startsWith('session:'))
  return tag ? tag.slice('session:'.length) : null
}

const sessions = computed(() => {
  const by = new Map()
  for (const b of batches.value) {
    const id = sessionOf(b)
    if (!id) continue
    const s = by.get(id) || { id, users: new Set(), pages: 0, errors: 0, replays: 0, first: b.started_at, last: b.started_at, paths: [] }
    if (b.user_id) s.users.add(b.user_id)
    if (b.kind === 'page') {
      s.pages++
      if (b.has_exception) s.errors++
      const path = (b.summary || '').replace(/^page /, '').split(' — ')[0]
      if (path && !s.paths.includes(path)) s.paths.push(path)
    } else {
      s.replays++
    }
    if (b.started_at < s.first) s.first = b.started_at
    if (b.started_at > s.last) s.last = b.started_at
    by.set(id, s)
  }
  return [...by.values()].sort((a, b) => (a.last < b.last ? 1 : -1))
})
const span = (s) => duration(Math.max(0, Date.parse(s.last) - Date.parse(s.first)) * 1000)
</script>

<template>
  <div class="space-y-5">
    <div>
      <h1 class="text-xl font-semibold tracking-tight">Sessions</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        Visitors' page views, grouped by session, newest first — open one to follow it as a timeline.
      </p>
    </div>
    <div class="flex flex-wrap items-center gap-2">
      <input v-model="user" class="lk-input lg:w-48" type="search" placeholder="User id" aria-label="User id" />
    </div>
    <ErrorBox :error="error" />
    <Empty v-if="!loading && !sessions.length && !error">
      No sessions yet. They need real user monitoring on (LOOKOUT_RUM) and the page script loaded.
    </Empty>
    <section v-if="sessions.length" class="lk-card">
      <ul class="divide-y divide-zinc-100 dark:divide-zinc-800">
        <li v-for="s in sessions" :key="s.id">
          <RouterLink
            :to="{ name: 'session', params: { id: s.id } }"
            class="flex flex-wrap items-center gap-x-4 gap-y-1 px-4 py-3 hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
          >
            <span class="font-mono text-[12.5px] text-zinc-500 dark:text-zinc-400">{{ s.id.slice(0, 10) }}</span>
            <span v-if="s.users.size" class="text-sm font-medium">user {{ [...s.users].join(', ') }}</span>
            <span v-else class="text-sm text-zinc-500 dark:text-zinc-400">anonymous</span>
            <span class="min-w-0 flex-1 truncate font-mono text-[12.5px]">{{ s.paths.slice(0, 4).join(' → ') }}</span>
            <Pill>{{ s.pages }} page view{{ s.pages === 1 ? '' : 's' }}</Pill>
            <Pill v-if="s.errors" tone="bad">{{ s.errors }} with errors</Pill>
            <Pill v-if="s.replays" tone="accent">replay</Pill>
            <span class="text-xs text-zinc-500 tabular-nums dark:text-zinc-400">{{ span(s) }}</span>
            <span class="text-xs text-zinc-500 dark:text-zinc-400"><TimeAgo :at="s.last" /></span>
          </RouterLink>
        </li>
      </ul>
    </section>
  </div>
</template>
