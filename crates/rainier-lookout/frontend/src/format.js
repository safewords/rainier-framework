/** `850µs`, `12.3ms`, `1.24s`, `2m 05s`. */
export function duration(us) {
  if (us === null || us === undefined) return '—'
  if (us < 1000) return `${us}µs`
  if (us < 1_000_000) {
    const ms = us / 1000
    return ms < 10 ? `${ms.toFixed(2)}ms` : ms < 100 ? `${ms.toFixed(1)}ms` : `${Math.round(ms)}ms`
  }
  if (us < 60_000_000) return `${(us / 1_000_000).toFixed(2)}s`
  const s = Math.floor(us / 1_000_000)
  return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`
}

/** "12s ago". */
export function ago(iso, now = Date.now()) {
  const seconds = (now - Date.parse(iso)) / 1000
  if (Number.isNaN(seconds)) return ''
  if (seconds < 1) return 'just now'
  if (seconds < 60) return `${Math.floor(seconds)}s ago`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`
  return `${Math.floor(seconds / 86400)}d ago`
}

export function absolute(iso) {
  const at = new Date(iso)
  return Number.isNaN(at.getTime()) ? iso : at.toLocaleString()
}

export function average(row) {
  return row.count ? Math.floor(row.sum_us / row.count) : 0
}

export const KINDS = [
  { key: 'request', label: 'Requests' },
  { key: 'job', label: 'Jobs' },
  { key: 'command', label: 'Commands' },
  { key: 'scheduled_task', label: 'Scheduled' },
  { key: 'other', label: 'Other' },
  { key: 'browser', label: 'Browser' },
]

export const TYPES = {
  request: 'Requests',
  query: 'Queries',
  cache: 'Cache',
  http_client: 'Outgoing HTTP',
  job: 'Queued jobs',
  mail: 'Mail',
  notification: 'Notifications',
  event: 'Events',
  scheduled_task: 'Scheduled tasks',
  command: 'Commands',
  exception: 'Exceptions',
  log: 'Logs',
  model: 'Models',
  view: 'Views',
  broadcast: 'Broadcasts',
  session: 'Sessions',
  auth_attempt: 'Auth attempts',
  n_plus_one: 'N+1 queries',
  slow: 'Slow',
  custom: 'Custom',
}

export function hasTag(tags, tag) {
  return Array.isArray(tags) && tags.includes(tag)
}

export function isNPlusOne(tags) {
  return hasTag(tags, 'n_plus_one') || hasTag(tags, 'n+1')
}

/** The colour family for an HTTP status. */
export function statusTone(status) {
  if (status >= 500) return 'bad'
  if (status >= 400) return 'warn'
  if (status >= 300) return 'info'
  return 'ok'
}

/** A case's id for a fingerprint: 16 hex digits of 64-bit FNV-1a over its UTF-8 bytes (`cases::case_id`). */
export function caseId(key) {
  const MASK = (1n << 64n) - 1n
  let hash = 0xcbf29ce484222325n
  for (const byte of new TextEncoder().encode(String(key ?? ''))) {
    hash ^= BigInt(byte)
    hash = (hash * 0x100000001b3n) & MASK
  }
  return hash.toString(16).padStart(16, '0')
}

/** The `p`th percentile (0–100) of a list of numbers, nearest-rank; null when empty. */
export function percentile(values, p) {
  const sorted = values.filter((v) => typeof v === 'number' && Number.isFinite(v)).sort((a, b) => a - b)
  if (!sorted.length) return null
  const rank = Math.ceil((p / 100) * sorted.length)
  return sorted[Math.min(sorted.length - 1, Math.max(0, rank - 1))]
}

/** `1,234`. */
export function count(n) {
  return n === null || n === undefined ? '—' : Number(n).toLocaleString()
}

/** The time ranges the dashboard and the job view offer, with the bucket length each reads. */
export const RANGES = [
  { key: '1h', period: 60, hours: 1 },
  { key: '6h', period: 600, hours: 6 },
  { key: '24h', period: 600, hours: 24 },
  { key: '7d', period: 3600, hours: 168 },
]

/** The start of a range ending now, as RFC 3339. */
export function since(hours, now = Date.now()) {
  return new Date(now - hours * 3600 * 1000).toISOString()
}

