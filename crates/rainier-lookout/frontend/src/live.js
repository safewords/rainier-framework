import { inject, onBeforeUnmount, ref, watch } from 'vue'

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

/** Run `refresh` every few seconds while Live is on and the tab is visible. */
export function usePolling(refresh, every = 5000) {
  const live = inject('live')
  let timer = null
  const stop = () => {
    if (timer) clearInterval(timer)
    timer = null
  }
  const start = () => {
    stop()
    if (live && live.value) {
      timer = setInterval(() => {
        if (document.visibilityState === 'visible') refresh()
      }, every)
    }
  }
  if (live) watch(live, start, { immediate: true })
  onBeforeUnmount(stop)
}
