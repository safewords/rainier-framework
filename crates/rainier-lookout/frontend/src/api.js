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

async function request(method, path, params, headers) {
  let response
  try {
    response = await fetch(`${config.api}${path}${query(params)}`, {
      method,
      credentials: 'same-origin',
      headers: { Accept: 'application/json', ...(headers || {}) },
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
  batches: (params) => request('GET', '/batches', params),
  batch: (id) => request('GET', `/batches/${path(id)}`),
  entries: (type, params) => request('GET', `/entries/${path(type)}`, params),
  aggregates: (type, params) => request('GET', `/aggregates/${path(type)}`, params),
  status: () => request('GET', '/status'),
  clear: () => request('DELETE', '/batches', null, { 'X-Lookout': '1' }),
}