/** `0.42`, `4.2`, `42` — a rate with as many decimals as it needs. */
export function rate(n) {
  if (n === null || n === undefined || !Number.isFinite(n)) return '—'
  if (n === 0) return '0'
  if (n < 10) return n.toFixed(2)
  if (n < 100) return n.toFixed(1)
  return Math.round(n).toLocaleString()
}

/** `45%`, `+45%` with `signed`. */
export function percent(n, signed = false) {
  if (n === null || n === undefined || !Number.isFinite(n)) return '—'
  const rounded = Math.abs(n) < 10 ? Math.round(n * 10) / 10 : Math.round(n)
  return `${signed && rounded > 0 ? '+' : ''}${rounded}%`
}

/** Totals over series points: `{ count, sum_us, max_us, average_us }`. */
export function totals(points) {
  let n = 0
  let sum = 0
  let max = 0
  for (const p of points || []) {
    n += p.count || 0
    sum += p.sum_us || 0
    max = Math.max(max, p.max_us || 0)
  }
  return { count: n, sum_us: sum, max_us: max, average_us: n ? Math.floor(sum / n) : 0 }
}

/**
 * A family's short, stable id — the server's `family_id`: FNV-1a over the
 * type, a newline and the key. What a URL carries instead of the key, which
 * for a query is its SQL.
 */
export function familyId(type, key) {
  return caseId(`${type}\n${key ?? ''}`)
}

/** The drill-down page for one family of one type, by id. */
export function issueRoute(type, key) {
  return { name: 'issue', params: { type, id: familyId(type, key) } }
}

/** An entry's family key: its fingerprint, or its summary when it has none. */
export function familyKey(entry) {
  return entry.family_hash || entry.summary
}

/** `850 µs`, `189 ms`, `1.2 s`, `2 min 05 s` — a duration in microseconds, units spelled apart. */
export function time(us) {
  if (us === null || us === undefined || !Number.isFinite(us)) return '—'
  if (us < 1000) return `${Math.round(us)} µs`
  if (us < 1_000_000) {
    const ms = us / 1000
    return `${ms < 10 ? ms.toFixed(2) : ms < 100 ? ms.toFixed(1) : Math.round(ms)} ms`
  }
  if (us < 60_000_000) {
    const s = us / 1_000_000
    return `${s < 10 ? s.toFixed(2) : s.toFixed(1)} s`
  }
  const s = Math.floor(us / 1_000_000)
  if (s < 3600) return `${Math.floor(s / 60)} min ${String(s % 60).padStart(2, '0')} s`
  return `${Math.floor(s / 3600)} h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')} min`
}

/** A value from an entry's content: the first of `keys` present, dotted names looked up as-is. */
export function field(content, ...keys) {
  if (!content || typeof content !== 'object') return null
  for (const key of keys) {
    const v = content[key]
    if (v !== undefined && v !== null && v !== '') return v
  }
  return null
}

/**
 * Add up aggregate histograms (`{ slot: count }`, as the API sends them).
 * Rows written before histograms were kept have none and add nothing.
 */
export function mergeHistograms(histograms) {
  const out = {}
  for (const h of histograms) {
    if (!h) continue
    for (const [slot, n] of Object.entries(h)) out[slot] = (out[slot] || 0) + n
  }
  return out
}

/**
 * The `p`th percentile, in microseconds, of a merged histogram — the same
 * reading as the server's `aggregate::percentile`: slot `s ≥ 1` holds
 * `[2^((s-1)/4), 2^(s/4))` µs; the nearest rank is placed inside its slot by
 * how far into the slot's count it falls, and never above `max` (the
 * slowest, known exactly). `null` when empty.
 */
export function histogramPercentile(histogram, p, max) {
  const slots = Object.entries(histogram || {}).map(([s, n]) => [Number(s), n]).sort((a, b) => a[0] - b[0])
  const total = slots.reduce((t, [, n]) => t + n, 0)
  if (!total) return null
  const rank = Math.max(1, Math.ceil((p / 100) * total))
  let seen = 0
  for (const [slot, n] of slots) {
    if (seen + n >= rank) {
      if (slot === 0) return 0
      const value = Math.round(2 ** ((slot - 1 + (rank - seen) / n) / 4))
      return typeof max === 'number' ? Math.min(value, max) : value
    }
    seen += n
  }
  return max ?? null
}
