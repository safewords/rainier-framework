/*
 * Lookout real user monitoring — how this page's requests to the API looked
 * from the browser. Served by the API at `{lookout path}/rum.js`; see the
 * framework's docs/lookout.md, "Real user monitoring".
 *
 * It reads the browser's own Resource Timing for each request to the API
 * that carries a Lookout token (`Server-Timing: lookout;desc=…`), adds what
 * the page knows about the device, the connection and its own load, and
 * reports in batches with `navigator.sendBeacon` — a plain-text POST that
 * needs no CORS preflight and is sent even as the page closes.
 *
 * It sends nothing it was not configured to: the URL's query and fragment are
 * dropped, and nothing on the page is read but timing and device facts.
 */
(function () {
  'use strict'
  var settings = __LOOKOUT_RUM_SETTINGS__
  var tag = document.currentScript
  // Where to report, and whose requests to report on: this script's own
  // origin and path, so a page needs nothing but the tag.
  if (!tag || !tag.src || !window.URL) return
  var self = new URL(tag.src)
  settings.origin = self.origin
  settings.endpoint = new URL('rum', self.href).href
  if (tag.dataset) {
    if (tag.dataset.sample !== undefined) settings.sample = Number(tag.dataset.sample)
    if (tag.dataset.slowMs !== undefined) settings.slowMs = Number(tag.dataset.slowMs)
  }
  if (!window.PerformanceObserver || !navigator.sendBeacon || window.__lookoutRum) return
  // A visitor who asked not to be tracked (Global Privacy Control) is not
  // measured either. This is performance monitoring, not tracking, but the
  // report does describe their device; the request is honoured broadly.
  if (navigator.globalPrivacyControl === true) return
  window.__lookoutRum = true

  // One decision per page view, so a sampled page reports all its requests
  // and its story is whole. A slow request is reported regardless.
  var sampled = Math.random() < settings.sample
  var queue = []
  var vitals = {}

  function round(n) {
    return typeof n === 'number' && isFinite(n) ? Math.round(n * 10) / 10 : undefined
  }
  function bare(url) {
    return String(url).split('#')[0].split('?')[0]
  }
  function metric(entry, name) {
    var timings = entry.serverTiming || []
    for (var i = 0; i < timings.length; i++) {
      if (timings[i].name === name) return timings[i]
    }
    return null
  }
  function tokenOf(entry) {
    var m = metric(entry, settings.metric)
    return m ? m.description : null
  }

  function call(entry, token) {
    // Phases are zero when the browser withholds them: without
    // Timing-Allow-Origin naming this page, or for a reused connection.
    var dns = entry.domainLookupEnd - entry.domainLookupStart
    var connect = entry.connectEnd - entry.connectStart
    var tls = entry.secureConnectionStart > 0 ? entry.connectEnd - entry.secureConnectionStart : 0
    var waiting = entry.requestStart > 0 ? entry.responseStart - entry.requestStart : 0
    var download = entry.responseStart > 0 ? entry.responseEnd - entry.responseStart : 0
    var queued = entry.requestStart > 0 ? Math.max(0, entry.requestStart - entry.startTime - dns - connect) : 0
    return {
      token: token,
      url: bare(entry.name),
      initiator: entry.initiatorType,
      duration: round(entry.duration),
      queued: round(queued),
      dns: round(dns),
      connect: round(connect - tls),
      tls: round(tls),
      waiting: round(waiting),
      download: round(download),
      protocol: entry.nextHopProtocol || undefined,
      transfer: entry.transferSize,
      encoded: entry.encodedBodySize,
      decoded: entry.decodedBodySize,
      status: entry.responseStatus || undefined,
      at: round(entry.startTime),
      // The server's own time, and the route that answered, as the response
      // said in its Server-Timing — so the report can split the wait even
      // when the request's own batch was not kept.
      server: round((metric(entry, settings.metric) || {}).duration),
      route: (metric(entry, settings.metric + '-route') || {}).description || undefined,
    }
  }

  function device() {
    var c = navigator.connection || {}
    var d = {
      language: navigator.language,
      timezone: (Intl.DateTimeFormat().resolvedOptions() || {}).timeZone,
      screen: screen.width + 'x' + screen.height,
      viewport: innerWidth + 'x' + innerHeight,
      dpr: devicePixelRatio,
      touch: navigator.maxTouchPoints || 0,
      cores: navigator.hardwareConcurrency,
      memory: navigator.deviceMemory,
      connection: c.effectiveType ? { type: c.effectiveType, rtt: c.rtt, downlink: c.downlink, save_data: !!c.saveData } : undefined,
    }
    var ua = navigator.userAgentData
    if (ua) {
      d.mobile = ua.mobile
      d.platform = ua.platform
    }
    return d
  }

  function page() {
    var nav = (performance.getEntriesByType('navigation') || [])[0]
    var p = {
      url: bare(location.href),
      referrer: document.referrer ? bare(document.referrer) : undefined,
      visibility: document.visibilityState,
      age: round(performance.now()),
    }
    if (nav) {
      p.load = {
        type: nav.type,
        ttfb: round(nav.responseStart),
        dom_ready: round(nav.domContentLoadedEventEnd),
        load: round(nav.loadEventEnd),
      }
    }
    p.vitals = vitals
    return p
  }

  function flush() {
    if (!queue.length) return
    var calls = queue.splice(0, 50)
    var body = JSON.stringify({ page: page(), device: device(), calls: calls })
    try {
      navigator.sendBeacon(settings.endpoint, new Blob([body], { type: 'text/plain' }))
    } catch (e) {
      // Nothing to be done: reporting must never break the page.
    }
    if (queue.length) flush()
  }

  function observe(type, handle, options) {
    try {
      new PerformanceObserver(function (list) {
        list.getEntries().forEach(handle)
      }).observe(Object.assign({ type: type, buffered: true }, options || {}))
    } catch (e) {
      // This browser does not know the type.
    }
  }

  // Lookout's own path on the API: the script and its reports are not
  // requests to report on.
  var own = self.href.replace(/rum\.js(\?.*)?$/, '')

  // --- session replay (LOOKOUT_RUM_REPLAY) -----------------------------------
  //
  // Off unless the API turned it on, and then only on a page that is not
  // excluded and whose visitor agreed (or where the site says consent is
  // implied). The recorder keeps the last two stretches of the page in
  // memory — each starting from a full snapshot, at most ~15 s apart — and
  // sends them only when a request to the API fails or is slow, or the page
  // throws. Everything is masked as it is recorded: text becomes asterisks,
  // inputs are never read, images, video, canvases and frames are blank
  // boxes, and anything marked data-lookout-block is left out entirely.
  var replay = { on: false, started: false, segments: [[]], lastToken: null, sent: 0, lastSent: 0 }
  var replaySettings = settings.replay || {}

  function excluded(path) {
    var globs = replaySettings.except || []
    for (var i = 0; i < globs.length; i++) {
      var pattern = new RegExp('^' + globs[i].split('*').map(function (part) {
        return part.replace(/[.+?^${}()|[\]\\]/g, '\\$&')
      }).join('.*') + '$')
      if (pattern.test(path)) return true
    }
    return false
  }

  function startReplay() {
    if (!replaySettings.enabled || replay.started || excluded(location.pathname)) return
    if (typeof CompressionStream !== 'function') return
    replay.started = true
    var s = document.createElement('script')
    s.src = new URL('rum-replay.js', self.href).href
    s.async = true
    s.onload = function () {
      if (!window.LookoutReplay) return
      window.LookoutReplay.record({
        emit: function (event, isCheckout) {
          // A new stretch at each full snapshot; only the last two are kept.
          if (isCheckout) {
            replay.segments.push([])
            if (replay.segments.length > 2) replay.segments.shift()
          }
          replay.segments[replay.segments.length - 1].push(event)
        },
        checkoutEveryNms: 15000,
        maskAllInputs: true,
        maskTextSelector: '*',
        blockSelector: 'img,picture,video,audio,canvas,iframe,object,embed,svg image,[data-lookout-block]',
        inlineImages: false,
        recordCanvas: false,
        collectFonts: false,
        inlineStylesheet: true,
        slimDOMOptions: 'all',
        sampling: { mousemove: 100, scroll: 150, input: 'last' },
      })
      replay.on = true
    }
    document.head.appendChild(s)
  }

  function upload(trigger) {
    if (!replay.on || !replay.lastToken) return
    // A page that keeps failing sends a few recordings, not one per failure.
    var now = Date.now()
    if (replay.sent >= 3 || now - replay.lastSent < 30000) return
    var events = [].concat.apply([], replay.segments)
    if (events.length < 2) return
    replay.sent++
    replay.lastSent = now
    var token = replay.lastToken
    var ms = events[events.length - 1].timestamp - events[0].timestamp
    var gzip = new Blob([JSON.stringify(events)]).stream().pipeThrough(new CompressionStream('gzip'))
    new Response(gzip).arrayBuffer().then(function (buffer) {
      if (buffer.byteLength > (replaySettings.maxBytes || 262144)) return
      var bytes = new Uint8Array(buffer)
      var binary = ''
      for (var i = 0; i < bytes.length; i += 0x8000) {
        binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000))
      }
      var body = JSON.stringify({ token: token, trigger: trigger, page: page(), device: device(), replay: btoa(binary), ms: ms })
      // A plain-text POST: no CORS preflight, like the beacon — but not a
      // beacon, which browsers cap at 64 KB.
      return fetch(settings.endpoint + '/replay', { method: 'POST', body: body, credentials: 'omit', mode: 'cors' })
    }).catch(function () {
      // A recording that could not be sent is simply lost.
    })
  }

  window.LookoutRum = {
    /** Let this page record for session replay, from the site's own consent prompt. */
    allowReplay: function () {
      replay.allowed = true
      startReplay()
    },
  }
  if (replaySettings.consent === 'implied' || window.LookoutRumConsent === true) {
    window.LookoutRum.allowReplay()
  }
  addEventListener('error', function () {
    upload('error')
  })
  addEventListener('unhandledrejection', function () {
    upload('error')
  })

  observe('resource', function (entry) {
    if (entry.name.indexOf(settings.origin) !== 0) return
    if (entry.initiatorType === 'beacon' || entry.name.indexOf(own) === 0) return
    var token = tokenOf(entry)
    if (!token) return
    replay.lastToken = token
    var failed = entry.responseStatus >= 400
    var slow = entry.duration >= settings.slowMs
    if (failed || slow) upload(failed ? 'failed' : 'slow')
    if (!sampled && !slow) return
    queue.push(call(entry, token))
    if (queue.length >= 20) flush()
  })

  // The page's own vitals, for context: the request's story is told inside it.
  observe('paint', function (entry) {
    if (entry.name === 'first-contentful-paint') vitals.fcp = round(entry.startTime)
  })
  observe('largest-contentful-paint', function (entry) {
    vitals.lcp = round(entry.startTime)
  })
  observe('layout-shift', function (entry) {
    if (!entry.hadRecentInput) vitals.cls = Math.round(((vitals.cls || 0) + entry.value) * 1000) / 1000
  })
  observe('event', function (entry) {
    if (!vitals.inp || entry.duration > vitals.inp) vitals.inp = round(entry.duration)
  }, { durationThreshold: 40 })

  setInterval(flush, 10000)
  addEventListener('visibilitychange', function () {
    if (document.visibilityState === 'hidden') flush()
  })
  addEventListener('pagehide', flush)
})()
