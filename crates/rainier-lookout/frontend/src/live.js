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
 * page that wants news subscribes here rather than opening its own.
 *
 * Two kinds of news: a `batch` the store kept, and a `pulse` — the newest
 * request, exception and job counts, sent whenever they move. Most requests
 * are counted rather than kept, so a page showing numbers listens for pulses;
 * a page listing batches or entries listens for batches.
 *
 * `connected` says whether the stream is open; while it is false, pages poll
 * instead — so a proxy that buffers the stream, or a server that has it off,
 * costs nothing but freshness.
 */
export function createStream(live) {
  const connected = ref(false)
  const batches = new Set()
  const pulses = new Set()
  let source = null

  const emit = (listeners, value) => {
    for (const listen of listeners) listen(value)
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
        emit(batches, JSON.parse(event.data))
      } catch {
        emit(batches, null)
      }
    })
    source.addEventListener('pulse', (event) => {
      try {
        emit(pulses, JSON.parse(event.data))
      } catch {
        // A pulse that does not parse is skipped; the next one replaces it.
      }
    })
    // Fell too far behind to be told one by one: reload everything.
    source.addEventListener('reset', () => {
      emit(batches, null)
      emit(pulses, null)
    })
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
      batches.add(listen)
      return () => batches.delete(listen)
    },
    /** Call `listen(pulse)` each time the counts move (`null`: reload). Returns the unsubscribe. */
    subscribePulse(listen) {
      pulses.add(listen)
      return () => pulses.delete(listen)
    },
  }
}

/** While the stream is open, a page still reloads this often, to catch anything no event announced. */
const FLOOR = 60_000

/**
 * Keep a page fresh while "Live" is on.
 *
 * With the stream connected, `refresh` runs when the news the page shows
 * arrives — `on: 'batches'` for lists of kept batches and entries, `'counts'`
 * for pages of numbers, `'both'` for pages with both — at most once per
 * `every / 2` and at least a second apart, and once a minute regardless.
 * `onPulse(pulse)`, when given, is called on every pulse at once, for a page
 * that can update its numbers in place rather than reload them.
 *
 * Without the stream, `refresh` runs every `every` milliseconds while the tab
 * is visible.
 */
export function usePolling(refresh, every = 5000, { on = 'batches', onPulse = null } = {}) {
  const live = inject('live')
  const stream = inject('stream', null)
  const gap = Math.max(1000, Math.floor(every / 2))
  let timer = null
  let pending = null
  let unsubscribe = []
  let last = 0

  const stop = () => {
    if (timer) clearInterval(timer)
    if (pending) clearTimeout(pending)
    for (const off of unsubscribe) off()
    timer = null
    pending = null
    unsubscribe = []
  }
  const soon = () => {
    if (pending) return
    pending = setTimeout(() => {
      pending = null
      last = Date.now()
      refresh()
    }, Math.max(0, gap - (Date.now() - last)))
  }
  const visible = () => document.visibilityState === 'visible'
  const start = () => {
    stop()
    if (!live || !live.value) return
    if (stream && stream.connected.value) {
      if (on === 'batches' || on === 'both') unsubscribe.push(stream.subscribe(soon))
      if (on === 'counts' || on === 'both') {
        unsubscribe.push(
          stream.subscribePulse((pulse) => {
            if (pulse && onPulse) onPulse(pulse)
            soon()
          }),
        )
      }
      timer = setInterval(() => visible() && soon(), FLOOR)
      // Catch up on whatever arrived while polling stopped.
      soon()
    } else {
      timer = setInterval(() => visible() && refresh(), every)
    }
  }

  if (live) watch(stream ? [live, stream.connected] : [live], start, { immediate: true })
  onBeforeUnmount(stop)
}

/**
 * Fold a pulse's points into a loaded series in place: a point whose bucket
 * is already there replaces it, a newer one is appended and the oldest
 * dropped, so the range keeps its length. `series` is a `/api/series`
 * answer; `type` and `period` pick the pulse's points. Returns whether
 * anything changed.
 */
export function mergePulse(series, pulse, type, period) {
  const points = pulse && pulse[type] && pulse[type][String(period)]
  if (!series || !Array.isArray(series.data) || !Array.isArray(points)) return false
  let changed = false
  for (const point of points) {
    const at = series.data.findIndex((p) => p.bucket_start === point.bucket_start)
    if (at >= 0) {
      series.data.splice(at, 1, point)
      changed = true
    } else if (!series.data.length || point.bucket_start > series.data[series.data.length - 1].bucket_start) {
      series.data.push(point)
      series.data.shift()
      changed = true
    }
  }
  return changed
}
