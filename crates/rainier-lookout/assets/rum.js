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

  // This page view, and what the page itself has done since its last report:
  // its load (once), its route changes, its script errors, its long tasks.
  // Reported as a `page` batch, apart from the API calls, and joined to them
  // by the view's id. `proof` is the latest API token this page received:
  // a frontend on another origin is not served by the API, so the only thing
  // that can vouch for its activity is a response the API sent it.
  var view = (function () {
    var bytes = new Uint8Array(16)
    ;(window.crypto || {}).getRandomValues ? crypto.getRandomValues(bytes) : bytes.forEach(function (_, i) { bytes[i] = Math.random() * 256 })
    return Array.prototype.map.call(bytes, function (b) { return ('0' + b.toString(16)).slice(-2) }).join('')
  })()
  var proof = null

  // The session: the application's, when the API names one
  // (lookout-session), else one the script keeps per tab, renewed after 30
  // idle minutes — so page views, API calls and replays can be laid out as
  // one timeline. And the user, as the API signed it (lookout-user).
  var serverSession = null
  var user = null
  var browserSession = (function () {
    try {
      var now = Date.now()
      var held = JSON.parse(sessionStorage.getItem('lookout.session') || 'null')
      var id = held && now - held.last < 30 * 60 * 1000 ? held.id : view
      sessionStorage.setItem('lookout.session', JSON.stringify({ id: id, last: now }))
      return id
    } catch (e) {
      return view
    }
  })()
  function session() {
    return serverSession || browserSession
  }
  var activity = { loadSent: false, navigations: [], errors: [], longTasks: { count: 0, total: 0, longest: 0 } }

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

  // What the page did since its last report, or null when there is nothing
  // to say. A sampled page view reports its activity; any page view reports
  // a script error.
  function pageActivity() {
    var a = activity
    var news = !a.loadSent || a.navigations.length || a.errors.length || a.longTasks.count
    if (!news || !(sampled || a.errors.length)) return null
    var out = {
      navigations: a.navigations.splice(0),
      errors: a.errors.splice(0),
      long_tasks: a.longTasks.count ? { count: a.longTasks.count, total: round(a.longTasks.total), longest: round(a.longTasks.longest) } : undefined,
      vitals: vitals,
    }
    a.longTasks = { count: 0, total: 0, longest: 0 }
    if (!a.loadSent) {
      var nav = (performance.getEntriesByType('navigation') || [])[0]
      if (nav) {
        out.load = {
          type: nav.type,
          dns: round(nav.domainLookupEnd - nav.domainLookupStart),
          connect: round(nav.connectEnd - nav.connectStart),
          ttfb: round(nav.responseStart),
          dom_ready: round(nav.domContentLoadedEventEnd),
          load: round(nav.loadEventEnd),
          transfer: nav.transferSize,
          protocol: nav.nextHopProtocol || undefined,
        }
      }
      a.loadSent = true
    }
    return out
  }

  function flush() {
    // Activity waits for a token to vouch for it; calls carry their own.
    var act = proof ? pageActivity() : null
    if (!queue.length && !act) return
    var calls = queue.splice(0, 50)
    var body = JSON.stringify({ view: view, session: session(), user: user || undefined, proof: proof || undefined, page: page(), device: device(), calls: calls, activity: act || undefined })
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
    s.src = new URL('rum-replay.js?v=' + (replaySettings.recorder || ''), self.href).href
    s.async = true
    s.onload = function () {
      if (!window.LookoutReplay) return
      window.LookoutReplay.record({
        emit: function (event, isCheckout) {
          // A new stretch at each full snapshot; only the last two are kept.
          if (isCheckout) {
            // Recording everything: the stretch just finished goes up now.
            if (always) send(replay.segments[replay.segments.length - 1], 'recording', false)
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

  // 'always': the whole page view, piece by piece. 'errors' (the default):
  // only the last ~30 s, and only when something goes wrong.
  var always = replaySettings.mode === 'always'

  function send(events, trigger, leaving) {
    if (!events || events.length < 2 || !replay.lastToken) return
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
      var body = JSON.stringify({
        token: token, trigger: trigger, view: view, session: session(), user: user || undefined,
        page: page(), device: device(), replay: btoa(binary), ms: ms,
      })
      // A plain-text POST: no CORS preflight, like the beacon — but not a
      // beacon, which browsers cap at 64 KB. As the page goes away, keepalive
      // lets a small last piece out (the browser's limit is also 64 KB).
      return fetch(settings.endpoint + '/replay', {
        method: 'POST', body: body, credentials: 'omit', mode: 'cors', keepalive: !!leaving && body.length < 60000,
      })
    }).catch(function () {
      // A recording that could not be sent is simply lost.
    })
  }

  function upload(trigger) {
    // Recording everything already sends it all; nothing to add.
    if (!replay.on || always) return
    // A page that keeps failing sends a few recordings, not one per failure.
    var now = Date.now()
    if (replay.sent >= 3 || now - replay.lastSent < 30000) return
    var events = [].concat.apply([], replay.segments)
    if (events.length < 2) return
    replay.sent++
    replay.lastSent = now
    send(events, trigger, false)
  }

  // The piece in progress when the page is hidden or closed.
  function leave() {
    if (!replay.on || !always) return
    var current = replay.segments[replay.segments.length - 1]
    replay.segments[replay.segments.length - 1] = []
    send(current, 'recording', true)
  }
  addEventListener('pagehide', leave)
  addEventListener('visibilitychange', function () {
    if (document.visibilityState === 'hidden') leave()
    // Back again: start the next piece from a full snapshot, so it plays on
    // its own as well as after the one before.
    else if (replay.on && always && window.LookoutReplay && window.LookoutReplay.record.takeFullSnapshot) {
      window.LookoutReplay.record.takeFullSnapshot(true)
    }
  })

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
    proof = token
    var s = metric(entry, settings.metric + '-session')
    if (s && s.description) serverSession = s.description
    var u = metric(entry, settings.metric + '-user')
    if (u && u.description) user = u.description
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

  // --- the page's own activity ----------------------------------------------

  // Route changes inside the app: what a single-page frontend calls a page.
  function routed() {
    var to = location.pathname
    var last = activity.navigations[activity.navigations.length - 1]
    if (last && last.to === to) return
    if (activity.navigations.length < 50) activity.navigations.push({ to: to, at: round(performance.now()) })
  }
  ;['pushState', 'replaceState'].forEach(function (name) {
    var original = history[name]
    if (typeof original !== 'function') return
    history[name] = function () {
      var result = original.apply(this, arguments)
      try {
        routed()
      } catch (e) {
        // Never in the app's way.
      }
      return result
    }
  })
  addEventListener('popstate', routed)

  // Script errors: the message and where, never the page's data.
  function failed(message, source, line, col) {
    if (activity.errors.length >= 20) return
    activity.errors.push({
      message: String(message || 'error').slice(0, 300),
      source: source ? bare(source) : undefined,
      line: line || undefined,
      col: col || undefined,
      at: round(performance.now()),
      page: location.pathname,
    })
  }
  addEventListener('error', function (event) {
    // A resource that failed to load is an error event without a message.
    if (event && event.message !== undefined) failed(event.message, event.filename, event.lineno, event.colno)
  })
  addEventListener('unhandledrejection', function (event) {
    var reason = event && event.reason
    failed('Unhandled rejection: ' + (reason && reason.message ? reason.message : String(reason)))
  })

  // Long tasks: the main thread busy for 50 ms or more — a frozen page.
  observe('longtask', function (entry) {
    activity.longTasks.count++
    activity.longTasks.total += entry.duration
    if (entry.duration > activity.longTasks.longest) activity.longTasks.longest = entry.duration
  })

  setInterval(flush, 10000)
  addEventListener('visibilitychange', function () {
    if (document.visibilityState === 'hidden') flush()
  })
  addEventListener('pagehide', flush)
})()
