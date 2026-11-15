import { inject, onBeforeUnmount, ref, watch } from 'vue'

import { config } from './config.js'

const KEY = 'lookout.live'

function stored() {
  try {
    return window.localStorage.getItem(KEY) === '1'
  } catch {
    return false
  }
}

/** The "Live" switch: one per app, remembered per browser. */
export function createLive() {
  const live = ref(stored())
  watch(live, (on) => {
    try {
      window.localStorage.setItem(KEY, on ? '1' : '0')
    } catch {
      // Storage refused (a private window): the switch still works.
    }
  })
  return live
}

/**
 * The live stream: one connection to `/api/stream` per tab, open while "Live"
 * is on, the tab is visible and the server streams (`LOOKOUT_STREAM`). Every
 * page that wants new batches subscribes here rather than opening its own.
 *
 * `connected` says whether events are arriving; while it is false, pages poll
 * instead — so a proxy that buffers the stream, or a server that has it off,
 * costs nothing but freshness.
 */
export function createStream(live) {
  const connected = ref(false)
  const listeners = new Set()
  let source = null

  const emit = (summary) => {
    for (const listen of listeners) listen(summary)
  }
  const close = () => {
    if (source) source.close()
    source = null
    connected.value = false
  }
  const open = () => {
    if (source || typeof window.EventSource !== 'function') return
    source = new window.EventSource(`${config.api}/stream`)
    source.addEventListener('open', () => {
      connected.value = true
    })
    source.addEventListener('batch', (event) => {
      try {
        emit(JSON.parse(event.data))
      } catch {
        emit(null)
      }
    })
    // Fell too far behind to be told one by one: reload everything.
    source.addEventListener('reset', () => emit(null))
    source.addEventListener('error', () => {
      // The browser reconnects by itself (every 3 s, the server's `retry`);
      // until it does, pages poll.
      connected.value = false
    })
  }
  const sync = () => {
    const wanted = config.stream && live.value && document.visibilityState === 'visible'
    if (wanted) open()
    else close()
  }

  watch(live, sync, { immediate: true })
  document.addEventListener('visibilitychange', sync)

  return {
    connected,
    /** Call `listen(summary)` for each new batch (`null`: reload). Returns the unsubscribe. */
    subscribe(listen) {
      listeners.add(listen)
      return () => listeners.delete(listen)
    },
  }
}

/**
 * Keep a page fresh while "Live" is on: on every new batch when the stream is
 * connected — at most once per `every / 2`, and at least a second apart —
 * and otherwise by asking every `every` milliseconds while the tab is visible.
 */
export function usePolling(refresh, every = 5000) {
  const live = inject('live')
  const stream = inject('stream', null)
  const gap = Math.max(1000, Math.floor(every / 2))
  let timer = null
  let pending = null
  let unsubscribe = null
  let last = 0

  const stop = () => {
    if (timer) clearInterval(timer)
    if (pending) clearTimeout(pending)
    if (unsubscribe) unsubscribe()
    timer = null
    pending = null
    unsubscribe = null
  }
  const soon = () => {
    if (pending) return
    pending = setTimeout(() => {
      pending = null
      last = Date.now()
      refresh()
    }, Math.max(0, gap - (Date.now() - last)))
  }
  const start = () => {
    stop()
    if (!live || !live.value) return
    if (stream && stream.connected.value) {
      unsubscribe = stream.subscribe(soon)
      // Catch up on whatever arrived while polling stopped.
      soon()
    } else {
      timer = setInterval(() => {
        if (document.visibilityState === 'visible') refresh()
      }, every)
    }
  }

  if (live) watch(stream ? [live, stream.connected] : [live], start, { immediate: true })
  onBeforeUnmount(stop)
}
