<script setup>
// A session replay (LOOKOUT_RUM_REPLAY): the seconds of the page before a
// request failed, was slow, or the page threw — as recorded, masked: text is
// asterisks, inputs empty, media blank boxes. Unpacked here, in the viewer's
// browser; the server never opens it.
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { Replayer } from '@rrweb/replay'
import '@rrweb/replay/dist/style.css'

import { api } from '../api.js'
import { duration } from '../format.js'

const props = defineProps({
  // The replay's batch: its id, or the batch itself when already loaded.
  id: { type: String, default: null },
  batch: { type: Object, default: null },
  // Several pieces of one page view or session, played as one, in order.
  ids: { type: Array, default: null },
  title: { type: String, default: 'Replay' },
})

const stage = ref(null)
const frame = ref(null)
const state = ref('loading') // loading | ready | failed | empty
const playing = ref(false)
const at = ref(0)
const total = ref(0)
const scale = ref(1)
const size = ref({ width: 0, height: 0 })
const content = ref({})
let replayer = null
let ticker = null

const TRIGGER = {
  failed: 'a request failed',
  slow: 'a request was slow',
  error: 'the page threw an error',
  recording: 'the visitor is recorded throughout',
}
const trigger = computed(() => TRIGGER[content.value.trigger] || 'something went wrong')
const page = computed(() => (content.value.page && content.value.page.url) || null)

async function unpack(base64) {
  const binary = atob(base64)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'))
  return JSON.parse(await new Response(stream).text())
}

// The longest pause kept between two pieces. A session's pieces are minutes
// or hours apart — the tab hidden, the visitor away — and nothing happened
// in between, so the timeline closes the gap rather than play it.
const GAP_MS = 1000

// Several pieces as one recording, in order, the time between them taken
// out. Each piece starts from a full snapshot, so it plays after any other;
// one recorded before the recorder kept the page's size with every piece
// borrows the last size seen.
function stitch(pieces) {
  const ordered = pieces
    .map((piece) => (Array.isArray(piece) ? piece.filter((e) => e && typeof e.timestamp === 'number') : []))
    .filter((piece) => piece.length)
    .map((piece) => piece.sort((a, b) => a.timestamp - b.timestamp))
    .sort((a, b) => a[0].timestamp - b[0].timestamp)
  const out = []
  let meta = null
  let shift = 0
  let end = null // the latest moment played so far; pieces can overlap
  for (const piece of ordered) {
    const start = piece[0].timestamp - shift
    if (end !== null && start - end > GAP_MS) shift += start - end - GAP_MS
    if (piece[0].type !== 4 && meta) out.push({ ...meta, timestamp: piece[0].timestamp - shift })
    for (const event of piece) {
      if (event.type === 4) meta = event
      out.push({ ...event, timestamp: event.timestamp - shift })
    }
    end = Math.max(end ?? -Infinity, piece[piece.length - 1].timestamp - shift)
  }
  return out.sort((a, b) => a.timestamp - b.timestamp)
}

function fit() {
  if (!frame.value || !size.value.width) return
  scale.value = Math.min(1, frame.value.clientWidth / size.value.width)
}

async function load() {
  try {
    const batches = props.batch
      ? [props.batch]
      : props.ids
        ? await Promise.all(props.ids.map(async (id) => (await api.batch(id)).data))
        : [(await api.batch(props.id)).data]
    const pieces = batches
      .map((b) => b.entries.find((e) => e.content && e.content.kind === 'replay'))
      .filter(Boolean)
    if (!pieces.length) {
      state.value = 'empty'
      return
    }
    content.value = pieces[pieces.length - 1].content
    const events = stitch(await Promise.all(pieces.map((p) => unpack(p.content.replay))))
    if (!Array.isArray(events) || events.length < 2) {
      state.value = 'empty'
      return
    }
    const meta = events.find((e) => e.type === 4 && e.data)
    size.value = { width: (meta && meta.data.width) || 1280, height: (meta && meta.data.height) || 800 }
    replayer = new Replayer(events, {
      root: stage.value,
      skipInactive: true,
      mouseTail: false,
      showWarning: false,
      // Nothing recorded runs: rrweb never replays scripts, and the frame
      // it draws into is sandboxed.
      UNSAFE_replayCanvas: false,
    })
    total.value = replayer.getMetaData().totalTime
    replayer.on('finish', () => {
      playing.value = false
      at.value = total.value
    })
    state.value = 'ready'
    fit()
  } catch {
    state.value = 'failed'
  }
}

function toggle() {
  if (!replayer) return
  if (playing.value) {
    replayer.pause()
    playing.value = false
  } else {
    replayer.play(at.value >= total.value ? 0 : at.value)
    playing.value = true
  }
}

function seek(event) {
  if (!replayer) return
  const box = event.currentTarget.getBoundingClientRect()
  const to = Math.max(0, Math.min(1, (event.clientX - box.left) / box.width)) * total.value
  at.value = to
  if (playing.value) replayer.play(to)
  else replayer.pause(to)
}

onMounted(() => {
  load()
  window.addEventListener('resize', fit)
  ticker = setInterval(() => {
    if (replayer && playing.value) at.value = Math.min(total.value, replayer.getCurrentTime())
  }, 100)
})
onBeforeUnmount(() => {
  window.removeEventListener('resize', fit)
  clearInterval(ticker)
  if (replayer) replayer.destroy()
})
watch(() => [props.id, props.batch, props.ids && props.ids.join()], () => {
  if (replayer) replayer.destroy()
  replayer = null
  playing.value = false
  at.value = 0
  state.value = 'loading'
  load()
})
</script>

<template>
  <section class="lk-card">
    <header class="lk-card-header">
      <h2 class="text-sm font-semibold">{{ title }}</h2>
      <span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400">
        recorded because {{ trigger }} — masked: text, inputs and media are not shown; time between recordings is skipped
      </span>
    </header>
    <div class="space-y-3 p-4">
      <p v-if="page" class="truncate font-mono text-[12px] text-zinc-500 dark:text-zinc-400">{{ page }}</p>
      <div
        ref="frame"
        class="relative w-full overflow-hidden rounded-lg border border-zinc-200 bg-white dark:border-zinc-800"
        :style="{ height: `${Math.round(size.height * scale) || 240}px` }"
      >
        <div
          ref="stage"
          class="lk-replay origin-top-left"
          :style="{ width: `${size.width}px`, height: `${size.height}px`, transform: `scale(${scale})` }"
        />
        <p v-if="state !== 'ready'" class="absolute inset-0 grid place-items-center text-sm text-zinc-500">
          {{ state === 'loading' ? 'Loading…' : state === 'empty' ? 'Nothing was recorded.' : 'This replay could not be played.' }}
        </p>
      </div>
      <div v-if="state === 'ready'" class="flex items-center gap-3">
        <button type="button" class="lk-btn" @click="toggle">{{ playing ? 'Pause' : 'Play' }}</button>
        <div class="relative h-2 flex-1 cursor-pointer rounded-full bg-zinc-200 dark:bg-zinc-800" role="slider" aria-label="Position" :aria-valuenow="Math.round(at)" :aria-valuemax="Math.round(total)" @click="seek">
          <div class="absolute inset-y-0 left-0 rounded-full bg-indigo-500" :style="{ width: `${total ? (at / total) * 100 : 0}%` }" />
        </div>
        <span class="text-xs text-zinc-500 tabular-nums dark:text-zinc-400">{{ duration(Math.round(at * 1000)) }} / {{ duration(Math.round(total * 1000)) }}</span>
      </div>
    </div>
  </section>
</template>
