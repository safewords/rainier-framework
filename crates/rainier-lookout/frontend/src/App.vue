<script setup>
import { computed, onBeforeUnmount, provide, ref, watch } from 'vue'
import { useRoute } from 'vue-router'

import { api } from './api.js'
import { config } from './config.js'
import { createLive } from './live.js'

const live = createLive()
const now = ref(Date.now())
const ticker = setInterval(() => (now.value = Date.now()), 10_000)
onBeforeUnmount(() => clearInterval(ticker))
provide('live', live)
provide('now', now)

// Bumped after a clear, so the current view remounts and reloads.
const generation = ref(0)

const route = useRoute()
const menu = ref(false)
watch(() => route.fullPath, () => (menu.value = false))

const sections = [
  {
    title: null,
    items: [
      { label: 'Dashboard', to: { name: 'dashboard' }, match: (r) => r.name === 'dashboard' },
    ],
  },
  {
    title: 'Batches',
    items: [
      ['request', 'Requests'],
      ['job', 'Jobs'],
      ['command', 'Commands'],
      ['scheduled_task', 'Scheduled tasks'],
      ['other', 'Other'],
    ].map(([kind, label]) => ({
      label,
      to: { name: 'batches', query: { kind } },
      match: (r) => r.name === 'batches' && (r.query.kind || 'request') === kind,
    })),
  },
  {
    title: 'Entries',
    items: [
      ['exception', 'Exceptions'],
      ['query', 'Queries'],
      ['slow', 'Slow'],
      ['n_plus_one', 'N+1 queries'],
      ['http_client', 'Outgoing HTTP'],
      ['cache', 'Cache'],
      ['job', 'Queued jobs'],
      ['log', 'Logs'],
      ['mail', 'Mail'],
      ['notification', 'Notifications'],
      ['event', 'Events'],
      ['model', 'Models'],
      ['view', 'Views'],
      ['auth_attempt', 'Auth attempts'],
    ].map(([type, label]) => ({
      label,
      to: { name: 'entries', params: { type } },
      match: (r) => r.name === 'entries' && r.params.type === type,
    })),
  },
  {
    title: 'Lookout',
    items: [{ label: 'Status', to: { name: 'status' }, match: (r) => r.name === 'status' }],
  },
]

const clearing = ref(false)
const notice = ref(null)
async function clear() {
  if (!window.confirm('Delete everything Lookout has recorded? This cannot be undone.')) return
  clearing.value = true
  notice.value = null
  try {
    await api.clear()
    notice.value = { tone: 'ok', text: 'Cleared.' }
    generation.value += 1
  } catch (error) {
    notice.value = { tone: 'bad', text: error.message }
  } finally {
    clearing.value = false
  }
}

const environment = computed(() => config.environment)
</script>

<template>
  <div class="min-h-dvh">
    <header
      class="sticky top-0 z-30 flex h-14 items-center gap-3 border-b border-zinc-200 bg-white/90 px-4 backdrop-blur dark:border-zinc-800 dark:bg-zinc-900/90"
    >
      <button
        type="button"
        class="lk-btn -ml-1 px-2 lg:hidden"
        :aria-expanded="menu"
        aria-controls="lookout-nav"
        aria-label="Menu"
        @click="menu = !menu"
      >
        <svg viewBox="0 0 20 20" class="size-4" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
          <path v-if="!menu" d="M3 5h14M3 10h14M3 15h14" />
          <path v-else d="M5 5l10 10M15 5L5 15" />
        </svg>
      </button>
      <RouterLink :to="{ name: 'dashboard' }" class="flex items-center gap-2 font-semibold tracking-tight">
        <svg viewBox="0 0 24 24" class="size-6 text-indigo-600 dark:text-indigo-400" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
          <circle cx="12" cy="12" r="9" />
          <circle cx="12" cy="12" r="3.5" fill="currentColor" stroke="none" />
          <path d="M12 1.5v4M12 18.5v4M1.5 12h4M18.5 12h4" />
        </svg>
        Lookout
      </RouterLink>
      <span
        v-if="environment"
        class="hidden rounded-full border border-zinc-200 px-2 py-0.5 text-xs text-zinc-500 sm:inline dark:border-zinc-700 dark:text-zinc-400"
      >{{ environment }}</span>
      <span class="flex-1" />
      <span
        v-if="notice"
        class="hidden text-xs sm:inline"
        :class="notice.tone === 'ok' ? 'text-emerald-600 dark:text-emerald-400' : 'text-rose-600 dark:text-rose-400'"
      >{{ notice.text }}</span>
      <button type="button" class="lk-btn" :aria-pressed="live" @click="live = !live">
        <span class="size-2 rounded-full" :class="live ? 'animate-pulse bg-emerald-500' : 'bg-zinc-400 dark:bg-zinc-600'" />
        Live
      </button>
      <button
        type="button"
        class="lk-btn text-rose-600 hover:bg-rose-50 dark:text-rose-400 dark:hover:bg-rose-500/10"
        :disabled="clearing"
        @click="clear"
      >Clear</button>
    </header>

    <div class="lg:grid lg:grid-cols-[14rem_minmax(0,1fr)]">
      <div v-if="menu" class="fixed inset-0 top-14 z-10 bg-zinc-950/30 lg:hidden" aria-hidden="true" @click="menu = false" />
      <nav
        id="lookout-nav"
        aria-label="Lookout"
        class="fixed inset-y-0 top-14 left-0 z-20 w-64 overflow-y-auto border-r border-zinc-200 bg-white px-3 py-4 transition-transform lg:sticky lg:z-0 lg:h-[calc(100dvh-3.5rem)] lg:w-auto lg:translate-x-0 dark:border-zinc-800 dark:bg-zinc-900"
        :class="menu ? 'translate-x-0' : '-translate-x-full'"
      >
        <div v-for="(section, s) in sections" :key="s" class="mb-4">
          <h3 v-if="section.title" class="mb-1 px-2 text-[11px] font-semibold tracking-wider text-zinc-400 uppercase dark:text-zinc-500">
            {{ section.title }}
          </h3>
          <RouterLink
            v-for="item in section.items"
            :key="item.label"
            :to="item.to"
            class="block rounded-md px-2 py-1.5 text-sm transition"
            :class="item.match(route)
              ? 'bg-indigo-50 font-semibold text-indigo-700 dark:bg-indigo-500/10 dark:text-indigo-300'
              : 'text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-800'"
            :aria-current="item.match(route) ? 'page' : undefined"
          >{{ item.label }}</RouterLink>
        </div>
      </nav>

      <main class="min-w-0 px-4 py-6 sm:px-6 lg:px-8">
        <RouterView :key="`${generation}:${route.fullPath}`" />
        <p class="mt-8 text-xs text-zinc-400 dark:text-zinc-500">
          Lookout<template v-if="config.store"> · store: {{ config.store }}</template>
        </p>
      </main>
    </div>
  </div>
</template>
