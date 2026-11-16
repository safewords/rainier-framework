import { RANGES } from './format.js'

const DAY = /^\d{4}-\d{2}-\d{2}$/

/** `YYYY-MM-DD` in local time, or null. */
export function parseDay(value) {
  if (typeof value !== 'string' || !DAY.test(value)) return null
  const [y, m, d] = value.split('-').map(Number)
  const date = new Date(y, m - 1, d)
  if (date.getFullYear() !== y || date.getMonth() !== m - 1 || date.getDate() !== d) return null
  return date
}

export function formatDay(date) {
  const y = date.getFullYear()
  const m = String(date.getMonth() + 1).padStart(2, '0')
  const d = String(date.getDate()).padStart(2, '0')
  return `${y}-${m}-${d}`
}

function periodFor(ms) {
  const minutes = ms / 60_000
  if (minutes <= 120) return 60
  if (minutes <= 5 * 24 * 60) return 600
  return 3600
}

function dayLabel(date) {
  return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
}

/**
 * The window the header selected, from the route query.
 * Presets run up to now. A custom range is `range=custom&from&to` as local
 * dates, inclusive. Missing or unusable values are the last 24 hours.
 */
export function parseRange(query, now = Date.now()) {
  const from = parseDay(query && query.from)
  const to = parseDay(query && query.to)
  if (query && query.range === 'custom' && from && to && to >= from) {
    const end = new Date(to.getFullYear(), to.getMonth(), to.getDate(), 23, 59, 59, 999)
    const ms = Math.max(60_000, end.getTime() - from.getTime())
    return {
      key: 'custom',
      label: from.getTime() === to.getTime() ? dayLabel(from) : `${dayLabel(from)} – ${dayLabel(to)}`,
      since: from.toISOString(),
      until: end.toISOString(),
      period: periodFor(ms),
      ms,
      preset: false,
    }
  }
  const preset = RANGES.find((r) => query && r.key === query.range) || RANGES[2]
  return {
    key: preset.key,
    label: preset.key,
    since: new Date(now - preset.hours * 3600 * 1000).toISOString(),
    until: null,
    period: preset.period,
    ms: preset.hours * 3600 * 1000,
    preset: true,
  }
}

/** A preset, or a custom range that still includes today: charts may follow now. */
export function followsNow(range, now = Date.now()) {
  if (!range || !range.until) return true
  const start = new Date(now)
  start.setHours(0, 0, 0, 0)
  return Date.parse(range.until) >= start.getTime()
}

/** The query keys that name the window, copied onto the next page. */
export function rangeQuery(query) {
  const out = {}
  if (!query || typeof query.range !== 'string' || !query.range) return out
  out.range = query.range
  if (query.range === 'custom') {
    if (typeof query.from === 'string') out.from = query.from
    if (typeof query.to === 'string') out.to = query.to
  }
  return out
}
