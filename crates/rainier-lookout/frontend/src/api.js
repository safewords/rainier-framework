import { config } from './config.js'

/** An API failure, with Rainier's validation errors when there are any. */
export class ApiError extends Error {
  constructor(status, message, errors) {
    super(message)
    this.status = status
    this.errors = errors || null
  }
}

function query(params) {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params || {})) {
    if (value === undefined || value === null || value === '' || value === false) continue
    search.set(key, value === true ? '1' : String(value))
  }
  const text = search.toString()
  return text ? `?${text}` : ''
}

async function request(method, path, params, headers, payload) {
  let response
  try {
    response = await fetch(`${config.api}${path}${query(params)}`, {
      method,
      credentials: 'same-origin',
      headers: {
        Accept: 'application/json',
        ...(payload !== undefined ? { 'Content-Type': 'application/json' } : {}),
        ...(headers || {}),
      },
      body: payload !== undefined ? JSON.stringify(payload) : undefined,
    })
  } catch {
    throw new ApiError(0, 'Lookout could not be reached.')
  }
  if (response.status === 204) return null
  let body = null
  try {
    body = await response.json()
  } catch {
    body = null
  }
  if (!response.ok) {
    const message = (body && body.message) || `Request failed (${response.status}).`
    throw new ApiError(response.status, message, body && body.errors)
  }
  return body
}

const path = (value) => encodeURIComponent(value)

export const api = {
  // params: kind, type, route, user, tag, trace, exception, slow, q (search), before, limit
  batches: (params) => request('GET', '/batches', params),
  batch: (id) => request('GET', `/batches/${path(id)}`),
  // params: before, limit, key (one family), q (search summary/key)
  entries: (type, params) => request('GET', `/entries/${path(type)}`, params),
  // params: period (60|600|3600), since (RFC 3339), order (count|average|max), limit
  aggregates: (type, params) => request('GET', `/aggregates/${path(type)}`, params),
  // params: since, period?, key?, keys (N busiest keys' own series), forecast (buckets)
  // -> { data: [{bucket_start,count,sum_us,max_us,average_us}], forecast: [{bucket_start,count,average_us}], period, keys: {key: points} }
  series: (type, params) => request('GET', `/series/${path(type)}`, params),
  // params: window (minutes), min, change (percent), forecast, limit
  // -> { data: [{kind,severity,type,key,current,previous,change_percent}], config, enabled }
  insights: (params) => request('GET', '/insights', params),
  // params: status (open|resolved|ignored), severity (minimum: low|medium|high|critical), q, limit
  cases: (params) => request('GET', '/cases', params),
  kase: (id) => request('GET', `/cases/${path(id)}`),
  // action: resolve | reopen | ignore | severity (with severity), note optional
  actOnCase: (id, body) => request('POST', `/cases/${path(id)}`, null, { 'X-Lookout': '1' }, body),
  status: () => request('GET', '/status'),
  clear: () => request('DELETE', '/batches', null, { 'X-Lookout': '1' }),
}
