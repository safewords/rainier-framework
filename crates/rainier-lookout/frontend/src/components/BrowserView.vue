<script setup>
// A request as the browser that made it saw it — a real user monitoring
// report (LOOKOUT_RUM): where the time went before, around and after the
// server's part, and the device, connection and page it happened on.
import { computed, onMounted, ref, watch } from 'vue'

import { api } from '../api.js'
import { duration } from '../format.js'

const props = defineProps({
  // The browser batch: its id, or the batch itself when already loaded.
  id: { type: String, default: null },
  batch: { type: Object, default: null },
  // The server's own time for the request, when known, in microseconds.
  serverUs: { type: Number, default: null },
})

const loaded = ref(props.batch)
const failed = ref(false)

async function load() {
  if (props.batch) {
    loaded.value = props.batch
    return
  }
  if (!props.id) return
  try {
    loaded.value = (await api.batch(props.id)).data
  } catch {
    failed.value = true
  }
}
onMounted(load)
watch(() => [props.id, props.batch], load)

const content = computed(() => {
  const e = loaded.value && loaded.value.entries && loaded.value.entries[0]
  return e && e.content && typeof e.content === 'object' ? e.content : {}
})
const timing = computed(() => content.value.timing || {})
const page = computed(() => content.value.page || {})
const device = computed(() => content.value.device || {})

const ms = (v) => (typeof v === 'number' && isFinite(v) ? v : 0)
const us = (v) => Math.round(ms(v) * 1000)

// The phases, in the order they happen. A phase the browser withheld (no
// Timing-Allow-Origin for this page, or a reused connection) is zero and
// simply does not appear.
const PHASES = [
  { key: 'queued', label: 'Queued', color: 'bg-zinc-400 dark:bg-zinc-500' },
  { key: 'dns', label: 'DNS', color: 'bg-sky-500' },
  { key: 'connect', label: 'Connect', color: 'bg-amber-500' },
  { key: 'tls', label: 'TLS', color: 'bg-orange-500' },
  { key: 'waiting', label: 'Waiting', color: 'bg-indigo-500' },
  { key: 'download', label: 'Download', color: 'bg-emerald-500' },
]
const total = computed(() => ms(timing.value.duration))
const phases = computed(() => {
  const t = timing.value
  const all = PHASES.map((p) => ({ ...p, ms: ms(t[p.key]) })).filter((p) => p.ms > 0)
  const sum = all.reduce((a, p) => a + p.ms, 0) || 1
  return all.map((p) => ({ ...p, share: (p.ms / Math.max(sum, total.value || sum)) * 100 }))
})
const detailed = computed(() => phases.value.some((p) => p.key !== 'queued'))

// The browser's total less the server's own time: the network both ways,
// the queueing, the handshake — everything the server never sees.
const outside = computed(() => {
  if (props.serverUs === null || !total.value) return null
  return Math.max(0, us(total.value) - props.serverUs)
})

const versioned = (o) => (o && o.name ? [o.name, o.version].filter(Boolean).join(' ') : null)
const size = (bytes) => {
  if (typeof bytes !== 'number' || bytes <= 0) return null
  return bytes < 1024 ? `${bytes} B` : bytes < 1048576 ? `${(bytes / 1024).toFixed(1)} KB` : `${(bytes / 1048576).toFixed(1)} MB`
}

const requestFacts = computed(() => {
  const t = timing.value
  return [
    ['In the browser', total.value ? duration(us(total.value)) : null],
    ['On the server', props.serverUs !== null ? duration(props.serverUs) : null],
    ['Outside the server', outside.value !== null ? duration(outside.value) : null],
    ['Protocol', t.protocol],
    ['Transferred', t.transfer === 0 && t.decoded > 0 ? 'from cache' : size(t.transfer)],
    ['Body', size(t.decoded)],
    ['Started by', t.initiator],
  ].filter(([, v]) => v !== null && v !== undefined && v !== '')
})

