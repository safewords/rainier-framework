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