const deviceFacts = computed(() => {
  const d = device.value
  const c = d.connection || {}
  return [
    ['Browser', versioned(d.browser)],
    ['System', versioned(d.os) || d.platform],
    ['Device', d.kind || (d.mobile === true ? 'mobile' : null)],
    ['Connection', c.type ? [c.type, c.rtt !== undefined ? `${c.rtt} ms rtt` : null, c.downlink ? `${c.downlink} Mb/s` : null].filter(Boolean).join(' · ') : null],
    ['Data saver', c.save_data ? 'on' : null],
    ['Screen', d.screen ? `${d.screen}${d.dpr ? ` @${d.dpr}x` : ''}` : null],
    ['Viewport', d.viewport],
    ['Memory', d.memory ? `${d.memory} GB` : null],
    ['Cores', d.cores],
    ['Touch points', d.touch || null],
    ['Address', d.ip],
    ['Language', d.language],
    ['Time zone', d.timezone],
  ].filter(([, v]) => v !== null && v !== undefined && v !== '')
})

const pageFacts = computed(() => {
  const p = page.value
  const v = p.vitals || {}
  const load = p.load || {}
  return [
    ['Page', p.url],
    ['Came from', p.referrer],
    ['Page age', p.age ? duration(us(p.age)) : null],
    ['Largest paint', v.lcp ? duration(us(v.lcp)) : null],
    ['First paint', v.fcp ? duration(us(v.fcp)) : null],
    ['Layout shift', v.cls !== undefined ? v.cls : null],
    ['Slowest input', v.inp ? duration(us(v.inp)) : null],
    ['Page TTFB', load.ttfb ? duration(us(load.ttfb)) : null],
  ].filter(([, v]) => v !== null && v !== undefined && v !== '')
})
</script>

<template>
  <section class="lk-card">
    <header class="lk-card-header">
      <h2 class="text-sm font-semibold">In the browser</h2>
      <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">reported by the visitor's browser — shown, not verified</span>
    </header>
    <p v-if="failed" class="p-4 text-sm text-zinc-500">This report could not be loaded.</p>
    <div v-else-if="loaded" class="space-y-5 p-4">
      <div v-if="phases.length">
        <div class="flex h-3 w-full overflow-hidden rounded-full bg-zinc-100 dark:bg-zinc-800" role="img" :aria-label="phases.map((p) => `${p.label} ${Math.round(p.ms)} ms`).join(', ')">
          <div v-for="p in phases" :key="p.key" :class="p.color" :style="{ width: `${p.share}%` }" :title="`${p.label}: ${duration(us(p.ms))}`" />
        </div>
        <ul class="mt-2 flex flex-wrap gap-x-4 gap-y-1 text-xs text-zinc-600 tabular-nums dark:text-zinc-300">
          <li v-for="p in phases" :key="p.key" class="flex items-center gap-1.5">
            <span class="inline-block size-2 rounded-full" :class="p.color" />{{ p.label }} <b class="font-semibold">{{ duration(us(p.ms)) }}</b>
          </li>
        </ul>
        <p v-if="!detailed" class="mt-2 text-xs text-zinc-500 dark:text-zinc-400">
          The browser withheld the phases: the API's <code>Timing-Allow-Origin</code> does not name this page's origin
          (<code>LOOKOUT_RUM_ORIGINS</code>).
        </p>
      </div>

      <dl class="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-3 lg:grid-cols-4">
        <div v-for="[name, value] in requestFacts" :key="name" class="min-w-0">
          <dt class="lk-th">{{ name }}</dt>
          <dd class="mt-0.5 text-sm break-words tabular-nums">{{ value }}</dd>
        </div>
      </dl>

      <div v-if="deviceFacts.length">
        <h3 class="lk-th mb-1.5">Device and connection</h3>
        <dl class="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-3 lg:grid-cols-4">
          <div v-for="[name, value] in deviceFacts" :key="name" class="min-w-0">
            <dt class="lk-th">{{ name }}</dt>
            <dd class="mt-0.5 text-sm break-words">{{ value }}</dd>
          </div>
        </dl>
      </div>

      <div v-if="pageFacts.length">
        <h3 class="lk-th mb-1.5">The page</h3>
        <dl class="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-3 lg:grid-cols-4">
          <div v-for="[name, value] in pageFacts" :key="name" class="min-w-0">
            <dt class="lk-th">{{ name }}</dt>
            <dd class="mt-0.5 text-sm break-words">{{ value }}</dd>
          </div>
        </dl>
      </div>
    </div>
    <p v-else class="p-4 text-sm text-zinc-500">Loading…</p>
  </section>
</template>
